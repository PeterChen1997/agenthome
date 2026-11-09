//! Claude Code 转录（`~/.claude/projects/**/*.jsonl`）的发现与解析。
//!
//! 字段结构见 docs/research/03-agent-session-formats.md §2.1。原件只读。

use crate::model::{
    ContentBlock, Message, ProviderId, Role, Session, SessionKind, SourceInfo, Usage,
};
use crate::providers::{CostSum, LineParser, LoadMessage, Restart, SourceProvider};
pub use crate::providers::{Discovery, Parsed};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
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
        let mut parser = self.new_parser(path)?;
        let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
        let reader = BufReader::with_capacity(1 << 20, file);
        let read_errors = crate::providers::feed_whole(&mut parser, reader, false)?;
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
    pub fn new_parser(&self, path: &Path) -> Result<ClaudeLineParser> {
        let (native_id, kind, parent_session_id) = self.identify(path)?;
        let session_id = format!(
            "{}:{}:{}",
            ProviderId::ClaudeCode.as_str(),
            self.machine_id,
            native_id
        );
        // 目录里的 custom-title.json 优先级与 jsonl 内的 custom-title 行相同（主会话才有）。
        let title_file =
            (kind == SessionKind::Main).then(|| path.with_extension("").join("custom-title.json"));
        Ok(ClaudeLineParser {
            session_id,
            native_id,
            kind,
            parent_session_id,
            machine_id: self.machine_id.clone(),
            title_file,
            plain: Vec::new(),
            asst: Vec::new(),
            asst_pos: HashMap::new(),
            asst_refs: HashMap::new(),
            seen_uuids: HashSet::new(),
            warnings: Vec::new(),
            ignored: BTreeMap::new(),
            custom_title: None,
            summary_title: None,
            cwd: None,
            git_branch: None,
            version: None,
            entrypoint: None,
            recorded_cost_usd: None,
            usage: Usage::default(),
            cost: CostSum::default(),
            message_count: 0,
            min_ts: None,
            max_ts: None,
            models: Vec::new(),
            first_user: None,
        })
    }

    /// 与 `discover` 同规则：主会话 `<项目>/<uuid>.jsonl`，子代理 `<项目>/<uuid>/subagents/agent-*.jsonl`，
    /// 不含 `.superseded-*` / `.orphaned-*` 旧副本。
    pub fn accepts_path(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        if name.contains(".superseded-") || name.contains(".orphaned-") || !name.ends_with(".jsonl")
        {
            return false;
        }
        let depth = rel.components().count();
        let shaped = depth == 2
            || (depth == 4
                && name.starts_with("agent-")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n == "subagents"));
        shaped && path.is_file()
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
    fn root(&self) -> &Path {
        ClaudeCodeProvider::root(self)
    }
    fn discover(&self) -> Discovery {
        ClaudeCodeProvider::discover(self)
    }
    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        ClaudeCodeProvider::parse_file(self, path)
    }
    fn line_parser(&self, path: &Path) -> Result<Option<Box<dyn LineParser>>> {
        Ok(Some(Box::new(self.new_parser(path)?)))
    }
    fn accepts(&self, path: &Path) -> Option<bool> {
        Some(self.accepts_path(path))
    }
}

/// Claude 转录的按行解析器。assistant 同一 `message.id` 的多行合并成一条。
///
/// 消息正文只在本批内持有；后面的续行要改已交出的 assistant 时，按消息 id 从库里取回再改。
pub struct ClaudeLineParser {
    session_id: String,
    native_id: String,
    kind: SessionKind,
    parent_session_id: Option<String>,
    machine_id: String,
    title_file: Option<PathBuf>,
    /// 本批新建、之后不会再变的消息（user / system / 无 message.id 的 assistant），按出现顺序。
    plain: Vec<Message>,
    /// 本批新建或被续行改动的 assistant，按首次出现顺序；`asst_pos`：message.id → 下标。
    asst: Vec<Message>,
    asst_pos: HashMap<String, usize>,
    /// 整个文件出现过的 assistant：message.id → 合并状态。
    asst_refs: HashMap<String, AsstRef>,
    /// resume 会把一段历史（含原 uuid）整体重写进同一文件，按 uuid 去重，先到者为准。
    seen_uuids: HashSet<String>,
    warnings: Vec<String>,
    ignored: BTreeMap<String, usize>,
    custom_title: Option<String>,
    summary_title: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    /// `cost-state` 记录的 `totalCostUSD`，后写覆盖先写（它是累计值）。
    recorded_cost_usd: Option<f64>,
    // 会话累计量：消息新增时加、被续行改动时先减旧值再加新值。
    usage: Usage,
    cost: CostSum,
    message_count: u32,
    min_ts: Option<i64>,
    max_ts: Option<i64>,
    /// (该模型最早一条消息的时间, 模型名)，按首次出现顺序。
    models: Vec<(i64, String)>,
    /// 时间最早的非 meta 用户手写消息：(时间, 标题)。
    first_user: Option<(i64, String)>,
}

struct AsstRef {
    /// 合并后消息的 id（首行的 uuid）。
    id: String,
    /// 带 stop_reason 的 usage 是否已定。
    has_final: bool,
}

impl LineParser for ClaudeLineParser {
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
        self.record(line_no, value, load)
    }

    fn take_changes(&mut self) -> Vec<Message> {
        let mut out = std::mem::take(&mut self.plain);
        out.append(&mut self.asst);
        self.asst_pos.clear();
        out.sort_by_key(|m| m.timestamp_ms);
        out
    }

    fn session(&mut self, source: SourceInfo) -> Session {
        let mut custom_title = self.custom_title.clone();
        if custom_title.is_none()
            && let Some(p) = &self.title_file
            && let Ok(bytes) = std::fs::read(p)
        {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => custom_title = str_field(&v, "customTitle"),
                Err(e) => self
                    .warnings
                    .push(format!("{} 不是合法 JSON: {e}", p.display())),
            }
        }
        let title = custom_title
            .or_else(|| self.summary_title.clone())
            .or_else(|| self.first_user.as_ref().map(|(_, t)| t.clone()));
        let mut models = self.models.clone();
        models.sort_by_key(|(ts, _)| *ts);
        let mut usage = self.usage;
        usage.input_includes_cache = false;
        let started_at_ms = self.min_ts.unwrap_or(source.mtime_ms);
        let updated_at_ms = self.max_ts.unwrap_or(source.mtime_ms);
        Session {
            id: self.session_id.clone(),
            provider: ProviderId::ClaudeCode,
            native_id: self.native_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            kind: self.kind,
            title,
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
            machine_id: self.machine_id.clone(),
            agent_version: self.version.clone(),
            entrypoint: self.entrypoint.clone(),
            started_at_ms,
            ended_at_ms: Some(updated_at_ms),
            updated_at_ms,
            models: models.into_iter().map(|(_, m)| m).collect(),
            usage,
            message_count: self.message_count,
            recorded_cost_usd: self.recorded_cost_usd,
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

impl ClaudeLineParser {
    fn record(
        &mut self,
        line_no: usize,
        v: Value,
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
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
            return Ok(());
        }
        match ty.as_str() {
            "user" => self.record_user(line_no, &v),
            "assistant" => return self.record_assistant(line_no, &v, load),
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
        Ok(())
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

    /// 新消息计入会话累计量。
    fn count_new(&mut self, m: &Message) {
        self.message_count += 1;
        self.min_ts = Some(
            self.min_ts
                .map_or(m.timestamp_ms, |t| t.min(m.timestamp_ms)),
        );
        self.max_ts = Some(
            self.max_ts
                .map_or(m.timestamp_ms, |t| t.max(m.timestamp_ms)),
        );
        if let Some(u) = &m.usage {
            self.usage.add(u);
        }
        self.cost
            .replace((None, None), (m.model.as_deref(), m.usage.as_ref()));
        if let Some(model) = &m.model {
            self.note_model(m.timestamp_ms, model);
        }
        if let Some(title) = first_user_title(m)
            && self
                .first_user
                .as_ref()
                .is_none_or(|(ts, _)| m.timestamp_ms < *ts)
        {
            self.first_user = Some((m.timestamp_ms, title));
        }
    }

    fn note_model(&mut self, ts: i64, model: &str) {
        if model.starts_with('<') {
            return;
        }
        match self.models.iter_mut().find(|(_, m)| m == model) {
            Some((first, _)) => *first = (*first).min(ts),
            None => self.models.push((ts, model.to_string())),
        }
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
        let m = Message {
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
        };
        self.count_new(&m);
        self.plain.push(m);
    }

    fn record_assistant(
        &mut self,
        line_no: usize,
        v: &Value,
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart> {
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
            let m = Message {
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
            };
            self.count_new(&m);
            self.plain.push(m);
            return Ok(());
        };

        let Some(r) = self.asst_refs.get(&key) else {
            let m = Message {
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
            };
            self.asst_refs.insert(
                key.clone(),
                AsstRef {
                    id: m.id.clone(),
                    has_final: m.stop_reason.is_some() && m.usage.is_some(),
                },
            );
            self.count_new(&m);
            self.asst_pos.insert(key, self.asst.len());
            self.asst.push(m);
            return Ok(());
        };

        // 续行：并进已有的那条。它若已在前几批交出去了，从库里取回来改。
        let idx = match self.asst_pos.get(&key) {
            Some(&i) => i,
            None => {
                let Some(existing) = load(&r.id) else {
                    return Err(Restart(format!(
                        "第 {line_no} 行要并入的 assistant 消息 {} 不在库里",
                        r.id
                    )));
                };
                self.asst.push(existing);
                self.asst_pos.insert(key.clone(), self.asst.len() - 1);
                self.asst.len() - 1
            }
        };
        let has_final = r.has_final;
        let m = &mut self.asst[idx];
        let before = (m.model.clone(), m.usage);
        m.blocks.extend(blocks);
        // usage：优先带 stop_reason 的那条；否则取最后一条（流式 output_tokens 单调递增）。
        let mut now_final = has_final;
        if !has_final && usage.is_some() {
            m.usage = usage;
            m.stop_reason = stop_reason;
            now_final = m.stop_reason.is_some();
        }
        if m.model.is_none() {
            m.model = model;
        }
        let after = (m.model.clone(), m.usage);
        let first_ts = m.timestamp_ms;
        if let Some(r) = self.asst_refs.get_mut(&key) {
            r.has_final = now_final;
        }
        if let Some(u) = &before.1 {
            crate::providers::usage_sub(&mut self.usage, u);
        }
        if let Some(u) = &after.1 {
            self.usage.add(u);
        }
        self.cost.replace(
            (before.0.as_deref(), before.1.as_ref()),
            (after.0.as_deref(), after.1.as_ref()),
        );
        if let Some(model) = &after.0 {
            self.note_model(first_ts, model);
        }
        Ok(())
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
        let m = Message {
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
        };
        self.count_new(&m);
        self.plain.push(m);
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
