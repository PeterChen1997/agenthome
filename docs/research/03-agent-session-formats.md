# AI Agent 本地会话记录：存储位置与格式调研

调研日期：2026-11-05。目标：为「AI Agent 对话归档仓库」设计可插拔 Provider，摸清各 coding agent / AI 助手在本地磁盘上保存会话的位置、格式、token/cost 字段、清理策略与追加写行为。

调研方法：本机只读检查（`ls` / `head` / `sqlite3 -readonly` / 只打印字段名，示例值已脱敏）+ 官方文档 + 开源解析器源码。本机实测版本：Claude Code 2.1.309（`entrypoint: claude-desktop`）、Codex CLI 0.140.0（rollout 由 Codex Desktop 0.163 写入）、OpenCode 1.17.18、Cursor 3.18.9、Gemini CLI（2026-01 的会话文件）。

> 标注约定：【本机实测】= 在本机直接观察到；【文档】= 官方文档；【社区】= 第三方解析器 / 博客，未经官方确认。

---

## 1. 总览对比表

| Agent | 主路径（macOS） | 格式 | token usage | cost | 自动清理 | 追加写 | 多机同步难度 |
|---|---|---|---|---|---|---|---|
| **Claude Code** | `~/.claude/projects/<encoded-cwd>/<session>.jsonl`；子代理 `<session>/subagents/agent-<id>.jsonl` | JSONL，每行一条记录，`uuid/parentUuid` 树 | 有：`message.usage`（input/output/cache_creation/cache_read，含 1h/5m 细分、thinking_tokens） | 无逐条 costUSD（旧版有）；有会话级 `cost-state` 记录 `totalCostUSD` + 按模型用量 | **有**：`cleanupPeriodDays` 默认 30 天（最小 1）；Desktop/Cowork 发起的会话例外，除非设 `desktopSessionCleanupPeriodDays` | 是（append-only；`--resume` 会续写同一文件；偶有 `.superseded-*`/`.orphaned-*` 副本） | 中：路径含机器绝对路径编码；文件可直接拷贝 |
| **Codex CLI/Desktop** | `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`（冷数据被压成 `.jsonl.zst`）；`archived_sessions/` 同构 | JSONL，`{timestamp, ordinal, type, payload}` | 有：`token_usage_record` + `event_msg.token_count`（input/cached/cache_write/output/reasoning/total） | 无 USD（只有 rate-limit / plan 信息） | 无自动删除【文档/社区】；但**自动 zstd 压缩**冷文件（本机：>~7–10 天的 rollout 全为 .zst） | 是（append；压缩文件续写时会先解压回 .jsonl） | 中：纯文件 + 派生 SQLite 索引（`state_5.sqlite`、`thread_history_1.sqlite`，可重建） |
| **Gemini CLI** | `~/.gemini/tmp/<sha256(project)>/chats/session-<ts>-<uuid8>.json`；`logs.json` | 单文件 JSON（整文件重写） | 有：每条 gemini 消息 `tokens{input,output,cached,thoughts,tool,total}` | 无 | **有**：`general.sessionRetention` 默认 enabled，`maxAge: "30d"`，`minRetention: "1d"`，`maxCount` 默认无限 | 否（整文件覆盖写） | 中：目录名是项目路径哈希 |
| **OpenCode** | `~/.local/share/opencode/opencode.db`（SQLite，v1.2+；旧版 `storage/{session,message,part}/*.json`） | SQLite：`session / message / part`，`data` 列为 JSON | 有：`session.tokens_*` 汇总 + `message.data.tokens{input,output,reasoning,cache{read,write}}` | **有**：`session.cost`、`message.data.cost` | 无（社区反馈 db 无限增长） | SQLite 事务写（WAL） | 中：单 db 文件 + WAL/SHM，需读一致快照 |
| **Cursor（IDE）** | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`（`cursorDiskKV`：`composerData:<id>`、`bubbleId:<composerId>:<bubbleId>`；`composerHeaders` 表）；workspaceStorage/*/state.vscdb 的 `composer.composerData`、`aiService.prompts` | SQLite KV + JSON blob，格式封闭 | 部分：bubble 的 `tokenCount{inputTokens,outputTokens}`、`tokenCountUpUntilHere`（本机样本多为 0） | 无 | 无（社区反馈 state.vscdb 可达数十 GB） | KV upsert | 高：单巨型 db（本机 578 MB），版本间 schema 漂移 |
| **Copilot CLI** | `~/.copilot/session-state/<sessionId>/events.jsonl`（旧：`history-session-state/<id>.json`） | JSONL 事件流 `{type,id,timestamp,parent_id,data}` | 有：`session.shutdown.data.model_metrics[model].usage`（input/output/cache_read/cache_write/reasoning） | 无 | 未知/未见文档 | 是 | 中 |
| **Copilot Chat (VS Code)** | `~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/<sessionId>.json`（新版为 .jsonl delta log）；空窗口 `globalStorage/emptyWindowChatSessions/` | JSON（v3：`requests[]`）或 JSONL 增量日志 | 基本无 | 无 | 无 | 新版是 | 中 |
| **Cline** | `~/.cline/data/tasks/<taskId>/`（旧：VS Code `globalStorage/saoudrizwan.claude-dev/tasks/<ts>/`）：`api_conversation_history.json`、`ui_messages.json`、`task_metadata.json` | JSON（Anthropic MessageParam 数组） | 有：`ui_messages` 的 `api_req_started` 事件带 tokens/cost；metadata 有 `tokensUsed` | **有**（`api_req_started.cost`）【社区】 | 无 | 整文件重写 | 中 |
| **Aider** | 项目目录 `.aider.chat.history.md`、`.aider.input.history`、可选 `.aider.llm.history`（`--llm-history-file`） | Markdown（`# aider chat started at <ts>` 分段，`####` 为用户输入） | 无结构化字段 | 无 | 无 | 是（append） | 低（随 repo 走，但常被 .gitignore） |
| **Windsurf** | `~/.codeium/windsurf/cascade/*.pb`、`~/.codeium/windsurf/memories/`；IDE 的 `Windsurf/User/workspaceStorage/*/state.vscdb` | protobuf 二进制，社区称部分加密 | 不可读 | 不可读 | 有（用户反馈旧会话被删/仅保留 N 条） | — | 高（基本不可解析） |
| **Kiro IDE** | `~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/`（JSON；Kiro CLI 用 SQLite） | JSON 文件 | 未知 | 未知 | 无（用户反馈 13GB+ 无清理） | — | 高 |
| **Amp** | `~/.local/share/amp/threads/T-<uuid>.json` | 单文件 JSON：`{id, created(ms), messages[], env?}`，content blocks + usage【社区】 | 有（message.usage）【社区】 | 未知 | 无 | 整文件重写 | 中；线程同时同步到 ampcode.com |
| **Goose** | `~/.local/share/goose/sessions/sessions.db`（v1.10+ SQLite；旧 `*.jsonl` 保留不再管理） | SQLite（sessions、messages、usage ledger） | 有（含 cache read/write） | 有 cost 字段【社区】 | 无 | SQLite | 中 |
| **Pi (pi-coding-agent)** | `~/.pi/agent/sessions/--<cwd-dashed>--/<ts>_<uuid>.jsonl` | JSONL 树（`id/parentId`，v3） | 有：`usage{input,output,cacheRead,cacheWrite,totalTokens}` | **有**：`usage.cost{input,output,cacheRead,cacheWrite,total}` | 无（手动 /resume 删除） | 是 | 低–中 |
| **Claude Desktop（Chat/Cowork）** | `~/Library/Application Support/Claude/local-agent-mode-sessions/<did>/<org>/local_<uuid>.json` + 同名工作目录（`audit.jsonl`、`.claude/`、`uploads/`、`outputs/`）；Code 会话转录在 `~/.claude/projects/` | JSON 状态文件 + `audit.jsonl` 审计流 | audit.jsonl 含模型与 token 用量【文档】 | 无 | 默认不删；企业可配 `chatSessionRetentionDays` 等 | audit.jsonl append | 高：按设备 id + org id 分层，含 HMAC 签名 |
| **ChatGPT 导出** | 导出 zip：`conversations.json`（大导出拆成 `conversations-N.json`） | JSON 数组；每个 conversation 为 `mapping` DAG + `current_node` | 无 | 无 | 不适用（一次性导出） | 不适用 | 低 |
| **Antigravity (Google)** | `~/.gemini/antigravity/conversations/<uuid>.pb`、`brain/<uuid>/`、`conversation_summaries.db` | protobuf 二进制 | 不可读 | 不可读 | 未知 | — | 高 |

---

## 2. 各 Agent 详细字段说明

### 2.1 Claude Code

#### 2.1.1 目录布局【本机实测 + 文档】

```
~/.claude/
  projects/<encoded-cwd>/            # cwd 中非字母数字字符全部替换为 "-"，如 -Users-me-code-hub-foo
    <session-uuid>.jsonl             # 主转录
    <session-uuid>.jsonl.superseded-<ts> / <session>.orphaned-<ts>-<sfx>.jsonl   # 被搁置的旧副本
    <session-uuid>/
      custom-title.json              # {"customTitle": "..."}
      subagents/agent-<id>.jsonl     # 子代理转录（同格式，isSidechain:true，带 agentId）
      subagents/agent-<id>.meta.json # {agentType, description, toolUseId, spawnDepth, requestShape, ...}
      tool-results/<toolu_id>.txt    # 超大工具输出外溢
      mcp-tasks/
    memory/                          # 自动记忆（MEMORY.md 等），不受清理影响
  history.jsonl                      # 提示历史：{display, pastedContents, timestamp(ms), project, sessionId}
  stats-cache.json                   # /usage 聚合缓存（见下）
  sessions/<pid>.json                # 运行中会话锁（peerToken 等），退出即删
  session-env/<session>/             # 会话环境元数据
  file-history/<session>/            # checkpoint 快照
  shell-snapshots/、paste-cache/、debug/、plans/、telemetry/、backups/
  todos/、statsig/、logs/            # legacy，不再写入
  settings.json、.credentials.json、.last-cleanup（上次清理时间 ISO 字符串）
~/.claude.json                       # 全局状态（账号、MCP、项目信任等）
```

`CLAUDE_CONFIG_DIR` 可整体迁移 `~/.claude`。

#### 2.1.2 转录记录类型（`type` 字段）【本机实测 2.1.197–2.1.309】

本机约 200 MB 样本统计到的类型：`user`、`assistant`、`system`、`attachment`、`queue-operation`、`last-prompt`、`custom-title`、`agent-name`、`file-history-snapshot`、`file-history-delta`、`atis-latch`、`cost-state`、`mode`、`api-request-shape`、`api-request-blob`、`api-request`、`relocated`、`bridge-session`、`artifact-comment-monitor`、`artifact-autoreact-ledger`、`frame-link`。旧版还有 `summary`（`{type:"summary", summary, leafUuid}`，用于 `--resume` 标题）与 `ai-title`。

> 官方立场：只文档化了存储位置，记录格式「internal to Claude Code」，版本间会变。Provider 必须对未知 `type` 容错（原样保留）。

**对话类记录（user / assistant / system / attachment）公共字段：**

| 字段 | 说明 |
|---|---|
| `uuid` / `parentUuid` | 记录 id 与父记录 id，构成树（compact 后 `parentUuid` 可为 null，另有 `logicalParentUuid`） |
| `sessionId` | 会话 uuid，与文件名一致 |
| `timestamp` | ISO 8601 |
| `cwd` / `gitBranch` / `version` | 工作目录、git 分支、Claude Code 版本 |
| `isSidechain` | 主文件内为 false；子代理文件内为 true |
| `agentId` | 仅子代理文件 |
| `userType` (`external`) / `entrypoint` (`cli` / `claude-desktop` / `sdk-*`) / `permissionMode` / `slug` | 环境信息 |
| `isMeta` | 注入的元消息（非用户手打） |
| `isCompactSummary` / `isVisibleInTranscriptOnly` | compact 后的摘要 user 消息 |
| `promptId` / `turnOrigin` / `turnPosition{promptIndex,turnIndex}` / `origin{kind}` / `promptSource` | 轮次定位（新版） |
| `requestId` (`req_…`) / `requestRef` / `requestedModel` / `apiBlockIndex` / `effort` / `perTurnEffort` / `advisorModel` / `thinkingDurationMs` | assistant 专有 |
| `toolUseResult` / `sourceToolAssistantUUID` / `permissionDecision{decision,source,reasonType}` / `sandboxed` | tool_result 型 user 消息专有 |

**`message` 结构**

- user：`{role:"user", content: string | ContentBlock[]}`。字符串 = 用户手打；数组多为 `tool_result`（`{type:"tool_result", tool_use_id, content: string|[{type:"text",text}], is_error?}`）、`text`、`image`。
- assistant：`{id:"msg_…", type:"message", role:"assistant", model, content: ContentBlock[], stop_reason, stop_sequence, stop_details, usage, container, context_management, diagnostics, input_transformations}`。
- ContentBlock 类型：`text{text}`、`tool_use{id,name,input,caller?}`、`thinking{thinking,signature}`、`tool_result`、`image{source{type:base64,media_type,data}}`；api-request-blob 中还见 `tool_addition`。
- **关键：同一个 API 响应会被拆成多条 assistant 记录**（每个 content block 一条，共享 `message.id` 与 `requestId`，`apiBlockIndex` 递增），且每条都带同一份 `usage`。统计 token 必须按 `message.id`（或 `message.id + requestId`）去重（ccusage 做法）。流式写入时 `output_tokens` 单调递增，取带 `stop_reason` 的最后一条。

**`message.usage`（2.1.3xx）**

```
input_tokens, output_tokens,
cache_creation_input_tokens, cache_read_input_tokens,
cache_creation: { ephemeral_1h_input_tokens, ephemeral_5m_input_tokens },
output_tokens_details: { thinking_tokens },
server_tool_use: { web_search_requests, web_fetch_requests },
service_tier, speed, inference_geo, fallback_credit,
iterations: [ { input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, cache_creation{…}, type } ]
```

旧版仅有前四项 + `service_tier` + `server_tool_use`。

**cost**：新版 assistant 记录**没有** `costUSD`（本机 0 条顶层 costUSD）；早期版本（≈1.0.x）每条 assistant 有 `costUSD`。新版改为会话级 `cost-state` 记录：

```json
{"type":"cost-state","sessionId":"…","totalCostUSD":1.03,"totalAPIDuration":123724,"totalToolDuration":15666,
 "totalLinesAdded":0,"totalLinesRemoved":0,"totalDuration":…,"startTime":1791687842764,
 "modelUsage":{"claude-opus-5-5":{"inputTokens":…,"outputTokens":…,"thinkingTokens":…,"cacheReadInputTokens":…,"cacheCreationInputTokens":…,"webSearchRequests":0,"costUSD":1.03}},
 "hasUnknownModelCost":false}
```

因此 cost 有两条来源：(a) `cost-state`（官方估算，可能缺失/不完整）；(b) 自己按模型价格表从 usage 计算（ccusage 的 `calculate` 模式）。

**system 记录**：`subtype` ∈ `stop_hook_summary`（hookInfos/hookErrors）、`api_error`、`compact_boundary`（`compactMetadata{trigger:manual|auto, preTokens, durationMs}` + `logicalParentUuid`）、`local_command`、`informational`；`level` ∈ `info|notice|suggestion|error`。

**其他非对话记录**：`attachment`（environment 快照、文件引用等，含 `rendered`）；`api-request` / `api-request-shape` / `api-request-blob`（**完整请求参数、system prompt、工具定义都落盘**，blob 按 hash 去重）；`queue-operation{operation:enqueue|dequeue|remove|popAll, content}`；`last-prompt{lastPrompt, leafUuid}`；`custom-title{customTitle}`；`agent-name{agentName}`；`relocated{relocatedCwd}`；`file-history-snapshot{messageId, snapshot{trackedFileBackups}}`；`mode{mode}`。标题优先级（txcript 社区结论）：`custom-title` > `agent-name` > 首个 `summary`。

**子代理**：旧版把 sidechain 写在同一文件（`isSidechain:true`）；2.1.x 起写到 `<session>/subagents/agent-<id>.jsonl` + `.meta.json`。ccusage 默认不统计子代理文件（社区说法），归档时必须一并收。

#### 2.1.3 `history.jsonl` / `stats-cache.json` / `~/.claude.json`

- `history.jsonl`：每行 `{display, pastedContents, timestamp(ms), project, sessionId}`，**不参与自动清理**（仅 HIPAA 配置下裁剪），`claude purge` 会按项目过滤删除。
- `stats-cache.json`：`{version, lastComputedDate, dailyActivity[{date,messageCount,sessionCount,toolCallCount}], dailyModelTokens[{date,tokensByModel}], modelUsage{model:…}, totalSessions, totalMessages, longestSession{sessionId,duration,messageCount,timestamp}, firstSessionDate, hourCounts, totalSpeculationTimeSavedMs}`。不删。
- `~/.claude.json`：账号、MCP、`projects[path]` 的信任/历史等。

#### 2.1.4 自动清理（核实结论，直接影响软链/拷贝决策）【文档 + 本机实测】

- 设置键 `cleanupPeriodDays`：**默认 30 天，最小 1，设 0 报错**；按文件「最后活动时间」删除 `projects/<project>/<session>.jsonl`、其 `subagents/`、`tool-results/`、`file-history/`、`debug/`、`plans/`、`session-env/`、scratchpad 等。每次启动做一次 sweep，`~/.claude/.last-cleanup` 记录时间。
- **例外**：由 Claude Desktop / Cowork 发起或最近续接的会话（`entrypoint: claude-desktop`）**不受 `cleanupPeriodDays` 影响**，除非设置 `desktopSessionCleanupPeriodDays`（需 ≥ v2.1.248）或由 managed settings 强制 / HIPAA 模式。本机验证：269 个 jsonl 超过 30 天、264 个超过 60 天仍在，全部 `entrypoint: claude-desktop`，与文档一致；但同一台机器上 CLI 发起的会话会在 30 天后消失。
- `--bare` 模式不跑 sweep；settings 无法解析时会**暂停** sweep。
- `CLAUDE_CODE_SKIP_PROMPT_HISTORY=1` / `-p --no-session-persistence` 会完全不写转录。
- 转录明文存储，包含 `.env`、命令输出等敏感内容（官方提示）。

**结论：对 Claude Code 不能只做软链/原地索引，必须在 30 天窗口内把文件拷走（或至少增量快照）；并且要同时拷 `subagents/` 和 `tool-results/`。**

---

### 2.2 OpenAI Codex CLI / Codex Desktop

#### 2.2.1 目录布局【本机实测】

```
~/.codex/
  sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<uuidv7>.jsonl        # 热数据
  sessions/YYYY/MM/DD/rollout-…jsonl.zst                                   # 冷数据，后台 zstd 压缩
  archived_sessions/rollout-…jsonl(.zst)                                   # 归档（codex archive / /archive）
  session_index.jsonl         # {id, thread_name, updated_at}
  history.jsonl               # {session_id, ts(秒), text}  —— 仅用户输入历史，受 history.persistence=save-all|none、history.max_bytes 控制；本机最后写入 2026-06，新版 Desktop 已不再更新
  state_5.sqlite              # threads 表：id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, tokens_used, archived, archived_at, git_sha/git_branch/git_origin_url, cli_version, first_user_message, model, reasoning_effort, history_mode, project_id, … ；还有 projects、thread_spawn_edges、external_agent_config_imports 等表
  thread_history_1.sqlite     # thread_turns / thread_items(item_json) / thread_realtime_items / thread_history_projection_state —— 从 rollout 投影出的分页索引（本机 1.3 GB）
  sqlite/codex.db             # Desktop：local_thread_catalog、thread_timeline_ledger、automations、inbox_items
  logs_2.sqlite、.codex-global-state.json、config.toml、auth.json（敏感）
  external_agent_session_imports.json  # Codex Desktop 会扫描 ~/.claude/projects/*.jsonl 做「外部 agent 会话导入」（记录 source_path + 检测到的 connector）
```

本机压缩情况：2026-09 的 rollout 41 个 .zst / 1 个 .jsonl；2026-10 为 70 / 31；2026-11 为 0 / 24；最新 .zst 日期 10-26、最老 .jsonl 日期 10-28 → **冷阈值约 7–10 天**（官方未文档化，由后台 worker 执行；resume 时透明读 .zst，续写前先解压回 .jsonl）。

#### 2.2.2 rollout 行结构【本机实测 + protocol.rs】

每行：`{"timestamp": ISO, "ordinal": n, "type": <RolloutItem>, "payload": {...}}`。`type` 取值：

| type | payload 要点 |
|---|---|
| `session_meta` | `id/session_id, timestamp, cwd, runtime_workspace_roots, originator("Codex Desktop"/"codex_cli_rs"), cli_version, source("vscode"/"cli"/…), thread_source, model_provider, base_instructions{text,provenance}, history_mode, context_window{window_id}, git{commit_hash,branch,repository_url}, forked_from_id, parent_thread_id, agent_nickname, agent_path, subagent_history_start_ordinal, creator_user_id/creator_account_id` |
| `turn_context` | `turn_id, root_turn_id, cwd, approval_policy, sandbox_policy, model, effort, summary, collaboration_mode, personality …`（每轮） |
| `response_item` | `payload.type` ∈ `message{role:user|assistant|developer, content:[{type:input_text|output_text, text}]}`、`reasoning{summary[], encrypted_content}`（推理内容加密，不可读）、`function_call{name, arguments(JSON string), call_id, status}`、`function_call_output{call_id, output}`、`custom_tool_call{name, input}` / `custom_tool_call_output{output:[{type:input_text,text}]}`、`additional_tools`、`agent_message`（多代理通信）、`web_search_call` 等；附 `internal_chat_message_metadata_passthrough{turn_id, create_time}`，行级 `metadata{client_authored, mcp_attribution}` |
| `event_msg` | `payload.type` ∈ `task_started{turn_id, model_context_window, started_at}`、`task_complete{last_agent_message, duration_ms, time_to_first_token_ms}`、`turn_aborted{reason}`、`token_count{info{total_token_usage, last_token_usage, model_context_window}, rate_limits{primary{used_percent,window_minutes,resets_at}, plan_type, credits}}`、`item_completed{item{type:UserMessage|AgentMessage|…}}`、`user_message`、`agent_message`、`thread_settings_applied`、`mcp_tool_call_end` … |
| `token_usage_record` | `thread_id, turn_id, session_id, root_turn_id, response_id, usage{…}, turn_token_usage{…}, thread_token_usage{…}` |
| `compacted` | `message, replacement_history[]`（compact 后替换的历史） |
| `world_state` | 完整环境快照（skills、model_catalog hash 等） |
| `inter_agent_communication_metadata` | 多代理 |

`TokenUsage` 字段：`input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens`（`token_count` 的 `total_token_usage` 是累计；`last_token_usage` 为本次）。**无 USD 成本**。

注意：用户 prompt 在 `event_msg.user_message` 与 `response_item.message(role:user)` 中各出现一次，需配对去重（sessionwiki 做法）。`ExecCommandEnd` 输出在 Extended 持久化模式下截断为 10 KB。

#### 2.2.3 清理与 OTEL

- 无自动删除策略；`codex archive <id>` 仅标记 `archived_at` 并移动文件。`memories.max_rollout_age_days=30` 只影响记忆生成。
- `[otel]`：`exporter = none|otlp-http|otlp-grpc`、`trace_exporter`、`log_user_prompt`（默认 false）、`environment`（默认 dev）。事件：`codex.conversation_starts`、`codex.api_request`、`codex.sse_event`（含 token 计数）、`codex.tool_result`、`codex.user_prompt`（PR #2103）。

---

### 2.3 Gemini CLI（及 Antigravity）

- 路径：`~/.gemini/tmp/<sha256(项目根路径)>/chats/session-<YYYY-MM-DDTHH-MM>-<uuid前8>.json`；同目录 `logs.json`（`[{sessionId, messageId, type:"user", message, timestamp}]`，仅用户输入）。手动 checkpoint（`/chat save <tag>`）另存为 `checkpoint-<tag>.json`。【本机实测】
- 会话文件结构：

```json
{"sessionId":"…","projectHash":"…","startTime":ISO,"lastUpdated":ISO,
 "messages":[
   {"id":uuid,"timestamp":ISO,"type":"user","content":"…"},
   {"id":uuid,"timestamp":ISO,"type":"gemini","content":"…",
    "thoughts":[{"subject","description","timestamp"}],
    "tokens":{"input","output","cached","thoughts","tool","total"},
    "model":"gemini-3-pro-preview",
    "toolCalls":[{"id","name","args","result":[{"functionResponse":{"id","name","response":{"output"}}}],"status":"success","timestamp","resultDisplay","displayName","description"}]}
 ]}
```

- 清理：`general.sessionRetention` —— `enabled` 默认 true，`maxAge` 默认 `"30d"`，`minRetention` 默认 `"1d"`，`maxCount` 默认无限。删除会话同时删除其 plans/tool outputs/logs。【文档】→ 同样需要拷贝而非软链。
- 遥测：`telemetry.enabled/target(local|gcp)/otlpEndpoint/outfile/logPrompts(默认 true)`；事件 `gemini_cli.user_prompt`、`api_request`、`api_response`（含 input/output/cached/thoughts/tool/total）、`tool_call`；指标 `gemini_cli.token.usage`、`gen_ai.client.token.usage`。无 cost。
- 官方文档提示：2026-06-18 起 Gemini CLI 被 **Antigravity CLI** 取代。本机 `~/.gemini/antigravity/` 下 `conversations/<uuid>.pb`（protobuf，101 个）、`brain/<uuid>/`、`conversation_summaries.db`（SQLite）——Antigravity 会话为二进制，暂不可解析，只能做「存在性/时间」级归档。

---

### 2.4 OpenCode

- 路径：`~/.local/share/opencode/opencode.db`（+ `-wal`、`-shm`）；v1.2 之前为 `storage/session/*.json`、`storage/message/<sessionId>/*.json`、`storage/part/<messageId>/*.json`，升级时自动迁移且不删旧文件。`snapshot/<projectId>/` 为文件快照，`log/opencode.log`。【本机实测 1.17.18】
- 核心表：
  - `session(id, project_id, workspace_id, parent_id, slug, directory, path, title, version, share_url, summary_additions/deletions/files/diffs, metadata, cost REAL, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, revert, permission, agent, model, time_created, time_updated, time_compacting, time_archived)`
  - `message(id, session_id, time_created, time_updated, data JSON)`，`data`：`{role, agent, mode, parentID, path{cwd,root}, cost, tokens{input,output,reasoning,cache{read,write}}, modelID, providerID, time{created,completed}, summary?, error?}`
  - `part(id, message_id, session_id, time_created, time_updated, data JSON)`，`data.type` ∈ `text`、`reasoning`、`tool{tool, callID, state{status,input,output,title,metadata,time}}`、`file`、`step-start`、`step-finish`、`snapshot`、`patch`、`agent`…
  - 另有 `project(id, worktree, vcs, name, …)`、`todo`、`event`、`session_share`、`workspace`、`account`（含 token，**敏感**）。
- `parent_id` 表示子代理会话。cost 直接可用。无清理策略。

---

### 2.5 Cursor（IDE Composer/Agent）

- 全局库：`~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`（本机 578 MB），表：
  - `ItemTable(key, value)`：应用状态；`composer.composerHeaders.*`、`workbench.panel.aichat.view.aichat.chatdata`（旧版 Chat）。
  - `cursorDiskKV(key, value)`：`composerData:<composerId>`（会话状态文档）、`bubbleId:<composerId>:<bubbleId>`（单条消息，本机 13.9k 条）、`checkpointId:*`、`codeBlockDiff:*`、`messageRequestContext:*`、`agentKv:blob:*`。
  - `composerHeaders(composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName)`：新版会话索引。
- 工作区库：`workspaceStorage/<hash>/state.vscdb` 的 `composer.composerData`（`{allComposers:[{type:"head",composerId,createdAt,unifiedMode,forceMode,…}]}`）、`aiService.prompts`、`aiService.generations`；`workspace.json` 给出 `folder` URI。
- `composerData` 字段：`composerId, name, createdAt, lastUpdatedAt, unifiedMode(agent|chat|edit), modelConfig, fullConversationHeadersOnly:[{bubbleId,type}]`（排序依据）、`conversation[]`（旧版内嵌）、`subagentComposerIds, subComposerIds, isAgentic, totalLinesAdded/Removed, usageData, status, context, codeBlockData …`。
- bubble 字段（`_v:2`）：`type`（1=user, 2=assistant）、`text`、`richText`、`thinking{text,signature}`、`toolFormerData{tool,name,toolCallId,params,status,result}`、`tokenCount{inputTokens,outputTokens}`、`tokenCountUpUntilHere`、`tokenDetailsUpUntilHere`、`modelInfo.modelName`、`createdAt`、`capabilityType`、`attachedCodeChunks`、`relevantFiles`、`diffHistories`、`checkpointId`、`summarizedComposers` …（本机样本 tokenCount 多为 0，不可靠）。
- Cursor CLI（`cursor-agent`）另存 `~/.cursor/chats/`（本机未使用）。
- 风险：格式封闭、无文档、随版本漂移（社区报告 2.x→3.x 多次变化），单库巨大且 WAL 必须一起复制；读取时应 `sqlite3 -readonly` + `immutable=1` 或先复制三件套。

---

### 2.6 GitHub Copilot CLI / Copilot Chat

**Copilot CLI**（`~/.copilot/`，本机仅有 `ide/`、`skills/`，无会话）【社区解析器】
- `session-state/<sessionId>/events.jsonl`（+ `workspace.json`/`metadata.json` 记 `cwd`）。旧版 `history-session-state/<id>.json`（单 JSON，无 token）。
- 事件封装 `{type, id, timestamp, parent_id?, data}`；`type` ∈ `session.start{session_id, copilot_version, start_time, context{cwd,git_root,branch,head_commit,repository}}`、`user.message`、`assistant.message`、`tool.execution_start{tool_call_id, tool_name, arguments}`、`tool.execution_complete{tool_call_id, success, result, model}`、`session.model_change{new_model}`、`session.shutdown{model_metrics{<model>:{usage{input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens}}}, current_model}`。
- token 只在 shutdown 汇总；无 cost；无官方文档。

**Copilot Chat（VS Code）**
- `~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/<sessionId>.json`（v3：`{version:3, sessionId, creationDate(ms), requests:[{requestId, timestamp, message{text, attachments}, modelId, agent{id,name}, response[...]}]}`）；空窗口会话在 `globalStorage/emptyWindowChatSessions/`。新版改为 `.jsonl` 增量日志（首行快照 + 后续 patch）。导出命令 `Chat: Export Session…`。token 基本无。【社区】

---

### 2.7 Cline（及 Roo/Kilo 同源）

- 新版 `~/.cline/data/tasks/<taskId>/`；旧版 `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/<ts>/`（Roo Code: `rooveterinaryinc.roo-cline`，Kilo: `kilocode.kilo-code`）。
- 文件：`api_conversation_history.json`（Anthropic MessageParam 数组 `[{role, content:[{type:text|tool_use|tool_result|image,…}]}]`）、`ui_messages.json`（UI 事件：`say/ask` + `api_req_started{tokensIn,tokensOut,cacheWrites,cacheReads,cost}`【社区】）、`task_metadata.json`（`taskId, createdAt, lastModified, modelInfo{id,provider}, tokensUsed{input,output}`）；全局索引 `state/taskHistory.json`【社区，未验证】。
- 企业版可配置把 prompt 同步到 S3。无本地清理。

---

### 2.8 Aider

- 工作目录：`.aider.chat.history.md`（`# aider chat started at YYYY-MM-DD HH:MM:SS` 分段；`#### ` 开头为用户输入；助手回复为正文；工具输出 `>` 引用）、`.aider.input.history`（readline 历史）、`.aider.llm.history`（需 `--llm-history-file`，完整 LLM 请求/响应）。路径由 `--chat-history-file` / `AIDER_CHAT_HISTORY_FILE` 等覆盖。
- 纯 Markdown，无结构化 usage/cost（成本只打印在终端/行内文本）。append 写；随 repo 走。本机未安装。

---

### 2.9 Windsurf

- `~/.codeium/windsurf/cascade/*.pb`、`~/.codeium/windsurf/implicit/`（社区称按 UUID 加密）、`memories/`（Markdown 记忆）；IDE 部分状态在 `~/Library/Application Support/Windsurf/User/workspaceStorage/*/state.vscdb`。无官方导出（feature request 中）。本机仅 `~/.codeium/windsurf/skills`，无会话。→ 只能做「存在性」Provider。

### 2.10 Kiro

- IDE：`~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/`（JSON，按项目分目录，社区工具 kiro-session-export / kiro-history 可解析；用户反馈无清理可达 13 GB）；Kiro CLI 用 SQLite。本机 `~/.kiro/` 仅 extensions/skills。

### 2.11 Amp

- `~/.local/share/amp/threads/T-<uuid>.json`：`{id:"T-…", created(ms), title?, messages:[{role, content:[blocks], usage?, state?, meta?}], env?}`【社区 sidecar/lore-cli 解析器】；线程同时同步到 ampcode.com（可网页导出）。本机无。

### 2.12 Goose

- v1.10+：`~/.local/share/goose/sessions/sessions.db`（sessions / messages / usage ledger，含 input/output/cache read/write 与 cost 字段【社区】）；旧版同目录 `*.jsonl` 保留但不再管理。请求级日志 `~/.local/state/goose/logs/`（只保留最近 10 个）。`goose session export` 可导 Markdown。本机 `~/.config/goose/` 仅 skills。

### 2.13 Pi（pi-coding-agent）

- `~/.pi/agent/sessions/--<cwd 的 / 换 ->--/<timestamp>_<uuid>.jsonl`。
- 首行 `{type:"session", version:3, id, timestamp, cwd, parentSession?}`；其余行 `{type, id(8hex), parentId, timestamp, …}` 构成树：`message{message:AgentMessage}`、`model_change{provider,modelId}`、`thinking_level_change`、`compaction{summary, firstKeptEntryId, tokensBefore}`、`branch_summary{fromId, summary}`、`custom`、`custom_message`、`label{targetId,label}`、`session_info{name}`。
- assistant message：`api, provider, model, stopReason(stop|length|toolUse|error|aborted), usage{input,output,cacheRead,cacheWrite,totalTokens, cost{input,output,cacheRead,cacheWrite,total}}`。**结构化 cost 最完整的格式之一。** 本机 `~/.pi/agent/` 无 sessions。

### 2.14 Claude Desktop（Chat / Cowork / Code）

- 标准版 `~/Library/Application Support/Claude/`（3P 版 `Claude-3p/`）：
  - `local-agent-mode-sessions/<ant-did>/<orgId>/local_<uuid>.json`：会话状态（本机字段：`sessionId, processName, cliSessionId, cwd, createdAt, lastActivityAt, model, isArchived, title, initialMessage, slashCommands, enabledMcpTools, sessionType(scheduled|…), scheduledTaskId, memoryEnabled, …`）；同名目录含 `audit.jsonl`（完整 prompt / 回复 / 工具 IO / thinking / 权限决策 / 模型与 token 用量，可带 HMAC 签名 `.audit-key`）、`.claude/`（内嵌的 Claude Code 工作目录）、`uploads/`、`outputs/`。
  - `side-session-notes/local_<uuid>/`、`claude-code-sessions/`（Code 会话元数据；转录仍在 `~/.claude/projects/`）、`IndexedDB/`（仅 UI 状态，**claude.ai 网页聊天不落本地**）。
- 清理：默认不删；企业可配 `chatSessionRetentionDays` / `coworkSessionRetentionDays` / `codeSessionRetentionDays`（1–3650）。
- 结论：Cowork/本地 Chat 可通过 `audit.jsonl` 归档；云端 claude.ai 对话只能走官方数据导出（与 ChatGPT 类似的 zip），本地无记录。

### 2.15 ChatGPT 数据导出

- zip 内：`conversations.json`（或 `conversations-0.json, -1.json…` 分片）、`chat.html`、`message_feedback.json`、`user.json`、`export_manifest.json`、图片目录。
- 每个 conversation：`{title, create_time, update_time, conversation_id, default_model_slug, current_node, mapping:{<nodeId>:{id, parent, children[], message:{id, author{role:system|user|assistant|tool, name?}, create_time, content{content_type:text|code|multimodal_text|…, parts[]}, status, end_turn, weight, metadata{model_slug,…}}}}}`。根节点 `parent:null, message:null`；编辑/重生成形成分支，从 `current_node` 回溯得主线。格式无官方规范且会变。

---

## 3. 通用协议：能否作为统一采集口

### 3.1 ACP（Agent Client Protocol，Zed 主推）
- JSON-RPC 2.0，Agent 作为 Client 子进程。方法：`initialize`、`session/new`、`session/load`（可选，需 `loadSession` 能力）、`session/prompt`、`session/cancel`；通知 `session/update`，`sessionUpdate` ∈ `user_message_chunk`、`agent_message_chunk`、`agent_thought_chunk`、`tool_call{toolCallId,title,kind,status}`、`tool_call_update{toolCallId,status,content}`、`plan`、`available_commands_update`、`current_mode_update`、`usage_update{used,size,cost?{amount,currency}}`、`notice`、`compaction_update`、`compaction_summary_chunk`。
- Zed 已支持：Claude Agent（`zed-industries/claude-code-acp`）、Codex（codex-acp）、Gemini CLI（参考实现）、OpenCode、Copilot、Cursor、Pi、Poolside、自定义 `agent_servers`。Zed 可「导入」外部 agent 历史到 Thread History（读各 agent 原生目录）。
- 评价：**只有当用户通过 ACP Client 发起会话时**，Client 才能拿到完整流（含 usage_update），所以 ACP 适合做「我们自己当 Client 的实时采集通道」，不能替代读取各 agent 原生落盘；协议本身不规定持久化格式，`session/load` 由 agent 自行实现回放。

### 3.2 MCP
- MCP Server 只能看到被调用的 tool 调用（输入/输出），看不到用户 prompt、助手文本、usage；也无「读取宿主转录」的标准接口。因此 MCP 不适合作统一采集口，只能作补充（例如让 Claude Code hook 的 `mcp_tool` 类型调用我们的「归档」工具）。

### 3.3 OpenTelemetry
- **Claude Code**：`CLAUDE_CODE_ENABLE_TELEMETRY=1` + `OTEL_METRICS_EXPORTER=otlp|prometheus|console` + `OTEL_LOGS_EXPORTER=otlp|console` + `OTEL_EXPORTER_OTLP_*`。指标：`claude_code.session.count`、`token.usage{type:input|output|cacheRead|cacheCreation, model}`、`cost.usage(USD)`、`lines_of_code.count`、`commit.count`、`pull_request.count`、`active_time.total`。事件：`claude_code.user_prompt{prompt_length, prompt(需 OTEL_LOG_USER_PROMPTS=1)}`、`tool_result{tool_name, tool_use_id, success, duration_ms}`、`api_request{model, cost_usd, cost_usd_micros, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cache_creation_5m/1h_tokens, request_id}`、`api_error`、`tool_decision`、`assistant_response`（需 `OTEL_LOG_ASSISTANT_RESPONSES=1`）、`api_request_body/api_response_body`（`OTEL_LOG_RAW_API_BODIES=1|file:<dir>` → 含完整对话）。Traces beta：`CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`。内容默认脱敏、60 KB 截断；**仓库级 settings 不能开启遥测**（仅 user/managed）。
- **Codex**：`[otel] exporter/trace_exporter/log_user_prompt/environment`；事件 `codex.api_request / sse_event / tool_result / user_prompt / conversation_starts`。
- **Gemini CLI**：`telemetry.outfile` 可直接写本地 JSON 文件（最省事的本地采集）。
- 评价：OTEL 是最标准的「实时 token/cost 流」，但默认不含内容，且需要用户在环境变量/settings 中开启；适合做仪表盘，不适合作主归档源。

---

## 4. 规范化会话模型（草案）

目标：所有 Provider 产出同一套实体；原始记录以 `raw` 字段保留以便无损回放。

```ts
type Provider = 'claude-code' | 'codex' | 'gemini-cli' | 'opencode' | 'cursor' | 'copilot-cli' | 'copilot-chat'
  | 'cline' | 'aider' | 'windsurf' | 'kiro' | 'amp' | 'goose' | 'pi' | 'claude-desktop' | 'chatgpt-export' | 'antigravity'

type Session = {
  id: string                       // 归档内全局唯一：`${provider}:${machineId}:${nativeId}`
  provider: Provider
  nativeId: string                 // 原生 session/thread/composer id
  parentSessionId?: string         // 子代理 / fork 来源（Claude subagents、OpenCode parent_id、Codex parent_thread_id、Pi parentSession）
  kind: 'main' | 'subagent' | 'fork' | 'scheduled'
  title?: string                   // custom-title > agent-name > summary > 首条用户消息截断
  cwd?: string; gitBranch?: string; gitRemote?: string; gitCommit?: string
  machineId: string; hostname?: string
  agentVersion?: string            // Claude Code `version` / Codex `cli_version`
  entrypoint?: string              // cli / desktop / vscode / sdk
  startedAt: string; endedAt?: string; updatedAt: string   // ISO
  models: string[]
  usage: Usage                     // 聚合
  cost?: Cost
  source: { path: string; sizeBytes: number; mtime: string; sha256?: string; compressed?: 'zstd' }
  retention: { agentAutoDeletes: boolean; expectedDeleteAfter?: string }   // 用于决定拷贝紧迫度
  tags?: string[]
}

type Turn = {                      // 一次用户提示 + 直到下一次用户提示之间的全部内容
  id: string; sessionId: string; index: number
  promptId?: string                // Claude promptId / Codex turn_id
  startedAt: string; endedAt?: string
  usage?: Usage; cost?: Cost
  compactionBoundary?: boolean     // 该轮前发生 compact
}

type Message = {
  id: string; sessionId: string; turnId: string
  nativeId?: string; parentId?: string          // 保留树结构（Claude parentUuid / Pi parentId / ChatGPT parent）
  role: 'user' | 'assistant' | 'system' | 'tool' | 'developer'
  isMeta?: boolean                              // 注入的环境/attachment/compact 摘要
  timestamp: string
  model?: string
  stopReason?: string
  blocks: ContentBlock[]
  usage?: Usage                                 // 仅 assistant；已按 nativeMessageId 去重
  raw?: unknown
}

type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'thinking'; text?: string; signature?: string; encrypted?: boolean }   // Codex reasoning 为 encrypted
  | { type: 'tool_call'; toolCallId: string; name: string; input: unknown; server?: string }
  | { type: 'tool_result'; toolCallId: string; output: string | unknown; isError?: boolean; durationMs?: number; truncated?: boolean; spilledPath?: string }
  | { type: 'image'; mediaType: string; data?: string; path?: string }
  | { type: 'file_ref'; path: string }
  | { type: 'compaction'; summary: string; preTokens?: number; trigger?: 'manual' | 'auto' }
  | { type: 'unknown'; nativeType: string; raw: unknown }

type ToolCall = {                 // 由 tool_call + tool_result 配对后派生，便于统计
  id: string; sessionId: string; messageId: string
  name: string; kind?: 'bash' | 'read' | 'edit' | 'write' | 'search' | 'mcp' | 'web' | 'subagent' | 'other'
  input: unknown; output?: unknown; isError?: boolean
  startedAt?: string; durationMs?: number
  permission?: { decision: 'accept' | 'reject'; source?: string }
}

type Usage = {
  inputTokens: number; outputTokens: number
  cacheReadTokens?: number; cacheWriteTokens?: number
  cacheWrite5mTokens?: number; cacheWrite1hTokens?: number   // Claude 独有
  reasoningTokens?: number                                     // Claude thinking_tokens / Codex reasoning_output_tokens / Gemini thoughts
  toolTokens?: number                                          // Gemini
  totalTokens?: number
  requests?: number
  inputIncludesCache?: boolean   // Goose/Codex 的 input 含 cache，Claude 的不含 —— 必须标注，否则跨 agent 对比会重复计数
}

type Cost = { amount: number; currency: 'USD'; source: 'agent-reported' | 'computed'; pricingVersion?: string }
```

字段映射要点：
- Claude Code：Turn 按 `promptId` / `turnPosition` 切；Message 的 `nativeId = message.id`，多条 assistant 记录合并为一条 Message（blocks 合并，usage 取带 stop_reason 的最后一条）；`cost-state` → Session.cost(agent-reported)。
- Codex：Turn = `turn_id`；assistant Message 由连续 `response_item` 聚合；usage 取 `token_usage_record.usage`（按 `response_id` 去重）。
- OpenCode：Message = `message` 行，blocks = `part` 行；cost 直接取。
- Cursor：Message = bubble，顺序按 `fullConversationHeadersOnly`。
- Pi：天然树 + cost，几乎一一对应。

---

## 5. 实时采集方式

| 方式 | 适用 | 优点 | 缺点 |
|---|---|---|---|
| **文件 watcher + 增量 tail**（fsevents/inotify；按 inode + byte offset 续读） | Claude Code、Codex、Copilot CLI、Pi、Aider（append-only JSONL/MD） | 零配置、内容完整、可追溯到 raw | 需处理：Claude 异步写（hook 触发时文件可能落后）、Codex 压缩/解压导致 inode 变化、`.superseded` 重命名、多进程同时写不同文件 |
| **整文件重读 + 内容哈希** | Gemini CLI、Amp、Cline、Copilot Chat、Claude Desktop local_*.json（整文件覆盖写） | 简单 | 大文件频繁重解析；用 mtime+size 预筛 |
| **SQLite 轮询 / WAL 变更检测** | OpenCode、Cursor、Goose、Kiro CLI | 结构化 | 必须只读打开（`?immutable=1` 或复制 db+wal+shm 后读）；Cursor 库巨大 |
| **Claude Code hooks** | Claude Code | `SessionStart/SessionEnd(reason: clear|resume|logout|prompt_input_exit|other)/UserPromptSubmit/Stop/SubagentStop/PreCompact/PostCompact` 均带 `session_id, transcript_path, cwd, prompt_id, agent_id`；`http` 类型 hook 可直接 POST 到本地归档服务；`SessionEnd` 是「该拷贝了」的最佳信号；`PreCompact` 是「compact 前快照」信号 | 需写入用户 `~/.claude/settings.json`；transcript 写入异步，SessionEnd 时应延迟几百 ms 再拷 |
| **OTEL 导出** | Claude Code、Codex、Gemini CLI、Claude Desktop 3P | 标准化 token/cost 事件，实时 | 默认无内容；需用户开启；Claude 仅 user/managed 设置生效 |
| **ACP Client** | 所有 ACP agent | 我们自己做 Client 时可拿到全流 + usage_update | 只覆盖经我们发起的会话 |

推荐组合：**watcher（主）+ Claude hooks（触发精确快照）+ OTEL（可选仪表盘）**。

---

## 6. 现有竞品 / 参考工具

| 工具 | 覆盖 | 做法 | 链接 |
|---|---|---|---|
| ccusage | Claude Code（JSONL）；兄弟项目覆盖 Codex 等 | 原地读 `~/.claude/projects`，按 `message.id:requestId` 去重，按价格表算 cost | github.com/ryoppippi/ccusage |
| claude-code-log | Claude Code | 原地读 → HTML/Markdown/TUI；Pydantic 模型定义了 user/assistant/summary/system/queue-operation/attachment | github.com/daaain/claude-code-log |
| claude-conversation-extractor | Claude Code | 原地读 → Markdown 导出 | github.com/ZeroSumQuant/claude-conversation-extractor |
| claude-log-viewer / claude-JSONL-browser / claude-transcript-viewer | Claude Code | Web 查看器，部分支持实时监控 | pypi claude-log-viewer；github.com/withLinda/claude-JSONL-browser |
| txcript | Claude Code、Codex、Cursor desktop/CLI、OpenCode… | Rust；有各格式逆向文档（`docs/formats/*.md`，价值很高） | docs.rs/crate/txcript |
| sessionwiki | Claude、Codex（含 .zst/archived）等 | Rust；原地读，大文件头尾窗口化 | docs.rs/crate/sessionwiki |
| lore-cli | aider、claude_code、cline、codex、continue、gemini、kilo、opencode、roo、amp、vscode | Rust；每 agent 一个 `Watcher`（watch_paths + find_sources），原地读 | docs.rs/crate/lore-cli |
| memorph | Cursor、Kiro、Claude… | Rust；会话格式互转/导入导出 | docs.rs/crate/memorph |
| kaizen-cli / vct-core / agtop / claudex / chasm-cli / codeburn / pond-db / i-self / terraphim-session-analyzer | 多 agent（含 Copilot CLI、Goose、Pi、Copilot Chat） | Rust/Go 解析器，源码可作字段参考 | docs.rs 对应 crate |
| Claudescope | Claude Code、Codex、opencode、Copilot CLI、Junie、pi、Antigravity | 本地 127.0.0.1 web，只读原目录，connector→统一模型，FTS，token/cost 分析，Markdown 脱敏导出 | github.com/vladar107/claudescope |
| cc-session | Claude Code、Codex、Gemini CLI、OpenCode、Cursor CLI、Kimi、Qwen Code | 桌面 app，SQLite FTS5 | github.com/tyql688/cc-session |
| agentic-session-explorer | Claude、Codex、Gemini、opencode | TUI，导出 Markdown/删除 | github.com/junxit/agentic-session-explorer |
| klovi | Claude、Codex、Cursor、OpenCode | 桌面查看器 | github.com/cookielab/klovi |
| nihen/ah (Agent History) | Claude、Codex、Gemini、Copilot、Cursor | Rust CLI 搜索 | github.com/nihen/ah |
| Agent Session Browser | 含 Pi、Antigravity | 只读 web/TUI，npx | （ProductHunt 条目） |
| SpecStory | Cursor、Copilot、Claude Code（VS Code 扩展） | **拷贝式**：自动把每个会话写成项目内 `.specstory/history/<ts>-<title>.md`，依赖 IDE 把 sqlite 刷盘后同步，有延迟；可提交进 git | docs.specstory.com |
| cursor-view / cursor-chat-browser / cursor-history | Cursor | 原地读 state.vscdb → web/导出 | github.com/saharmor/cursor-view |
| codex-transcripts / codex-jsonl-viewer / Codex Trace / codex-logs | Codex | 原地读 rollout（含 CODEX_HOME、archived），HTML/TUI/桌面 | github.com/prateek/codex-transcripts |
| Codex Desktop 自身 | Claude Code | `external_agent_session_imports.json`：扫描 `~/.claude/projects/*.jsonl` 导入 | 本机实测 |
| Zed Thread History | 所有 ACP agent | 从各 agent 原生目录「导入」为归档线程 | zed.dev/docs/ai/external-agents |
| kiro-session-export / kiro-history、Goose `session export`、chatgpt-to-markdown、pionxzh/chatgpt-exporter | 单 agent | 导出脚本 | 见参考链接 |

观察：几乎所有竞品都是**原地只读 + 内存/FTS 索引**，没有解决「30 天自动删除」问题；只有 SpecStory 做了持久拷贝（但仅 IDE 侧、Markdown 有损）。这是本项目的差异化点。

---

## 7. 「软链还是拷贝」决策建议

1. **必须拷贝**（原目录会被自动清理）：Claude Code（CLI 发起 30 天；Desktop 发起虽默认保留但可被 managed/HIPAA/`desktopSessionCleanupPeriodDays` 改变）、Gemini CLI（30 天）、Windsurf（用户反馈被裁剪）。
2. **建议拷贝**（不删但会变形/膨胀/格式封闭）：Codex（冷文件被 zstd 压缩；拷贝时可保留 .zst 节省空间，索引时解压）、Cursor（单库巨大且 schema 漂移，拷贝时需同时拷 `-wal/-shm` 或用 `sqlite3 .backup`）、OpenCode / Goose（db 无限增长）。
3. **可软链/原地索引**：Aider（随 repo）、Pi、Amp、Cline、Copilot（无清理，纯文件）。
4. 拷贝策略：内容寻址（sha256）+ 按 `sessionId` 去重的增量快照；对 append-only 文件只追加 diff；保留 raw 原件 + 规范化 JSON 两份；记录 `source.path` 以便用户溯源。
5. 触发：Claude Code 用 `SessionEnd`/`Stop`/`PreCompact` hook（http 类型）通知归档服务；其余用 watcher；兜底每小时全量扫描。

---

## 8. 参考链接

**官方文档**
- Claude Code `.claude` 目录与自动清理：https://code.claude.com/docs/en/claude-directory
- Claude Code settings / 设置参考（`cleanupPeriodDays`、`desktopSessionCleanupPeriodDays`）：https://code.claude.com/docs/en/settings 、https://code.claude.com/docs/en/settings-reference
- Claude Code 监控与 OTEL：https://code.claude.com/docs/en/monitoring-usage
- Claude Code hooks：https://code.claude.com/docs/en/hooks
- Claude Desktop 本地数据：https://claude.com/docs/third-party/claude-desktop/data-storage
- Codex 配置参考（history、otel）：https://learn.chatgpt.com/docs/config-file/config-reference
- Codex protocol.rs（TokenUsage / RolloutItem）：https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
- Codex OTEL PR：https://github.com/openai/codex/pull/2103
- Gemini CLI 会话管理（sessionRetention）：https://geminicli.com/docs/cli/session-management/
- Gemini CLI 遥测：https://geminicli.com/docs/cli/telemetry/
- OpenCode 1.2 SQLite 迁移：https://learnopencode.com/en/changelog/v1.2.0 ；https://github.com/anomalyco/opencode/issues/32716
- Cline prompt storage：https://docs.cline.bot/enterprise-solutions/monitoring/prompt-storage
- Aider 选项：https://aider.chat/docs/config/options.html
- Windsurf memories：https://docs.windsurf.com/windsurf/cascade/memories
- Pi session format：https://cdn.jsdelivr.net/npm/@mariozechner/pi-coding-agent@0.64.0/docs/session.md
- Goose logs：https://block.github.io/goose/docs/guides/logs
- ACP：https://agentclientprotocol.com/protocol/overview ；https://agentclientprotocol.com/protocol/prompt-turn
- Zed 外部 agent：https://zed.dev/docs/ai/external-agents
- SpecStory：https://docs.specstory.com/features

**社区解析器 / 格式逆向**
- txcript 格式文档（Cursor、Claude Code）：https://docs.rs/crate/txcript/latest/source/docs/formats/cursor-desktop.md ；https://docs.rs/crate/txcript/0.11.0/source/docs/formats/claude-code.md
- claude-code-log models：https://github.com/daaain/claude-code-log/blob/main/claude_code_log/models.py
- ccusage 去重 commit：https://github.com/ryoppippi/ccusage/commit/b1d04ac
- sessionwiki Codex adapter：https://docs.rs/crate/sessionwiki/latest/source/src/adapters/codex.rs
- Codex 会话生命周期 / 压缩（博客）：https://codex.danielvaughan.com/2026/06/08/codex-cli-session-lifecycle-archive-resume-fork-rollout-persistence-management/
- Copilot CLI 事件模型：https://docs.rs/crate/vct-core/2.4.1/source/src/models/copilot.rs ；https://docs.rs/crate/kaizen-cli/latest/source/src/collect/tail/copilot_cli.rs
- Copilot Chat 会话文件指南：https://github.com/NiclasOlofsson/remember-mcp-vscode/wiki/VS-Code-Copilot-Chat-Session-Files-Technical-Guide
- lore-cli watchers：https://docs.rs/crate/lore-cli/latest/source/src/capture/watchers/amp.rs
- Amp adapter：https://pkg.go.dev/github.com/marcus/sidecar/internal/adapter/amp
- Goose SQLite 迁移说明：https://instagit.com/block/goose/goose-sqlite-session-storage-jsonl-migration.md
- Kiro 存储膨胀 issue：https://github.com/kirodotdev/Kiro/issues/5469 ；导出工具 https://github.com/MeXenon/kiro-session-export
- Windsurf 导出 feature request：https://feedback.windsurf.com/feature-requests/p/export-chat
- ChatGPT 导出格式：https://difegam.github.io/chatgpt-to-markdown/explanation/export-format ；https://community.openai.com/t/decoding-exported-data-by-parsing-conversations-json-and-or-chat-html/403144
- Cursor state.vscdb 膨胀 / 丢失讨论：https://forum.cursor.com/t/agent-chat-db-state-vscdb-grows-without-bound-50-bubble-rows-per-visible-message-96-gb-and-climbing/170508
- 多 agent 查看器：https://github.com/vladar107/claudescope ；https://github.com/tyql688/cc-session ；https://github.com/junxit/agentic-session-explorer ；https://github.com/cookielab/klovi ；https://github.com/nihen/ah ；https://github.com/saharmor/cursor-view ；https://github.com/prateek/codex-transcripts
