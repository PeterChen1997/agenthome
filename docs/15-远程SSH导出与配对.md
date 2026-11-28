# 远程用量：受限 SSH 导出与配对

> CARM-284 远程用量采集（方案 A：Mac 经 SSH 拉取）的单元 9。代码在 `crates/core/src/remote_ssh.rs`、`crates/cli/src/remote.rs`，端到端验收脚本 `script/test-remote-ssh.sh`。首版配对走 CLI；Mac 后台定时拉取在单元 12，导入在单元 7。

## 默认关闭

AgentHome 不监听任何端口、不自动发现、不做穿透。采集端没执行 `remote authorize` 之前，没有任何钥匙能读到用量。Mac 配对和采集端授权都必须显式给出网段，且只接受内网 / Tailscale / 回环地址：`10/8`、`172.16/12`、`192.168/16`、`100.64/10`、`127/8`、`169.254/16`、`fc00::/7`、`fe80::/10`、`::1`。

## 配对步骤

1. **采集端**：看本机 SSH 主机密钥指纹，通过可信渠道（当面、已有的加密聊天）告诉 Mac 那边：

   ```bash
   agenthome remote host-fingerprint          # 读 /etc/ssh/ssh_host_*_key.pub
   ```

2. **Mac**：配对。扫到的主机密钥指纹和填写的对不上就拒绝；对上了就固定到这台远端专用的 known_hosts，并生成专用 ed25519 钥匙：

   ```bash
   agenthome remote pair --name runner --host <采集端地址> --user <采集端用户> \
     --fingerprint SHA256:<上一步的指纹> --allow <采集端所在网段>
   ```

   文件在 `~/AgentHome/remote/<name>/`：`id_ed25519`（专用私钥，0600）、`known_hosts`（只含这一台的主机密钥）、`remote.json`。

3. **采集端**：把 Mac 打印的公钥存成文件，授权：

   ```bash
   agenthome remote authorize --name mac --pubkey mac.pub --from <Mac 的地址或网段>
   ```

   会在 `~/.ssh/authorized_keys` 写一行（同名已存在则替换）：

   ```
   restrict,from="<网段>",command="'<agenthome 绝对路径>' ssh-export" ssh-ed25519 AAAA… agenthome:mac
   ```

4. **Mac**：拉一页验证：`agenthome remote fetch --name runner --after 0`。

撤销：采集端 `agenthome remote revoke --name mac`；Mac 端 `agenthome remote unpair --name runner`。`agenthome remote list` 列出已授权的钥匙。

## 这把钥匙能做什么

- `restrict`：不分配终端，不允许端口 / agent / X11 转发，不执行 `~/.ssh/rc`。
- `command=`：不管客户端要执行什么，sshd 都只运行 `agenthome ssh-export`。客户端原本的命令放在 `SSH_ORIGINAL_COMMAND`，导出命令只认 `agenthome-export v1 after=<十进制>` 这一种写法，其他一律拒绝。所以登录 shell、`cat` 任意文件、scp、sftp 都执行不了。
- `from=`：来源地址不在网段内，sshd 直接拒绝这把钥匙。
- 导出只读本机用量库，一页序列化后不超过 256 KiB，20 秒内没完成就退出。报错只写类别，不带路径。

## Mac 端每次连接

- 先解析目标地址，任何一个解析结果不在允许网段内就不连，然后直接连解析出的 IP。
- `-F /dev/null` 不读用户的 `~/.ssh/config`，所以 ProxyCommand、转发等配置都不会带进来。另外固定 `StrictHostKeyChecking=yes`、`IdentitiesOnly=yes`、`IdentityAgent=none`、`ClearAllForwardings=yes`、`BatchMode=yes`。
- 总时限 30 秒，超时就杀掉 ssh。stdout 最多读 256 KiB + 1 字节，返回内容按 `deny_unknown_fields` 校验，不合规整页拒收。
- 对端 `machine_id` 和本机相同（把本机当远端）时拒绝。
- 错误分类（`RemoteErrorKind`）：`net_not_allowed`、`host_key_mismatch`、`unauthorized`、`unreachable`、`timeout`、`bad_response`、`self_pairing`、`local`。上报包含失败类别。收到不合规响应时，再附退出码、错误类别，以及 stdout / stderr 各自捕获的字节数和首个非空行的类别。响应原文留在本机。

## 导出页

```json
{"v":1,"store_id":"<32 hex>","after":0,"next":625,"high_water":1500,"more":true,
 "page":{"v":1,"machine_id":"<uuid>","records":[ …usage_v1::UsageRecord… ]}}
```

下次请求用 `next` 作为游标，`more=false` 表示已到高水位。`store_id` 变了说明采集端的库重建过，旧游标作废，要走完整快照（单元 8）。

## 验收脚本

```bash
cargo build -p agenthome-cli && script/test-remote-ssh.sh target/debug/agenthome
```

脚本以当前用户在 `127.0.0.1:22950` 起一个临时 sshd，不需要 root，也不碰系统 sshd 和 `~/.ssh`。它逐项检查：错误指纹、非允许网段、未授权、登录 shell、`-tt`、`cat`、拼接命令、scp、sftp、`-L`、`-D`、`-R`、把本机当远端、来源不在 `from=` 内、撤销后、主机密钥被换、采集端卡住时的超时，并按游标把 1500 条用量分 3 页拉完。依赖 `python3`、`openssl`、`curl`。

## Mac 后台拉取（CARM-299）

App 的用量同步线程（`crates/app/src/usage_sync.rs`）同时负责本机用量采集和远端拉取：

- **本机采集**：与 Runner 上的 `agenthome collect` 同一个 `Collector`，写 `~/AgentHome/usage/usage.db`（首页统计读它）。Mac 的限额比 Runner 宽：读 256 MiB/s、一轮一个 1 GiB 分片、状态目录 2 GiB。命令行采集进程占着锁时 App 不另起一份，一分钟后再试。
- **远端拉取**：`remote_pull::pull_ssh`。打开 App 就拉，之后每 5 分钟拉一次；一轮最多 32 页，没拉完 2 秒后接着拉。失败从 30 秒开始翻倍退避，最长 5 分钟。Mac 休眠时单调时钟会停，所以到点和唤醒都按墙钟判断：两次看钟相隔超过 45 秒就当作刚醒来，立即补拉并重置退避。
- **一次只拉一台、不留半提交**：同一远端同时只能有一个拉取（远端目录下的 `pull.lock`，跨进程有效）。每页的用量和游标在同一个事务里提交，App 退出、休眠或断网时，停在哪两页之间都行，下次从已确认的游标续拉。游标失效时自动走完整快照，快照没拉完时旧统计不变。
- **首页**：左栏底部状态行在远端拉取失败或有积压时显示该远端（例如「远端 runner 拉取失败 · 2 分钟前成功」）。悬停能看到本机采集和每台远端的上次成功时间、积压和失败类别。上次成功时间存在 `remote/<名字>/last_pull.json`，重开 App 后还没连上也能看到。
- **Sentry**：标题为 `Usage sync failed: <阶段>/<类别>`，带 `remote.stage`、`remote.kind`。本机采集（`collect`）还带固定代码步骤 `code.operation`、允许上报的 `error.chain`（如 SQLite 错误码），单文件失败附来源类型 `source.provider`。远端名、地址、路径、金额、载荷和错误原文留在本机。每种失败每次运行只报一次。
- **手动拉取**：`agenthome remote pull --name <名字>`，逻辑和 App 后台相同，会一直拉到没有新变更，然后打印合计。

验收脚本：`cargo build -p agenthome-cli && script/test-remote-pull.sh target/debug/agenthome`。脚本走真实的受限 SSH，依次验证首次分页拉取、断网失败（`sync/unreachable`）、恢复后续拉断网期间的新增和修订、采集端库重建后的完整快照，每一步都和采集端真值逐项对比。
