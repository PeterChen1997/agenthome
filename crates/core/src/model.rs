//! 规范化会话模型（P0 子集）。
//!
//! 完整字段草案见 docs/research/03-agent-session-formats.md §4。
//! P0 只保留"列表 + 回放 + 基础用量统计"需要的字段，时间一律用 Unix 毫秒。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 会话来源 agent。存库时用 `as_str()`。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderId {
    ClaudeCode,
    Codex,
    GeminiCli,
    OpenCode,
    Cursor,
    Other(String),
}

impl ProviderId {
    pub fn as_str(&self) -> &str {
        match self {
            ProviderId::ClaudeCode => "claude-code",
            ProviderId::Codex => "codex",
            ProviderId::GeminiCli => "gemini-cli",
            ProviderId::OpenCode => "opencode",
            ProviderId::Cursor => "cursor",
            ProviderId::Other(s) => s.as_str(),
        }
    }

    pub fn parse(s: &str) -> ProviderId {
        match s {
            "claude-code" => ProviderId::ClaudeCode,
            "codex" => ProviderId::Codex,
            "gemini-cli" => ProviderId::GeminiCli,
            "opencode" => ProviderId::OpenCode,
            "cursor" => ProviderId::Cursor,
            other => ProviderId::Other(other.to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    Main,
    Subagent,
    Fork,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Main => "main",
            SessionKind::Subagent => "subagent",
            SessionKind::Fork => "fork",
        }
    }

    pub fn parse(s: &str) -> SessionKind {
        match s {
            "subagent" => SessionKind::Subagent,
            "fork" => SessionKind::Fork,
            _ => SessionKind::Main,
        }
    }
}

/// token 用量。`input_includes_cache` 必须标注：Codex / Goose 的 input 含缓存，
/// Claude 不含；不标会跨 agent 重复计数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub reasoning_tokens: u64,
    pub requests: u64,
    pub input_includes_cache: bool,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_write_5m_tokens += other.cache_write_5m_tokens;
        self.cache_write_1h_tokens += other.cache_write_1h_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.requests += other.requests;
        self.input_includes_cache |= other.input_includes_cache;
    }

    /// 粗略总量（用于列表排序与展示，不是计费口径）。
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// 原件信息，用于溯源与增量跳过。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    /// 原件是否为压缩归档（Codex 的 `.jsonl.zst`）。
    pub compressed: bool,
}

/// 一次会话。`id = "{provider}:{machine_id}:{native_id}"`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub provider: ProviderId,
    pub native_id: String,
    pub parent_session_id: Option<String>,
    pub kind: SessionKind,
    /// custom-title > summary > 首条用户消息截断。
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub machine_id: String,
    pub agent_version: Option<String>,
    /// cli / desktop / vscode / sdk
    pub entrypoint: Option<String>,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub updated_at_ms: i64,
    pub models: Vec<String>,
    pub usage: Usage,
    pub message_count: u32,
    /// agent 自己记录的成本真值（Claude `cost-state.totalCostUSD`）；Codex 无。
    pub recorded_cost_usd: Option<f64>,
    /// 按内嵌定价表对各消息 usage 估算的成本之和；没有任何可计价消息时为 None。
    pub estimated_cost_usd: Option<f64>,
    pub source: SourceInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Role {
        match s {
            "assistant" => Role::Assistant,
            "system" => Role::System,
            "tool" => Role::Tool,
            _ => Role::User,
        }
    }
}

/// 消息内容块。解析不了的进 `Unknown`，不丢原文。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        output: String,
        is_error: bool,
    },
    Image {
        media_type: String,
    },
    Compaction {
        summary: String,
    },
    Unknown {
        native_type: String,
        raw: serde_json::Value,
    },
}

/// 一条消息。Claude 同一次 API 响应会拆成多条 assistant 行（共享 message.id），
/// Provider 负责合并成一条，`usage` 只保留一份。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub native_id: Option<String>,
    pub parent_id: Option<String>,
    pub role: Role,
    /// 注入的环境信息、attachment、compact 摘要等非用户手写内容。
    pub is_meta: bool,
    pub timestamp_ms: i64,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub blocks: Vec<ContentBlock>,
    pub usage: Option<Usage>,
}

impl Message {
    /// 拼接所有 Text 块，供列表预览与全文索引。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if let ContentBlock::Text { text } = b {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// 搜索片段里包住命中文字的标记（Unicode 私用区字符，正文里不会出现）。界面按它们切段高亮。
pub const SEARCH_MARK_START: char = '\u{E000}';
pub const SEARCH_MARK_END: char = '\u{E001}';

/// 全文搜索的一条命中。`message_id` 为 None 表示命中的是会话标题。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: Option<String>,
    pub session_title: String,
    pub provider: ProviderId,
    pub project_name: String,
    pub role: Option<Role>,
    /// 消息时间；标题命中时为会话最后更新时间。
    pub timestamp_ms: i64,
    /// 命中处前后各一小段，命中文字用 [`SEARCH_MARK_START`] / [`SEARCH_MARK_END`] 包住，换行压成空格。
    pub snippet: String,
}

/// 列表页用的轻量摘要，不带消息体。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub provider: ProviderId,
    pub kind: SessionKind,
    pub title: String,
    pub cwd: Option<String>,
    /// cwd 最后一段，给项目树用。
    pub project_name: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub message_count: u32,
    pub usage: Usage,
    pub models: Vec<String>,
    /// 父会话 id：Claude 子代理指向主会话，Codex 子线程 / 续写文件指向原会话。
    pub parent_session_id: Option<String>,
    /// 原件路径（入库时的 `source.path`）；空串视为 None。
    pub source_path: Option<String>,
    /// 会话级成本（USD）：有 agent 自记真值（Claude cost-state `totalCostUSD`）时用真值，
    /// 否则为会话内消息 `cost_usd` 之和；全部消息都无法计价时为 None。
    /// 注意：Claude 主会话的真值已含其子代理，不要再把子代理成本叠加上去。
    pub cost_usd: Option<f64>,
    /// `cost_usd` 的出处：`Recorded` = agent-reported，`Estimated` = 按定价表 computed，
    /// `Unknown` = 无成本。会话级不会出现 `Mixed`。
    pub cost_source: CostSource,
}

/// 列表查询。
#[derive(Clone, Debug, Default)]
pub struct SessionQuery {
    /// 全文搜索（FTS5），None 表示不过滤。
    pub text: Option<String>,
    pub provider: Option<ProviderId>,
    pub cwd: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

/// 按天聚合的用量，仪表盘用。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// YYYY-MM-DD（本地时区）
    pub day: String,
    pub provider: ProviderId,
    pub model: String,
    pub usage: Usage,
    pub message_count: u64,
    /// 估算成本（USD），按消息 `cost_usd` 求和；未知模型不计入。
    pub cost_usd: f64,
}

/// 统计周期，边界按本地时区。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Period {
    /// 今天 00:00 起。
    Today,
    /// 本周一 00:00 起。
    Week,
    /// 本月 1 日 00:00 起。
    Month,
    /// 最近 N 天（滚动，now - N×24h）。
    Days(u32),
    All,
}

impl Period {
    /// 周期起点（Unix 毫秒）；`All` 为 None。
    pub fn since_ms(&self) -> Option<i64> {
        use chrono::{Datelike, Local, TimeZone};
        let now = Local::now();
        let midnight = |d: chrono::NaiveDate| {
            Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default())
                .earliest()
                .map(|t| t.timestamp_millis())
                .unwrap_or(0)
        };
        let today = now.date_naive();
        match self {
            Period::Today => Some(midnight(today)),
            Period::Week => {
                let back = i64::from(today.weekday().num_days_from_monday());
                Some(midnight(today - chrono::Duration::days(back)))
            }
            Period::Month => Some(midnight(today.with_day(1).unwrap_or(today))),
            Period::Days(n) => Some(now.timestamp_millis() - i64::from(*n) * 86_400_000),
            Period::All => None,
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Period::Today => "today".to_string(),
            Period::Week => "week".to_string(),
            Period::Month => "month".to_string(),
            Period::Days(n) => format!("{n}d"),
            Period::All => "all".to_string(),
        }
    }
}

/// 成本数字的出处。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostSource {
    /// 全部按定价表估算。
    Estimated,
    /// 全部来自 agent 自己记录的真值。
    Recorded,
    /// 一部分会话有真值，其余估算。
    Mixed,
    /// 没有任何可计价数据。
    Unknown,
}

/// 周期汇总。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub period: Period,
    pub provider: Option<ProviderId>,
    pub usage: Usage,
    pub cost_usd: f64,
    pub cost_source: CostSource,
    /// 周期内有消息的会话数。
    pub sessions: u64,
    /// 周期内的消息数（所有角色）。
    pub messages: u64,
}

/// 按模型 / 按项目的分项；`key` 为模型名或项目名。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub key: String,
    pub provider: ProviderId,
    pub usage: Usage,
    pub cost_usd: f64,
}

/// 归档健康：原件还在不在、快不快到清理期、压没压缩。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub provider: ProviderId,
    pub sessions: u64,
    /// 原件路径已不存在、只剩索引的会话数。
    pub archive_only: u64,
    /// Claude 默认 30 天清理：原件 mtime 距今 > 23 天的会话数。
    pub expiring_within_7d: u64,
    /// 原件为 `.zst` 压缩归档的会话数（Codex）。
    pub compressed: u64,
}

/// 一个额度窗口（5 小时 / 7 天 / Codex primary ...）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub used_percent: f32,
    pub resets_at_ms: Option<i64>,
    pub window_minutes: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credits {
    pub balance: Option<String>,
    pub unlimited: bool,
    pub has_credits: bool,
    pub reset_credits: Option<u32>,
    pub expires_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freshness {
    Live,
    Cached,
    Stale,
}

/// 一次额度快照。`source` 为来源文件路径，`fetched_at_ms` 为该文件 mtime。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub provider: ProviderId,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub credits: Option<Credits>,
    pub fetched_at_ms: i64,
    pub source: String,
    pub freshness: Freshness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    UserMessage,
    AssistantText,
    Thinking,
    ToolCall,
    ToolResult,
    SessionEnd,
}

/// 某来源的最近活动。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderActivity {
    pub provider: ProviderId,
    /// provider.detect()：本机有没有这个 agent 的数据目录。
    pub detected: bool,
    pub last_activity_ms: Option<i64>,
    pub last_kind: Option<ActivityKind>,
    /// 最近 10 分钟内有更新的会话数。
    pub active_sessions: u32,
}
