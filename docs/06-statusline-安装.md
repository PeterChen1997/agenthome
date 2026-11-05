# statusline 安装：让 AgentHome 看到 Claude 的剩余额度

Claude Code 不提供查询订阅额度的本地接口；唯一合规的来源是 statusline（调研 `docs/research/04-usage-apis.md` §1.5）：每次 API 响应后，Claude Code 把一段 JSON 写进 `settings.json` 里 `statusLine.command` 的 stdin，其中 `rate_limits.five_hour / seven_day.{used_percentage, resets_at}` 就是 `/usage` 看到的数字（仅 claude.ai 订阅用户有；API key 用户没有 `rate_limits`，脚本仍会落盘但窗口为空）。

`script/agenthome-statusline.sh` 做的事：把 stdin 原样写到 `~/AgentHome/signals/claude-statusline.json`（先写 `.tmp` 再 `mv`，读的一方永远看到完整文件），再把 stdin 转给你原来的 statusline 命令；没有原命令就打印一行「模型 · 5h x% · 7d y%」。

## 安装

1. 让脚本可执行：

   ```sh
   chmod +x /Users/peterchen/code_hub/agenthome/script/agenthome-statusline.sh
   ```

2. 看一下 `~/.claude/settings.json` 里现在有没有 `statusLine`：

   ```sh
   jq .statusLine ~/.claude/settings.json
   ```

3. 改 `~/.claude/settings.json`（AgentHome 不会替你改这个文件）：

   - 之前没有 statusline：

     ```json
     {
       "statusLine": {
         "type": "command",
         "command": "/Users/peterchen/code_hub/agenthome/script/agenthome-statusline.sh"
       }
     }
     ```

   - 之前已有 statusline 命令（例如 `~/.claude/statusline.sh` 或 `ccusage statusline`）：把原命令放进环境变量，脚本会把 JSON 原样转给它，显示效果不变：

     ```json
     {
       "statusLine": {
         "type": "command",
         "command": "AGENTHOME_ORIG_STATUSLINE='~/.claude/statusline.sh' /Users/peterchen/code_hub/agenthome/script/agenthome-statusline.sh"
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

把 `statusLine` 改回原来的值（或删掉），再 `rm ~/AgentHome/signals/claude-statusline.json`。

## 注意

- 脚本落盘失败（比如目录不可写）会静默退出 0，不会让 statusline 报错；但这也意味着 `quota` 看不到更新，排查先看 `~/AgentHome/signals/` 是否可写。
- 落盘目录可用 `AGENTHOME_SIGNALS_DIR` 覆盖；AgentHome 读的默认路径固定是 `~/AgentHome/signals/claude-statusline.json`。
- JSON 里包含 `session_id`、`transcript_path`、`cwd` 等本机信息，文件只在本机，不随归档同步。
