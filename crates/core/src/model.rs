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
}

/// 一次会话。`id = "{provider}:{machine_id}:{native_id}"`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// 列表页用的轻量摘要，不带消息体。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// YYYY-MM-DD（本地时区）
    pub day: String,
    pub provider: ProviderId,
    pub model: String,
    pub usage: Usage,
    pub message_count: u64,
}
