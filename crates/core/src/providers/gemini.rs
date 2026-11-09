//! Gemini（Google）会话的发现与解析。来源根目录是 `~/.gemini`，下面有两种数据：
//!
//! 1. **Gemini CLI**：`tmp/<项目>/chats/session-<YYYY-MM-DDTHH-MM>-<uuid前8>.json|.jsonl`，
//!    子代理在 `tmp/<项目>/chats/<父 sessionId>/<agentId>.jsonl`。`<项目>` 旧版是项目根路径的
//!    sha256，0.4x 起是 slug（`~/.gemini/projects.json` 记 路径→slug，目录里 `.project_root` 记路径）。
//!    - 旧版 `.json`：整文件 JSON `{sessionId, projectHash, startTime, lastUpdated, messages:[…]}`，整文件覆盖写。
//!    - 新版 `.jsonl`：首行元数据，之后逐行追加。消息记录（带 `id`）同 id 后写覆盖前写、位置不变；
//!      `{"$set":{…}}` 改元数据；`{"$rewindTo":id}` 删掉该条及之后；`{"$patch":{…}}` 改内容 / 工具结果、
//!      删除（`removeIds`）、重排（`orderIds`）。回放规则照抄官方 `loadConversationRecord`。
//!    - `tokens{input,output,cached,thoughts,tool,total}` 来自 Gemini API usageMetadata：
//!      `input`（promptTokenCount）**含**缓存读，`total = input + output + thoughts + tool`。
//!      入库统一成「新鲜输入」口径：input = input − cached + tool，cache_read = cached，
//!      output = output + thoughts（思考按输出计费，与 Claude / Codex 的 output 含推理一致），
//!      reasoning = thoughts，`input_includes_cache = false`。
//! 2. **Antigravity**（Gemini CLI 的继任者，桌面应用与 `agy`）：`antigravity/conversations/<uuid>.pb`
//!    是加密 / 压缩过的 protobuf（熵≈8 bit/字节，无公开 schema），内容解不开；
//!    只从同目录的 `conversation_summaries.db`（SQLite，只读打开）取标题、时间、工作区、父会话，
//!    做「存在性 + 标题 + 时间」级归档，消息为空，`ignored_records["antigravity-pb-undecoded"]` 计数。
//!    `antigravity-backup/`、`antigravity-ide/` 是同一批会话的迁移副本，不扫。
//!
//! 两种数据都记为 `ProviderId::GeminiCli`（栖息地里同一位住户），用 `entrypoint` 区分：`cli` / `antigravity`。

use crate::model::{ContentBlock, Message, ProviderId, Role, Session, SessionKind, Usage};
use crate::providers::{Discovery, Parsed, SourceProvider};
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub struct GeminiProvider {
    root: PathBuf,
    machine_id: String,
}

impl GeminiProvider {
    /// `root` 是 `~/.gemini`（见 `paths::gemini_dir`）。
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            machine_id: crate::providers::machine_id(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Gemini CLI 的 `tmp/` 或 Antigravity 的 `conversations/` 任一存在即可。
    pub fn detect(&self) -> bool {
        self.root.join("tmp").is_dir() || self.antigravity_dir().join("conversations").is_dir()
    }

    fn antigravity_dir(&self) -> PathBuf {
        self.root.join("antigravity")
    }

    pub fn discover(&self) -> Discovery {
        let mut out = Discovery::default();
        // Gemini CLI：tmp/<项目>/chats/session-*.json(l)，子代理 tmp/<项目>/chats/<父>/<agent>.json(l)。
        let tmp = self.root.join("tmp");
        for entry in walkdir::WalkDir::new(&tmp)
            .min_depth(3)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() || !is_chat_file(entry.path()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&tmp) else {
                continue;
            };
            let parts: Vec<_> = rel.components().collect();
            let in_chats = parts.get(1).is_some_and(|c| c.as_os_str() == "chats");
            let name = entry.file_name().to_string_lossy();
            let ok = match parts.len() {
                3 => in_chats && name.starts_with("session-"),
                4 => in_chats,
                _ => false,
            };
            if ok {
                out.files.push(entry.into_path());
            }
        }
        // Antigravity：conversations/<uuid>.pb（跳过写入中的 .tmp）。
        if let Ok(rd) = std::fs::read_dir(self.antigravity_dir().join("conversations")) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_file() && p.extension().is_some_and(|x| x == "pb") {
                    out.files.push(p);
                }
            }
        }
        out.files.sort();
        out
    }

    pub fn parse_file(&self, path: &Path) -> Result<Parsed> {
        if path.extension().is_some_and(|x| x == "pb") {
            self.parse_antigravity(path)
        } else {
            self.parse_cli(path)
        }
    }

    fn make_id(&self, native: &str) -> String {
        format!(
            "{}:{}:{native}",
            ProviderId::GeminiCli.as_str(),
            self.machine_id
        )
    }

    // ---------------- Gemini CLI ----------------

    fn parse_cli(&self, path: &Path) -> Result<Parsed> {
        let source = crate::providers::source_info(path, false)?;
        let text =
            std::fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
        let mut warnings = Vec::new();
        let mut ignored: BTreeMap<String, usize> = BTreeMap::new();

        let (meta, records) = if path.extension().is_some_and(|x| x == "jsonl") {
            replay_jsonl(&text, &mut warnings)
        } else {
            let v: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} 不是合法 JSON", path.display()))?;
            let records = v
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let meta = match v {
                Value::Object(mut m) => {
                    m.remove("messages");
                    m
                }
                _ => anyhow::bail!("{} 顶层不是对象", path.display()),
            };
            (meta, records)
        };

        // 文件位置：tmp/<项目>/chats/<文件> 或 tmp/<项目>/chats/<父>/<文件>。
        let chats_dir = path.parent().context("没有上级目录")?;
        let (project_dir, parent_from_path) = if chats_dir.file_name().is_some_and(|n| n == "chats")
        {
            (chats_dir.parent(), None)
        } else {
            let parent_id = chats_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            (chats_dir.parent().and_then(Path::parent), parent_id)
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("文件名不是 UTF-8")?;

        let session_native = str_of(&meta, "sessionId").unwrap_or_else(|| stem.to_string());
        let is_subagent =
            parent_from_path.is_some() || str_of(&meta, "kind").as_deref() == Some("subagent");
        let (native_id, parent_session_id, kind) = match (&parent_from_path, is_subagent) {
            (Some(parent), _) => (
                format!("{parent}/{session_native}"),
                Some(self.make_id(parent)),
                SessionKind::Subagent,
            ),
            (None, true) => (session_native.clone(), None, SessionKind::Subagent),
            (None, false) => (session_native.clone(), None, SessionKind::Main),
        };
        let session_id = self.make_id(&native_id);

        let mut messages = Vec::new();
        for (idx, rec) in records.iter().enumerate() {
            map_record(&session_id, idx, rec, &mut messages, &mut ignored);
        }

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

        let title = str_of(&meta, "summary")
            .map(|s| one_line(&s, 80))
            .filter(|s| !s.is_empty())
            .or_else(|| messages.iter().find_map(first_user_title));
        let started_at_ms = str_of(&meta, "startTime")
            .and_then(|s| parse_ts(&s))
            .or_else(|| messages.first().map(|m| m.timestamp_ms))
            .unwrap_or(source.mtime_ms);
        let updated_at_ms = str_of(&meta, "lastUpdated")
            .and_then(|s| parse_ts(&s))
            .into_iter()
            .chain(messages.last().map(|m| m.timestamp_ms))
            .max()
            .unwrap_or(source.mtime_ms)
            .max(started_at_ms);
        let cwd = meta
            .get("directories")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| project_dir.and_then(|d| self.project_root_of(d)));

        let session = Session {
            id: session_id,
            provider: ProviderId::GeminiCli,
            native_id,
            parent_session_id,
            kind,
            title,
            cwd,
            git_branch: None,
            machine_id: self.machine_id.clone(),
            agent_version: None,
            entrypoint: Some("cli".to_string()),
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
            warnings,
            ignored_records: ignored,
        })
    }

    /// 项目目录 → 项目根路径：`.project_root` 标记文件 > `projects.json`（slug）> 对其中路径算 sha256（旧版哈希目录）。
    fn project_root_of(&self, project_dir: &Path) -> Option<String> {
        if let Ok(s) = std::fs::read_to_string(project_dir.join(".project_root")) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        let name = project_dir.file_name()?.to_str()?;
        let registry: Value =
            serde_json::from_str(&std::fs::read_to_string(self.root.join("projects.json")).ok()?)
                .ok()?;
        let projects = registry.get("projects")?.as_object()?;
        projects.iter().find_map(|(path, slug)| {
            let hash = sha256_hex(path);
            (slug.as_str() == Some(name) || hash == name).then(|| path.clone())
        })
    }

    // ---------------- Antigravity ----------------

    fn parse_antigravity(&self, path: &Path) -> Result<Parsed> {
        let source = crate::providers::source_info(path, false)?;
        let conv_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("文件名不是 UTF-8")?
            .to_string();
        let mut warnings = Vec::new();
        let mut ignored: BTreeMap<String, usize> = BTreeMap::new();
        ignored.insert("antigravity-pb-undecoded".to_string(), 1);

        let db = self.antigravity_dir().join("conversation_summaries.db");
        let row = match read_summary(&db, &conv_id) {
            Ok(Some(row)) => Some(row),
            Ok(None) => {
                warnings.push(format!(
                    "{conv_id} 在 conversation_summaries.db 里没有摘要行"
                ));
                None
            }
            Err(e) => {
                warnings.push(format!("读取 {} 失败: {e:#}", db.display()));
                None
            }
        };
        let row = row.unwrap_or_default();

        let created_ms = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        let updated_at_ms = row
            .last_modified_ms
            .into_iter()
            .chain(row.last_user_input_ms)
            .max()
            .unwrap_or(source.mtime_ms);
        let started_at_ms = created_ms
            .into_iter()
            .chain(row.last_user_input_ms)
            .min()
            .unwrap_or(updated_at_ms)
            .min(updated_at_ms);
        let (parent_session_id, kind) = match row.parent.as_deref() {
            Some(p) if !p.is_empty() => (Some(self.make_id(p)), SessionKind::Subagent),
            _ => (None, SessionKind::Main),
        };

        let session = Session {
            id: self.make_id(&conv_id),
            provider: ProviderId::GeminiCli,
            native_id: conv_id,
            parent_session_id,
            kind,
            title: row.title,
            cwd: row.workspace,
            git_branch: None,
            machine_id: self.machine_id.clone(),
            agent_version: None,
            entrypoint: Some("antigravity".to_string()),
            started_at_ms,
            ended_at_ms: Some(updated_at_ms),
            updated_at_ms,
            models: Vec::new(),
            usage: Usage::default(),
            message_count: 0,
            recorded_cost_usd: None,
            estimated_cost_usd: None,
            source,
        };
        Ok(Parsed {
            session,
            messages: Vec::new(),
            warnings,
            ignored_records: ignored,
        })
    }
}

impl SourceProvider for GeminiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::GeminiCli
    }
    fn detect(&self) -> bool {
        GeminiProvider::detect(self)
    }
    fn root(&self) -> &Path {
        GeminiProvider::root(self)
    }
    fn discover(&self) -> Discovery {
        GeminiProvider::discover(self)
    }
    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        GeminiProvider::parse_file(self, path)
    }
}

fn is_chat_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x == "json" || x == "jsonl")
}

/// 按官方 `loadConversationRecord` 的规则回放 `.jsonl`，返回（元数据，按顺序的消息记录）。
fn replay_jsonl(text: &str, warnings: &mut Vec<String>) -> (Map<String, Value>, Vec<Value>) {
    let mut meta = Map::new();
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Value> = HashMap::new();

    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("第 {} 行不是合法 JSON: {e}", idx + 1));
                continue;
            }
        };
        let Some(obj) = rec.as_object() else {
            warnings.push(format!("第 {} 行不是对象", idx + 1));
            continue;
        };
        if let Some(rewind) = obj.get("$rewindTo").and_then(Value::as_str) {
            match order.iter().position(|id| id == rewind) {
                Some(pos) => {
                    for id in order.drain(pos..) {
                        by_id.remove(&id);
                    }
                }
                None => {
                    order.clear();
                    by_id.clear();
                }
            }
        } else if let Some(patch) = obj.get("$patch").and_then(Value::as_object) {
            apply_patch(patch, &mut by_id);
            if let Some(updates) = patch.get("updates").and_then(Value::as_array) {
                for u in updates.iter().filter_map(Value::as_object) {
                    apply_patch(u, &mut by_id);
                }
            }
            if let Some(remove) = patch.get("removeIds").and_then(Value::as_array) {
                for id in remove.iter().filter_map(Value::as_str) {
                    by_id.remove(id);
                    order.retain(|x| x != id);
                }
            }
            if let Some(ids) = patch.get("orderIds").and_then(Value::as_array) {
                let ordered: Vec<String> = ids
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|id| by_id.contains_key(*id))
                    .map(str::to_string)
                    .collect();
                order.retain(|id| !ordered.contains(id));
                order.extend(ordered);
            }
        } else if let Some(set) = obj.get("$set").and_then(Value::as_object) {
            for (k, v) in set {
                meta.insert(k.clone(), v.clone());
            }
        } else if let Some(id) = obj.get("id").and_then(Value::as_str) {
            if !by_id.contains_key(id) {
                order.push(id.to_string());
            }
            by_id.insert(id.to_string(), rec.clone());
        } else if obj.contains_key("sessionId") {
            for (k, v) in obj {
                meta.insert(k.clone(), v.clone());
            }
        } else {
            warnings.push(format!("第 {} 行是未知记录", idx + 1));
        }
    }
    let records = order.iter().filter_map(|id| by_id.remove(id)).collect();
    (meta, records)
}

fn apply_patch(patch: &Map<String, Value>, by_id: &mut HashMap<String, Value>) {
    let Some(existing) = patch
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| by_id.get_mut(id))
    else {
        return;
    };
    if let Some(content) = patch.get("content").filter(|c| !c.is_null()) {
        existing["content"] = content.clone();
    }
    let Some(tc_patches) = patch.get("toolCalls").and_then(Value::as_array) else {
        return;
    };
    if existing.get("type").and_then(Value::as_str) != Some("gemini") {
        return;
    }
    let Some(calls) = existing.get_mut("toolCalls").and_then(Value::as_array_mut) else {
        return;
    };
    for p in tc_patches {
        let (Some(id), Some(result)) = (p.get("id").and_then(Value::as_str), p.get("result"))
        else {
            continue;
        };
        if let Some(tc) = calls
            .iter_mut()
            .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
        {
            tc["result"] = result.clone();
        }
    }
}

/// 一条消息记录 → 规范化消息。gemini 消息的工具结果另起一条 `Role::Tool` 消息。
fn map_record(
    session_id: &str,
    idx: usize,
    rec: &Value,
    out: &mut Vec<Message>,
    ignored: &mut BTreeMap<String, usize>,
) {
    let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
    let native = rec.get("id").and_then(Value::as_str).map(str::to_string);
    let id = native.clone().unwrap_or_else(|| format!("m{idx}"));
    let timestamp_ms = rec
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .unwrap_or(0);
    let content_blocks = content_to_blocks(rec.get("content"));
    let base = |role: Role, is_meta: bool, blocks: Vec<ContentBlock>| Message {
        id: id.clone(),
        session_id: session_id.to_string(),
        native_id: native.clone(),
        parent_id: None,
        role,
        is_meta,
        timestamp_ms,
        model: None,
        stop_reason: None,
        blocks,
        usage: None,
    };
    match ty {
        "user" => out.push(base(Role::User, false, content_blocks)),
        "info" | "error" | "warning" => out.push(base(Role::System, true, content_blocks)),
        "gemini" => {
            let mut blocks = Vec::new();
            for t in rec
                .get("thoughts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let subject = t.get("subject").and_then(Value::as_str).unwrap_or("");
                let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                let text = match (subject.is_empty(), desc.is_empty()) {
                    (true, _) => desc.to_string(),
                    (false, true) => subject.to_string(),
                    (false, false) => format!("**{subject}** {desc}"),
                };
                if !text.is_empty() {
                    blocks.push(ContentBlock::Thinking { text });
                }
            }
            blocks.extend(content_blocks);
            let calls: Vec<&Value> = rec
                .get("toolCalls")
                .and_then(Value::as_array)
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            let mut results = Vec::new();
            let mut result_ts = timestamp_ms;
            for (n, tc) in calls.iter().enumerate() {
                let call_id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{id}#{n}"));
                blocks.push(ContentBlock::ToolCall {
                    tool_call_id: call_id.clone(),
                    name: tc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: tc.get("args").cloned().unwrap_or(Value::Null),
                });
                if let Some(output) = tool_output(tc) {
                    let status = tc.get("status").and_then(Value::as_str);
                    results.push(ContentBlock::ToolResult {
                        tool_call_id: call_id,
                        output,
                        is_error: status.is_some_and(|s| s != "success"),
                    });
                    if let Some(ts) = tc
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_ts)
                    {
                        result_ts = result_ts.max(ts);
                    }
                }
            }
            let mut msg = base(Role::Assistant, false, blocks);
            msg.model = rec.get("model").and_then(Value::as_str).map(str::to_string);
            msg.usage = rec.get("tokens").map(tokens_to_usage);
            out.push(msg);
            if !results.is_empty() {
                let mut tool = base(Role::Tool, false, results);
                tool.id = format!("{id}:tool-results");
                tool.native_id = None;
                tool.timestamp_ms = result_ts;
                out.push(tool);
            }
        }
        other => {
            *ignored
                .entry(format!(
                    "message:{}",
                    if other.is_empty() { "?" } else { other }
                ))
                .or_default() += 1;
        }
    }
}

/// `tokens` → 「新鲜输入」口径（见模块注释）。
fn tokens_to_usage(t: &Value) -> Usage {
    let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cached");
    let thoughts = n("thoughts");
    Usage {
        input_tokens: n("input").saturating_sub(cached) + n("tool"),
        output_tokens: n("output") + thoughts,
        cache_read_tokens: cached,
        reasoning_tokens: thoughts,
        requests: 1,
        input_includes_cache: false,
        ..Usage::default()
    }
}

/// `content` 是字符串或 Gemini `Part[]`（`{text}` / `{inlineData:{mimeType}}` / `{functionCall}` …）。
fn content_to_blocks(content: Option<&Value>) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    match content {
        Some(Value::String(s)) if !s.is_empty() => {
            blocks.push(ContentBlock::Text { text: s.clone() })
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(text) = p.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                } else if let Some(inline) = p.get("inlineData") {
                    blocks.push(ContentBlock::Image {
                        media_type: inline
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                    });
                } else {
                    let native_type = p
                        .as_object()
                        .and_then(|o| o.keys().next().cloned())
                        .unwrap_or_else(|| "part".to_string());
                    blocks.push(ContentBlock::Unknown {
                        native_type,
                        raw: p.clone(),
                    });
                }
            }
        }
        Some(v @ Value::Object(_)) => {
            blocks.extend(content_to_blocks(Some(&Value::Array(vec![v.clone()]))))
        }
        _ => {}
    }
    blocks
}

/// 工具结果文本：`result[].functionResponse.response.output | error`，否则 `resultDisplay`。
fn tool_output(tc: &Value) -> Option<String> {
    let mut parts = Vec::new();
    for r in tc
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(resp) = r.get("functionResponse").and_then(|f| f.get("response")) else {
            continue;
        };
        match resp.get("output").or_else(|| resp.get("error")) {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(v) => parts.push(v.to_string()),
            None => parts.push(resp.to_string()),
        }
    }
    if !parts.is_empty() {
        return Some(parts.join("\n"));
    }
    match tc.get("resultDisplay") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) if !v.is_null() => Some(v.to_string()),
        _ => None,
    }
}

fn first_user_title(m: &Message) -> Option<String> {
    if m.role != Role::User || m.is_meta {
        return None;
    }
    let t = one_line(&m.text(), 80);
    (!t.is_empty()).then_some(t)
}

fn one_line(s: &str, max: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn str_of(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
struct SummaryRow {
    title: Option<String>,
    workspace: Option<String>,
    parent: Option<String>,
    last_modified_ms: Option<i64>,
    last_user_input_ms: Option<i64>,
}

/// 只读打开 Antigravity 的摘要库，取一行。标题在 `raw_summary`（protobuf）的字段 1；
/// `title` 列本机实测全空，`preview` 是对话片段，只在两者都没有时兜底。
fn read_summary(db: &Path, conv_id: &str) -> Result<Option<SummaryRow>> {
    use rusqlite::{OpenFlags, OptionalExtension};
    let conn = rusqlite::Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("只读打开 {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let row = conn
        .query_row(
            "SELECT title, preview, raw_summary, workspace_uris, parent_conversation_id,
                    CAST(last_modified_time AS TEXT), CAST(last_user_input_time AS TEXT)
             FROM conversation_summaries WHERE conversation_id = ?1",
            [conv_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((title, preview, raw, workspaces, parent, modified, user_input)) = row else {
        return Ok(None);
    };
    let title = title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| raw.as_deref().and_then(proto_string_field1))
        .or(preview)
        .map(|t| one_line(&t, 80))
        .filter(|t| !t.is_empty());
    let workspace = workspaces
        .and_then(|w| serde_json::from_str::<Vec<String>>(&w).ok())
        .and_then(|v| v.into_iter().next())
        .map(|u| percent_decode(u.strip_prefix("file://").unwrap_or(&u)));
    Ok(Some(SummaryRow {
        title,
        workspace,
        parent,
        last_modified_ms: modified.as_deref().and_then(parse_sqlite_ts),
        last_user_input_ms: user_input.as_deref().and_then(parse_sqlite_ts),
    }))
}

/// protobuf 顶层字段 1（wire type 2）解成 UTF-8；不是这个形状就返回 None。
fn proto_string_field1(buf: &[u8]) -> Option<String> {
    let (&tag, rest) = buf.split_first()?;
    if tag != 0x0A {
        return None;
    }
    let mut len: usize = 0;
    let mut shift = 0;
    let mut i = 0;
    loop {
        let b = *rest.get(i)?;
        len |= usize::from(b & 0x7F) << shift;
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
    let bytes = rest.get(i..i.checked_add(len)?)?;
    String::from_utf8(bytes.to_vec())
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// GORM 写的 `2026-02-01 01:30:54.026222+00:00`。
fn parse_sqlite_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%:z")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s))
        .ok()
        .map(|d| d.timestamp_millis())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(h) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(h, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_title_and_percent_decode() {
        let mut buf = vec![0x0A, 5];
        buf.extend_from_slice(b"hello");
        buf.extend_from_slice(&[0x10, 0x01]);
        assert_eq!(proto_string_field1(&buf).as_deref(), Some("hello"));
        assert_eq!(proto_string_field1(&[0x12, 1, b'x']), None);
        assert_eq!(proto_string_field1(&[0x0A, 9, b'x']), None);
        assert_eq!(percent_decode("/Users/a%20b/%E4%B8%AD"), "/Users/a b/中");
        assert!(parse_sqlite_ts("2026-02-01 01:30:54.026222+00:00").is_some());
    }
}
