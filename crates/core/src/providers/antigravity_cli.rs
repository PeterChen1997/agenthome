//! Antigravity CLI（`agy`）会话的发现与解析。Gemini CLI 在 2026 年对个人账号（含 Google AI Pro）停服后，
//! 个人用户改用它。来源根目录 `~/.gemini/antigravity-cli`：
//!
//! - `conversations/<uuid>.db`：每个对话一个 SQLite（WAL 模式，`agy` 退出时合并回主文件）。
//!   `steps` 表一行一步，`metadata` / `step_payload` 是**没加密**的 protobuf，没有公开 schema，
//!   字段号对照 `agy -p --output-format stream-json` 的输出逆推（2026-11-10，agy 1.5.3）：
//!   - `step_type` 14 用户输入：payload `19.2` = 原文。
//!   - `step_type` 15 模型回复：payload `20.1` = 正文，`20.3` = 思考摘要，
//!     `20.7` = 工具调用（重复：`1` 调用 id、`2` 工具名、`3` 参数 JSON）。
//!     metadata `9` = 用量：`2` 新鲜输入（**不含**缓存读）、`3` 输出、`5` 缓存读、`9` 思考、`11` 回复 id。
//!   - `step_type` 132 工具结果：metadata `4.1` = 调用 id，payload `140.2.1` = 结果摘要。
//!   - 每步 metadata `1` = 时间（`1` 秒、`2` 纳秒）。
//!   - `gen_metadata` 表每次模型调用一行：`1.4.11` 回复 id、`1.19` 实际服务的模型名。
//! - `conversation_summaries.db`：标题、工作区、父对话，表结构与 Antigravity 桌面版相同。
//!
//! 入库口径同 Gemini CLI：input = 新鲜输入，cache_read = 缓存读，output = 输出 + 思考（思考按输出计费），
//! reasoning = 思考，`input_includes_cache = false`。
//!
//! 已知限制：进行中的对话写在 `-wal` 里，主文件的大小与修改时间要等 `agy` 合并后才变，入库会晚到那时；
//! SQLite 原件不进 raw 层（每次写入都会改动整页，按版本归档只会无限增长）。

use crate::model::{ContentBlock, Message, ProviderId, Role, Session, SessionKind, Usage};
use crate::providers::gemini::{one_line, read_summary};
use crate::providers::{Discovery, Parsed, SourceProvider, proto};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

const USER_INPUT: i64 = 14;
const AGENT_RESPONSE: i64 = 15;
const TOOL_RESULT: i64 = 132;

pub struct AntigravityCliProvider {
    root: PathBuf,
    machine_id: String,
}

impl AntigravityCliProvider {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            machine_id: crate::providers::machine_id(),
        }
    }

    fn conversations_dir(&self) -> PathBuf {
        self.root.join("conversations")
    }

    fn make_id(&self, native: &str) -> String {
        format!(
            "{}:{}:{native}",
            ProviderId::AntigravityCli.as_str(),
            self.machine_id
        )
    }

    fn is_conversation(&self, path: &Path) -> bool {
        path.parent() == Some(self.conversations_dir().as_path())
            && path.extension().is_some_and(|x| x == "db")
    }

    fn parse(&self, path: &Path) -> Result<Parsed> {
        let source = crate::providers::source_info(path, false)?;
        let conv_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("文件名不是 UTF-8")?
            .to_string();
        let session_id = self.make_id(&conv_id);
        let mut warnings = Vec::new();
        let mut ignored: BTreeMap<String, usize> = BTreeMap::new();

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("只读打开 {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;

        // 回复 id → 实际服务的模型名。
        let mut models_by_response: HashMap<String, String> = HashMap::new();
        let mut stmt = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx")?;
        for data in stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0))? {
            let Some(data) = data? else { continue };
            let Some(head) = proto::bytes(&data, 1) else {
                continue;
            };
            if let (Some(rid), Some(model)) = (
                proto::path(head, &[4]).and_then(|u| proto::string(u, 11)),
                proto::string(head, 19),
            ) {
                models_by_response.insert(rid, model);
            }
        }

        let mut messages: Vec<Message> = Vec::new();
        let mut stmt =
            conn.prepare("SELECT idx, step_type, metadata, step_payload FROM steps ORDER BY idx")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (idx, step_type, meta, payload) = row?;
            let timestamp_ms = proto::bytes(&meta, 1)
                .map(|t| {
                    let secs = proto::varint(t, 1).unwrap_or(0) as i64;
                    let nanos = proto::varint(t, 2).unwrap_or(0) as i64;
                    secs * 1000 + nanos / 1_000_000
                })
                .unwrap_or(source.mtime_ms);
            let base = |role: Role, blocks: Vec<ContentBlock>| Message {
                id: format!("{conv_id}/{idx}"),
                session_id: session_id.clone(),
                native_id: None,
                parent_id: None,
                role,
                is_meta: false,
                timestamp_ms,
                model: None,
                stop_reason: None,
                blocks,
                usage: None,
            };
            match step_type {
                USER_INPUT => {
                    match proto::path(&payload, &[19]).and_then(|p| proto::string(p, 2)) {
                        Some(text) => {
                            messages.push(base(Role::User, vec![ContentBlock::Text { text }]))
                        }
                        None => warnings.push(format!("第 {idx} 步用户输入没有正文（字段 19.2）")),
                    }
                }
                AGENT_RESPONSE => {
                    let Some(body) = proto::bytes(&payload, 20) else {
                        warnings.push(format!("第 {idx} 步模型回复没有字段 20，格式可能变了"));
                        continue;
                    };
                    let mut blocks = Vec::new();
                    if let Some(text) = proto::string(body, 3).filter(|t| !t.trim().is_empty()) {
                        blocks.push(ContentBlock::Thinking { text });
                    }
                    if let Some(text) = proto::string(body, 1).filter(|t| !t.trim().is_empty()) {
                        blocks.push(ContentBlock::Text { text });
                    }
                    for call in proto::all_bytes(body, 7) {
                        let args = proto::string(call, 3).unwrap_or_default();
                        blocks.push(ContentBlock::ToolCall {
                            tool_call_id: proto::string(call, 1).unwrap_or_default(),
                            name: proto::string(call, 2).unwrap_or_default(),
                            input: serde_json::from_str(&args)
                                .unwrap_or(serde_json::Value::String(args)),
                        });
                    }
                    let mut msg = base(Role::Assistant, blocks);
                    match proto::bytes(&meta, 9) {
                        Some(u) => {
                            msg.usage = Some(usage_of(u));
                            msg.model = proto::string(u, 11)
                                .and_then(|rid| models_by_response.get(&rid).cloned());
                            msg.native_id = proto::string(u, 11);
                        }
                        None => warnings.push(format!("第 {idx} 步模型回复没有用量（字段 9）")),
                    }
                    messages.push(msg);
                }
                TOOL_RESULT => {
                    let call_id = proto::path(&meta, &[4])
                        .and_then(|m| proto::string(m, 1))
                        .unwrap_or_default();
                    let output = proto::path(&payload, &[140, 2])
                        .and_then(|r| proto::string(r, 1))
                        .unwrap_or_default();
                    messages.push(base(
                        Role::Tool,
                        vec![ContentBlock::ToolResult {
                            tool_call_id: call_id,
                            output,
                            is_error: false,
                        }],
                    ));
                }
                other => {
                    *ignored
                        .entry(format!("antigravity-cli:step_type:{other}"))
                        .or_default() += 1;
                }
            }
        }

        let summary = match read_summary(&self.root.join("conversation_summaries.db"), &conv_id) {
            Ok(row) => row,
            Err(e) => {
                warnings.push(format!("读取 conversation_summaries.db 失败: {e:#}"));
                None
            }
        }
        .unwrap_or_default();

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
        let title = summary.title.clone().or_else(|| {
            messages
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| one_line(&m.text(), 80))
                .filter(|t| !t.is_empty())
        });
        let started_at_ms = messages
            .first()
            .map(|m| m.timestamp_ms)
            .unwrap_or(source.mtime_ms);
        let updated_at_ms = messages
            .last()
            .map(|m| m.timestamp_ms)
            .into_iter()
            .chain(summary.last_modified_ms)
            .max()
            .unwrap_or(source.mtime_ms)
            .max(started_at_ms);
        let (parent_session_id, kind) = match summary.parent.as_deref() {
            Some(p) if !p.is_empty() => (Some(self.make_id(p)), SessionKind::Subagent),
            _ => (None, SessionKind::Main),
        };

        let session = Session {
            id: session_id,
            provider: ProviderId::AntigravityCli,
            native_id: conv_id,
            parent_session_id,
            kind,
            title,
            cwd: summary.workspace,
            git_branch: None,
            machine_id: self.machine_id.clone(),
            agent_version: None,
            entrypoint: Some("agy".to_string()),
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
}

/// metadata 字段 9 → 规范化用量（新鲜输入口径，思考计入输出）。
fn usage_of(u: &[u8]) -> Usage {
    let n = |f: u32| proto::varint(u, f).unwrap_or(0);
    let thinking = n(9);
    Usage {
        input_tokens: n(2),
        output_tokens: n(3) + thinking,
        cache_read_tokens: n(5),
        reasoning_tokens: thinking,
        requests: 1,
        input_includes_cache: false,
        ..Usage::default()
    }
}

impl SourceProvider for AntigravityCliProvider {
    fn id(&self) -> ProviderId {
        ProviderId::AntigravityCli
    }

    fn detect(&self) -> bool {
        self.conversations_dir().is_dir()
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self) -> Discovery {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.conversations_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| self.is_conversation(p) && p.is_file())
            .collect();
        files.sort();
        Discovery {
            files,
            skipped_superseded: 0,
        }
    }

    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        self.parse(path)
    }

    fn accepts(&self, path: &Path) -> Option<bool> {
        Some(self.is_conversation(path) && path.is_file())
    }

    fn watch_suffixes(&self) -> &'static [&'static str] {
        &[".db"]
    }

    /// SQLite 原件按页改写，不进 raw 层（见模块注释）。
    fn archive_raw(&self, _path: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::proto::encode as e;

    fn ts(secs: u64) -> Vec<u8> {
        e::msg(1, &[e::uint(1, secs), e::uint(2, 500_000_000)])
    }

    /// 照真实 `agy` 对话的结构手搓一份：提问 → 回复（两个工具调用）→ 两个工具结果 → 回复。
    /// 数字取自 2026-11-10 真实对话 83dc4381 的 stream-json 输出。
    fn fixture(root: &Path) -> PathBuf {
        let conv = "83dc4381-1c12-44d3-84e7-f32643bcd313";
        std::fs::create_dir_all(root.join("conversations")).unwrap();
        let path = root.join("conversations").join(format!("{conv}.db"));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL DEFAULT 0,
                status integer NOT NULL DEFAULT 0, metadata blob, step_payload blob);
             CREATE TABLE gen_metadata (idx integer PRIMARY KEY, data blob, size integer NOT NULL DEFAULT 0);",
        )
        .unwrap();
        let usage = |input, output, cache, thinking, rid: &str| {
            let mut parts = vec![e::uint(1, 1418), e::uint(2, input), e::uint(3, output)];
            if cache > 0 {
                parts.push(e::uint(5, cache));
            }
            parts.push(e::uint(9, thinking));
            parts.push(e::bytes(11, rid.as_bytes()));
            e::msg(9, &parts)
        };
        let call = |id: &str, name: &str, args: &str| {
            e::msg(
                7,
                &[
                    e::bytes(1, id.as_bytes()),
                    e::bytes(2, name.as_bytes()),
                    e::bytes(3, args.as_bytes()),
                ],
            )
        };
        let steps: Vec<(i64, Vec<u8>, Vec<u8>)> = vec![
            (
                USER_INPUT,
                ts(1_794_316_393),
                e::msg(
                    19,
                    &[e::bytes(2, "数一下 fruit.txt 里有多少水果".as_bytes())],
                ),
            ),
            (
                AGENT_RESPONSE,
                [ts(1_794_316_393), usage(9326, 239, 0, 108, "r1")].concat(),
                e::msg(
                    20,
                    &[
                        e::bytes(3, b"Reading fruit.txt first."),
                        call("call_a", "read_file", r#"{"Path":"/p/fruit.txt"}"#),
                        call(
                            "call_b",
                            "run_command",
                            r#"{"CommandLine":"wc -l fruit.txt"}"#,
                        ),
                    ],
                ),
            ),
            (
                TOOL_RESULT,
                [ts(1_794_316_400), e::msg(4, &[e::bytes(1, b"call_a")])].concat(),
                e::msg(140, &[e::msg(2, &[e::bytes(1, b"Showing lines 1-2 of 2")])]),
            ),
            (
                TOOL_RESULT,
                [ts(1_794_316_400), e::msg(4, &[e::bytes(1, b"call_b")])].concat(),
                e::msg(
                    140,
                    &[e::msg(2, &[e::bytes(1, b"Command exited with code 0.")])],
                ),
            ),
            (
                AGENT_RESPONSE,
                [ts(1_794_316_401), usage(2488, 159, 7272, 17, "r2")].concat(),
                e::msg(20, &[e::bytes(1, "一共 8 个水果。".as_bytes())]),
            ),
            (101, ts(1_794_316_402), Vec::new()),
        ];
        for (idx, (ty, meta, payload)) in steps.into_iter().enumerate() {
            db.execute(
                "INSERT INTO steps (idx, step_type, status, metadata, step_payload) VALUES (?1, ?2, 3, ?3, ?4)",
                rusqlite::params![idx as i64, ty, meta, payload],
            )
            .unwrap();
        }
        for (idx, rid) in ["r1", "r2"].into_iter().enumerate() {
            let data = e::msg(
                1,
                &[
                    e::msg(4, &[e::bytes(11, rid.as_bytes())]),
                    e::bytes(19, b"gemini-4-argon-trial"),
                ],
            );
            db.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
                rusqlite::params![idx as i64, data],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn 解析出对话结构与每次回复的用量() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let provider = AntigravityCliProvider::new(dir.path().to_path_buf());
        assert!(provider.detect());
        assert_eq!(provider.discover().files, vec![path.clone()]);
        let parsed = provider.parse_file(&path).unwrap();
        assert!(
            parsed
                .warnings
                .iter()
                .all(|w| w.contains("conversation_summaries")),
            "{:?}",
            parsed.warnings
        );
        let roles: Vec<Role> = parsed.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Tool,
                Role::Assistant
            ]
        );
        assert_eq!(
            parsed.ignored_records.get("antigravity-cli:step_type:101"),
            Some(&1)
        );

        let s = &parsed.session;
        assert_eq!(s.provider, ProviderId::AntigravityCli);
        assert_eq!(s.title.as_deref(), Some("数一下 fruit.txt 里有多少水果"));
        assert_eq!(s.models, ["gemini-4-argon-trial"]);
        // 新鲜输入不含缓存读，直接相加；思考计入输出。
        assert_eq!(s.usage.input_tokens, 9326 + 2488);
        assert_eq!(s.usage.cache_read_tokens, 7272);
        assert_eq!(s.usage.output_tokens, 239 + 108 + 159 + 17);
        assert_eq!(s.usage.reasoning_tokens, 108 + 17);
        assert_eq!(s.usage.requests, 2);
        assert_eq!(s.started_at_ms, 1_794_316_393_500);

        let first = &parsed.messages[1];
        let calls: Vec<&str> = first
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, ["call_a", "call_b"]);
        // 工具结果要能挂回对应的调用。
        assert!(matches!(
            &parsed.messages[3].blocks[0],
            ContentBlock::ToolResult { tool_call_id, output, .. }
                if tool_call_id == "call_b" && output == "Command exited with code 0."
        ));
        assert_eq!(parsed.messages[4].text(), "一共 8 个水果。");
    }

    #[test]
    fn 只认对话库_不认_wal_与锁文件() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let provider = AntigravityCliProvider::new(dir.path().to_path_buf());
        assert_eq!(provider.accepts(&path), Some(true));
        let wal = path.with_extension("db-wal");
        std::fs::write(&wal, b"x").unwrap();
        assert_eq!(provider.accepts(&wal), Some(false));
        assert_eq!(provider.discover().files.len(), 1);
    }
}
