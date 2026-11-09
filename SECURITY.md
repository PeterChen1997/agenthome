# Security Policy / 安全策略

## What AgentHome accesses / AgentHome 访问什么

**English**

AgentHome is a local-only desktop app and CLI. Its security model:

- **Reads local files only.** It reads agent session transcripts (for example `~/.claude/projects/`, `~/.codex/sessions/`) and a small set of agent metadata files (for example the plan type in `~/.claude.json`). It never modifies, moves or symlinks anything inside an agent's own directories.
- **Writes only under `~/AgentHome/`**: the raw archive (byte-for-byte copies of transcripts), the SQLite index, quota signals and app preferences.
- **Never reads credentials.** It does not read API keys, OAuth tokens, `.credentials.json`, or the macOS Keychain, and it does not call undocumented usage endpoints.
- **Never uploads your data.** There is no telemetry, analytics or cloud sync. AgentHome itself makes no network requests with your session data.
- **Two opt-in / visible exceptions:**
  - Claude subscription quota is read by running the official `claude` CLI's `/usage` command in a background pseudo-terminal. That CLI talks to Anthropic with its own login, exactly as when you run it yourself; AgentHome only reads the text it prints.
  - Installing the Claude statusline helper from Settings (or `agenthome statusline install`) edits `~/.claude/settings.json`. This only happens when you click it, and the original file is backed up first.

Your archive contains your full conversation history, which may include secrets you pasted into an agent. Protect `~/AgentHome/` like any other sensitive directory (FileVault, careful sync-folder choices).

**中文**

AgentHome 是纯本地的桌面应用与 CLI，安全模型如下：

- **只读本地文件**：读取 agent 的会话转录（如 `~/.claude/projects/`、`~/.codex/sessions/`）和少量元数据文件（如 `~/.claude.json` 里的套餐类型）。绝不修改、移动或软链 agent 自己目录里的任何文件。
- **只在 `~/AgentHome/` 下写入**：raw 原件归档（转录的逐字节拷贝）、SQLite 索引、额度信号与应用偏好。
- **不读取任何凭据**：不读 API key、OAuth token、`.credentials.json` 或 macOS 钥匙串，也不调用未公开的用量端点。
- **不上传你的数据**：没有遥测、统计或云同步，AgentHome 自身不会把会话数据发到网络。
- **两处可见 / 需主动开启的例外**：
  - Claude 订阅额度通过在后台伪终端运行官方 `claude` CLI 的 `/usage` 获得。该 CLI 用它自己的登录态与 Anthropic 通信，和你手动运行时完全一样；AgentHome 只读取它打印的文字。
  - 在设置页（或 `agenthome statusline install`）安装 Claude statusline 包装脚本会修改 `~/.claude/settings.json`，只在你点击时发生，改前先备份原文件。

归档里是你完整的对话历史，可能包含你曾粘贴给 agent 的密钥。请像对待其他敏感目录一样保护 `~/AgentHome/`（开启 FileVault，谨慎选择同步盘）。

## Supported versions / 支持的版本

Only the latest release receives security fixes. / 只有最新发布版本会收到安全修复。

## Reporting a vulnerability / 报告漏洞

**Please do not open a public issue for security problems.** / **请不要为安全问题发公开 issue。**

Use GitHub private vulnerability reporting: go to the repository's **Security** tab and click **Report a vulnerability**. Include the affected version, steps to reproduce, and the impact you observed.

请使用 GitHub 私密漏洞报告：进入仓库的 **Security** 页，点击 **Report a vulnerability**，写明受影响版本、复现步骤和观察到的影响。

We aim to acknowledge reports within 7 days and to share a fix timeline after triage. Please give us a reasonable window to release a fix before public disclosure.

我们争取在 7 天内确认收到，分析后告知修复计划。公开披露前，请给我们留出合理的修复时间。

Examples of in-scope issues / 属于本策略的问题举例:

- AgentHome reading credentials, or sending session data over the network. / AgentHome 读取了凭据，或把会话数据发到网络。
- AgentHome modifying files in an agent's own directories. / AgentHome 修改了 agent 自己目录里的文件。
- Path traversal or code execution through crafted session files. / 通过构造的会话文件实现路径穿越或代码执行。
- Unsafe handling of `~/.claude/settings.json` by the statusline installer. / statusline 安装器对 `~/.claude/settings.json` 的不安全处理。
