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

use crate::model::{
    ContentBlock, Message, ProviderId, Role, Session, SessionKind, SourceInfo, Usage,
};
use crate::providers::{
    CostSum, Discovery, LineParser, LoadMessage, Parsed, Restart, SourceProvider,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexProvider {
    root: PathBuf,
    /// Codex 归档会话的目录（`~/.codex/archived_sessions`，平铺）。归档就是把 rollout 从 `sessions/` 挪过去，
    /// 不扫它会漏掉这些会话的用量（真实库 42 个、约 $47.5，docs/12 §4.3）。
    archive_root: Option<PathBuf>,
    machine_id: String,
}

impl CodexProvider {
    /// `root` 是默认的 `~/.codex/sessions` 时自动带上 `~/.codex/archived_sessions`；其他根目录（测试）不带。
    pub fn new(root: PathBuf) -> Self {
        let archive = (root == crate::paths::codex_sessions_dir())
            .then(crate::paths::codex_archived_sessions_dir);
        Self::with_archive(root, archive)
    }

    /// 显式指定归档目录。
    pub fn with_archive(root: PathBuf, archive_root: Option<PathBuf>) -> Self {
        Self {
            root,
            archive_root,
            machine_id: crate::providers::machine_id(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn archive_root(&self) -> Option<&Path> {
        self.archive_root.as_deref()
    }

    pub fn detect(&self) -> bool {
        self.root.is_dir() || self.archive_root.as_ref().is_some_and(|a| a.is_dir())
    }

    /// 路径属于 `sessions` 还是 `archived_sessions`。
    pub fn source_root_of(&self, path: &Path) -> Option<&Path> {
        if path.starts_with(&self.root) {
            Some(&self.root)
        } else {
            self.archive_root.as_deref().filter(|a| path.starts_with(a))
        }
    }

    /// 归档目录里的一个 rollout 在 `sessions/YYYY/MM/DD/` 下的对应位置（按文件名里的日期），不含扩展名。
    fn live_counterpart(&self, name: &str) -> Option<PathBuf> {
        let stem = name.trim_end_matches(".zst").trim_end_matches(".jsonl");
        let date = stem.strip_prefix("rollout-")?.get(..10)?;
        let mut parts = date.split('-');
        let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
        Some(self.root.join(y).join(m).join(d).join(stem))
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
        if let Some(archive) = &self.archive_root {
            // 同一会话在 sessions/ 与 archived_sessions/ 都有时以 sessions/ 为准（同 .jsonl 与 .zst 并存的规则）。
            let live: HashSet<String> = out.files.iter().filter_map(|p| file_stem(p)).collect();
            let mut plain: HashSet<PathBuf> = HashSet::new();
            let mut zst: Vec<PathBuf> = Vec::new();
            for entry in walkdir::WalkDir::new(archive)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
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
            let mut archived: Vec<PathBuf> = plain.iter().cloned().collect();
            for p in zst {
                if plain.contains(&p.with_extension("")) {
                    out.skipped_superseded += 1;
                } else {
                    archived.push(p);
                }
            }
            for p in archived {
                if file_stem(&p).is_some_and(|s| live.contains(&s)) {
                    out.skipped_superseded += 1;
                } else {
                    out.files.push(p);
                }
            }
        }
        out.files.sort();
        out
    }

    pub fn parse_file(&self, path: &Path) -> Result<Parsed> {
        let compressed = path.extension().is_some_and(|e| e == "zst");
        let source = crate::providers::source_info(path, compressed)?;
        let mut parser = self.new_parser(path)?;
        let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
        let reader: Box<dyn BufRead> = if compressed {
            let dec = zstd::Decoder::new(file)
                .with_context(|| format!("zstd 解压 {}", path.display()))?;
            Box::new(BufReader::with_capacity(1 << 20, dec))
        } else {
            Box::new(BufReader::with_capacity(1 << 20, file))
        };
        let read_errors = crate::providers::feed_whole(&mut parser, reader, true)?;
        let messages = parser.take_changes();
        let mut session = parser.session(source);
        // 整文件解析按消息逐条求和（与增量路径的累计值只在浮点末位上可能不同）。
        session.estimated_cost_usd = crate::pricing::estimate_messages(&messages);
        let mut warnings = parser.take_warnings();
        warnings.extend(read_errors);
        Ok(Parsed {
            session,
            messages,
            warnings,
            ignored_records: parser.take_ignored(),
        })
    }

    /// 一个还没喂过任何行的按行解析器；整文件解析与增量续读共用。
    pub fn new_parser(&self, path: &Path) -> Result<CodexLineParser> {
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
        let session_id = format!(
            "{}:{}:{file_id}",
            ProviderId::Codex.as_str(),
            self.machine_id
        );
        Ok(CodexLineParser {
            machine_id: self.machine_id.clone(),
            file_id,
            session_id,
            batch: Vec::new(),
            batch_pos: HashMap::new(),
            pending: None,
            last_assistant: None,
            mode: UsageMode::Unknown,
            count_applied: Vec::new(),
            seen_response_ids: HashSet::new(),
            last_total: None,
            current_model: None,
            meta_id: None,
            meta_ts: None,
            parent_thread_id: None,
            cwd: None,
            git_branch: None,
            cli_version: None,
            originator: None,
            pushed: 0,
            first_ts: None,
            last_ts: None,
            models: Vec::new(),
            first_user: None,
            usage: Usage::default(),
            cost: CostSum::default(),
            warnings: Vec::new(),
            ignored: BTreeMap::new(),
        })
    }

    /// 与 `discover` 同规则：`<root>/YYYY/MM/DD/rollout-*.jsonl`，以及没有同名 `.jsonl` 并存的 `.jsonl.zst`。
    pub fn accepts_path(&self, path: &Path) -> bool {
        let Some(root) = self.source_root_of(path) else {
            return false;
        };
        let archived = root != self.root;
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let depth = if archived { 1 } else { 4 };
        if rel.components().count() != depth || !name.starts_with("rollout-") || !path.is_file() {
            return false;
        }
        if archived
            && self.live_counterpart(name).is_some_and(|live| {
                live.with_extension("jsonl").exists() || live.with_extension("jsonl.zst").exists()
            })
        {
            return false;
        }
        if name.ends_with(".jsonl") {
            true
        } else if name.ends_with(".jsonl.zst") {
            !path.with_extension("").exists()
        } else {
            false
        }
    }
}

impl SourceProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }
    fn detect(&self) -> bool {
        CodexProvider::detect(self)
    }
    fn root(&self) -> &Path {
        CodexProvider::root(self)
    }
    fn discover(&self) -> Discovery {
        CodexProvider::discover(self)
    }
    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        CodexProvider::parse_file(self, path)
    }
    fn line_parser(&self, path: &Path) -> Result<Option<Box<dyn LineParser>>> {
        Ok(Some(Box::new(self.new_parser(path)?)))
    }
    fn accepts(&self, path: &Path) -> Option<bool> {
        Some(self.accepts_path(path))
    }
    fn source_root(&self, path: &Path) -> Option<&Path> {
        self.source_root_of(path)
    }
}

/// `rollout-…` 去掉 `.jsonl` / `.jsonl.zst` 后的文件名。
fn file_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    Some(
        name.trim_end_matches(".zst")
            .trim_end_matches(".jsonl")
            .to_string(),
    )
}

/// usage 的来源。有 `token_usage_record` 就全用它；整个文件都没有时才用 `token_count` 累计差值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageMode {
    /// 还没见过任何 usage。
    Unknown,
    /// 已按 token_count 差值挂过 usage（`count_applied` 记着，遇到第一条 record 时全部撤掉）。
    Count,
    /// 见过 token_usage_record，之后 token_count 一律不计。
    Record,
}

/// Codex rollout 的按行解析器。
///
/// 正在聚合的 assistant（`pending`）一直留在内存里，每批结束时把它的当前样子交出去（库里 upsert）；
/// usage 迟到时挂到最近一条 assistant 上，它若已在前几批交出去了，从库里取回来改。
pub struct CodexLineParser {
    machine_id: String,
    file_id: String,
    session_id: String,
    /// 本批新建或被改动的消息，按出现顺序；`batch_pos`：消息 id → 下标。
    batch: Vec<Message>,
    batch_pos: HashMap<String, usize>,
    /// 正在聚合的 assistant 消息。
    pending: Option<Message>,
    /// 最近一条已收口的 assistant 消息 id（usage 归属）。
    last_assistant: Option<String>,
    mode: UsageMode,
    /// `UsageMode::Count` 下挂过的 (消息 id, usage)。
    count_applied: Vec<(String, Usage)>,
    seen_response_ids: HashSet<String>,
    last_total: Option<Usage>,
    current_model: Option<String>,
    meta_id: Option<String>,
    meta_ts: Option<i64>,
    parent_thread_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    cli_version: Option<String>,
    originator: Option<String>,
    // 会话累计量（不含 pending，快照时再算上）。
    pushed: usize,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    models: Vec<String>,
    first_user: Option<String>,
    usage: Usage,
    cost: CostSum,
    warnings: Vec<String>,
    ignored: BTreeMap<String, usize>,
}

impl LineParser for CodexLineParser {
    fn feed(
        &mut self,
        line_no: usize,
        line: &[u8],
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(e) => {
                self.warnings
                    .push(format!("第 {line_no} 行不是合法 JSON: {e}"));
                return Ok(());
            }
        };
        self.record(line_no, &value, load)
    }

    fn take_changes(&mut self) -> Vec<Message> {
        let mut out = std::mem::take(&mut self.batch);
        self.batch_pos.clear();
        if let Some(p) = &self.pending {
            out.push(p.clone());
        }
        out
    }

    fn session(&mut self, source: SourceInfo) -> Session {
        let make_id = |native: &str| {
            format!(
                "{}:{}:{native}",
                ProviderId::Codex.as_str(),
                self.machine_id
            )
        };
        // 文件 id 与 session_meta.id 不同（续写文件）时仍用文件 id，避免覆盖原会话；原会话记为父。
        let continuation_of = self.meta_id.as_ref().filter(|m| **m != self.file_id);
        let parent_session_id = self
            .parent_thread_id
            .as_ref()
            .or(continuation_of)
            .map(|p| make_id(p));
        let pending = self.pending.as_ref();
        let mut models = self.models.clone();
        if let Some(model) = pending.and_then(|p| p.model.as_ref())
            && !models.contains(model)
        {
            models.push(model.clone());
        }
        let first_ts = self.first_ts.or(pending.map(|p| p.timestamp_ms));
        let last_ts = pending.map(|p| p.timestamp_ms).or(self.last_ts);
        let started_at_ms = self.meta_ts.or(first_ts).unwrap_or(source.mtime_ms);
        let updated_at_ms = last_ts.unwrap_or(source.mtime_ms).max(started_at_ms);
        let message_count = self.pushed + usize::from(pending.is_some());
        // 消息级 usage 已在 `parse_usage` / `diff_usage` 里减去缓存读，会话级沿用"新鲜输入"口径，
        // `input_includes_cache` 保持 false，否则 `pricing::estimate_usd` 会把缓存读再扣一次。
        let mut usage = self.usage;
        usage.input_includes_cache = false;
        Session {
            id: self.session_id.clone(),
            provider: ProviderId::Codex,
            native_id: self.file_id.clone(),
            parent_session_id,
            kind: SessionKind::Main,
            title: self.first_user.clone(),
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
            machine_id: self.machine_id.clone(),
            agent_version: self.cli_version.clone(),
            entrypoint: self.originator.clone(),
            started_at_ms,
            ended_at_ms: Some(updated_at_ms),
            updated_at_ms,
            models,
            usage,
            message_count: message_count as u32,
            recorded_cost_usd: None,
            estimated_cost_usd: self.cost.value(),
            source,
        }
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn take_ignored(&mut self) -> BTreeMap<String, usize> {
        std::mem::take(&mut self.ignored)
    }
}

impl CodexLineParser {
    fn record(
        &mut self,
        line_no: usize,
        v: &Value,
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
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
            "event_msg" => return self.record_event(payload, load),
            "token_usage_record" => return self.record_usage(line_no, payload, load),
            "compacted" => {
                self.flush_assistant();
                let summary = str_field(payload, "message").unwrap_or_default();
                self.push(self.message(
                    format!("l{line_no}"),
                    Role::System,
                    true,
                    ts.unwrap_or(0),
                    Some("compacted".to_string()),
                    vec![ContentBlock::Compaction { summary }],
                ));
            }
            "" => self.warnings.push(format!("第 {line_no} 行缺少 type 字段")),
            other => *self.ignored.entry(other.to_string()).or_default() += 1,
        }
        Ok(())
    }

    fn message(
        &self,
        id: String,
        role: Role,
        is_meta: bool,
        ts: i64,
        stop_reason: Option<String>,
        blocks: Vec<ContentBlock>,
    ) -> Message {
        Message {
            id,
            session_id: self.session_id.clone(),
            native_id: None,
            parent_id: None,
            role,
            is_meta,
            timestamp_ms: ts,
            model: None,
            stop_reason,
            blocks,
            usage: None,
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
                        self.push(self.message(
                            format!("l{line_no}"),
                            Role::User,
                            is_meta,
                            ts,
                            None,
                            vec![ContentBlock::Text { text }],
                        ));
                    }
                    "developer" | "system" => {
                        self.flush_assistant();
                        self.push(self.message(
                            format!("l{line_no}"),
                            Role::System,
                            true,
                            ts,
                            Some(role.to_string()),
                            vec![ContentBlock::Text { text }],
                        ));
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
                self.push(self.message(
                    format!("l{line_no}"),
                    Role::Tool,
                    false,
                    ts,
                    None,
                    vec![ContentBlock::ToolResult {
                        tool_call_id: str_field(p, "call_id").unwrap_or_default(),
                        output,
                        is_error: false,
                    }],
                ));
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
                let mut m = self.message(
                    format!("l{line_no}"),
                    Role::Assistant,
                    false,
                    ts,
                    None,
                    blocks,
                );
                m.native_id = str_field(p, "id");
                m.model = self.current_model.clone();
                self.pending = Some(m);
            }
        }
    }

    fn record_event(&mut self, p: &Value, load: &mut LoadMessage<'_>) -> Result<(), Restart> {
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
                    return Ok(());
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
                if delta.total_tokens() > 0 && self.mode != UsageMode::Record {
                    self.mode = UsageMode::Count;
                    let id = self.ensure_target();
                    self.apply_usage(&id, &delta, load)?;
                    self.count_applied.push((id, delta));
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
        Ok(())
    }

    fn record_usage(
        &mut self,
        line_no: usize,
        p: &Value,
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
        let Some(rid) = str_field(p, "response_id") else {
            self.warnings.push(format!(
                "第 {line_no} 行 token_usage_record 缺少 response_id"
            ));
            return Ok(());
        };
        if !self.seen_response_ids.insert(rid) {
            *self
                .ignored
                .entry("token_usage_record:duplicate".to_string())
                .or_default() += 1;
            return Ok(());
        }
        let Some(u) = p.get("usage").map(parse_usage) else {
            self.warnings
                .push(format!("第 {line_no} 行 token_usage_record 缺少 usage"));
            return Ok(());
        };
        self.flush_assistant();
        if self.mode == UsageMode::Count {
            // 文件里出现了 record：之前按 token_count 差值挂的 usage 全部撤掉，只认 record。
            for (id, applied) in std::mem::take(&mut self.count_applied) {
                let m = self.message_mut(&id, load)?;
                let before = (m.model.clone(), m.usage.take());
                crate::providers::usage_sub(&mut self.usage, &applied);
                self.cost.replace(
                    (before.0.as_deref(), before.1.as_ref()),
                    (before.0.as_deref(), None),
                );
            }
        }
        self.mode = UsageMode::Record;
        let id = self.ensure_target();
        self.apply_usage(&id, &u, load)
    }

    /// usage 归到最近一条 assistant 消息；没有则记警告（不丢数：挂到一条合成的 assistant 上）。
    fn ensure_target(&mut self) -> String {
        if let Some(id) = &self.last_assistant {
            return id.clone();
        }
        self.warnings
            .push("usage 记录出现在任何 assistant 消息之前，已挂到合成消息".to_string());
        let mut m = self.message(
            format!("usage-{}", self.pushed),
            Role::Assistant,
            true,
            self.last_ts.unwrap_or(0),
            None,
            Vec::new(),
        );
        m.model = self.current_model.clone();
        let id = m.id.clone();
        self.push(m);
        id
    }

    fn apply_usage(
        &mut self,
        id: &str,
        u: &Usage,
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
        let m = self.message_mut(id, load)?;
        let model = m.model.clone();
        let before = m.usage;
        match &mut m.usage {
            Some(existing) => existing.add(u),
            None => m.usage = Some(*u),
        }
        let after = m.usage;
        self.usage.add(u);
        self.cost.replace(
            (model.as_deref(), before.as_ref()),
            (model.as_deref(), after.as_ref()),
        );
        Ok(())
    }

    /// 本批里的消息直接改；已在前几批交出去的从库里取回来，放进本批。
    fn message_mut(
        &mut self,
        id: &str,
        load: &mut LoadMessage<'_>,
    ) -> Result<&mut Message, Restart> {
        let idx = match self.batch_pos.get(id) {
            Some(&i) => i,
            None => {
                let Some(m) = load(id) else {
                    return Err(Restart(format!("要挂 usage 的消息 {id} 不在库里")));
                };
                self.batch_pos.insert(id.to_string(), self.batch.len());
                self.batch.push(m);
                self.batch.len() - 1
            }
        };
        Ok(&mut self.batch[idx])
    }

    fn flush_assistant(&mut self) {
        if let Some(m) = self.pending.take() {
            self.push(m);
        }
    }

    fn push(&mut self, m: Message) {
        if m.role == Role::Assistant {
            self.last_assistant = Some(m.id.clone());
        }
        self.pushed += 1;
        if self.first_ts.is_none() {
            self.first_ts = Some(m.timestamp_ms);
        }
        self.last_ts = Some(m.timestamp_ms);
        if let Some(model) = &m.model
            && !self.models.contains(model)
        {
            self.models.push(model.clone());
        }
        if self.first_user.is_none() {
            self.first_user = first_user_title(&m);
        }
        if let Some(u) = &m.usage {
            self.usage.add(u);
        }
        self.cost
            .replace((None, None), (m.model.as_deref(), m.usage.as_ref()));
        self.batch_pos.insert(m.id.clone(), self.batch.len());
        self.batch.push(m);
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
