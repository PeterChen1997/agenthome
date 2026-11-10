# statusline 安装：让 AgentHome 看到 Claude 的剩余额度

> **只有终端里的 Claude Code 会调用 statusline**（2026-11-08 实测）：Claude 桌面版（含 Code 标签页）与 SDK 会话不渲染状态栏，脚本一次都不会被调用，`~/AgentHome/signals/` 也不会出现。只用桌面版时，装好后需要偶尔在终端里用 `claude` 聊一句才能拿到快照；快照 10 分钟后标为已过期，但仍显示最后一次的数值与重置时间。设置页在「已安装但没有快照」时会提示这一点。**2026-11-08 起 app 还会每 15 分钟在后台运行官方 `claude` 的 `/usage` 读取额度（见 docs/09 §3），只用桌面版也有数据；statusline 变成终端用户的可选补充。**


Claude Code 不提供查询订阅额度的本地接口；唯一合规的来源是 statusline（调研 `docs/research/04-usage-apis.md` §1.5）：每次 API 响应后，Claude Code 把一段 JSON 写进 `settings.json` 里 `statusLine.command` 的 stdin，其中 `rate_limits.five_hour / seven_day.{used_percentage, resets_at}` 就是 `/usage` 看到的数字（仅 claude.ai 订阅用户有；API key 用户没有 `rate_limits`，脚本仍会落盘但窗口为空）。

`script/agenthome-statusline.sh` 做的事：把 stdin 原样写到 `~/AgentHome/signals/claude-statusline.json`（先写 `.tmp` 再 `mv`，读的一方永远看到完整文件），再把 stdin 转给你原来的 statusline 命令；没有原命令就打印一行「模型 · 5h x% · 7d y%」。

## 用 CLI / 设置页安装（推荐）

`agenthome-core` 的 `statusline_setup` 模块把下面的手工步骤做成了可逆的安装器，CLI 与设置页（9.4，UI 待接）都调它：

```sh
agenthome statusline status      # 只读：未安装 / 已安装（含被包装的原命令）/ 设置文件不存在 / 无法解析 / 无法识别
agenthome statusline install     # 安装或更新，幂等
agenthome statusline uninstall   # 还原
```

可用 `--settings <路径>`、`--bin-dir <目录>` 指向别的位置（测试用）；默认是 `~/.claude/settings.json` 与 `~/AgentHome/bin`。

`install` 做的事：

1. 把内嵌在程序里的包装脚本写到 `~/AgentHome/bin/agenthome-statusline.sh`（755，原子替换；内容已一致时不动）。
2. 改 `settings.json` 前先把原文件逐字节备份成同目录的 `settings.json.agenthome-backup-<时间戳>`，CLI 会打印备份路径。
3. 只改 `statusLine.command` 一个值：原来有命令就写成 `AGENTHOME_ORIG_STATUSLINE='<原命令>' ~/AgentHome/bin/agenthome-statusline.sh`（即下文手工安装的环境变量方式），原来没有就新增 `{"type":"command","command":"<脚本路径>"}`。其余键和键顺序、`statusLine` 里的 `padding` / `refreshInterval` 等字段都原样保留。
4. 写入是「同目录临时文件 + rename」；`settings.json` 是符号链接时改它指向的文件，链接保留。
5. 已经接入（包括按下文手工指向仓库脚本的形态）时不会再包一层，只会把脚本路径改到托管位置并保留原命令。

`uninstall` 把 `statusLine.command` 还原成被包装的原命令；原来没有 statusline 就删掉 `statusLine` 键。同样先备份、原子写。不删除托管脚本和 `~/AgentHome/signals/` 下的快照。

拒绝执行、且不写任何文件的情况：`settings.json` 不是合法 JSON 或顶层不是对象；`statusLine` 不是对象、`type` 不是 `command`、`command` 不是字符串；命令里出现了 `agenthome-statusline.sh` 但不是上面的格式（例如 `bash …/agenthome-statusline.sh | …`）。这些情况请按下文手工处理。另外 `~/.claude` 目录不存在时也不会安装。

已知限制：如果你是按下文「直接改脚本顶部 `ORIG=`」的方式手工安装的，安装器读不到原命令，卸载会删掉 `statusLine` 键，需要自己把原命令加回去。

## 手工安装

1. 让脚本可执行：

   ```sh
   chmod +x /path/to/agenthome/script/agenthome-statusline.sh
   ```

2. 看一下 `~/.claude/settings.json` 里现在有没有 `statusLine`：

   ```sh
   jq .statusLine ~/.claude/settings.json
   ```

3. 改 `~/.claude/settings.json`（手工方式下自己改；用上面的 CLI 则由安装器代改）：

   - 之前没有 statusline：

     ```json
     {
       "statusLine": {
         "type": "command",
         "command": "/path/to/agenthome/script/agenthome-statusline.sh"
       }
     }
     ```

   - 之前已有 statusline 命令（例如 `~/.claude/statusline.sh` 或 `ccusage statusline`）：把原命令放进环境变量，脚本会把 JSON 原样转给它，显示效果不变：

     ```json
     {
       "statusLine": {
         "type": "command",
         "command": "AGENTHOME_ORIG_STATUSLINE='~/.claude/statusline.sh' /path/to/agenthome/script/agenthome-statusline.sh"
       }
     }
     ```

     也可以不用环境变量，直接改脚本顶部的 `ORIG=` 变量。

   可选：`"refreshInterval": 30` 让 Claude Code 每 30 秒刷一次而不只是在 API 响应后刷。

4. 开一个新的 Claude Code 会话，随便说一句话触发一次 API 响应，然后验证：

   ```sh
   ls -l ~/AgentHome/signals/claude-statusline.json
   CARGO_TARGET_DIR=target/core cargo run -p agenthome-cli --release -- quota
   ```

   `quota` 应打印 5 小时 / 7 天窗口的已用百分比与重置时间；文件 mtime 超过 10 分钟会标 `stale`。

## 卸载

`agenthome statusline uninstall`；或手工把 `statusLine` 改回原来的值（或删掉）。不再需要额度快照的话再 `rm ~/AgentHome/signals/claude-statusline.json`。

## 注意

- 脚本落盘失败（比如目录不可写）会静默退出 0，不会让 statusline 报错；但这也意味着 `quota` 看不到更新，排查先看 `~/AgentHome/signals/` 是否可写。
- 落盘目录可用 `AGENTHOME_SIGNALS_DIR` 覆盖；AgentHome 读的默认路径固定是 `~/AgentHome/signals/claude-statusline.json`。
- JSON 里包含 `session_id`、`transcript_path`、`cwd` 等本机信息，文件只在本机，不随归档同步。
