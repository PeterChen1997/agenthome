//! Claude Code 转录（`~/.claude/projects/**/*.jsonl`）的发现与解析。
//!
//! 字段结构见 docs/research/03-agent-session-formats.md §2.1。原件只读。

use crate::model::{ContentBlock, Message, ProviderId, Role, Session, SessionKind, Usage};
use crate::providers::SourceProvider;
pub use crate::providers::{Discovery, Parsed};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct ClaudeCodeProvider {
    root: PathBuf,
    machine_id: String,
}

impl ClaudeCodeProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            machine_id: crate::providers::machine_id(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 根目录是否存在。
    pub fn detect(&self) -> bool {
        self.root.is_dir()
    }

    /// `<root>/<encoded-cwd>/<uuid>.jsonl` 为主会话；
    /// `<root>/<encoded-cwd>/<uuid>/subagents/agent-*.jsonl` 为子代理。
    pub fn discover(&self) -> Discovery {
        let mut out = Discovery::default();
        for entry in walkdir::WalkDir::new(&self.root)
            .min_depth(2)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name.contains(".superseded-") || name.contains(".orphaned-") {
                out.skipped_superseded += 1;
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }
            let depth = entry.depth();
            let is_main = depth == 2;
            let is_sub = depth == 4
                && name.starts_with("agent-")
                && entry
                    .path()
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n == "subagents");
            if is_main || is_sub {
                out.files.push(entry.into_path());
            }
        }
        out.files.sort();
        out
    }

    /// 逐行解析一个转录文件。坏行进 warnings，不中断。
    pub fn parse_file(&self, path: &Path) -> Result<Parsed> {
        let source = crate::providers::source_info(path, false)?;
        let (native_id, kind, parent_session_id) = self.identify(path)?;
        let session_id = format!(
            "{}:{}:{}",
            ProviderId::ClaudeCode.as_str(),
            self.machine_id,
            native_id
        );

        let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
        let reader = BufReader::with_capacity(1 << 20, file);

        let mut st = ParseState::new(session_id.clone());
        for (idx, line) in reader.split(b'\n').enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    st.warnings.push(format!("第 {} 行读取失败: {e}", idx + 1));
                    continue;
                }
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = match serde_json::from_slice(&line) {
                Ok(v) => v,
                Err(e) => {
                    st.warnings
                        .push(format!("第 {} 行不是合法 JSON: {e}", idx + 1));
                    continue;
                }
            };
            st.record(idx + 1, value);
        }
        st.flush_assistant();

        // 目录里的 custom-title.json 优先级与 jsonl 内的 custom-title 行相同（主会话才有）。
        if st.custom_title.is_none() && kind == SessionKind::Main {
            let p = path.with_extension("").join("custom-title.json");
            if let Ok(bytes) = std::fs::read(&p) {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) => st.custom_title = str_field(&v, "customTitle"),
                    Err(e) => st
                        .warnings
                        .push(format!("{} 不是合法 JSON: {e}", p.display())),
                }
            }
        }

        let mut messages = st.messages;
        messages.sort_by_key(|m| m.timestamp_ms);

        let mut usage = Usage::default();
        let mut models: Vec<String> = Vec::new();
        for m in &messages {
            if let Some(u) = &m.usage {
                usage.add(u);
            }
            if let Some(model) = &m.model
                && !model.starts_with('<')
                && !models.contains(model)
            {
                models.push(model.clone());
            }
        }
        usage.input_includes_cache = false;

        let title = st
            .custom_title
            .or(st.summary_title)
            .or_else(|| messages.iter().find_map(first_user_title));

        let started_at_ms = messages
            .first()
            .map(|m| m.timestamp_ms)
            .unwrap_or(source.mtime_ms);
        let updated_at_ms = messages
            .last()
            .map(|m| m.timestamp_ms)
            .unwrap_or(source.mtime_ms);

        let session = Session {
            id: session_id,
            provider: ProviderId::ClaudeCode,
            native_id,
            parent_session_id,
            kind,
            title,
            cwd: st.cwd,
            git_branch: st.git_branch,
            machine_id: self.machine_id.clone(),
            agent_version: st.version,
            entrypoint: st.entrypoint,
            started_at_ms,
            ended_at_ms: Some(updated_at_ms),
            updated_at_ms,
            models,
            usage,
            message_count: messages.len() as u32,
            recorded_cost_usd: st.recorded_cost_usd,
            estimated_cost_usd: crate::pricing::estimate_messages(&messages),
            source,
        };
        Ok(Parsed {
            session,
            messages,
            warnings: st.warnings,
            ignored_records: st.ignored,
        })
    }

    /// 从路径判断 (native_id, kind, parent_session_id)。
    fn identify(&self, path: &Path) -> Result<(String, SessionKind, Option<String>)> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("文件名不是 UTF-8")?
            .to_string();
        let parent_dir = path.parent().context("无父目录")?;
        if parent_dir.file_name().is_some_and(|n| n == "subagents") {
            let parent_uuid = parent_dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .context("子代理目录结构异常")?;
            // agentId 只在父会话内唯一（父会话被 fork 后子代理文件会随之复制），
            // 所以 native_id 带上父会话 uuid。
            let agent_id = stem.strip_prefix("agent-").unwrap_or(&stem);
            let parent_id = format!(
                "{}:{}:{}",
                ProviderId::ClaudeCode.as_str(),
                self.machine_id,
                parent_uuid
            );
            Ok((
                format!("{parent_uuid}/{agent_id}"),
                SessionKind::Subagent,
                Some(parent_id),
            ))
        } else {
            Ok((stem, SessionKind::Main, None))
        }
    }
}

impl SourceProvider for ClaudeCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ClaudeCode
    }
    fn detect(&self) -> bool {
        ClaudeCodeProvider::detect(self)
    }
    fn discover(&self) -> Discovery {
        ClaudeCodeProvider::discover(self)
    }
    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        ClaudeCodeProvider::parse_file(self, path)
    }
}

/// 逐行累积状态。assistant 同一 `message.id` 的多行合并成一条。
struct ParseState {
    session_id: String,
    messages: Vec<Message>,
    warnings: Vec<String>,
    ignored: BTreeMap<String, usize>,
    /// 正在合并的 assistant：message.id -> (消息, 带 stop_reason 的 usage 是否已定)
    pending: HashMap<String, PendingAssistant>,
    pending_order: Vec<String>,
    /// resume 会把一段历史（含原 uuid）整体重写进同一文件，按 uuid 去重，先到者为准。
    seen_uuids: HashSet<String>,
    custom_title: Option<String>,
    summary_title: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    /// `cost-state` 记录的 `totalCostUSD`，后写覆盖先写（它是累计值）。
    recorded_cost_usd: Option<f64>,
}

struct PendingAssistant {
    message: Message,
    has_final_usage: bool,
}

impl ParseState {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            messages: Vec::new(),
            warnings: Vec::new(),
            ignored: BTreeMap::new(),
            pending: HashMap::new(),
            pending_order: Vec::new(),
            seen_uuids: HashSet::new(),
            custom_title: None,
            summary_title: None,
            cwd: None,
            git_branch: None,
            version: None,
            entrypoint: None,
            recorded_cost_usd: None,
        }
    }

    fn record(&mut self, line_no: usize, v: Value) {
        let ty = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if matches!(ty.as_str(), "user" | "assistant" | "system")
            && let Some(uuid) = v.get("uuid").and_then(Value::as_str)
            && !self.seen_uuids.insert(uuid.to_string())
        {
            *self
                .ignored
                .entry("duplicate-uuid".to_string())
                .or_default() += 1;
            return;
        }
        match ty.as_str() {
            "user" => self.record_user(line_no, &v),
            "assistant" => self.record_assistant(line_no, &v),
            "system" => self.record_system(&v),
            "summary" => {
                if self.summary_title.is_none() {
                    self.summary_title = str_field(&v, "summary").filter(|s| !s.trim().is_empty());
                }
            }
            "custom-title" => {
                // 后写的覆盖先写的（用户可多次改名）。
                if let Some(t) = str_field(&v, "customTitle").filter(|s| !s.trim().is_empty()) {
                    self.custom_title = Some(t);
                }
            }
            "cost-state" => match v.get("totalCostUSD").and_then(Value::as_f64) {
                Some(c) => self.recorded_cost_usd = Some(c),
                None => self
                    .warnings
                    .push(format!("第 {line_no} 行 cost-state 缺少 totalCostUSD")),
            },
            "" => self.warnings.push(format!("第 {line_no} 行缺少 type 字段")),
            other => *self.ignored.entry(other.to_string()).or_default() += 1,
        }
    }

    fn take_env(&mut self, v: &Value) {
        if self.cwd.is_none() {
            self.cwd = str_field(v, "cwd");
        }
        if self.git_branch.is_none() {
            self.git_branch = str_field(v, "gitBranch");
        }
        if self.version.is_none() {
            self.version = str_field(v, "version");
        }
        if self.entrypoint.is_none() {
            self.entrypoint = str_field(v, "entrypoint");
        }
    }

    fn common(&mut self, line_no: usize, v: &Value) -> (String, Option<String>, i64) {
        self.take_env(v);
        let uuid = match str_field(v, "uuid") {
            Some(u) => u,
            None => {
                self.warnings.push(format!("第 {line_no} 行缺少 uuid"));
                format!("{}#L{line_no}", self.session_id)
            }
        };
        let ts = match v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
        {
            Some(t) => t,
            None => {
                self.warnings
                    .push(format!("第 {line_no} 行 timestamp 缺失或无法解析"));
                0
            }
        };
        (uuid, str_field(v, "parentUuid"), ts)
    }

    fn record_user(&mut self, line_no: usize, v: &Value) {
        let (uuid, parent, ts) = self.common(line_no, v);
        let is_meta = bool_field(v, "isMeta") || bool_field(v, "isCompactSummary");
        let content = v.pointer("/message/content");
        let mut blocks = match content {
            Some(Value::String(s)) => {
                if bool_field(v, "isCompactSummary") {
                    vec![ContentBlock::Compaction { summary: s.clone() }]
                } else {
                    vec![ContentBlock::Text { text: s.clone() }]
                }
            }
            Some(Value::Array(arr)) => arr.iter().map(parse_block).collect(),
            _ => {
                self.warnings
                    .push(format!("第 {line_no} 行 user 记录无 message.content"));
                Vec::new()
            }
        };
        blocks.retain(|b| !matches!(b, ContentBlock::Text { text } if text.is_empty()));
        let role = if blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            Role::Tool
        } else {
            Role::User
        };
        self.messages.push(Message {
            id: uuid,
            session_id: self.session_id.clone(),
            native_id: None,
            parent_id: parent,
            role,
            is_meta,
            timestamp_ms: ts,
            model: None,
            stop_reason: None,
            blocks,
            usage: None,
        });
    }

    fn record_assistant(&mut self, line_no: usize, v: &Value) {
        let (uuid, parent, ts) = self.common(line_no, v);
        let msg = v.get("message").cloned().unwrap_or(Value::Null);
        let native_id = str_field(&msg, "id");
        let blocks: Vec<ContentBlock> = match msg.get("content") {
            Some(Value::Array(arr)) => arr.iter().map(parse_block).collect(),
            Some(Value::String(s)) => vec![ContentBlock::Text { text: s.clone() }],
            _ => Vec::new(),
        };
        let stop_reason = str_field(&msg, "stop_reason");
        let model = str_field(&msg, "model");
        // `<synthetic>` 是本地生成的占位回复，没有真实 API 请求，不计 usage。
        let usage = if model.as_deref().is_some_and(|m| m.starts_with('<')) {
            None
        } else {
            msg.get("usage").map(parse_usage)
        };

        // 没有 message.id 的行（极旧版本 / synthetic）单独成一条。
        let Some(key) = native_id.clone() else {
            self.messages.push(Message {
                id: uuid,
                session_id: self.session_id.clone(),
                native_id: None,
                parent_id: parent,
                role: Role::Assistant,
                is_meta: false,
                timestamp_ms: ts,
                model,
                stop_reason,
                blocks,
                usage,
            });
            return;
        };

        match self.pending.get_mut(&key) {
            Some(p) => {
                p.message.blocks.extend(blocks);
                // usage：优先带 stop_reason 的那条；否则取最后一条（流式 output_tokens 单调递增）。
                if !p.has_final_usage && usage.is_some() {
                    p.message.usage = usage;
                    p.message.stop_reason = stop_reason.clone();
                    p.has_final_usage = stop_reason.is_some();
                }
                if p.message.model.is_none() {
                    p.message.model = model;
                }
            }
            None => {
                self.pending_order.push(key.clone());
                self.pending.insert(
                    key,
                    PendingAssistant {
                        has_final_usage: stop_reason.is_some() && usage.is_some(),
                        message: Message {
                            id: uuid,
                            session_id: self.session_id.clone(),
                            native_id,
                            parent_id: parent,
                            role: Role::Assistant,
                            is_meta: false,
                            timestamp_ms: ts,
                            model,
                            stop_reason,
                            blocks,
                            usage,
                        },
                    },
                );
            }
        }
    }

    fn record_system(&mut self, v: &Value) {
        self.take_env(v);
        let subtype = str_field(v, "subtype").unwrap_or_default();
        let Some(uuid) = str_field(v, "uuid") else {
            *self.ignored.entry("system".to_string()).or_default() += 1;
            return;
        };
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or(0);
        let blocks = if subtype == "compact_boundary" {
            vec![ContentBlock::Compaction {
                summary: String::new(),
            }]
        } else if let Some(text) = str_field(v, "content").filter(|s| !s.is_empty()) {
            vec![ContentBlock::Text { text }]
        } else {
            // 钩子摘要、api_error 等没有可读正文，只记类型，不入正文索引。
            *self.ignored.entry(format!("system:{subtype}")).or_default() += 1;
            return;
        };
        self.messages.push(Message {
            id: uuid,
            session_id: self.session_id.clone(),
            native_id: None,
            parent_id: str_field(v, "parentUuid"),
            role: Role::System,
            is_meta: true,
            timestamp_ms: ts,
            model: None,
            stop_reason: Some(subtype),
            blocks,
            usage: None,
        });
    }

    fn flush_assistant(&mut self) {
        for key in self.pending_order.drain(..) {
            if let Some(p) = self.pending.remove(&key) {
                self.messages.push(p.message);
            }
        }
    }
}

fn parse_block(b: &Value) -> ContentBlock {
    let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "text" => ContentBlock::Text {
            text: str_field(b, "text").unwrap_or_default(),
        },
        "thinking" => ContentBlock::Thinking {
            text: str_field(b, "thinking").unwrap_or_default(),
        },
        "tool_use" => ContentBlock::ToolCall {
            tool_call_id: str_field(b, "id").unwrap_or_default(),
            name: str_field(b, "name").unwrap_or_default(),
            input: b.get("input").cloned().unwrap_or(Value::Null),
        },
        "tool_result" => ContentBlock::ToolResult {
            tool_call_id: str_field(b, "tool_use_id").unwrap_or_default(),
            output: tool_result_text(b.get("content")),
            is_error: bool_field(b, "is_error"),
        },
        "image" => ContentBlock::Image {
            media_type: b
                .pointer("/source/media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/*")
                .to_string(),
        },
        other => ContentBlock::Unknown {
            native_type: other.to_string(),
            raw: b.clone(),
        },
    }
}

/// tool_result.content 可能是字符串或 `[{type:text,text}]`。
fn tool_result_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| match x.get("type").and_then(Value::as_str) {
                Some("text") => str_field(x, "text"),
                Some("image") => Some("[image]".to_string()),
                _ => Some(x.to_string()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn parse_usage(u: &Value) -> Usage {
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("/input_tokens"),
        output_tokens: n("/output_tokens"),
        cache_read_tokens: n("/cache_read_input_tokens"),
        cache_write_tokens: n("/cache_creation_input_tokens"),
        cache_write_5m_tokens: n("/cache_creation/ephemeral_5m_input_tokens"),
        cache_write_1h_tokens: n("/cache_creation/ephemeral_1h_input_tokens"),
        reasoning_tokens: n("/output_tokens_details/thinking_tokens"),
        requests: 1,
        input_includes_cache: false,
    }
}

/// 首条非 meta 用户手写消息的前 80 字符（换行压成空格）。
fn first_user_title(m: &Message) -> Option<String> {
    if m.role != Role::User || m.is_meta {
        return None;
    }
    let text = m.text();
    let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return None;
    }
    Some(joined.chars().take(80).collect())
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// ISO 8601 → Unix 毫秒。
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}
