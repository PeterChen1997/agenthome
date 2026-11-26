# Linux 采集端安装

在 agent 所属的 Linux 用户下构建或安装。用户的主目录在开机时须可访问。采集端只在本地写用量库。Mac 通过已有的内网或 Tailscale SSH 服务拉取用量。

## 本机构建和打包

准备仓库锁定的 Rust 工具链、本机 C 工具链和 glibc 开发文件。在与目标机架构和 glibc 兼容的 Linux 上，从仓库根目录执行：

```bash
(
  set -eu
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
  export CARGO_PROFILE_RELEASE_STRIP=debuginfo
  export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=/build-home"
  cargo build --locked --release -p agenthome-cli --bin agenthome

  package="agenthome-linux-$(uname -m)"
  mkdir -p "dist/$package"
  install -m 0755 "$CARGO_TARGET_DIR/release/agenthome" "dist/$package/agenthome"
  install -m 0644 script/agenthome-collector.service LICENSE NOTICE THIRD_PARTY_LICENSES.md "dist/$package/"
  cd dist
  (
    cd "$package"
    sha256sum agenthome agenthome-collector.service LICENSE NOTICE THIRD_PARTY_LICENSES.md > SHA256SUMS
  )
  tar --format=ustar --owner=0 --group=0 --numeric-owner -czf "$package.tar.gz" \
    "$package"/{agenthome,agenthome-collector.service,LICENSE,NOTICE,THIRD_PARTY_LICENSES.md,SHA256SUMS}
  sha256sum "$package.tar.gz" > "$package.tar.gz.sha256"
)
```

## 安装与验收

把压缩包及同名 `.sha256` 文件放在一起，在该目录校验并安装。以下使用包名中的架构后缀匹配当前机器。

```bash
(
  set -eu
  package="agenthome-linux-$(uname -m)"
  sha256sum --check --strict "$package.tar.gz.sha256"
  tar -xzf "$package.tar.gz"
  cd "$package"
  sha256sum --check --strict SHA256SUMS

  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  install -Dm0755 agenthome "$HOME/.local/bin/agenthome"
  install -Dm0644 agenthome-collector.service "$unit_dir/agenthome-collector.service"
  systemctl --user daemon-reload
)
```

重新安装前，先按下文停用服务。保留相同的安装位置，SSH 固定命令会引用该位置。

### 检查资源控制模式

启动前看 cgroup 的挂载类型：

```bash
stat -fc %T /sys/fs/cgroup
findmnt -t cgroup,cgroup2 -o TARGET,FSTYPE
```

- **统一 cgroup v2**。第一条命令输出 `cgroup2fs`。用户管理器获得 `cpu`、`memory`、`pids`、`io` 控制器时，单元配置的 CPU 配额、内存上限、任务上限和 I/O 权重可生效。实际值按下文在内核里核对。
- **cgroup v1 或混合模式**。只有 `cgroup` 挂载为 v1，`cgroup` 与 `cgroup2` 同时挂载为混合模式。这两种模式下，用户服务配置的 `CPUQuota`、`MemoryHigh`、`MemoryMax`、`TasksMax` 与 `IOWeight` 不生效，即使 `systemctl show` 显示了配置。此时采集端依靠进程内预算（并发 1、源读取预算 2 MiB/s、每轮分片预算 64 MiB、状态目录预算 512 MiB）以及进程级调度优先级和创建任务限制。预算的具体行为见[远程用量库](14-远程用量库.md#采集服务调度与资源上限单元-10)。

两种模式都设置 `Nice=19` 和 `IOSchedulingClass=idle`，降低 CPU 和 I/O 调度优先级。`LimitNPROC=512` 给采集进程及其后代设置软硬限额。创建进程或线程时，Linux 按真实 UID 汇总该用户已有的线程数，达到限额时会拒绝采集进程继续创建任务。共享用户下其他进程的线程也计入这个阈值。

### 验证启动与自启

启用 linger，使 systemd 开机时启动用户管理器并在退出登录后保留它。命令若需要管理员权限，请由机器管理员为该用户启用，再确认 `Linger=yes`。

```bash
loginctl enable-linger "$(id -un)"
loginctl show-user "$(id -un)" -p Linger
```

先启动并检查运行状态与进程级限制：

```bash
unit=agenthome-collector.service
systemctl --user start "$unit"
systemctl --user status "$unit" --no-pager
systemctl --user show "$unit" -p ActiveState -p MainPID -p Nice \
  -p IOSchedulingClass -p LimitNPROC -p LimitNPROCSoft
pid=$(systemctl --user show "$unit" -p MainPID --value)
ps -o ni= -p "$pid"
ionice -p "$pid"
grep '^Max processes' "/proc/$pid/limits"
```

预期服务为 `active (running)`，`ps` 显示 `19`，`ionice` 显示 `idle`，`Max processes` 的两列均为 `512`。

在统一 cgroup v2 环境下，再核对用户管理器可用的控制器和本单元的内核限额：

```bash
unit=agenthome-collector.service
user_cgroup=$(systemctl show "user@$(id -u).service" -p ControlGroup --value)
cat "/sys/fs/cgroup${user_cgroup}/cgroup.controllers"
systemctl --user show "$unit" -p CPUQuotaPerSecUSec -p MemoryHigh -p MemoryMax -p TasksMax -p IOWeight
service_cgroup=$(systemctl --user show "$unit" -p ControlGroup --value)
for setting in cpu.max memory.high memory.max pids.max io.weight; do
  printf '%s: ' "$setting"
  cat "/sys/fs/cgroup${service_cgroup}/$setting"
done
```

内核的 `cpu.max` 两列分别是配额和周期，配额应为周期的 10%。`memory.high`、`memory.max` 应分别为 `100663296`、`134217728` 字节，`pids.max` 应为 `16`，`io.weight` 应显示 `default 10`。如果内核文件缺失或数值不符，对应的 cgroup 限额未生效。需要这些限额时，由机器管理员检查控制器委派。

核对适用于当前机器的设置后，启用开机自启并确认 `enabled`：

```bash
systemctl --user enable --now agenthome-collector.service
systemctl --user is-enabled agenthome-collector.service
```

在下一次计划内重启后、采集用户首次登录前，由管理员通过另一个账号执行以下命令。将 `COLLECTOR_USER` 替换为采集用户的登录名，记录 `enabled`、`active (running)` 及启动时间。

```bash
systemctl --user --machine COLLECTOR_USER@.host status agenthome-collector.service --no-pager
```

采集参数、状态目录、日志、备份与恢复见[远程用量库](14-远程用量库.md#采集服务调度与资源上限单元-10)。服务沿用 `agenthome collect` 的默认参数，无界面、无行为埋点，通过本地 journal 和状态文件诊断。

## SSH 授权和撤销

用采集端的 `"$HOME/.local/bin/agenthome" remote list` 检查 AgentHome 的专用 SSH 授权，空输出表示当前没有授权。需要拉取时，按[受限 SSH 导出与配对](15-远程SSH导出与配对.md#配对步骤)核对主机指纹，在 Mac 生成专用公钥后再在采集端授权。采集端执行该文档的 `remote authorize` 命令时，用 `"$HOME/.local/bin/agenthome"` 调用，使 sshd 的固定命令指向已安装的二进制。

只需切断远程读取时，按配对文档执行 `remote revoke`，再在 Mac 上执行 `remote unpair`。只停用采集服务时，独立运行的 SSH 导出仍可读取已采集的用量，退役机器时应同时撤销各专用授权。

## 停用

```bash
systemctl --user disable --now agenthome-collector.service
```

停用后保留身份文件及用量历史。linger 对整个用户生效，停用本服务时无需关闭它。
