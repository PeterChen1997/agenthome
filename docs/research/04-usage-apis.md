# AI 用量 / 配额数据来源调研（OpenAI + Claude 为主）

> 调研日期：2026-11-05。目标：为"统计 OpenAI 与 Claude 整体使用情况、token 消耗、剩余用量"的桌面应用梳理所有可行数据来源。
> 标注约定：**[官方]** = 官方文档核实；**[源码]** = 开源项目源码核实（openai/codex、CodexBar docs、ccusage 等）；**[本机]** = 在本机只读检查核实（仅记录文件名/字段名，未记录任何值）；**[未核实]** = 仅来自二手资料/社区报告。

---

## 0. 数据来源总览表

| # | 来源 | 覆盖对象 | 鉴权 | 能否拿到"剩余额度" | 实时性 | ToS 风险 | 推荐度 |
|---|------|----------|------|-------------------|--------|----------|--------|
| A1 | Anthropic Admin API：`/v1/organizations/usage_report/messages` + `/v1/organizations/cost_report`（legacy） | Claude **API 组织**（Console）的 token/成本 | Admin API key `sk-ant-admin01-…`（仅组织管理员；个人账号无 Admin API） | 否（只有历史用量/成本，无配额剩余；可结合 Rate Limits API 推算） | ~5 min 延迟，建议 ≤1 次/分钟轮询 | 无 | ★★★★（API 用户） |
| A2 | Anthropic Usage & Cost **Analytics API**（新，beta）：`/v1/organizations/analytics/usage` + `/analytics/cost` | 同上 + Claude Enterprise（Analytics key）；可按 `product`（claude_code 等）、`actor` 分组 | Admin API key 或 Analytics API key；需 `anthropic-beta: analytics-usage-cost-2026-08-24` | 否 | 同上 | 无 | ★★★★（新集成优先） |
| A3 | Claude Code Analytics API：`/v1/organizations/usage_report/claude_code` | 用 Console 账号登录的 Claude Code 用户（按日、按人） | Admin API key | 否 | ≤1h 延迟，仅日粒度 | 无 | ★★★（团队） |
| A4 | **OAuth usage 端点** `GET https://api.anthropic.com/api/oauth/usage` | Claude **Pro/Max/Ultra 订阅**（即 Claude Code `/usage` 的数据源） | `Authorization: Bearer <claudeAiOauth.accessToken>` + `anthropic-beta: oauth-2025-04-20`；token 来自 `~/.claude/.credentials.json` 或 macOS Keychain `Claude Code-credentials` | **是**：`five_hour` / `seven_day` / `seven_day_opus` / `seven_day_sonnet` 的 `utilization` + `resets_at`、`extra_usage` | 实时（Claude Code 自身也会被该端点限流，见 §1.4） | **高**：未公开端点；Anthropic 2026-02 起明文禁止第三方使用 Free/Pro/Max OAuth token，并禁止"收集/存储/中转 Claude.ai 凭据或 session token" | ★★（功能最强但合规最差） |
| A5 | claude.ai Web 端点（`sessionKey` cookie）：`/api/organizations/{org}/usage` 等 | 同 A4 | 浏览器 cookie `sessionKey=sk-ant-…` | 是 | 实时；Cloudflare 可能拦截 | 高（同上，属"收集 session token"） | ★ |
| A6 | Claude Code **statusline stdin JSON** `rate_limits.five_hour/seven_day.{used_percentage,resets_at}` | Pro/Max/Ultra 订阅 | 无需（Claude Code 自己推送给你的脚本） | **是**（官方、合规） | 每次 API 响应后更新；仅在 Claude Code 运行时 | **无**（官方特性） | ★★★★★（订阅用户首选） |
| A7 | Claude Code CLI PTY 跑 `/usage` 再解析屏幕 | 同上 | 无需 | 是 | 慢（启动 CLI 数秒），每次调用消耗一次 usage 请求 | 低（走官方二进制） | ★★（兜底） |
| A8 | Claude Code OpenTelemetry（`CLAUDE_CODE_ENABLE_TELEMETRY=1` + OTLP） | 任何计划的 Claude Code token/成本 | 无需；本地 OTLP collector | 否（只有消耗量） | 近实时（默认 60s 导出） | 无 | ★★★★（精确、本地、跨计划） |
| A9 | 本地 transcript `~/.claude/projects/**/*.jsonl` 的 `message.usage` + 定价表 | 任何计划的 Claude Code token/估算成本 | 无需（只读文件） | 否（可用 5h 滑窗自行估算，但不准） | 实时（文件追加） | 无 | ★★★★★（历史统计基础） |
| A10 | `~/.claude/stats-cache.json` | Claude Code 本机聚合统计（按日/按模型） | 无需 | 否 | 由 CLI 定期重算 | 无 | ★★★（非官方文档，格式可能变） |
| A11 | Messages API 响应头 `anthropic-ratelimit-*` | API key 用户的 RPM/ITPM/OTPM | 需你自己发 API 请求 | 部分（分钟级令牌桶剩余，非月度配额） | 实时 | 无 | ★★（仅当 app 自己代理请求） |
| A12 | 响应头 `anthropic-ratelimit-unified-{5h,7d}-utilization/-reset` | 订阅 OAuth 请求 | 需拦截 Claude Code 流量（代理） | 是 | 实时 | 中（需代理 + 非文档化） | ★ |
| O1 | OpenAI Usage API `/v1/organization/usage/completions` 等 + Costs `/v1/organization/costs` | OpenAI **API 组织** | Admin API key | 否 | 分钟/小时/日桶 | 无 | ★★★★ |
| O2 | ChatGPT 后端 `GET https://chatgpt.com/backend-api/wham/usage`（Codex CLI `/status` 同源） | **ChatGPT Plus/Pro/Business 订阅下的 Codex 配额** | `Authorization: Bearer <tokens.access_token>` + `chatgpt-account-id: <tokens.account_id>`，均来自 `~/.codex/auth.json` | **是**：`rate_limit.primary_window/secondary_window.{used_percent,limit_window_seconds,reset_at}`、`credits`、`plan_type` | 实时 | 中：未文档化端点；OpenAI 未明文禁止第三方 harness，但端点可随时变更 | ★★★ |
| O3 | Codex app-server JSON-RPC（`codex app-server` → `account/rateLimits/read`） | 同 O2 | 无需（走官方二进制） | 是 | 实时 | **低**（官方 CLI 接口） | ★★★★★（Codex 首选） |
| O4 | Codex rollout `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` 的 `event_msg/token_count` | Codex token 消耗 + **最近一次** `rate_limits` 快照 | 无需 | 是（快照，仅在 Codex 有活动时更新） | 文件追加 | 无 | ★★★★★（历史 + 兜底剩余额度） |
| O5 | 响应头 `x-codex-primary-used-percent` 等 | Codex 请求 | 需拦截 Codex 流量 | 是 | 实时 | 中 | ★ |
| O6 | 响应头 `x-ratelimit-*` | OpenAI API 请求 | 需自己发请求 | 部分（分钟级） | 实时 | 无 | ★★ |
| G1 | Gemini CLI：`cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota` | Gemini CLI 配额 | `~/.gemini/oauth_creds.json` | 是（`remainingFraction`, `resetTime`） | 实时 | 高：私有端点；个人/AI Pro/Ultra 账号的 CLI OAuth 已于 2026-06-18 停服 | ★ |
| C1 | GitHub Copilot：`GET https://api.github.com/copilot_internal/user` | premium requests 配额 | GitHub OAuth token（device flow，`read:user`） | 是（`quota_snapshots.premium_interactions`） | 实时 | 中（内部端点） | ★★ |
| C2 | GitHub 公开 billing API `/users/{username}/settings/billing/premium_request/usage` | 同上（用量） | fine-grained PAT | 用量而非剩余 | 日 | 无 | ★★★ |
| U1 | Cursor：`cursor.com/api/usage-summary`、`/api/usage?user=`、`/api/auth/me` | Cursor 计划用量 | `WorkosCursorSessionToken` cookie 或 `state.vscdb` 里的 `cursorAuth/accessToken` | 是 | 实时 | 高（无公开 API） | ★ |
| R1 | OpenRouter：`GET https://openrouter.ai/api/v1/key` + `/api/v1/credits` | OpenRouter key 额度/余额 | 普通 API key（Bearer） | **是**（`limit_remaining`、`total_credits - total_usage`） | 实时（credits 可能 60s 缓存） | **无**（官方公开） | ★★★★ |

---

## 1. Anthropic / Claude 详细

### 1.1 Admin API — Usage & Cost（legacy，但仍支持）**[官方]**

来源：https://platform.claude.com/docs/en/manage-claude/usage-cost-api

- **鉴权**：Admin API key（`sk-ant-admin01-…`）、带 `org:admin` scope 的 OAuth token、或未绑定 workspace 的 personal/service-account key；workspace key 不可用。**个人账号无 Admin API**（需在 Console 建组织）。Header：`x-api-key` + `anthropic-version: 2026-10-21`。
- **Usage 端点** `GET /v1/organizations/usage_report/messages`
  - 参数：`starting_at`、`ending_at`、`bucket_width`（`1m`/`1h`/`1d`）、`group_by[]`（`model`/`workspace_id`/`api_key_id`/`service_tier`/`context_window`/`inference_geo`/`speed`）、过滤 `models[]`/`service_tiers[]`/`context_window[]`/`api_key_ids[]`/`workspace_ids[]`/`inference_geos[]`/`speeds[]`、`limit`、`page`。
  - 桶数限制：`1m` 默认 60 / 最大 1440；`1h` 默认 24 / 最大 168；`1d` 默认 7 / 最大 31。
  - 字段（Analytics API 示例中核实）：`uncached_input_tokens`、`cache_creation.ephemeral_1h_input_tokens`、`cache_creation.ephemeral_5m_input_tokens`、`cache_read_input_tokens`、`output_tokens`、`server_tool_use.web_search_requests`。
- **Cost 端点** `GET /v1/organizations/cost_report`：仅 `1d`；USD，以"最小单位（cents）的 decimal string"返回；`group_by[]=workspace_id|description`；不含 Priority Tier 成本。
- 数据延迟约 5 分钟；建议轮询 ≤1 次/分钟；分页 `has_more`/`next_page`。
- 默认 workspace 的 `workspace_id` 为 `null`；Claude Code 的 Console 用户用量落在自动创建的 "Claude Code" workspace。
- **不覆盖**：Pro/Max 订阅用量（归属 claude.ai org）。

### 1.2 Usage & Cost **Analytics API**（新，beta）**[官方]**

来源：https://platform.claude.com/docs/en/manage-claude/usage-cost-analytics-api

- 端点：`GET /v1/organizations/analytics/usage`、`GET /v1/organizations/analytics/cost`。
- Header：`anthropic-beta: analytics-usage-cost-2026-08-24` + `anthropic-version`。
- 参数：`time_bucket_width`（`1m`/`1h`/`1d`，可省略→整段汇总）、`group_by[]`（≤5 维：`model`、`workspace_id`、`api_key_id`、`actor`、`service_tier`、`product`…）、`order_by`（`total_tokens`/`output_tokens`/`uncached_input_tokens`/`requests`/`time`）、`order`、各维度 `*_ids[]` 过滤（≤100）。
- `products[]=claude_code&group_by[]=actor` → 按人统计 Claude Code 成本（仅 Console 登录的用户）。
- Claude Enterprise 用 Analytics API key（`read:analytics`，在 claude.ai/analytics/api-keys 创建）。

### 1.3 Claude Code Analytics API **[官方]**

来源：https://platform.claude.com/docs/en/manage-claude/claude-code-analytics-api

- `GET /v1/organizations/usage_report/claude_code?starting_at=YYYY-MM-DD&limit=&page=`，单日、按 actor 一行。
- 字段：`actor{type:user_actor|api_actor, email_address|api_key_name}`、`customer_type`(`api`|`subscription`)、`terminal_type`、`core_metrics{num_sessions, lines_of_code{added,removed}, commits_by_claude_code, pull_requests_by_claude_code}`、`tool_actions{edit_tool|multi_edit_tool|write_tool|notebook_edit_tool{accepted,rejected}}`、`model_breakdown[]{model, tokens{input,output,cache_read,cache_creation}, estimated_cost{currency,amount(cents)}}`。
- 延迟 ≤1h；免费；**明确不含 Pro/Max/Team/Enterprise 订阅登录的用量**。

### 1.4 Pro/Max 订阅：`/usage` 的数据源 = OAuth usage 端点 **[源码 + 官方旁证]**

**官方旁证**（code.claude.com/docs/en/costs）："When the request for your plan limits fails, most often because **the usage endpoint is rate limited**, `/usage` shows the last usage bars it loaded on this machine within the past 60 minutes" → 证实 `/usage` 是向一个远端 usage 端点请求，且该端点有限流、Claude Code 本地缓存 60 分钟快照。

**端点细节（CodexBar `docs/claude.md`、cship、ccusage-py 等多方一致，Anthropic 未公开文档）**：

| 项 | 值 | 来源 |
|---|---|---|
| URL | `GET https://api.anthropic.com/api/oauth/usage`（CodexBar 加 `?cedar_ember=1`，400/403 时去掉重试） | [源码] CodexBar docs/claude.md |
| Header | `Authorization: Bearer <access_token>`、`anthropic-beta: oauth-2025-04-20` | [源码] CodexBar、cship |
| 所需 scope | `user:profile`（仅 `user:inference` 的 token——如 `claude setup-token` 产出——不能调） | [源码] CodexBar |
| Token 来源 | `~/.claude/.credentials.json` → `claudeAiOauth.accessToken`（Linux/部分 macOS）；macOS 主要在 Keychain item `Claude Code-credentials`（同样是 JSON，含 `claudeAiOauth{accessToken,refreshToken,expiresAt,scopes,subscriptionType}`）。**本机核实**：`.credentials.json` 目前只含 `mcpOAuth`（MCP 服务器的 OAuth），**不含** `claudeAiOauth`——与 CodexBar issue #1844 描述的 Claude Code 2.1.x 行为一致，说明主凭据在 Keychain。 | [本机] + [源码] |
| 响应字段 | `five_hour{utilization, resets_at}`、`seven_day{…}`、`seven_day_opus{…}`、`seven_day_sonnet{…}`、`limits[].weekly_scoped`（模型级周窗）、`seven_day_routines`/`seven_day_cowork`、`extra_usage`（月度花费/上限） | [源码] CodexBar；ccusage-py 0.1.11 示例 |
| `utilization` 单位 | **0–100 百分比**（cship 注释），`resets_at` 可为 null | [源码] cship |
| 配套 | `GET https://api.anthropic.com/api/oauth/profile`（身份/计划） | [源码] CodexBar |

**刷新策略参考**：Claude Code 自身缓存 60 min；CodexBar 自适应/1–30 min；Usage4Claude 1 min 活跃→3/5/10 min 退避。

**ToS 风险（关键）** **[官方]**：code.claude.com/docs/en/legal-and-compliance：
- "OAuth authentication is intended exclusively for purchasers of Claude … subscription plans and is designed to support ordinary use of Claude Code and other native Anthropic applications."
- "developers may not collect, store, or intermediate Claude.ai credentials or session tokens"。
- Anthropic 保留无预告执行权。2026-01 起已对伪装 Claude Code 的第三方客户端做服务端拦截（[未核实] 二手报道）。
- **结论**：读取 Keychain 里的 OAuth token 去请求 `/api/oauth/usage` 虽然"只读、不消耗 token"，但字面上属于"使用 OAuth token 于其它工具"+"收集/存储凭据"。合规路径是 A6（statusline）和 A7（PTY 跑官方 CLI）。

### 1.5 Claude Code statusline（官方、合规的"剩余额度"来源）**[官方]**

来源：https://code.claude.com/docs/en/statusline

- settings.json 配置 `statusLine.command`，Claude Code 在每次 API 响应后把 JSON 写入脚本 stdin（可加 `refreshInterval` 秒级定时）。
- 关键字段：
  - `rate_limits.five_hour.{used_percentage(0–100), resets_at(epoch s)}`、`rate_limits.seven_day.{…}`、`rate_limits.spend_limit.{used_percentage,resets_at,used_usd,limit_usd,period}`（gateway 场景）。
  - **仅 claude.ai Pro/Max/Ultra 订阅者出现**，且在会话首个 API 响应后才有；窗口过期后字段被丢弃。
  - `cost.total_cost_usd`（本地按列表价估算）、`context_window.{total_input_tokens,total_output_tokens,context_window_size,used_percentage,current_usage}`、`prompt_cache.*`、`session_id`、`transcript_path`、`model`、`version`。
- **实现建议**：目标 app 安装一个极小 statusline 脚本（或 hook），把 stdin JSON 原样追加写到 app 自己的本地文件（如 `~/Library/Application Support/<app>/claude-ratelimits.jsonl`），app 读该文件即可得到最新 5h/7d 百分比与 reset 时间。零 ToS 风险，但仅在用户跑 Claude Code 时更新（Claude-Code-Usage-Monitor 的 `--statusline` 模式就是这么做的）。

### 1.6 Messages API 响应头 **[官方]**

来源：https://platform.claude.com/docs/en/api/rate-limits

- `retry-after`
- `anthropic-ratelimit-requests-{limit,remaining,reset}`
- `anthropic-ratelimit-tokens-{limit,remaining,reset}`（当前最严格的那条）
- `anthropic-ratelimit-input-tokens-{limit,remaining,reset}`、`anthropic-ratelimit-output-tokens-{limit,remaining,reset}`
- `anthropic-priority-input-tokens-*`、`anthropic-priority-output-tokens-*`（Priority Tier）
- `anthropic-fast-*`（fast mode）、`anthropic-workspace-id`
- reset 为 RFC 3339；remaining 四舍五入到千。令牌桶算法，非固定窗口。
- ITPM 只计 `input_tokens + cache_creation_input_tokens`，cache read 不计（Haiku 3.5 除外）。
- 429 可能是月度 spend cap（`error.details.error_code = enforced_spend_limit_reached`，无 retry-after）。

**订阅 OAuth 请求的统一限额头** **[未核实，社区一致]**：`anthropic-ratelimit-unified-5h-utilization`（0.0–1.0 小数）、`anthropic-ratelimit-unified-5h-reset`（epoch）、`…-7d-…`、`…-7d_sonnet-…`、`anthropic-ratelimit-unified-representative-claim`（`five_hour`/`seven_day`，当前约束窗口）、`…-status`（allowed/rejected）。claude-code issue #55333 请求把这些头持久化给 hook/statusline，说明目前 Claude Code 不直接暴露；只能经 `ANTHROPIC_BASE_URL` 代理截获。

### 1.7 本地 transcript JSONL + 定价表（ccusage 做法）**[本机 + 源码]**

- 路径：`~/.claude/projects/<cwd-slug>/<sessionId>.jsonl`（本机 1511 个文件）；`CLAUDE_CONFIG_DIR` 可改；Claude Code 默认 `cleanupPeriodDays=30` 会清理旧文件（历史统计需 app 自己增量归档）。
- 每行顶层字段（本机核实）：`type`、`uuid`、`parentUuid`、`timestamp`、`sessionId`、`requestId`、`requestedModel`、`cwd`、`version`、`gitBranch`、`isSidechain`、`entrypoint`、`effort`…
- `message.model`、`message.id`、`message.usage`：
  - `input_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens`、`output_tokens`
  - `cache_creation{ephemeral_1h_input_tokens, ephemeral_5m_input_tokens}`（1h/5m 拆分，用于准确计费）
  - `output_tokens_details.thinking_tokens`、`server_tool_use{web_search_requests, web_fetch_requests}`、`service_tier`、`speed`、`inference_geo`、`iterations[]`
  - 旧版本记录可能带 `costUSD`（ccusage `auto` 模式优先用它）。
- **去重**：同一响应可能被写多行（流式 chunk / 重试），按 `message.id + requestId` 去重（ccusage 做法）。
- **归档对话**：同一 jsonl 已包含完整对话（user/assistant/tool_result），目标 app 可直接索引做"对话归档"。

### 1.8 OpenTelemetry 导出 **[官方]**

来源：https://code.claude.com/docs/en/monitoring-usage

- 环境变量：`CLAUDE_CODE_ENABLE_TELEMETRY=1`、`OTEL_METRICS_EXPORTER=otlp|prometheus|console`、`OTEL_LOGS_EXPORTER=otlp|console`、`OTEL_EXPORTER_OTLP_PROTOCOL=grpc|http/json|http/protobuf`、`OTEL_EXPORTER_OTLP_ENDPOINT`、`OTEL_EXPORTER_OTLP_HEADERS`、`OTEL_METRIC_EXPORT_INTERVAL`（默认 60000 ms）、`OTEL_LOGS_EXPORT_INTERVAL`（5000 ms）、`OTEL_METRICS_INCLUDE_SESSION_ID` 等基数控制。注意：**project 级 settings 里的 `OTEL_*` 会被忽略**，需放在 user settings / 环境变量 / managed settings。
- 指标：`claude_code.session.count`、`claude_code.lines_of_code.count{type}`、`claude_code.pull_request.count`、`claude_code.commit.count`、`claude_code.cost.usage`（USD，attrs `model, query_source, speed, effort, agent.name, skill.name, mcp_server.name…`）、`claude_code.token.usage`（attrs `type=input|output|cacheRead|cacheCreation, model, …`）、`claude_code.code_edit_tool.decision`、`claude_code.active_time.total`。
- 事件（logs）：`claude_code.api_request`（`model, cost_usd, cost_usd_micros, duration_ms, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens, request_id`）、`claude_code.api_error`、`claude_code.user_prompt`、`claude_code.tool_result`、`claude_code.tool_decision`…
- 目标 app 可内嵌一个最小 OTLP/HTTP 接收器（`http://127.0.0.1:4318/v1/metrics|/v1/logs`），引导用户在 `~/.claude/settings.json` 的 `env` 里写入这些变量。

### 1.9 `~/.claude/stats-cache.json` **[本机]**

存在（本机 2.4 KB）。结构：`version`、`lastComputedDate`、`dailyActivity[]{date,messageCount,sessionCount,toolCallCount}`、`dailyModelTokens[]{date,tokensByModel{<model>:n}}`、`modelUsage{<model>{inputTokens,outputTokens,cacheReadInputTokens,cacheCreationInputTokens,webSearchRequests,costUSD,contextWindow,maxOutputTokens}}`、`totalSessions`、`totalMessages`、`longestSession{sessionId,duration,messageCount,timestamp}`、`firstSessionDate`、`hourCounts{<hour>:n}`、`totalSpeculationTimeSavedMs`。非官方文档、本机最后更新 2026-02-26（似乎不是每次启动都重算，可靠性一般）。另有 `~/.claude/usage-data/report.html`（`/insights` 报告）、`~/.claude/history.jsonl`（prompt 历史）。

### 1.10 Pro/Max 限额规则（供 UI 文案）**[官方/半官方]**

- 5 小时滚动会话窗 + 7 天周窗；Max 另有模型级周窗（Opus/Fable/Sonnet）；claude.ai / Desktop / Claude Code / Cowork 共用。Anthropic 不公布精确 token 数。
- `/usage` 还显示：按 skill/subagent/plugin/MCP 的归因、建议、Loops、usage-credits 月度花费（`extra_usage`）。
- 2026-05-13 ~ 08-31 有"周限额 +50%"促销（support.claude.com/articles/15910845）。

---

## 2. OpenAI 详细

### 2.1 Usage API + Costs API **[官方]**

来源：https://developers.openai.com/api/docs/api-reference/usage

- 鉴权：**Admin API key**（Organization → Admin keys）。
- 端点：`GET /v1/organization/usage/completions`、`/usage/embeddings`、`/usage/images`、`/usage/audio_speeches`、`/usage/audio_transcriptions`、`/usage/moderations`、`/usage/vector_stores`、`/usage/code_interpreter_sessions`；`GET /v1/organization/costs`。
- 参数：`start_time`（必填，Unix s）、`end_time`、`bucket_width`（`1m`/`1h`/`1d`；costs 仅 `1d`）、`project_ids[]`、`user_ids[]`、`api_key_ids[]`、`models[]`、`batch`、`group_by[]`、`limit`、`page`；costs 另有 `line_items[]`。
- 桶上限：`1m` 60/1440、`1h` 24/168、`1d` 7/31；costs `limit` 1–180（默认 7）。
- completions 结果字段：`input_tokens`、`input_cached_tokens`、`output_tokens`、`input_audio_tokens`、`output_audio_tokens`、`num_model_requests`，分组键 `project_id`/`user_id`/`api_key_id`/`model`/`batch`/`service_tier`。
- costs 结果字段：`amount{value,currency}`、`line_item`、`project_id`、`api_key_id`、`user_id`、`api_source`、`quantity`、`quantity_unit`。
- 不含 ChatGPT 订阅/Codex 订阅用量。

### 2.2 ChatGPT 订阅下 Codex 配额（`/status` 的数据源）**[源码：openai/codex]**

**端点**（`codex-rs/backend-client/src/client/rate_limit_resets.rs`）：
```
PathStyle::ChatGptApi => {base}/wham/usage            // base = https://chatgpt.com/backend-api
PathStyle::CodexApi   => {base}/api/codex/usage       // API-key 模式
reset credits: {base}/wham/rate-limit-reset-credits  (+ /consume)
thread usage : {base}/wham/usage/thread_usage/query
```
- Header（`codex-rs/http-client/src/openai_headers.rs`）：`Authorization: Bearer <access_token>`、`chatgpt-account-id: <account_id>`；可选 `x-openai-codex-luna-reserve: 1`。
- **Token 来源** `~/.codex/auth.json`（本机核实字段）：`auth_mode`、`OPENAI_API_KEY`（可为 null）、`tokens{id_token, access_token, refresh_token, account_id}`、`last_refresh`。`id_token` JWT 的 `https://api.openai.com/auth` claim 含 `chatgpt_plan_type`、`chatgpt_user_id`、`chatgpt_account_id`（`codex-rs/login/src/token_data.rs`）。CodexBar 不自己刷新 token，交给 Codex CLI；token 过期前 5 min 视为需刷新。
- **响应**（JSON，来自 `codex_backend_openapi_models` 外部 crate，字段名以社区 + PR 讨论为准 **[部分未核实]**）：`rate_limit{allowed, primary_window{used_percent, limit_window_seconds, reset_at}, secondary_window{…}}`、`additional_rate_limits[]`（如 Spark）、`rate_limit_reset_credits`、`account_id`、`user_id`、`rate_limit_upsell`、`plan_type`、credits。`limit_window_seconds=2592000` 表示固定 30 天窗。
- **协议层快照**（`codex-rs/protocol/src/protocol.rs`，**[源码核实]**）：`RateLimitSnapshot{limit_id, limit_name, normal_model_slug, primary: RateLimitWindow, secondary, credits: CreditsSnapshot, individual_limit: SpendControlLimitSnapshot, spend_control_reached, plan_type, rate_limit_reached_type}`；`RateLimitWindow{used_percent: f64, window_minutes: Option<i64>, resets_at: Option<i64>}`；`CreditsSnapshot{has_credits, unlimited, balance: Option<String>}`；`SpendControlLimitSnapshot{limit, used, remaining_percent, resets_at}`。
- **响应头**（`codex-rs/codex-api/src/rate_limits.rs`，**[源码核实]**）：`x-codex-primary-used-percent`、`x-codex-primary-window-minutes`、`x-codex-primary-reset-at`（已从 `reset-after-seconds` 改为绝对时间戳）、`x-codex-secondary-*`（secondary 系列实际形如 `x-codex-secondary-primary-used-percent`，即 `<limit>-primary-*` 前缀模式）、`x-codex-<limit-name>-limit-name`、`x-codex-credits-has-credits`、`x-codex-credits-unlimited`、`x-codex-credits-balance`、`x-codex-rate-limit-reached-type`、`x-codex-promo-message`。
- **官方 CLI 接口（推荐）**：`codex -s read-only -a never app-server` → JSON-RPC over stdio：`initialize` → `account/read` → `account/rateLimits/read`，返回 windows + credits + email + plan（CodexBar 的 CLI fallback）。
- **限额规则**（learn.chatgpt.com/docs/pricing **[官方]**）：Plus/Business 有 5 小时窗 + 周窗；Pro 目前无 5 小时窗；可买 credits；本地消息与云端任务共用额度；dashboard：https://chatgpt.com/codex/settings/usage。
- **ToS**：未找到 OpenAI 明文禁止第三方使用 ChatGPT OAuth token（与 Anthropic 形成对比）；主要风险是未文档化端点变更（2026-05-27 曾有 breaking change，[未核实]）。

### 2.3 Codex rollout JSONL **[本机核实]**

- 路径：`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`（本机 56 个）；索引 `~/.codex/session_index.jsonl{id, thread_name, updated_at}`；另有 `~/.codex/logs_2.sqlite`、`state_5.sqlite`、`thread_history_1.sqlite` 等 SQLite（CodexBar 的 cost scan 读 SQLite+JSONL）。
- 行类型：`session_meta`、`turn_context`、`response_item`（`message`/`reasoning`/`function_call`/`custom_tool_call`…）、`event_msg`（`token_count`、`task_started`、`task_complete`…）、`token_usage_record`、`compacted`。
- `event_msg` / `payload.type == "token_count"`：
  - `payload.info.total_token_usage{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`（**会话累计**，`input_tokens` 已包含 `cached_input_tokens`）
  - `payload.info.last_token_usage{…}`（最近一轮）
  - `payload.info.model_context_window`
  - `payload.rate_limits{limit_id, limit_name, primary{used_percent, window_minutes, resets_at}, secondary, credits{has_credits, unlimited, balance}, individual_limit, spend_control_reached, plan_type, rate_limit_reached_type}` ← **本机最新快照即剩余额度，无需任何网络/鉴权**（m4fn3/Codex-Usage-Tracker 纯靠这个）。本机样本 `primary.window_minutes=10080`（7 天）、`secondary=null`。
  - `info` 可为 null（仅限额更新事件）。
- 成本：Codex 订阅不按 token 计费，但可用 LiteLLM 价格表估算"等价 API 成本"（CodexBar、ccusage codex 均如此）。

### 2.4 OpenAI API 响应头 **[官方]**

`x-ratelimit-limit-requests`、`x-ratelimit-limit-tokens`、`x-ratelimit-remaining-requests`、`x-ratelimit-remaining-tokens`、`x-ratelimit-reset-requests`（如 `1s`）、`x-ratelimit-reset-tokens`（如 `6m0s`）；project 级 `x-ratelimit-{limit,remaining,reset}-project-tokens`。

---

## 3. 其他可顺带覆盖

| 平台 | 端点 / 来源 | 鉴权 | 字段 | 备注 |
|---|---|---|---|---|
| **Gemini CLI** | `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`（body `{"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}}` → projectId、tier）；`POST …/v1internal:retrieveUserQuota`（body `{"project": id}`） | `~/.gemini/oauth_creds.json{access_token, refresh_token, id_token, expiry_date}`；刷新走 `oauth2.googleapis.com/token`，client_id 需从已安装 Gemini CLI 的 `oauth2.js` 提取 | buckets `remainingFraction`、`resetTime`、`modelId`；tier `paidTier.name`/`standard-tier`/`free-tier`/`legacy-tier` | **[源码 CodexBar docs/gemini.md]**；2026-06-18 起个人/AI Pro/Ultra 的 CLI OAuth 停服（`UNSUPPORTED_CLIENT`），仅 Workspace/Standard/Enterprise 可用；本地还有 `~/.gemini/tmp/**/logs.json` 可做 token 统计（ccusage gemini） |
| **GitHub Copilot** | `GET https://api.github.com/copilot_internal/user`，headers `Authorization: token <oauth>`、`Editor-Version: vscode/1.96.2`、`Editor-Plugin-Version: copilot-chat/0.26.7`、`User-Agent: GitHubCopilotChat/0.26.7`、`X-Github-Api-Version: 2025-04-01` | GitHub OAuth device flow（scope `read:user`）；或复用 `gh` token [未核实] | `quota_snapshots.premium_interactions{entitlement, remaining, unlimited, percent_remaining}`、`quota_snapshots.chat`、`quota_reset_date`、`copilot_plan`、`access_type_sku` | **[源码 CodexBar docs/copilot.md]**；公开替代：`GET /users/{username}/settings/billing/premium_request/usage`（fine-grained PAT，只有用量） |
| **Cursor** | `GET https://cursor.com/api/usage-summary`（计划用量/on-demand/账期）、`GET /api/auth/me`、`GET /api/usage?user=<id>`（legacy 请求数）、`GET /api/dashboard/v2/usage-status?scope=user`、`POST /api/dashboard/get-filtered-usage-events`（需 `Origin: https://cursor.com`） | cookie `WorkosCursorSessionToken=<user_id>%3A%3A<jwt>`（浏览器导入）或 `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` `ItemTable` 键 `cursorAuth/accessToken` | `usedCents`、`limitCents`、`periodStart`、`resetsAt`、`isUnified`、`tokenUsage.totalCents` | **[源码 CodexBar docs/cursor.md]**；无公开 API，403 后 CodexBar 冷却 6h |
| **OpenRouter** | `GET https://openrouter.ai/api/v1/key`（旧文档路径 `/api/v1/auth/key`，返回同一结构）；`GET /api/v1/credits`；`GET /api/v1/activity`（需 Management key） | `Authorization: Bearer <api key>` | key：`limit`、`limit_remaining`（null=无限）、`usage`（累计）、`usage_daily/weekly/monthly`（有则返回）、`is_free_tier`、`rate_limit`（已弃用）；credits：`total_credits`、`total_usage` → 余额 = 差 | **[官方 openrouter.ai/docs/api/reference/limits]**；完全公开合规 |

---

## 4. 成本估算方法

### 4.1 定价表来源
1. **LiteLLM** `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`（3.4 MB，键为模型名，含 `litellm_provider` 前缀变体如 `anthropic/`、`bedrock/`、`openai/`）。字段：`input_cost_per_token`、`output_cost_per_token`、`cache_creation_input_token_cost`（5m）、`cache_creation_input_token_cost_above_1hr`（1h）、`cache_read_input_token_cost`、`input_cost_per_token_above_200k_tokens` / `output_cost_per_token_above_200k_tokens`（长上下文分段价）、`max_input_tokens`、`max_output_tokens`、`mode`、`supports_prompt_caching`。**[官方 raw 核实]**
2. **models.dev** `https://models.dev/api.json`（ccusage 的第二来源，按 provider→model 组织，`cost{input,output,cache_read,cache_write}` 单位 USD/百万）。**[源码 ccusage pricing.rs]**
3. Claude Code 自身：按列表价本地计算；企业可通过 managed settings `modelPricing{multiplier, overrides{<model>{四个每百万价}}}` 覆盖。**[官方]**
4. 官方价格页：platform.claude.com/docs/en/about-claude/pricing、openai.com/api/pricing（人工维护兜底）。

### 4.2 计算规则（ccusage 实践 + Anthropic 官方）
```
cost = input_tokens            * input_price
     + output_tokens           * output_price            (thinking tokens 计入 output)
     + cache_creation_5m       * cache_creation_price    (≈ 1.25 × input)
     + cache_creation_1h       * cache_creation_1h_price (≈ 2 × input)
     + cache_read_input_tokens * cache_read_price        (≈ 0.1 × input)
```
- Claude transcript 若有 `cache_creation{ephemeral_5m,ephemeral_1h}` 拆分则分别计价，否则 `cache_creation_input_tokens` 整体按 5m 价。
- OpenAI/Codex：`input_tokens` 已含 `cached_input_tokens`，应按 `(input - cached) * input + cached * cached_input_price + output * output_price`；`reasoning_output_tokens` 已含在 `output_tokens`。
- 1.1× 数据驻留（`inference_geo=us`）溢价、batch 5 折、Priority Tier 单独计价。
- 模式：`auto`（有 `costUSD` 用之，否则算）/`calculate`/`display`（ccusage）。
- 离线：内嵌定价快照 + 启动后异步刷新并本地缓存（ccusage 把 LiteLLM + models.dev 压缩内嵌于二进制）。
- 模型名归一化：`claude-opus-4-5-20251101` → 去日期后缀匹配；Bedrock/Vertex 前缀映射；未知模型标记 `unknown pricing` 而非静默 0。

---

## 5. 竞品 / 参考产品

### 5.1 总表（star 取自 `gh api`，2026-11-05）

| 名称 | 链接 | Star | 平台 | 技术栈 | 数据来源 | 显示剩余额度 | 归档对话 |
|---|---|---|---|---|---|---|---|
| CodexBar | github.com/steipete/CodexBar | 22,996 | macOS 14+ 菜单栏（+Linux Qt、CLI、第三方 Windows 移植） | Swift 6.2 / WidgetKit / Sparkle | Claude OAuth usage → cookie → CLI PTY；Codex wham/usage → app-server RPC → PTY；111 provider | 是（5h/7d/Opus/Sonnet/credits/extra usage） | 否（有本地 cost scan） |
| ccusage | github.com/ccusage/ccusage（原 ryoppippi/ccusage） | 19,335 | CLI（npx / Nix），已重写为 Rust | Rust（原 TS） | 本地 JSONL（Claude、Codex、OpenCode、Gemini、Copilot CLI…18 源）+ LiteLLM/models.dev 定价 | 否（`statusline` 子命令读 Claude Code stdin 的 rate_limits；5h block 估算） | 否 |
| cockpit-tools | github.com/jlcodes99/cockpit-tools | 19,727 | 桌面（多账号管理） | — | Antigravity/Codex/Copilot/Windsurf/Kiro/Cursor/Gemini-cli/CodeBuddy 配额 | 是 | 否 |
| ccstatusline | github.com/sirmalloc/ccstatusline | 13,526 | Claude Code statusline | TypeScript | statusline stdin JSON | 是（rate_limits） | 否 |
| Claude-Code-Usage-Monitor | github.com/Maciek-roboblog/Claude-Code-Usage-Monitor | 8,789 | 终端 TUI | Python / Rich | 本地 JSONL；5h block + 计划预设（pro/max5/max20/custom P90）；`--statusline` 读官方 rate_limits；`--api` 实验性 OAuth usage | 估算（或 statusline 真值） | 否 |
| quotio | github.com/nguyenphutrong/quotio | 4,909 | macOS 菜单栏 | Swift | Claude/Gemini/OpenAI/Qwen/Antigravity 多账号配额 + 自动 failover | 是 | 否 |
| EchoBird | github.com/edison7009/EchoBird | 3,516 | 桌面 all-in-one | — | 账号/配额管理 + 路由 | 是 | 否 |
| claude-powerline | github.com/Owloops/claude-powerline | 1,175 | statusline | TypeScript | statusline JSON | 是 | 否 |
| CodeZeno/Claude-Code-Usage-Monitor | github.com/CodeZeno/Claude-Code-Usage-Monitor | 686 | Windows 任务栏 | Rust | Claude/Codex/Cursor 限额与 reset | 是 | 否 |
| Usage4Claude | github.com/f-is-h/Usage4Claude | 415 | macOS 13+ 菜单栏 | Swift/SwiftUI | claude.ai `sessionKey` cookie（浏览器登录/手动粘贴）；Codex 浏览器登录 | 是（5h/7d/extra/按模型周窗/credits） | 否 |
| codexU | github.com/shanggqm/codexU | 359 | macOS 桌面 widget | Swift | Codex 配额 + token + 任务板 | 是 | 否 |
| quota-float | github.com/change-42-yhmm/quota-float | 365 | Win/macOS 悬浮 | — | 本地 Codex Desktop 会话 | 是 | 否 |
| douglasmonsky/codex-usage-tracker | github.com/douglasmonsky/codex-usage-tracker | 196 | 本地 MCP + dashboard | Python | rollout JSONL（token、credits、cache、thread） | 是（快照） | 部分（thread 分析） |
| claude-codex-usage-dashboard | github.com/frankchiu-dev/claude-codex-usage-dashboard | 173 | Windows 本地 web | JS | Claude + Codex 限额 | 是 | 否 |
| CodexMeter | github.com/raycalrui/CodexMeter | 116 | macOS 菜单栏 | Swift | Codex | 是 | 否 |
| headroom | github.com/domanski-ai/headroom | 111 | 本地 dashboard | Python | Claude + Codex 多账号"只读、不花 token" | 是 | 否 |
| claude-codex-battery | github.com/dennykim123/claude-codex-battery | 104 | macOS 菜单栏 | Swift | Claude + Codex | 是 | 否 |
| LimitHUD | github.com/zetagao/LimitHUD | 82 | macOS HUD | Swift | 读 Chrome cookies | 是 | 否 |
| claude-usage-widget | github.com/bozdemir/claude-usage-widget | 76 | 跨平台 widget + CLI | Python | 本地 JSONL + 可选 Codex | 估算 | 否 |
| par_cc_usage | github.com/paulrobello/par_cc_usage | 84 | TUI | Python | 本地 JSONL | 估算 | 否 |
| Codex-Usage-Tracker | github.com/m4fn3/Codex-Usage-Tracker | 0 | macOS 菜单栏 | Swift | 仅读 `~/.codex` rollout 的 `rate_limits`（无凭据） | 是（快照） | 否 |
| cship（crate） | docs.rs/crate/cship | — | CLI | Rust | `api/oauth/usage` + `oauth-2025-04-20` | 是 | 否 |

> 未找到名为 `claude-monitor`、`cc-usage`、`claude-usage`（penberg）或 `codex-usage`（ccusage/chrisbuilds）的独立仓库（gh 404），可能已改名或并入上表项目。

### 5.2 "怎么取剩余额度"实现细节对比

| 方法 | 端点/机制 | 鉴权 | 刷新 | ToS 风险 | 采用者 |
|---|---|---|---|---|---|
| Claude OAuth usage | `GET api.anthropic.com/api/oauth/usage`，`anthropic-beta: oauth-2025-04-20` | Keychain `Claude Code-credentials` / `.credentials.json` 的 `claudeAiOauth.accessToken`（需 `user:profile` scope） | CodexBar 自适应/1–30 min | 高（明文禁止） | CodexBar、cship、ccusage-py、Claude-Code-Usage-Monitor `--api` |
| Claude Web cookie | `claude.ai/api/organizations` → `/api/organizations/{org}/usage`、`/overage_spend_limit`、`/prepaid/credits` | `sessionKey` cookie（Safari/Chrome/Firefox 导入或手动粘贴） | 1–10 min | 高 + Cloudflare 拦截 | CodexBar（fallback）、Usage4Claude、LimitHUD |
| Claude statusline | Claude Code 推 `rate_limits` 到脚本 stdin | 无 | 每次响应 / `refreshInterval` | 无 | ccusage statusline、ccstatusline、claude-powerline、Claude-Code-Usage-Monitor `--statusline` |
| Claude CLI PTY | 起 `claude --allowed-tools "" --strict-mcp-config --settings '{"disableAllHooks":true}'`，发 `/usage`，解析 "Current session"/"Current week" | 无（用用户已登录的 CLI） | 按需 | 低 | CodexBar（fallback） |
| Codex wham/usage | `GET chatgpt.com/backend-api/wham/usage` + `chatgpt-account-id` | `~/.codex/auth.json` tokens | 自适应 | 中 | CodexBar、codexusage、codex-switch、agent-toolbox |
| Codex app-server RPC | `codex -s read-only -a never app-server` → `account/rateLimits/read` | 无 | 按需 | 低 | CodexBar（CLI fallback） |
| Codex rollout 快照 | 最新 `token_count.rate_limits` | 无 | 文件变更监听 | 无 | Codex-Usage-Tracker、quota-float、codex-usage-tracker |
| Codex 响应头 | `x-codex-primary-used-percent` 等 | 需代理 | 实时 | 中 | codex-multi-auth 等 |
| Copilot | `api.github.com/copilot_internal/user` | device-flow OAuth | — | 中 | CodexBar、quotas crate、pi-copilot-usage |
| Gemini | `v1internal:retrieveUserQuota` | `~/.gemini/oauth_creds.json` | — | 高/已部分停服 | CodexBar、gemini-cli-usage |
| Cursor | `cursor.com/api/usage-summary` 等 | cookie / `state.vscdb` | ≥15 min（cost） | 高 | CodexBar、ai-usagebar、Toragonite/cursor-usage |
| OpenRouter | `/api/v1/key`、`/api/v1/credits` | API key | 60 s 缓存 | 无 | CodexBar |

---

## 6. 对目标 app 的 `UsageProvider` 接口建议

### 6.1 分层：历史消耗（Consumption）与剩余额度（Quota）分开

```ts
type Provider = 'anthropic' | 'openai' | 'gemini' | 'copilot' | 'cursor' | 'openrouter'
type Surface  = 'claude-code' | 'claude-api' | 'codex-cli' | 'openai-api' | 'gemini-cli' | ...

type TokenUsage = {
  input: number            // 未缓存输入
  cacheRead: number
  cacheWrite5m: number
  cacheWrite1h: number
  output: number           // 含 thinking/reasoning
  reasoning?: number
  requests: number
}

type UsageRecord = {
  provider: Provider; surface: Surface
  model: string; timestamp: string; sessionId: string
  dedupKey: string         // claude: message.id+requestId；codex: rollout+ordinal
  usage: TokenUsage
  costUsd: { value: number; source: 'recorded' | 'estimated' | 'unknown'; pricingVersion: string }
  source: 'claude-transcript' | 'codex-rollout' | 'otel' | 'admin-api' | 'usage-api'
}

type QuotaWindow = {
  id: 'five_hour' | 'seven_day' | 'seven_day_opus' | 'seven_day_sonnet' | 'primary' | 'secondary' | 'monthly' | string
  label: string
  usedPercent: number      // 0–100，统一单位（Anthropic unified 头是 0–1，需 ×100）
  resetsAt: string | null  // ISO
  windowMinutes?: number
}

type QuotaSnapshot = {
  provider: Provider; surface: Surface; accountId?: string; plan?: string
  windows: QuotaWindow[]
  credits?: { balance?: string; unlimited: boolean; hasCredits: boolean }
  extraUsage?: { usedUsd?: number; limitUsd?: number; period?: 'monthly' }
  fetchedAt: string
  freshness: 'live' | 'cached' | 'stale'
  source: 'statusline' | 'codex-rollout' | 'codex-app-server' | 'oauth-usage' | 'web-cookie' | 'cli-pty' | 'openrouter-key' | ...
  tosRisk: 'none' | 'low' | 'medium' | 'high'
}

type ConsumptionProvider = {
  id: string
  scan(opts: { since?: string }): AsyncIterable<UsageRecord>   // 全量/增量
  watch(cb: (r: UsageRecord) => void): () => void              // FSEvents 监听
}

type QuotaProvider = {
  id: string
  tosRisk: QuotaSnapshot['tosRisk']
  isAvailable(): Promise<boolean>            // 凭据/文件/CLI 是否存在
  fetch(): Promise<QuotaSnapshot>
  minInterval: number                        // 节流（秒）
}
```

### 6.2 推荐的 provider 实现优先级

**Claude**
1. `ClaudeTranscriptProvider`（Consumption）：FSEvents 监听 `~/.claude/projects`，增量解析 jsonl，去重，LiteLLM 定价；同时把原始对话索引入本地 SQLite 以支持"归档对话"。因 Claude Code 30 天自动清理，app 需自己持久化。
2. `ClaudeStatuslineQuotaProvider`（Quota，默认开启）：app 安装/合并一个 statusline 脚本（保留用户原有 statusline：脚本先 `tee` 到 app 文件再 exec 原命令），读最新 `rate_limits`。标 `tosRisk: none`。
3. `ClaudeOtelProvider`（Consumption，可选）：内置 OTLP HTTP receiver，引导写入 `~/.claude/settings.json#env`。提供 `cost_usd` 真值（含 1.1× geo 等）。
4. `ClaudeAdminApiProvider`（Consumption，可选）：用户粘贴 Admin key → Analytics API。
5. `ClaudeOAuthQuotaProvider`（Quota，**默认关闭、需用户显式同意并提示 ToS 风险**）：读 Keychain `Claude Code-credentials` → `/api/oauth/usage`。不要存储 token（内存即用即弃），不要伪装 Claude Code UA。
6. `ClaudeCliPtyQuotaProvider`（Quota，兜底）：仅在用户点"刷新"时跑一次。

**OpenAI / Codex**
1. `CodexRolloutProvider`（Consumption + Quota 快照）：监听 `~/.codex/sessions`，解析 `token_count`；最新 `rate_limits` 即零风险配额快照。
2. `CodexAppServerQuotaProvider`（Quota，默认）：spawn `codex app-server`，JSON-RPC `account/rateLimits/read`；超时 → SIGTERM → SIGKILL。
3. `CodexWhamQuotaProvider`（Quota，可选、中风险）：读 `auth.json` → `wham/usage`。token 过期交给 Codex CLI 刷新，不自己写回 `auth.json`。
4. `OpenAIAdminApiProvider`（Consumption）：Admin key → Usage/Costs API。

**其他**：`OpenRouterKeyProvider`（零风险、简单，可作首批）；Copilot/Gemini/Cursor 列为 P2。

### 6.3 工程注意
- 全部网络 provider 带 `User-Agent: <App>/<ver> (<url>)`（Anthropic 官方建议），不伪装官方客户端。
- 所有凭据只读、不落盘、不上传；Keychain 访问失败则静默跳过而非反复弹窗（CodexBar 教训）。
- 限流：Claude usage 端点会 429；缓存最近快照 + 指数退避；对用户展示 "fetched N min ago"。
- 单位归一：Anthropic OAuth `utilization` 是 0–100；unified 响应头是 0–1；Codex `used_percent` 0–100；Gemini `remainingFraction` 0–1（剩余）。
- 模型名归一化表 + 未知模型显式标 `unknown pricing`。
- 定价快照内嵌 + 后台刷新 + 记录 `pricingVersion`，重算历史成本时可追溯。

---

## 7. 参考链接

**官方**
- Anthropic Usage & Cost API（legacy）：https://platform.claude.com/docs/en/manage-claude/usage-cost-api
- Anthropic Usage & Cost Analytics API：https://platform.claude.com/docs/en/manage-claude/usage-cost-analytics-api
- Claude Code Analytics API：https://platform.claude.com/docs/en/manage-claude/claude-code-analytics-api
- Anthropic rate limits / 响应头：https://platform.claude.com/docs/en/api/rate-limits
- Claude Code OpenTelemetry：https://code.claude.com/docs/en/monitoring-usage
- Claude Code statusline：https://code.claude.com/docs/en/statusline
- Claude Code costs（/usage、modelPricing、usage endpoint 限流说明）：https://code.claude.com/docs/en/costs
- Claude Code 法务与合规（OAuth 限制）：https://code.claude.com/docs/en/legal-and-compliance
- Claude 周限额促销（2026-05~08）：https://support.claude.com/en/articles/15910845
- OpenAI Usage/Costs API：https://developers.openai.com/api/docs/api-reference/usage
- OpenAI rate limits 头：https://developers.openai.com/api/docs/guides/rate-limits
- Codex 限额与 dashboard：https://learn.chatgpt.com/docs/pricing
- OpenRouter limits：https://openrouter.ai/docs/api/reference/limits
- Gemini Code Assist quotas：https://developers.google.com/gemini-code-assist/resources/quotas
- LiteLLM 价格表：https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json
- models.dev：https://models.dev/api.json

**源码**
- openai/codex `codex-rs/backend-client/src/client/rate_limit_resets.rs`（wham/usage 路径）、`codex-rs/codex-api/src/rate_limits.rs`（x-codex-* 头）、`codex-rs/protocol/src/protocol.rs`（RateLimitSnapshot）、`codex-rs/http-client/src/openai_headers.rs`、`codex-rs/login/src/token_data.rs`
- CodexBar docs：https://github.com/steipete/CodexBar/blob/main/docs/claude.md、docs/codex.md、docs/copilot.md、docs/cursor.md、docs/gemini.md、docs/openrouter.md
- ccusage：https://github.com/ccusage/ccusage（`rust/crates/ccusage-core/src/pricing.rs`）；https://ccusage.com/guide/cost-modes
- cship usage_limits.rs：https://docs.rs/crate/cship/latest/source/src/usage_limits.rs
- Claude-Code-Usage-Monitor：https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor
- Usage4Claude：https://github.com/f-is-h/Usage4Claude
- Codex-Usage-Tracker（纯 rollout 读取）：https://github.com/m4fn3/Codex-Usage-Tracker
- claude-code issue #55333（请求持久化 ratelimit 头）：https://github.com/anthropics/claude-code/issues/55333
- codex PR #11260（header 解析讨论）：https://github.com/openai/codex/pull/11260

**二手/未核实**
- Anthropic 封禁第三方 OAuth 报道：https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access
- 反向工程 Claude Code 限额：https://www.claudecodecamp.com/p/i-tried-to-reverse-engineer-claude-code-s-usage-limits
