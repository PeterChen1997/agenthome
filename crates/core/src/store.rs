//! SQLite 索引层。可随时 `rm` 后从原件重建，所以 schema 变更直接升 `SCHEMA_VERSION` 重建表。
//!
//! 全文索引 `message_fts` 用 FTS5 `trigram` 分词（bundled SQLite ≥ 3.34 支持），
//! 中文子串可直接匹配；若运行时不支持则退回 `unicode61`（中文按整段切，只能整词命中）。

use crate::model::{
    ActivityKind, ContentBlock, CostBreakdown, CostSource, DailyUsage, Message, Period,
    ProviderActivity, ProviderHealth, ProviderId, Role, SEARCH_MARK_END, SEARCH_MARK_START,
    SearchHit, Session, SessionKind, SessionQuery, SessionSummary, SourceInfo, Usage, UsageTotals,
};
use crate::providers::SourceProvider;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

/// v4：Codex 会话级 `input_includes_cache` 改为 false（10.29），旧库里的 Codex 会话行需要重建。
const SCHEMA_VERSION: i32 = 4;

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id                  TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    native_id           TEXT NOT NULL,
    parent_session_id   TEXT,
    kind                TEXT NOT NULL,
    title               TEXT,
    cwd                 TEXT,
    project_name        TEXT NOT NULL,
    git_branch          TEXT,
    machine_id          TEXT NOT NULL,
    agent_version       TEXT,
    entrypoint          TEXT,
    started_at_ms       INTEGER NOT NULL,
    ended_at_ms         INTEGER,
    updated_at_ms       INTEGER NOT NULL,
    models_json         TEXT NOT NULL,
    input_tokens        INTEGER NOT NULL,
    output_tokens       INTEGER NOT NULL,
    cache_read_tokens   INTEGER NOT NULL,
    cache_write_tokens  INTEGER NOT NULL,
    cache_write_5m_tokens INTEGER NOT NULL,
    cache_write_1h_tokens INTEGER NOT NULL,
    reasoning_tokens    INTEGER NOT NULL,
    requests            INTEGER NOT NULL,
    input_includes_cache INTEGER NOT NULL,
    message_count       INTEGER NOT NULL,
    -- agent 自记的成本真值（Claude cost-state），Codex 为 NULL。
    recorded_cost_usd   REAL,
    -- 按定价表对消息 usage 估算之和；无可计价消息为 NULL。
    estimated_cost_usd  REAL,
    source_path         TEXT NOT NULL,
    source_size_bytes   INTEGER NOT NULL,
    source_mtime_ms     INTEGER NOT NULL,
    source_compressed   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_cwd ON sessions(cwd);
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);

-- uuid 只在单个文件内唯一：fork / resume 会把历史行连同 uuid 复制进新文件，
-- 所以主键是 (session_id, id)。
CREATE TABLE IF NOT EXISTS messages (
    id              TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    native_id       TEXT,
    parent_id       TEXT,
    role            TEXT NOT NULL,
    is_meta         INTEGER NOT NULL,
    timestamp_ms    INTEGER NOT NULL,
    model           TEXT,
    stop_reason     TEXT,
    blocks_json     TEXT NOT NULL,
    -- usage 拆列便于按天聚合；NULL 表示该消息无 usage（非 assistant）。
    input_tokens        INTEGER,
    output_tokens       INTEGER,
    cache_read_tokens   INTEGER,
    cache_write_tokens  INTEGER,
    cache_write_5m_tokens INTEGER,
    cache_write_1h_tokens INTEGER,
    reasoning_tokens    INTEGER,
    -- 按定价表估算；未知模型或无 usage 为 NULL。
    cost_usd            REAL,
    PRIMARY KEY (session_id, id)
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_messages_time ON messages(timestamp_ms);

CREATE TABLE IF NOT EXISTS sources (
    path        TEXT PRIMARY KEY,
    size_bytes  INTEGER NOT NULL,
    mtime_ms    INTEGER NOT NULL,
    session_id  TEXT NOT NULL
);
"#;

impl Store {
    /// 打开（不存在则创建）。自动建父目录、开 WAL、建表。
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("打开数据库 {}", path.display()))?;
        // busy_timeout：app 的同步线程与 CLI `scan` 可能同时写，写锁冲突时等待而不是立刻失败。
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        )?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            // 索引层可重建：版本不符直接推倒重来。
            conn.execute_batch(
                "DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS sources; DROP TABLE IF EXISTS message_fts;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        let fts = "CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(\
                   message_id UNINDEXED, session_id UNINDEXED, content, tokenize = 'trigram')";
        if let Err(e) = conn.execute_batch(fts) {
            eprintln!("警告: SQLite 不支持 trigram 分词，退回 unicode61（{e}）");
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(\
                 message_id UNINDEXED, session_id UNINDEXED, content, tokenize = 'unicode61')",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Store { conn })
    }

    /// 写入/覆盖会话元数据（单独事务）。
    pub fn upsert_session(&mut self, s: &Session) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_session_tx(&tx, s)?;
        tx.commit()?;
        Ok(())
    }

    /// 整体替换某会话的消息（单独事务）。
    pub fn replace_messages(&mut self, session_id: &str, messages: &[Message]) -> Result<()> {
        let tx = self.conn.transaction()?;
        replace_messages_tx(&tx, session_id, messages)?;
        tx.commit()?;
        Ok(())
    }

    /// 一次解析结果整体落库：会话 + 消息 + 来源状态，一个事务。
    pub fn store_parsed(&mut self, session: &Session, messages: &[Message]) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_session_tx(&tx, session)?;
        replace_messages_tx(&tx, &session.id, messages)?;
        tx.execute(
            "INSERT INTO sources(path, size_bytes, mtime_ms, session_id) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size_bytes = excluded.size_bytes,
                 mtime_ms = excluded.mtime_ms, session_id = excluded.session_id",
            params![
                session.source.path.to_string_lossy(),
                session.source.size_bytes as i64,
                session.source.mtime_ms,
                session.id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 按 updated_at 倒序列出会话；`text` 非空时先走 FTS 取会话集合。
    pub fn list_sessions(&self, q: &SessionQuery) -> Result<Vec<SessionSummary>> {
        let mut sql = String::from(
            "SELECT id, provider, kind, title, cwd, project_name, started_at_ms, updated_at_ms,
                    message_count, models_json, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                    reasoning_tokens, requests, input_includes_cache,
                    parent_session_id, source_path, recorded_cost_usd, estimated_cost_usd
             FROM sessions WHERE 1 = 1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(text) = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            if text.chars().count() >= 3 {
                sql.push_str(
                    " AND id IN (SELECT session_id FROM message_fts WHERE message_fts MATCH ?)",
                );
                // 整体当短语匹配，避免用户输入被当作 FTS 查询语法。
                args.push(Box::new(format!("\"{}\"", text.replace('"', "\"\""))));
            } else {
                // trigram 对少于 3 字符的查询不返回结果，退回 LIKE。
                sql.push_str(" AND id IN (SELECT session_id FROM message_fts WHERE content LIKE ? ESCAPE '\\')");
                let escaped = text
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                args.push(Box::new(format!("%{escaped}%")));
            }
        }
        if let Some(p) = &q.provider {
            sql.push_str(" AND provider = ?");
            args.push(Box::new(p.as_str().to_string()));
        }
        if let Some(cwd) = &q.cwd {
            sql.push_str(" AND cwd = ?");
            args.push(Box::new(cwd.clone()));
        }
        sql.push_str(" ORDER BY updated_at_ms DESC");
        if q.limit > 0 {
            sql.push_str(&format!(" LIMIT {} OFFSET {}", q.limit, q.offset));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_summary)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 全文搜索消息正文与会话标题，按相关度返回至多 `limit` 条命中，每条带高亮片段。
    /// 三个字以上走 FTS5（trigram）；更短的退回 LIKE，片段在 Rust 里截取。
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let text = query.trim();
        if text.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let select = "SELECT f.session_id, f.message_id, {SNIPPET}, s.title, s.provider,
                    s.project_name, s.updated_at_ms, m.role, m.timestamp_ms, f.content
             FROM message_fts f
             JOIN sessions s ON s.id = f.session_id
             LEFT JOIN messages m ON m.session_id = f.session_id AND m.id = f.message_id";
        let fts = text.chars().count() >= 3;
        let sql = if fts {
            // snippet(表, 列, 开始标记, 结束标记, 省略号, 最多 token 数)；trigram 下一个 token 约一个字。
            select.replace(
                "{SNIPPET}",
                "snippet(message_fts, 2, char(57344), char(57345), '…', 48)",
            ) + " WHERE message_fts MATCH ?1 ORDER BY rank LIMIT ?2"
        } else {
            select.replace("{SNIPPET}", "NULL")
                + " WHERE f.content LIKE ?1 ESCAPE '\\' ORDER BY s.updated_at_ms DESC LIMIT ?2"
        };
        let pattern = if fts {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            let escaped = text
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], |r| {
            let snippet: Option<String> = r.get(2)?;
            let content: String = r.get(9)?;
            let title: Option<String> = r.get(3)?;
            let updated_at_ms: i64 = r.get(6)?;
            let role: Option<String> = r.get(7)?;
            let timestamp_ms: Option<i64> = r.get(8)?;
            Ok(SearchHit {
                session_id: r.get(0)?,
                message_id: r.get(1)?,
                session_title: title.unwrap_or_default(),
                provider: ProviderId::parse(&r.get::<_, String>(4)?),
                project_name: r.get(5)?,
                role: role.as_deref().map(Role::parse),
                timestamp_ms: timestamp_ms.unwrap_or(updated_at_ms),
                snippet: snippet
                    .map(|s| collapse_whitespace(&s))
                    .unwrap_or_else(|| like_snippet(&content, text)),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{SESSION_SELECT} WHERE id = ?1"))?;
        Ok(stmt.query_row([id], row_to_session).optional()?)
    }

    /// 按完整 id、id 前缀、native_id 前缀或子代理 agent id 前缀找会话。
    /// 主会话 uuid 前缀同时命中它的子代理（子代理 native_id 以父 uuid 开头）时取主会话；其余多匹配报错。
    pub fn find_session(&self, prefix: &str) -> Result<Option<Session>> {
        if prefix.is_empty() {
            bail!("会话 id 不能为空");
        }
        if let Some(s) = self.get_session(prefix)? {
            return Ok(Some(s));
        }
        let like = format!("{}%", prefix.replace('%', "").replace('_', "\\_"));
        let mut stmt = self.conn.prepare(&format!(
            "{SESSION_SELECT} WHERE id LIKE ?1 ESCAPE '\\' OR native_id LIKE ?1 ESCAPE '\\'
                OR substr(native_id, instr(native_id, '/') + 1) LIKE ?1 ESCAPE '\\' LIMIT 50"
        ))?;
        let found = stmt
            .query_map([&like], row_to_session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mains = found.iter().filter(|s| s.kind == SessionKind::Main).count();
        match (found.len(), mains) {
            (0, _) => Ok(None),
            (1, _) => Ok(found.into_iter().next()),
            (_, 1) => Ok(found.into_iter().find(|s| s.kind == SessionKind::Main)),
            _ => bail!(
                "前缀 {prefix} 匹配到 {} 个会话，请输入更长的 id",
                found.len()
            ),
        }
    }

    /// 某会话的消息，按时间升序。
    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, session_id, native_id, parent_id, role, is_meta, timestamp_ms, model,
                    stop_reason, blocks_json, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, cache_write_5m_tokens, cache_write_1h_tokens, reasoning_tokens
             FROM messages WHERE session_id = ?1 ORDER BY timestamp_ms, rowid",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            let blocks_json: String = r.get(9)?;
            let blocks: Vec<ContentBlock> = serde_json::from_str(&blocks_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let input: Option<i64> = r.get(10)?;
            let usage = input.map(|_| Usage {
                input_tokens: get_u64(r, 10),
                output_tokens: get_u64(r, 11),
                cache_read_tokens: get_u64(r, 12),
                cache_write_tokens: get_u64(r, 13),
                cache_write_5m_tokens: get_u64(r, 14),
                cache_write_1h_tokens: get_u64(r, 15),
                reasoning_tokens: get_u64(r, 16),
                requests: 1,
                input_includes_cache: false,
            });
            Ok(Message {
                id: r.get(0)?,
                session_id: r.get(1)?,
                native_id: r.get(2)?,
                parent_id: r.get(3)?,
                role: Role::parse(&r.get::<_, String>(4)?),
                is_meta: r.get::<_, i64>(5)? != 0,
                timestamp_ms: r.get(6)?,
                model: r.get(7)?,
                stop_reason: r.get(8)?,
                blocks,
                usage,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// (project_name, cwd, 会话数)，按会话数倒序。cwd 为空的归为 "" 一组。
    pub fn projects(&self) -> Result<Vec<(String, String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_name, COALESCE(cwd, ''), COUNT(*) AS n FROM sessions
             GROUP BY cwd ORDER BY n DESC, project_name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, get_u64(r, 2))))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 最近 `days` 天（本地时区）按 天 × provider × 模型 聚合 assistant 用量。
    pub fn stats_daily(&self, days: u32) -> Result<Vec<DailyUsage>> {
        let since_ms = chrono::Local::now().timestamp_millis() - i64::from(days) * 86_400_000;
        let mut stmt = self.conn.prepare(
            "SELECT date(m.timestamp_ms / 1000, 'unixepoch', 'localtime') AS day,
                    s.provider, COALESCE(m.model, ''),
                    SUM(m.input_tokens), SUM(m.output_tokens), SUM(m.cache_read_tokens),
                    SUM(m.cache_write_tokens), SUM(m.cache_write_5m_tokens),
                    SUM(m.cache_write_1h_tokens), SUM(m.reasoning_tokens), COUNT(*),
                    COALESCE(SUM(m.cost_usd), 0)
             FROM messages m JOIN sessions s ON s.id = m.session_id
             WHERE m.input_tokens IS NOT NULL AND m.timestamp_ms >= ?1
             GROUP BY day, s.provider, m.model ORDER BY day DESC, s.provider, m.model",
        )?;
        let rows = stmt.query_map([since_ms], |r| {
            let n = get_u64(r, 10);
            Ok(DailyUsage {
                day: r.get(0)?,
                provider: ProviderId::parse(&r.get::<_, String>(1)?),
                model: r.get(2)?,
                usage: Usage {
                    input_tokens: get_u64(r, 3),
                    output_tokens: get_u64(r, 4),
                    cache_read_tokens: get_u64(r, 5),
                    cache_write_tokens: get_u64(r, 6),
                    cache_write_5m_tokens: get_u64(r, 7),
                    cache_write_1h_tokens: get_u64(r, 8),
                    reasoning_tokens: get_u64(r, 9),
                    requests: n,
                    input_includes_cache: false,
                },
                message_count: n,
                cost_usd: r.get::<_, f64>(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 周期汇总。时间按消息 timestamp（本地时区边界见 `Period::since_ms`）。
    ///
    /// 成本口径：`All` 时优先用会话级真值（Claude cost-state），没有的用估算；
    /// 有真值的主会话，其子代理的估算不再加（cost-state 已含子代理）。
    /// 其余周期只能按消息估算（真值无法切到天）。
    pub fn stats_totals(
        &self,
        period: Period,
        provider: Option<&ProviderId>,
    ) -> Result<UsageTotals> {
        let (filter, args) = period_filter(period, provider);
        let sql = format!(
            "SELECT COALESCE(SUM(m.input_tokens),0), COALESCE(SUM(m.output_tokens),0),
                    COALESCE(SUM(m.cache_read_tokens),0), COALESCE(SUM(m.cache_write_tokens),0),
                    COALESCE(SUM(m.cache_write_5m_tokens),0), COALESCE(SUM(m.cache_write_1h_tokens),0),
                    COALESCE(SUM(m.reasoning_tokens),0),
                    SUM(m.input_tokens IS NOT NULL), MAX(s.input_includes_cache),
                    COUNT(*), COUNT(DISTINCT m.session_id),
                    COALESCE(SUM(m.cost_usd),0), SUM(m.cost_usd IS NOT NULL)
             FROM messages m JOIN sessions s ON s.id = m.session_id WHERE 1 = 1 {filter}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let (usage, messages, sessions, est_cost, priced) =
            stmt.query_row(rusqlite::params_from_iter(args.iter()), |r| {
                let mut u = Usage {
                    input_tokens: get_u64(r, 0),
                    output_tokens: get_u64(r, 1),
                    cache_read_tokens: get_u64(r, 2),
                    cache_write_tokens: get_u64(r, 3),
                    cache_write_5m_tokens: get_u64(r, 4),
                    cache_write_1h_tokens: get_u64(r, 5),
                    reasoning_tokens: get_u64(r, 6),
                    requests: get_u64(r, 7),
                    input_includes_cache: false,
                };
                u.input_includes_cache = get_u64(r, 8) != 0;
                Ok((
                    u,
                    get_u64(r, 9),
                    get_u64(r, 10),
                    r.get::<_, f64>(11)?,
                    get_u64(r, 12),
                ))
            })?;

        let (cost_usd, cost_source) = if period == Period::All {
            let mut sql = String::from(
                "SELECT COALESCE(SUM(CASE WHEN s.recorded_cost_usd IS NOT NULL THEN s.recorded_cost_usd
                                          WHEN p.recorded_cost_usd IS NOT NULL THEN 0
                                          ELSE COALESCE(s.estimated_cost_usd, 0) END), 0),
                        SUM(s.recorded_cost_usd IS NOT NULL),
                        SUM(s.recorded_cost_usd IS NULL AND p.recorded_cost_usd IS NULL
                            AND s.estimated_cost_usd IS NOT NULL)
                 FROM sessions s LEFT JOIN sessions p ON p.id = s.parent_session_id WHERE 1 = 1",
            );
            let mut a: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(p) = provider {
                sql.push_str(" AND s.provider = ?");
                a.push(Box::new(p.as_str().to_string()));
            }
            let (cost, recorded, estimated): (f64, u64, u64) =
                self.conn
                    .query_row(&sql, rusqlite::params_from_iter(a.iter()), |r| {
                        Ok((r.get(0)?, get_u64(r, 1), get_u64(r, 2)))
                    })?;
            let src = match (recorded, estimated) {
                (0, 0) => CostSource::Unknown,
                (_, 0) => CostSource::Recorded,
                (0, _) => CostSource::Estimated,
                _ => CostSource::Mixed,
            };
            (cost, src)
        } else if priced == 0 {
            (0.0, CostSource::Unknown)
        } else {
            (est_cost, CostSource::Estimated)
        };
        Ok(UsageTotals {
            period,
            provider: provider.cloned(),
            usage,
            cost_usd,
            cost_source,
            sessions,
            messages,
        })
    }

    /// 周期内按 provider × 模型 的用量与估算成本，按成本倒序。
    pub fn stats_by_model(&self, period: Period) -> Result<Vec<CostBreakdown>> {
        let (filter, args) = period_filter(period, None);
        let sql = format!(
            "SELECT s.provider, COALESCE(m.model, ''), {USAGE_SUM_COLS}
             FROM messages m JOIN sessions s ON s.id = m.session_id
             WHERE m.input_tokens IS NOT NULL {filter}
             GROUP BY s.provider, m.model ORDER BY cost DESC, total DESC, m.model, s.provider"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_breakdown)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 周期内按 provider × 项目名 的用量与估算成本（同一项目不同来源各一行），按成本倒序取前 `limit`。
    pub fn stats_by_project(&self, period: Period, limit: usize) -> Result<Vec<CostBreakdown>> {
        let (filter, args) = period_filter(period, None);
        let sql = format!(
            "SELECT s.provider, s.project_name, {USAGE_SUM_COLS}
             FROM messages m JOIN sessions s ON s.id = m.session_id
             WHERE m.input_tokens IS NOT NULL {filter}
             GROUP BY s.provider, s.project_name
             ORDER BY cost DESC, total DESC, s.project_name, s.provider LIMIT {}",
            if limit == 0 { -1 } else { limit as i64 }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_breakdown)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 归档健康。`archive_only` 要逐个 stat 原件路径。
    pub fn archive_health(&self) -> Result<Vec<ProviderHealth>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, source_path, source_mtime_ms, source_compressed FROM sessions",
        )?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        const EXPIRE_AFTER_MS: i64 = 23 * 86_400_000;
        let mut out: Vec<ProviderHealth> = Vec::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? != 0,
            ))
        })?;
        for row in rows {
            let (provider, path, mtime_ms, compressed) = row?;
            let provider = ProviderId::parse(&provider);
            let h = match out.iter_mut().find(|h| h.provider == provider) {
                Some(h) => h,
                None => {
                    out.push(ProviderHealth {
                        provider: provider.clone(),
                        sessions: 0,
                        archive_only: 0,
                        expiring_within_7d: 0,
                        compressed: 0,
                    });
                    out.last_mut().expect("刚 push")
                }
            };
            h.sessions += 1;
            let exists = Path::new(&path).exists();
            if !exists {
                h.archive_only += 1;
            } else if provider == ProviderId::ClaudeCode && now_ms - mtime_ms > EXPIRE_AFTER_MS {
                h.expiring_within_7d += 1;
            }
            if compressed {
                h.compressed += 1;
            }
        }
        out.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
        Ok(out)
    }

    /// 各来源最近活动。`detected` 取自 `provider.detect()`，所以要把 provider 传进来；
    /// 库里有数据但未传 provider 的来源也会列出（`detected = false`）。
    pub fn activity(&self, providers: &[&dyn SourceProvider]) -> Result<Vec<ProviderActivity>> {
        let mut ids: Vec<(ProviderId, bool)> =
            providers.iter().map(|p| (p.id(), p.detect())).collect();
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT provider FROM sessions")?;
        for p in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let p = ProviderId::parse(&p?);
            if !ids.iter().any(|(id, _)| *id == p) {
                ids.push((p, false));
            }
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut out = Vec::new();
        for (provider, detected) in ids {
            let latest: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM sessions WHERE provider = ?1 ORDER BY updated_at_ms DESC LIMIT 1",
                    [provider.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let mut last_activity_ms = None;
            let mut last_kind = None;
            if let Some(sid) = latest {
                let row: Option<(i64, String, String)> = self
                    .conn
                    .query_row(
                        "SELECT timestamp_ms, role, blocks_json FROM messages WHERE session_id = ?1
                         ORDER BY timestamp_ms DESC, rowid DESC LIMIT 1",
                        [&sid],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((ts, role, blocks_json)) = row {
                    last_activity_ms = Some(ts);
                    let blocks: Vec<ContentBlock> =
                        serde_json::from_str(&blocks_json).unwrap_or_default();
                    last_kind = Some(activity_kind(Role::parse(&role), &blocks));
                }
            }
            let active_sessions: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE provider = ?1 AND updated_at_ms >= ?2",
                params![provider.as_str(), now_ms - 10 * 60_000],
                |r| r.get(0),
            )?;
            out.push(ProviderActivity {
                provider,
                detected,
                last_activity_ms,
                last_kind,
                active_sessions: active_sessions.max(0) as u32,
            });
        }
        Ok(out)
    }

    /// 上次入库时记录的原件 size / mtime，用于增量跳过。
    pub fn source_state(&self, path: &Path) -> Result<Option<SourceInfo>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT size_bytes, mtime_ms FROM sources WHERE path = ?1")?;
        Ok(stmt
            .query_row([path.to_string_lossy()], |r| {
                Ok(SourceInfo {
                    path: PathBuf::from(path),
                    size_bytes: get_u64(r, 0),
                    mtime_ms: r.get(1)?,
                    compressed: false,
                })
            })
            .optional()?)
    }

    /// (会话数, 消息数)
    pub fn counts(&self) -> Result<(u64, u64)> {
        let s: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let m: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok((s as u64, m as u64))
    }
}

const SESSION_SELECT: &str =
    "SELECT id, provider, native_id, parent_session_id, kind, title, cwd, git_branch, machine_id,
            agent_version, entrypoint, started_at_ms, ended_at_ms, updated_at_ms, models_json,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cache_write_5m_tokens,
            cache_write_1h_tokens, reasoning_tokens, requests, input_includes_cache, message_count,
            source_path, source_size_bytes, source_mtime_ms, recorded_cost_usd, estimated_cost_usd,
            source_compressed
     FROM sessions";

/// 聚合列：7 个 usage 分量 + requests + 消息数 + 估算成本 + 总 token（排序用）。
const USAGE_SUM_COLS: &str =
    "COALESCE(SUM(m.input_tokens),0), COALESCE(SUM(m.output_tokens),0),
     COALESCE(SUM(m.cache_read_tokens),0), COALESCE(SUM(m.cache_write_tokens),0),
     COALESCE(SUM(m.cache_write_5m_tokens),0), COALESCE(SUM(m.cache_write_1h_tokens),0),
     COALESCE(SUM(m.reasoning_tokens),0), COUNT(*), MAX(s.input_includes_cache),
     COALESCE(SUM(m.cost_usd),0) AS cost,
     COALESCE(SUM(m.input_tokens + m.output_tokens + m.cache_read_tokens + m.cache_write_tokens),0) AS total";

/// 周期 + provider 过滤子句（以 ` AND` 开头）及其参数。
fn period_filter(
    period: Period,
    provider: Option<&ProviderId>,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = String::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(since) = period.since_ms() {
        sql.push_str(" AND m.timestamp_ms >= ?");
        args.push(Box::new(since));
    }
    if let Some(p) = provider {
        sql.push_str(" AND s.provider = ?");
        args.push(Box::new(p.as_str().to_string()));
    }
    (sql, args)
}

fn row_to_breakdown(r: &Row) -> rusqlite::Result<CostBreakdown> {
    Ok(CostBreakdown {
        provider: ProviderId::parse(&r.get::<_, String>(0)?),
        key: r.get(1)?,
        usage: Usage {
            input_tokens: get_u64(r, 2),
            output_tokens: get_u64(r, 3),
            cache_read_tokens: get_u64(r, 4),
            cache_write_tokens: get_u64(r, 5),
            cache_write_5m_tokens: get_u64(r, 6),
            cache_write_1h_tokens: get_u64(r, 7),
            reasoning_tokens: get_u64(r, 8),
            requests: get_u64(r, 9),
            input_includes_cache: get_u64(r, 10) != 0,
        },
        cost_usd: r.get::<_, f64>(11)?,
    })
}

/// 从最近一条消息推断活动类型：看角色，再看最后一个内容块。
fn activity_kind(role: Role, blocks: &[ContentBlock]) -> ActivityKind {
    match role {
        Role::User => ActivityKind::UserMessage,
        Role::Tool => ActivityKind::ToolResult,
        Role::System => ActivityKind::SessionEnd,
        Role::Assistant => match blocks.last() {
            Some(ContentBlock::ToolCall { .. }) => ActivityKind::ToolCall,
            Some(ContentBlock::Thinking { .. }) => ActivityKind::Thinking,
            _ => ActivityKind::AssistantText,
        },
    }
}

fn upsert_session_tx(tx: &Connection, s: &Session) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions(id, provider, native_id, parent_session_id, kind, title, cwd, project_name,
             git_branch, machine_id, agent_version, entrypoint, started_at_ms, ended_at_ms, updated_at_ms,
             models_json, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
             cache_write_5m_tokens, cache_write_1h_tokens, reasoning_tokens, requests, input_includes_cache,
             message_count, source_path, source_size_bytes, source_mtime_ms, recorded_cost_usd,
             estimated_cost_usd, source_compressed)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32)
         ON CONFLICT(id) DO UPDATE SET
             provider = excluded.provider, native_id = excluded.native_id,
             parent_session_id = excluded.parent_session_id, kind = excluded.kind, title = excluded.title,
             cwd = excluded.cwd, project_name = excluded.project_name, git_branch = excluded.git_branch,
             machine_id = excluded.machine_id, agent_version = excluded.agent_version,
             entrypoint = excluded.entrypoint, started_at_ms = excluded.started_at_ms,
             ended_at_ms = excluded.ended_at_ms, updated_at_ms = excluded.updated_at_ms,
             models_json = excluded.models_json, input_tokens = excluded.input_tokens,
             output_tokens = excluded.output_tokens, cache_read_tokens = excluded.cache_read_tokens,
             cache_write_tokens = excluded.cache_write_tokens,
             cache_write_5m_tokens = excluded.cache_write_5m_tokens,
             cache_write_1h_tokens = excluded.cache_write_1h_tokens,
             reasoning_tokens = excluded.reasoning_tokens, requests = excluded.requests,
             input_includes_cache = excluded.input_includes_cache, message_count = excluded.message_count,
             source_path = excluded.source_path, source_size_bytes = excluded.source_size_bytes,
             source_mtime_ms = excluded.source_mtime_ms, recorded_cost_usd = excluded.recorded_cost_usd,
             estimated_cost_usd = excluded.estimated_cost_usd,
             source_compressed = excluded.source_compressed",
        params![
            s.id,
            s.provider.as_str(),
            s.native_id,
            s.parent_session_id,
            s.kind.as_str(),
            s.title,
            s.cwd,
            project_name(s.cwd.as_deref()),
            s.git_branch,
            s.machine_id,
            s.agent_version,
            s.entrypoint,
            s.started_at_ms,
            s.ended_at_ms,
            s.updated_at_ms,
            serde_json::to_string(&s.models)?,
            s.usage.input_tokens as i64,
            s.usage.output_tokens as i64,
            s.usage.cache_read_tokens as i64,
            s.usage.cache_write_tokens as i64,
            s.usage.cache_write_5m_tokens as i64,
            s.usage.cache_write_1h_tokens as i64,
            s.usage.reasoning_tokens as i64,
            s.usage.requests as i64,
            s.usage.input_includes_cache as i64,
            i64::from(s.message_count),
            s.source.path.to_string_lossy(),
            s.source.size_bytes as i64,
            s.source.mtime_ms,
            s.recorded_cost_usd,
            s.estimated_cost_usd,
            s.source.compressed as i64,
        ],
    )?;
    // 标题也进全文索引（message_id 为 NULL 的行）。
    tx.execute(
        "DELETE FROM message_fts WHERE session_id = ?1 AND message_id IS NULL",
        [&s.id],
    )?;
    if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
        tx.execute(
            "INSERT INTO message_fts(message_id, session_id, content) VALUES (NULL, ?1, ?2)",
            params![s.id, t],
        )?;
    }
    Ok(())
}

fn replace_messages_tx(tx: &Connection, session_id: &str, messages: &[Message]) -> Result<()> {
    tx.execute("DELETE FROM messages WHERE session_id = ?1", [session_id])?;
    tx.execute(
        "DELETE FROM message_fts WHERE session_id = ?1 AND message_id IS NOT NULL",
        [session_id],
    )?;
    let mut ins = tx.prepare_cached(
        "INSERT INTO messages(id, session_id, native_id, parent_id, role, is_meta, timestamp_ms, model,
             stop_reason, blocks_json, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
             cache_write_5m_tokens, cache_write_1h_tokens, reasoning_tokens, cost_usd)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
    )?;
    let mut fts = tx.prepare_cached(
        "INSERT INTO message_fts(message_id, session_id, content) VALUES (?1, ?2, ?3)",
    )?;
    for m in messages {
        if m.session_id != session_id {
            bail!("消息 {} 的 session_id 与目标会话不一致", m.id);
        }
        let u = m.usage.as_ref();
        let col = |f: fn(&Usage) -> u64| u.map(|u| f(u) as i64);
        ins.execute(params![
            m.id,
            m.session_id,
            m.native_id,
            m.parent_id,
            m.role.as_str(),
            m.is_meta as i64,
            m.timestamp_ms,
            m.model,
            m.stop_reason,
            serde_json::to_string(&m.blocks)?,
            col(|u| u.input_tokens),
            col(|u| u.output_tokens),
            col(|u| u.cache_read_tokens),
            col(|u| u.cache_write_tokens),
            col(|u| u.cache_write_5m_tokens),
            col(|u| u.cache_write_1h_tokens),
            col(|u| u.reasoning_tokens),
            m.model
                .as_deref()
                .zip(u)
                .and_then(|(model, u)| crate::pricing::estimate_usd(model, u)),
        ])?;
        let text = m.text();
        if !text.is_empty() {
            fts.execute(params![m.id, m.session_id, text])?;
        }
    }
    Ok(())
}

/// cwd 最后一段；无 cwd 时为空串。
pub fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|c| c.trim_end_matches('/').rsplit('/').next())
        .unwrap_or("")
        .to_string()
}

fn get_u64(r: &Row, idx: usize) -> u64 {
    r.get::<_, Option<i64>>(idx)
        .ok()
        .flatten()
        .unwrap_or(0)
        .max(0) as u64
}

fn usage_from_row(r: &Row, base: usize) -> rusqlite::Result<Usage> {
    Ok(Usage {
        input_tokens: get_u64(r, base),
        output_tokens: get_u64(r, base + 1),
        cache_read_tokens: get_u64(r, base + 2),
        cache_write_tokens: get_u64(r, base + 3),
        cache_write_5m_tokens: get_u64(r, base + 4),
        cache_write_1h_tokens: get_u64(r, base + 5),
        reasoning_tokens: get_u64(r, base + 6),
        requests: get_u64(r, base + 7),
        input_includes_cache: r.get::<_, i64>(base + 8)? != 0,
    })
}

fn models_from_json(s: &str, idx: usize) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_summary(r: &Row) -> rusqlite::Result<SessionSummary> {
    let title: Option<String> = r.get(3)?;
    // 会话级成本：agent 自记真值优先（agent-reported），否则用入库时的估算列。
    // `estimated_cost_usd` 由 `pricing::estimate_messages` 算出，与该会话 messages.cost_usd 之和同口径
    // （逐条 `estimate_usd`，未知模型跳过，全不可计价为 NULL），直接读列省掉每行一次子查询。
    let recorded: Option<f64> = r.get(21)?;
    let estimated: Option<f64> = r.get(22)?;
    let (cost_usd, cost_source) = match (recorded, estimated) {
        (Some(c), _) => (Some(c), CostSource::Recorded),
        (None, Some(c)) => (Some(c), CostSource::Estimated),
        (None, None) => (None, CostSource::Unknown),
    };
    Ok(SessionSummary {
        id: r.get(0)?,
        provider: ProviderId::parse(&r.get::<_, String>(1)?),
        kind: SessionKind::parse(&r.get::<_, String>(2)?),
        title: title.unwrap_or_default(),
        cwd: r.get(4)?,
        project_name: r.get(5)?,
        started_at_ms: r.get(6)?,
        updated_at_ms: r.get(7)?,
        message_count: r.get::<_, i64>(8)?.max(0) as u32,
        models: models_from_json(&r.get::<_, String>(9)?, 9)?,
        usage: usage_from_row(r, 10)?,
        parent_session_id: r.get(19)?,
        source_path: r.get::<_, Option<String>>(20)?.filter(|p| !p.is_empty()),
        cost_usd,
        cost_source,
    })
}

fn row_to_session(r: &Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        provider: ProviderId::parse(&r.get::<_, String>(1)?),
        native_id: r.get(2)?,
        parent_session_id: r.get(3)?,
        kind: SessionKind::parse(&r.get::<_, String>(4)?),
        title: r.get(5)?,
        cwd: r.get(6)?,
        git_branch: r.get(7)?,
        machine_id: r.get(8)?,
        agent_version: r.get(9)?,
        entrypoint: r.get(10)?,
        started_at_ms: r.get(11)?,
        ended_at_ms: r.get(12)?,
        updated_at_ms: r.get(13)?,
        models: models_from_json(&r.get::<_, String>(14)?, 14)?,
        usage: usage_from_row(r, 15)?,
        message_count: r.get::<_, i64>(24)?.max(0) as u32,
        recorded_cost_usd: r.get(28)?,
        estimated_cost_usd: r.get(29)?,
        source: SourceInfo {
            path: PathBuf::from(r.get::<_, String>(25)?),
            size_bytes: get_u64(r, 26),
            mtime_ms: r.get(27)?,
            compressed: r.get::<_, i64>(30)? != 0,
        },
    })
}

/// 把换行与连续空白压成单个空格，片段在一行里显示。
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// LIKE 路径的片段：找到第一次出现（不区分大小写），前后各取一段并加高亮标记。
fn like_snippet(content: &str, needle: &str) -> String {
    const CONTEXT: usize = 24;
    let chars: Vec<char> = content.chars().collect();
    let lower: Vec<char> = content.to_lowercase().chars().collect();
    let target: Vec<char> = needle.to_lowercase().chars().collect();
    // to_lowercase 可能改变长度（极少数字符）；长度不一致时退化为不区分大小写的近似
    let pos = if lower.len() == chars.len() {
        lower
            .windows(target.len())
            .position(|w| w == target.as_slice())
    } else {
        None
    };
    let Some(start) = pos else {
        return collapse_whitespace(&chars.iter().take(CONTEXT * 2).collect::<String>());
    };
    let end = start + target.len();
    let from = start.saturating_sub(CONTEXT);
    let to = (end + CONTEXT).min(chars.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.extend(&chars[from..start]);
    out.push(SEARCH_MARK_START);
    out.extend(&chars[start..end]);
    out.push(SEARCH_MARK_END);
    out.extend(&chars[end..to]);
    if to < chars.len() {
        out.push('…');
    }
    collapse_whitespace(&out)
}
