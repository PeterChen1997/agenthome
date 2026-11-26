//! 采集端本地用量库（CARM-284 方案单元 6）。
//!
//! 和索引层 `store.rs` 不同，这个库**不能推倒重建**：原日志轮转或被清理后，这里就是那些花费唯一的记录。
//! 所以 schema 版本不认识时报错而不是删表，来源文件消失只标记「不可见」，不删已发生的用量。
//!
//! 三张表：
//! - `usage`：每个 `(source, event_key)` 一行，存它当前的修订（协议 v1 字段，机器只有本机一台）。
//!   每次插入或改动都分配一个新的 `seq`；导出端按 `seq` 递增分页，读到的总是最新修订。
//! - `sources`：每个来源文件读到哪（大小 + 修改时间）、现在还看不看得见。路径只在本机库里，不进协议。
//! - `meta`：本库属于哪台机器（`machine_id`）、已分配到的最大 `seq`（单调递增，永不回退、不复用）、
//!   建库时随机生成的 `store_id`。库丢了重建后 `seq` 会从 1 重来，导入端靠 `store_id` 变了发现旧游标作废。
//!
//! 一致性：一个文件的用量和它的读取进度在**同一个事务**里提交。进程在提交前被杀，进度没前移，
//! 下一轮从头重读这个文件；重读产生的同键同内容记录不分配新 `seq`，所以不重不漏。
//!
//! 修订：同键内容变了（解析更正、同一请求后补的 token）整行替换、`revision + 1`、新 `seq`；
//! 同键但时间更晚的是 fork / resume 复制的旧响应，保留最早的那条（与 `usage_projection::dedup` 同口径）。
//! 显式撤回（`retract`）后该键保持撤回，之后重读到同键也不复活。

use crate::device::{DeviceIdentity, hex};
use crate::model::ProviderId;
use crate::providers::SourceProvider;
use crate::usage_projection;
use crate::usage_v1::{self, Op, UsageRecord};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    source                TEXT NOT NULL,
    event_key             TEXT NOT NULL,
    seq                   INTEGER NOT NULL UNIQUE,
    revision              INTEGER NOT NULL,
    op                    TEXT NOT NULL,
    vendor                TEXT,
    model                 TEXT,
    ts_ms                 INTEGER NOT NULL,
    requests              INTEGER NOT NULL,
    input_tokens          INTEGER NOT NULL,
    output_tokens         INTEGER NOT NULL,
    cache_read_tokens     INTEGER NOT NULL,
    cache_write_5m_tokens INTEGER NOT NULL,
    cache_write_1h_tokens INTEGER NOT NULL,
    reasoning_tokens      INTEGER NOT NULL,
    PRIMARY KEY (source, event_key)
);
CREATE TABLE IF NOT EXISTS sources (
    source   TEXT NOT NULL,
    path     TEXT NOT NULL,
    size     INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    visible  INTEGER NOT NULL,
    PRIMARY KEY (source, path)
);
"#;

/// 一个来源文件读到的位置。来源解析器整文件出用量，所以进度就是「上次读时的大小和修改时间」，
/// 两者都没变就跳过。
///
/// SQLite 来源（OpenCode）新请求先写进旁边的 `-wal`，主文件要等合并才变；所以有 `-wal` 时把它也算进去
/// （大小相加、修改时间取较新），否则 WAL 里的新用量会一直被当成「没变」跳过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceProgress {
    pub size: u64,
    pub mtime_ms: i64,
}

impl SourceProgress {
    pub fn of(path: &Path) -> Result<Self> {
        let mut progress = Self::of_one(&std::fs::metadata(path)?)?;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        if let Ok(meta) = std::fs::metadata(&wal) {
            let w = Self::of_one(&meta)?;
            progress.size += w.size;
            progress.mtime_ms = progress.mtime_ms.max(w.mtime_ms);
        }
        Ok(progress)
    }

    fn of_one(meta: &std::fs::Metadata) -> Result<Self> {
        let mtime_ms = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Ok(Self {
            size: meta.len(),
            mtime_ms,
        })
    }
}

/// 一次提交里每条记录的去向。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CommitStats {
    /// 新键。
    pub inserted: usize,
    /// 同键内容变了，升修订。
    pub revised: usize,
    /// 同键同内容（重读），不分配新 `seq`。
    pub unchanged: usize,
    /// 同键但时间更晚（fork / resume 复制的响应），保留已有的更早那条。
    pub kept_earlier: usize,
    /// 已显式撤回的键又被读到，保持撤回。
    pub kept_retracted: usize,
}

impl CommitStats {
    pub fn add(&mut self, other: CommitStats) {
        self.inserted += other.inserted;
        self.revised += other.revised;
        self.unchanged += other.unchanged;
        self.kept_earlier += other.kept_earlier;
        self.kept_retracted += other.kept_retracted;
    }
}

/// 一个文件采集失败的类别。日志、状态文件和以后上报 Mac 都只用它，不带路径和原始报错
/// （报错里常有路径、甚至行内容）。失败的文件进度不前移，下一轮重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// 发现之后、读之前文件没了（agent 轮转 / 清理），下一轮按不可见处理。
    Vanished,
    PermissionDenied,
    /// 用量库或其所在磁盘写满。
    DiskFull,
    /// 其他读文件的 I/O 错误。
    Io,
    /// 来源还没接入用量投影。
    Unsupported,
    /// 解析 / 投影失败。
    Parse,
    /// 写用量库失败（含记录不合规整笔回滚）。
    Store,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vanished => "vanished",
            Self::PermissionDenied => "permission_denied",
            Self::DiskFull => "disk_full",
            Self::Io => "io",
            Self::Unsupported => "unsupported",
            Self::Parse => "parse",
            Self::Store => "store",
        }
    }

    /// 给错误归类：先看错误链里的 I/O 与 SQLite 错误码（文件没了、无权限、磁盘满），
    /// 再看 `collect_file` 按阶段打的标记；都没有算 `Io`。
    pub fn of(error: &anyhow::Error) -> Self {
        for cause in error.chain() {
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                match io.kind() {
                    std::io::ErrorKind::NotFound => return Self::Vanished,
                    std::io::ErrorKind::PermissionDenied => return Self::PermissionDenied,
                    std::io::ErrorKind::StorageFull => return Self::DiskFull,
                    _ => {}
                }
            }
            if let Some(rusqlite::Error::SqliteFailure(e, _)) =
                cause.downcast_ref::<rusqlite::Error>()
                && e.code == rusqlite::ErrorCode::DiskFull
            {
                return Self::DiskFull;
            }
        }
        // `collect_file` 的阶段标记是最外层的 context，anyhow 能直接取回。
        error.downcast_ref::<Self>().copied().unwrap_or(Self::Io)
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 导出用：一条变更 = 某个键在 `seq` 时刻的最新修订。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub seq: u64,
    pub record: UsageRecord,
}

pub struct UsageStore {
    conn: Connection,
    machine_id: String,
    store_id: String,
}

impl UsageStore {
    /// 打开（不存在则创建）本机用量库。库属于创建它的 `machine_id`：拿别的机器身份打开直接报错，
    /// 防止换了 `device.json` 后把两台机器的用量混在一个库里。
    pub fn open(path: &Path, identity: &DeviceIdentity) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {}", parent.display()))?;
        }
        let conn = Connection::open(path).context("打开用量库")?;
        // synchronous = FULL：这里是部分花费唯一的记录，掉电也不能丢已提交的事务。
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA busy_timeout = 5000;",
        )?;
        Self::init(conn, identity.machine_id())
    }

    pub fn in_memory(identity: &DeviceIdentity) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, identity.machine_id())
    }

    fn init(mut conn: Connection, machine_id: &str) -> Result<Self> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            bail!(
                "用量库版本 {version} 不认识（本程序是 {SCHEMA_VERSION}），请升级 AgentHome；不会删表重建"
            );
        }
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        let owner: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'machine_id'", [], |r| {
                r.get(0)
            })
            .optional()?;
        match owner {
            Some(owner) if owner != machine_id => {
                bail!("用量库属于另一台机器，不能用本机身份写入（迁机请连同 device.json 一起恢复）")
            }
            Some(_) => {}
            None => {
                let mut seed = [0u8; 16];
                getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("取随机数失败：{e}"))?;
                tx.execute(
                    "INSERT INTO meta (key, value)
                     VALUES ('machine_id', ?1), ('last_seq', '0'), ('store_id', ?2)",
                    params![machine_id, hex(&seed)],
                )?;
            }
        }
        let store_id: String =
            tx.query_row("SELECT value FROM meta WHERE key = 'store_id'", [], |r| {
                r.get(0)
            })?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(Self {
            conn,
            machine_id: machine_id.to_string(),
            store_id,
        })
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 建库时随机生成的库标识（32 位 hex）。导出时随游标一起给出：变了说明库被重建过，旧游标作废。
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// 已分配的最大 `seq`（高水位）。没有任何记录时为 0。
    pub fn high_water(&self) -> Result<u64> {
        last_seq(&self.conn)
    }

    /// 上次提交时这个文件的进度；从没提交过为 `None`。
    pub fn progress(&self, source: &ProviderId, path: &Path) -> Result<Option<SourceProgress>> {
        Ok(self
            .conn
            .query_row(
                "SELECT size, mtime_ms FROM sources WHERE source = ?1 AND path = ?2",
                params![source.as_str(), path_key(path)?],
                |r| {
                    Ok(SourceProgress {
                        size: r.get::<_, i64>(0)? as u64,
                        mtime_ms: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// 在一个事务里写入一个文件的全部用量和它的读取进度。任何一条不合规整笔回滚，进度不前移。
    pub fn commit_file(
        &mut self,
        source: &ProviderId,
        path: &Path,
        progress: SourceProgress,
        records: &[UsageRecord],
    ) -> Result<CommitStats> {
        let key = path_key(path)?;
        let tx = self.conn.transaction()?;
        let mut stats = CommitStats::default();
        for r in records {
            r.validate()?;
            ensure!(r.machine_id == self.machine_id, "记录不属于本机");
            ensure!(r.source == source.as_str(), "记录来源与文件来源不一致");
            stats.add(upsert(&tx, r)?);
        }
        tx.execute(
            "INSERT INTO sources (source, path, size, mtime_ms, visible) VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT (source, path) DO UPDATE SET
                 size = excluded.size, mtime_ms = excluded.mtime_ms, visible = 1",
            params![
                source.as_str(),
                key,
                i64::try_from(progress.size)?,
                progress.mtime_ms
            ],
        )?;
        tx.commit()?;
        Ok(stats)
    }

    /// 按这一轮发现的文件更新可见性：库里有、这轮没发现的标成不可见（原日志轮转或被清理），
    /// 重新出现的恢复可见。**不动用量**：已发生的花费保留，也不产生撤回。返回 `(隐藏, 恢复)` 个数。
    pub fn refresh_visibility(
        &mut self,
        source: &ProviderId,
        present: &[PathBuf],
    ) -> Result<(usize, usize)> {
        let present: HashSet<String> =
            present.iter().map(|p| path_key(p)).collect::<Result<_>>()?;
        let tx = self.conn.transaction()?;
        let rows: Vec<(String, bool)> = {
            let mut stmt = tx.prepare("SELECT path, visible FROM sources WHERE source = ?1")?;
            stmt.query_map([source.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let (mut hidden, mut revived) = (0, 0);
        for (path, visible) in rows {
            let now = present.contains(&path);
            if now != visible {
                tx.execute(
                    "UPDATE sources SET visible = ?3 WHERE source = ?1 AND path = ?2",
                    params![source.as_str(), path, now],
                )?;
                if now {
                    revived += 1;
                } else {
                    hidden += 1;
                }
            }
        }
        tx.commit()?;
        Ok((hidden, revived))
    }

    /// 不可见（原件已不在）的来源文件数。
    pub fn invisible_sources(&self) -> Result<usize> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM sources WHERE visible = 0", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
    }

    /// 显式撤回一条（已确认的解析更正）：升修订、用量清零、分配新 `seq`，导入端据此扣掉。
    /// 键不存在或已撤回返回 `false`。
    pub fn retract(&mut self, source: &ProviderId, event_key: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let Some(existing) = load(&tx, source.as_str(), event_key)? else {
            return Ok(false);
        };
        if existing.op == Op::Retract {
            return Ok(false);
        }
        let seq = next_seq(&tx)?;
        tx.execute(
            "UPDATE usage SET seq = ?3, revision = revision + 1, op = 'retract', requests = 0,
                 input_tokens = 0, output_tokens = 0, cache_read_tokens = 0,
                 cache_write_5m_tokens = 0, cache_write_1h_tokens = 0, reasoning_tokens = 0
             WHERE source = ?1 AND event_key = ?2",
            params![source.as_str(), event_key, to_i64(seq)?],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// `seq > after` 的变更，按 `seq` 升序，最多 `limit` 条。每个键只出现一次（它的最新修订）。
    pub fn changes_since(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, source, event_key, revision, op, vendor, model, ts_ms, requests,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
                    cache_write_1h_tokens, reasoning_tokens
             FROM usage WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![to_i64(after)?, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    row_record(r, 1, &self.machine_id)?,
                ))
            },
        )?;
        rows.map(|row| {
            let (seq, record) = row?;
            Ok(Change { seq, record })
        })
        .collect()
    }

    /// `[from_ms, to_ms)` 内仍计入的用量（撤回的不算），供按机器统计（`usage_report`）。
    pub fn records_in(&self, from_ms: i64, to_ms: i64) -> Result<Vec<UsageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, event_key, revision, op, vendor, model, ts_ms, requests,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
                    cache_write_1h_tokens, reasoning_tokens
             FROM usage WHERE op = 'upsert' AND ts_ms >= ?1 AND ts_ms < ?2",
        )?;
        let rows = stmt.query_map(params![from_ms, to_ms], |r| {
            row_record(r, 0, &self.machine_id)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 把 WAL 合并回主库并截断（`wal_checkpoint(TRUNCATE)`），状态目录接近预算时先腾出 WAL 占的空间。
    /// 有读者占着时合并不完整，不报错，按实际剩下的大小算。
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// 运行中做一致的整库备份（`VACUUM INTO`）。`dest` 必须不存在。
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        ensure!(!dest.exists(), "备份目标已存在");
        let dest = dest.to_str().context("备份路径不是 UTF-8")?;
        self.conn.execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }
}

/// 一个文件这一轮的采集结果。
#[derive(Debug, Default)]
pub struct FileOutcome {
    /// 大小和修改时间都没变，没读。
    pub skipped: bool,
    pub stats: CommitStats,
    /// 解析与投影的警告，调用方要计数或展示。
    pub warnings: Vec<String>,
}

/// 增量采集一个文件：进度没变就跳过，否则整文件投影、文件内去重后与进度一起提交。
///
/// 先取进度再解析：解析期间文件又被追加，记下的是旧进度，下一轮会再读一次，不会漏。
/// 出错时按阶段带上 [`FailureKind`]，调用方用 `FailureKind::of` 归类。
pub fn collect_file(
    store: &mut UsageStore,
    identity: &DeviceIdentity,
    provider: &dyn SourceProvider,
    path: &Path,
) -> Result<FileOutcome> {
    let source = provider.id();
    let progress = SourceProgress::of(path).context(FailureKind::Io)?;
    if store.progress(&source, path).context(FailureKind::Store)? == Some(progress) {
        return Ok(FileOutcome {
            skipped: true,
            ..FileOutcome::default()
        });
    }
    let Some(file) = provider.usage_events(path).context(FailureKind::Parse)? else {
        return Err(anyhow::anyhow!("来源 {} 还没接入用量投影", source.as_str())
            .context(FailureKind::Unsupported));
    };
    let (records, mut warnings) = usage_projection::records(identity, &source, &file);
    warnings.splice(0..0, file.warnings);
    let records = usage_projection::dedup(records);
    let stats = store
        .commit_file(&source, path, progress, &records)
        .context(FailureKind::Store)?;
    Ok(FileOutcome {
        skipped: false,
        stats,
        warnings,
    })
}

/// 一个来源这一轮的采集汇总。
#[derive(Debug, Default)]
pub struct SourceReport {
    pub files: usize,
    pub skipped: usize,
    pub stats: CommitStats,
    pub warnings: usize,
    /// 读失败的文件按类别计数（不含路径）；失败的文件进度不前移，下一轮重试。
    pub failures: BTreeMap<FailureKind, usize>,
    pub hidden: usize,
    pub revived: usize,
}

/// 采集一个来源：发现文件、逐个 `collect_file`、更新可见性。单个文件失败按类别计入 `failures` 继续下一个。
/// 不限流、不看磁盘预算；采集服务用 `collector::Collector`（分片、限速、状态目录预算）。
pub fn collect_source(
    store: &mut UsageStore,
    identity: &DeviceIdentity,
    provider: &dyn SourceProvider,
) -> Result<SourceReport> {
    let discovery = provider.discover();
    let mut report = SourceReport {
        files: discovery.files.len(),
        ..SourceReport::default()
    };
    for path in &discovery.files {
        match collect_file(store, identity, provider, path) {
            Ok(outcome) => {
                report.skipped += usize::from(outcome.skipped);
                report.stats.add(outcome.stats);
                report.warnings += outcome.warnings.len();
            }
            Err(e) => *report.failures.entry(FailureKind::of(&e)).or_default() += 1,
        }
    }
    let (hidden, revived) = store.refresh_visibility(&provider.id(), &discovery.files)?;
    report.hidden = hidden;
    report.revived = revived;
    Ok(report)
}

impl SourceReport {
    /// 失败的文件总数。
    pub fn failed(&self) -> usize {
        self.failures.values().sum()
    }
}

fn path_key(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .context("来源路径不是 UTF-8")
}

fn to_i64(n: u64) -> Result<i64> {
    i64::try_from(n).context("数值超出 SQLite 整数范围")
}

fn last_seq(conn: &Connection) -> Result<u64> {
    let v: String = conn.query_row("SELECT value FROM meta WHERE key = 'last_seq'", [], |r| {
        r.get(0)
    })?;
    v.parse().context("meta.last_seq 损坏")
}

fn next_seq(tx: &Transaction<'_>) -> Result<u64> {
    let seq = last_seq(tx)? + 1;
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'last_seq'",
        [seq.to_string()],
    )?;
    Ok(seq)
}

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Upsert => "upsert",
        Op::Retract => "retract",
    }
}

pub(crate) fn row_record(
    r: &rusqlite::Row<'_>,
    at: usize,
    machine_id: &str,
) -> rusqlite::Result<UsageRecord> {
    let op = match r.get::<_, String>(at + 3)?.as_str() {
        "retract" => Op::Retract,
        _ => Op::Upsert,
    };
    let u = |i: usize| r.get::<_, i64>(at + i).map(|v| v as u64);
    Ok(UsageRecord {
        v: usage_v1::VERSION,
        source: r.get(at)?,
        event_key: r.get(at + 1)?,
        revision: u(2)?,
        op,
        machine_id: machine_id.to_string(),
        vendor: r.get(at + 4)?,
        model: r.get(at + 5)?,
        ts_ms: r.get(at + 6)?,
        requests: r.get(at + 7)?,
        input_tokens: u(8)?,
        output_tokens: u(9)?,
        cache_read_tokens: u(10)?,
        cache_write_5m_tokens: u(11)?,
        cache_write_1h_tokens: u(12)?,
        reasoning_tokens: u(13)?,
    })
}

fn load(tx: &Transaction<'_>, source: &str, event_key: &str) -> Result<Option<UsageRecord>> {
    Ok(tx
        .query_row(
            "SELECT source, event_key, revision, op, vendor, model, ts_ms, requests,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
                    cache_write_1h_tokens, reasoning_tokens
             FROM usage WHERE source = ?1 AND event_key = ?2",
            params![source, event_key],
            |r| row_record(r, 0, ""),
        )
        .optional()?)
}

/// 除协议字段（键、修订、操作）外内容都相同。
fn same_usage(a: &UsageRecord, b: &UsageRecord) -> bool {
    a.vendor == b.vendor
        && a.model == b.model
        && a.ts_ms == b.ts_ms
        && a.requests == b.requests
        && a.input_tokens == b.input_tokens
        && a.output_tokens == b.output_tokens
        && a.cache_read_tokens == b.cache_read_tokens
        && a.cache_write_5m_tokens == b.cache_write_5m_tokens
        && a.cache_write_1h_tokens == b.cache_write_1h_tokens
        && a.reasoning_tokens == b.reasoning_tokens
}

/// 写入一条（修订号、操作由库决定，忽略传入的 `revision` / `op`）。
fn upsert(tx: &Transaction<'_>, r: &UsageRecord) -> Result<CommitStats> {
    let mut stats = CommitStats::default();
    let revision = match load(tx, &r.source, &r.event_key)? {
        None => {
            stats.inserted = 1;
            1
        }
        Some(old) if old.op == Op::Retract => {
            stats.kept_retracted = 1;
            return Ok(stats);
        }
        Some(old) if r.ts_ms > old.ts_ms => {
            stats.kept_earlier = 1;
            return Ok(stats);
        }
        Some(old) if same_usage(&old, r) => {
            stats.unchanged = 1;
            return Ok(stats);
        }
        Some(old) => {
            stats.revised = 1;
            old.revision + 1
        }
    };
    let seq = next_seq(tx)?;
    tx.execute(
        "INSERT INTO usage (source, event_key, seq, revision, op, vendor, model, ts_ms, requests,
             input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
             cache_write_1h_tokens, reasoning_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (source, event_key) DO UPDATE SET
             seq = excluded.seq, revision = excluded.revision, op = excluded.op,
             vendor = excluded.vendor, model = excluded.model, ts_ms = excluded.ts_ms,
             requests = excluded.requests, input_tokens = excluded.input_tokens,
             output_tokens = excluded.output_tokens, cache_read_tokens = excluded.cache_read_tokens,
             cache_write_5m_tokens = excluded.cache_write_5m_tokens,
             cache_write_1h_tokens = excluded.cache_write_1h_tokens,
             reasoning_tokens = excluded.reasoning_tokens",
        params![
            r.source,
            r.event_key,
            to_i64(seq)?,
            to_i64(revision)?,
            op_str(Op::Upsert),
            r.vendor,
            r.model,
            r.ts_ms,
            r.requests,
            to_i64(r.input_tokens)?,
            to_i64(r.output_tokens)?,
            to_i64(r.cache_read_tokens)?,
            to_i64(r.cache_write_5m_tokens)?,
            to_i64(r.cache_write_1h_tokens)?,
            to_i64(r.reasoning_tokens)?,
        ],
    )?;
    Ok(stats)
}
