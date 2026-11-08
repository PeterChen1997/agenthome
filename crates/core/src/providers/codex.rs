//! Codex CLI / Desktop rollout（`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]`）的发现与解析。
//!
//! 行结构见 docs/research/03-agent-session-formats.md §2.2。原件只读；`.zst` 在内存里流式解压。
//!
//! 聚合规则：
//! - 连续的 assistant 侧 `response_item`（reasoning / message / function_call / custom_tool_call ...）
//!   合成一条 assistant 消息；遇到用户输入、工具输出或 usage 记录时收口。
//! - usage 优先取 `token_usage_record`（按 `response_id` 去重）；旧版 rollout 没有它时，
//!   退回 `event_msg.token_count.info.total_token_usage` 的相邻差值（它是会话累计，差值之和恰等于总量）。
//! - Codex 原始 `input_tokens` 含缓存读；入库前减去 `cached_input_tokens`，统一成与 Claude 一致的
//!   "新鲜输入"口径，`input_includes_cache = false`。

use crate::model::{ContentBlock, Message, ProviderId, Role, Session, SessionKind, Usage};
use crate::providers::{Discovery, Parsed, SourceProvider};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexProvider {
    root: PathBuf,
    machine_id: String,
}

impl CodexProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            machine_id: crate::providers::machine_id(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn detect(&self) -> bool {
        self.root.is_dir()
    }

    /// `<root>/YYYY/MM/DD/rollout-*.jsonl` 与 `.jsonl.zst`；同一会话两者并存时取 `.jsonl`（热数据更新）。
    pub fn discover(&self) -> Discovery {
        let mut out = Discovery::default();
        let mut plain: HashSet<PathBuf> = HashSet::new();
        let mut zst: Vec<PathBuf> = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .min_depth(4)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if !name.starts_with("rollout-") {
                continue;
            }
            if name.ends_with(".jsonl") {
                plain.insert(entry.into_path());
            } else if name.ends_with(".jsonl.zst") {
                zst.push(entry.into_path());
            }
        }
        out.files.extend(plain.iter().cloned());
        for p in zst {
            if plain.contains(&p.with_extension("")) {
                out.skipped_superseded += 1;
            } else {
                out.files.push(p);
            }
        }
        out.files.sort();
        out
    }

    pub fn parse_file(&self, path: &Path) -> Result<Parsed> {
        let compressed = path.extension().is_some_and(|e| e == "zst");
        let source = crate::providers::source_info(path, compressed)?;
        let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
        let reader: Box<dyn BufRead> = if compressed {
            let dec = zstd::Decoder::new(file)
                .with_context(|| format!("zstd 解压 {}", path.display()))?;
            Box::new(BufReader::with_capacity(1 << 20, dec))
        } else {
            Box::new(BufReader::with_capacity(1 << 20, file))
        };

        // 文件名 `rollout-<ts>-<uuid>.jsonl`：uuid 作为兜底 native_id。
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.trim_end_matches(".zst").trim_end_matches(".jsonl"))
            .context("文件名不是 UTF-8")?;
        // 去掉 `rollout-<YYYY-MM-DDTHH-MM-SS>-` 前缀后就是文件级 id；
        // resume 续写会生成 `<thread>_<new-uuid>` 这样的新文件，内容是续篇、不重叠。
        let file_id = stem
            .strip_prefix("rollout-")
            .filter(|r| r.len() > 20 && r.as_bytes().get(19) == Some(&b'-'))
            .map(|r| &r[20..])
            .unwrap_or(stem)
            .to_string();

        let mut st = ParseState::new();
        for (idx, line) in reader.split(b'\n').enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    st.warnings.push(format!("第 {} 行读取失败: {e}", idx + 1));
                    break;
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
            st.record(idx + 1, &value);
        }
        st.flush_assistant();

        let make_id = |native: &str| {
            format!(
                "{}:{}:{native}",
                ProviderId::Codex.as_str(),
                self.machine_id
            )
        };
        // 文件 id 与 session_meta.id 不同（续写文件）时用文件 id，避免覆盖原会话；原会话记为父。
        let (native_id, continuation_of) = match &st.meta_id {
            Some(meta) if *meta != file_id => (file_id.clone(), Some(meta.clone())),
            _ => (file_id.clone(), None),
        };
        let session_id = make_id(&native_id);
        let parent_session_id = st
            .parent_thread_id
            .as_deref()
            .or(continuation_of.as_deref())
            .map(make_id);

        // usage 来源选择：有 token_usage_record 就全用它，否则用 token_count 差值。
        let picked = if st.record_usages.is_empty() {
            std::mem::take(&mut st.count_usages)
        } else {
            std::mem::take(&mut st.record_usages)
        };
        let mut messages = st.messages;
        for (idx, u) in picked {
            if let Some(m) = messages.get_mut(idx) {
                match &mut m.usage {
                    Some(existing) => existing.add(&u),
                    None => m.usage = Some(u),
                }
            }
        }
        for m in &mut messages {
            m.session_id = session_id.clone();
        }

        // 会话级 usage = 各消息 usage 之和；消息级已在 `parse_usage` / `diff_usage` 里减去缓存读，
        // 所以这里沿用"新鲜输入"口径，`input_includes_cache` 保持 false，
        // 否则 `pricing::estimate_usd` 会把缓存读再扣一次。
        let mut usage = Usage::default();
        let mut models: Vec<String> = Vec::new();
        for m in &messages {
            if let Some(u) = &m.usage {
                usage.add(u);
            }
            if let Some(model) = &m.model
                && !models.contains(model)
            {
                models.push(model.clone());
            }
        }

        let title = messages.iter().find_map(first_user_title);
        let started_at_ms = st
            .meta_ts
            .or_else(|| messages.first().map(|m| m.timestamp_ms))
            .unwrap_or(source.mtime_ms);
        let updated_at_ms = messages
            .last()
            .map(|m| m.timestamp_ms)
            .unwrap_or(source.mtime_ms)
            .max(started_at_ms);

        let session = Session {
            id: session_id,
            provider: ProviderId::Codex,
            native_id,
            parent_session_id,
            kind: SessionKind::Main,
            title,
            cwd: st.cwd,
            git_branch: st.git_branch,
            machine_id: self.machine_id.clone(),
            agent_version: st.cli_version,
            entrypoint: st.originator,
            started_at_ms,
            ended_at_ms: Some(updated_at_ms),
            updated_at_ms,
            models,
            usage,
            message_count: messages.len() as u32,
            recorded_cost_usd: None,
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
}

impl SourceProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }
    fn detect(&self) -> bool {
        CodexProvider::detect(self)
    }
    fn discover(&self) -> Discovery {
        CodexProvider::discover(self)
    }
    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        CodexProvider::parse_file(self, path)
    }
}

struct ParseState {
    messages: Vec<Message>,
    warnings: Vec<String>,
    ignored: BTreeMap<String, usize>,
    /// 正在聚合的 assistant 消息。
    pending: Option<Message>,
    /// 最近一条 assistant 消息在 `messages` 里的下标（usage 归属）。
    last_assistant: Option<usize>,
    /// (消息下标, usage)：来自 token_usage_record。
    record_usages: Vec<(usize, Usage)>,
    seen_response_ids: HashSet<String>,
    /// (消息下标, usage)：来自 token_count 累计差值。
    count_usages: Vec<(usize, Usage)>,
    last_total: Option<Usage>,
    current_model: Option<String>,
    meta_id: Option<String>,
    meta_ts: Option<i64>,
    parent_thread_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    cli_version: Option<String>,
    originator: Option<String>,
}

impl ParseState {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            warnings: Vec::new(),
            ignored: BTreeMap::new(),
            pending: None,
            last_assistant: None,
            record_usages: Vec::new(),
            seen_response_ids: HashSet::new(),
            count_usages: Vec::new(),
            last_total: None,
            current_model: None,
            meta_id: None,
            meta_ts: None,
            parent_thread_id: None,
            cwd: None,
            git_branch: None,
            cli_version: None,
            originator: None,
        }
    }

    fn record(&mut self, line_no: usize, v: &Value) {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = v.get("payload").unwrap_or(&Value::Null);
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::providers::claude_code::parse_iso_ms);
        match ty {
            "session_meta" => self.record_meta(payload, ts),
            "turn_context" => {
                if let Some(m) = str_field(payload, "model") {
                    self.current_model = Some(m);
                }
                if self.cwd.is_none() {
                    self.cwd = str_field(payload, "cwd");
                }
            }
            "response_item" => self.record_item(line_no, payload, ts.unwrap_or(0)),
            "event_msg" => self.record_event(payload),
            "token_usage_record" => self.record_usage(line_no, payload),
            "compacted" => {
                self.flush_assistant();
                let summary = str_field(payload, "message").unwrap_or_default();
                self.push(Message {
                    id: format!("l{line_no}"),
                    session_id: String::new(),
                    native_id: None,
                    parent_id: None,
                    role: Role::System,
                    is_meta: true,
                    timestamp_ms: ts.unwrap_or(0),
                    model: None,
                    stop_reason: Some("compacted".to_string()),
                    blocks: vec![ContentBlock::Compaction { summary }],
                    usage: None,
                });
            }
            "" => self.warnings.push(format!("第 {line_no} 行缺少 type 字段")),
            other => *self.ignored.entry(other.to_string()).or_default() += 1,
        }
    }

    fn record_meta(&mut self, p: &Value, ts: Option<i64>) {
        // fork / resume 可能写多条 session_meta，以第一条为准。
        if self.meta_id.is_some() {
            *self
                .ignored
                .entry("session_meta:extra".to_string())
                .or_default() += 1;
            return;
        }
        self.meta_id = str_field(p, "id").or_else(|| str_field(p, "session_id"));
        self.meta_ts = p
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::providers::claude_code::parse_iso_ms)
            .or(ts);
        self.parent_thread_id = str_field(p, "parent_thread_id");
        self.cwd = str_field(p, "cwd");
        self.git_branch = p
            .pointer("/git/branch")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.cli_version = str_field(p, "cli_version");
        self.originator = str_field(p, "originator");
    }

    fn record_item(&mut self, line_no: usize, p: &Value, ts: i64) {
        let item_type = p.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "message" => {
                let role = p.get("role").and_then(Value::as_str).unwrap_or("");
                let text = content_text(p.get("content"));
                match role {
                    "assistant" => {
                        self.append_assistant(line_no, ts, p, vec![ContentBlock::Text { text }])
                    }
                    "user" => {
                        self.flush_assistant();
                        // `<environment_context>` / `<user_instructions>` 等注入内容不是用户手写。
                        let is_meta = text.trim_start().starts_with('<');
                        self.push(Message {
                            id: format!("l{line_no}"),
                            session_id: String::new(),
                            native_id: None,
                            parent_id: None,
                            role: Role::User,
                            is_meta,
                            timestamp_ms: ts,
                            model: None,
                            stop_reason: None,
                            blocks: vec![ContentBlock::Text { text }],
                            usage: None,
                        });
                    }
                    "developer" | "system" => {
                        self.flush_assistant();
                        self.push(Message {
                            id: format!("l{line_no}"),
                            session_id: String::new(),
                            native_id: None,
                            parent_id: None,
                            role: Role::System,
                            is_meta: true,
                            timestamp_ms: ts,
                            model: None,
                            stop_reason: Some(role.to_string()),
                            blocks: vec![ContentBlock::Text { text }],
                            usage: None,
                        });
                    }
                    _ => self
                        .warnings
                        .push(format!("第 {line_no} 行 message 的 role 未知: {role:?}")),
                }
            }
            "reasoning" => {
                // summary 是可读摘要；encrypted_content 不可读，不保留。
                let text = p
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| str_field(s, "text"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                self.append_assistant(line_no, ts, p, vec![ContentBlock::Thinking { text }]);
            }
            "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" => {
                let raw = p
                    .get("arguments")
                    .or_else(|| p.get("input"))
                    .or_else(|| p.get("action"))
                    .cloned()
                    .unwrap_or(Value::Null);
                // arguments 是 JSON 字符串；能解析就存结构，否则存原串。
                let input = match &raw {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(raw.clone()),
                    other => other.clone(),
                };
                let block = ContentBlock::ToolCall {
                    tool_call_id: str_field(p, "call_id")
                        .or_else(|| str_field(p, "id"))
                        .unwrap_or_default(),
                    name: str_field(p, "name").unwrap_or_else(|| item_type.to_string()),
                    input,
                };
                self.append_assistant(line_no, ts, p, vec![block]);
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                self.flush_assistant();
                let output = match p.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    other => content_text(other),
                };
                self.push(Message {
                    id: format!("l{line_no}"),
                    session_id: String::new(),
                    native_id: None,
                    parent_id: None,
                    role: Role::Tool,
                    is_meta: false,
                    timestamp_ms: ts,
                    model: None,
                    stop_reason: None,
                    blocks: vec![ContentBlock::ToolResult {
                        tool_call_id: str_field(p, "call_id").unwrap_or_default(),
                        output,
                        is_error: false,
                    }],
                    usage: None,
                });
            }
            "" => self
                .warnings
                .push(format!("第 {line_no} 行 response_item 缺少 payload.type")),
            other => {
                *self
                    .ignored
                    .entry(format!("response_item:{other}"))
                    .or_default() += 1;
            }
        }
    }

    fn append_assistant(&mut self, line_no: usize, ts: i64, p: &Value, blocks: Vec<ContentBlock>) {
        match &mut self.pending {
            Some(m) => m.blocks.extend(blocks),
            None => {
                self.pending = Some(Message {
                    id: format!("l{line_no}"),
                    session_id: String::new(),
                    native_id: str_field(p, "id"),
                    parent_id: None,
                    role: Role::Assistant,
                    is_meta: false,
                    timestamp_ms: ts,
                    model: self.current_model.clone(),
                    stop_reason: None,
                    blocks,
                    usage: None,
                });
            }
        }
    }

    fn record_event(&mut self, p: &Value) {
        let ev = p.get("type").and_then(Value::as_str).unwrap_or("");
        match ev {
            "token_count" => {
                // info 可为 null（纯限额更新）。
                let Some(total) = p
                    .pointer("/info/total_token_usage")
                    .filter(|v| !v.is_null())
                    .map(parse_usage)
                else {
                    *self
                        .ignored
                        .entry("event_msg:token_count:no_info".to_string())
                        .or_default() += 1;
                    return;
                };
                self.flush_assistant();
                let delta = match &self.last_total {
                    Some(prev) => diff_usage(prev, &total).unwrap_or_else(|| {
                        // 累计值回退（compact 后重置）：退回本次 last_token_usage。
                        p.pointer("/info/last_token_usage")
                            .map(parse_usage)
                            .unwrap_or(total)
                    }),
                    None => total,
                };
                self.last_total = p.pointer("/info/total_token_usage").map(parse_usage);
                if delta.total_tokens() > 0 {
                    self.attach_usage(delta, &mut |st, idx, u| st.count_usages.push((idx, u)));
                }
            }
            "" => *self.ignored.entry("event_msg".to_string()).or_default() += 1,
            other => {
                *self
                    .ignored
                    .entry(format!("event_msg:{other}"))
                    .or_default() += 1
            }
        }
    }

    fn record_usage(&mut self, line_no: usize, p: &Value) {
        let Some(rid) = str_field(p, "response_id") else {
            self.warnings.push(format!(
                "第 {line_no} 行 token_usage_record 缺少 response_id"
            ));
            return;
        };
        if !self.seen_response_ids.insert(rid) {
            *self
                .ignored
                .entry("token_usage_record:duplicate".to_string())
                .or_default() += 1;
            return;
        }
        let Some(u) = p.get("usage").map(parse_usage) else {
            self.warnings
                .push(format!("第 {line_no} 行 token_usage_record 缺少 usage"));
            return;
        };
        self.flush_assistant();
        self.attach_usage(u, &mut |st, idx, u| st.record_usages.push((idx, u)));
    }

    /// 把 usage 归到最近一条 assistant 消息；没有则记警告（不丢数：挂到一条合成的 assistant 上）。
    fn attach_usage(&mut self, u: Usage, sink: &mut dyn FnMut(&mut ParseState, usize, Usage)) {
        let idx = match self.last_assistant {
            Some(i) => i,
            None => {
                self.warnings
                    .push("usage 记录出现在任何 assistant 消息之前，已挂到合成消息".to_string());
                self.push(Message {
                    id: format!("usage-{}", self.messages.len()),
                    session_id: String::new(),
                    native_id: None,
                    parent_id: None,
                    role: Role::Assistant,
                    is_meta: true,
                    timestamp_ms: self.messages.last().map(|m| m.timestamp_ms).unwrap_or(0),
                    model: self.current_model.clone(),
                    stop_reason: None,
                    blocks: Vec::new(),
                    usage: None,
                });
                self.messages.len() - 1
            }
        };
        sink(self, idx, u);
    }

    fn flush_assistant(&mut self) {
        if let Some(m) = self.pending.take() {
            self.push(m);
        }
    }

    fn push(&mut self, m: Message) {
        if m.role == Role::Assistant {
            self.last_assistant = Some(self.messages.len());
        }
        self.messages.push(m);
    }
}

fn parse_usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cached_input_tokens");
    Usage {
        input_tokens: n("input_tokens").saturating_sub(cached),
        output_tokens: n("output_tokens"),
        cache_read_tokens: cached,
        cache_write_tokens: n("cache_write_input_tokens"),
        cache_write_5m_tokens: 0,
        cache_write_1h_tokens: 0,
        reasoning_tokens: n("reasoning_output_tokens"),
        requests: 1,
        input_includes_cache: false,
    }
}

/// `cur - prev`；任一分量回退则返回 None。
fn diff_usage(prev: &Usage, cur: &Usage) -> Option<Usage> {
    Some(Usage {
        input_tokens: cur.input_tokens.checked_sub(prev.input_tokens)?,
        output_tokens: cur.output_tokens.checked_sub(prev.output_tokens)?,
        cache_read_tokens: cur.cache_read_tokens.checked_sub(prev.cache_read_tokens)?,
        cache_write_tokens: cur
            .cache_write_tokens
            .checked_sub(prev.cache_write_tokens)?,
        cache_write_5m_tokens: 0,
        cache_write_1h_tokens: 0,
        reasoning_tokens: cur.reasoning_tokens.checked_sub(prev.reasoning_tokens)?,
        requests: 1,
        input_includes_cache: false,
    })
}

/// `content: [{type: input_text|output_text, text}]` 拼成一段。
fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| str_field(x, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// 首条非 meta 用户输入前 80 字符（换行压成空格）。
fn first_user_title(m: &Message) -> Option<String> {
    if m.role != Role::User || m.is_meta {
        return None;
    }
    let joined: String = m.text().split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return None;
    }
    Some(joined.chars().take(80).collect())
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}
