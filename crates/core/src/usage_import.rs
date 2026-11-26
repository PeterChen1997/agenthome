//! Mac 端远程用量导入库（CARM-284 方案单元 7）。
//!
//! Mac 经 SSH 从各采集端按游标拉取用量页（`usage_v1::UsagePage`），写进这个库。它和索引库
//! `store.rs` 是两个文件：索引库随时可以删了重建，远端用量不在里面，重建后不受影响。
//!
//! 三张表：
//! - `peers`：配对过的采集端。连接名（用户起的、只在本机）绑定到对方的 `machine_id`；
//!   一台机器只能配对一次，本机不能当远端配对。
//! - `cursors`：每台采集端读到哪：对方用量库的 `store_id` + 已确认的 `seq`。
//! - `usage`：`(machine_id, source, event_key)` 唯一，存这个事件已收到的最新修订。
//!
//! 幂等：同键只接受更大的 `revision`，整行替换；相同或更旧的修订忽略。一页的用量和游标在
//! **同一个事务**里提交，所以重复收到、乱序到达、旧页晚到、提交后回执丢了再重拉，结果都一样，
//! 每次请求只计一次。游标只前移不后退；页的起点超过已确认的游标（中间缺页）整页拒收。
//! 对方 `store_id` 变了（库被重建，`seq` 从头编号）返回 [`StoreReset`]，由完整快照（单元 8）处理。
//!
//! 完整快照（单元 8）：游标失效（`store_id` 变了，或对方从旧备份恢复、高水位比已确认的游标还低）时，
//! 按 [`ImportStore::begin_snapshot`] 固定对方当时的高水位，从 `seq = 0` 分页拉到这个高水位，
//! 页先写进暂存表 `snapshot_usage`，不碰 `usage`；中断了旧统计照常可查，下次从暂存到的位置续拉。
//! 拉够之后 [`ImportStore::finish_snapshot`] 在一个事务里替换这台机器的数据并把游标设到新库，
//! 之后照常增量。快照里没有、导入库里有的键（对方原件已被清理）保留，不以重扫的结果抹掉已发生的花费。

use crate::device::{DeviceIdentity, is_uuid};
use crate::usage_v1::{Op, UsagePage, UsageRecord};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

const SCHEMA_VERSION: i32 = 1;
/// 连接名长度上限。
const MAX_LABEL_LEN: usize = 64;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS peers (
    machine_id   TEXT PRIMARY KEY,
    label        TEXT NOT NULL UNIQUE,
    paired_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS cursors (
    machine_id TEXT PRIMARY KEY REFERENCES peers (machine_id),
    store_id   TEXT NOT NULL,
    seq        INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    machine_id            TEXT NOT NULL,
    source                TEXT NOT NULL,
    event_key             TEXT NOT NULL,
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
    PRIMARY KEY (machine_id, source, event_key)
);
CREATE TABLE IF NOT EXISTS snapshots (
    machine_id     TEXT PRIMARY KEY REFERENCES peers (machine_id),
    store_id       TEXT NOT NULL,
    high_water     INTEGER NOT NULL,
    staged_through INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshot_usage (
    machine_id            TEXT NOT NULL,
    source                TEXT NOT NULL,
    event_key             TEXT NOT NULL,
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
    PRIMARY KEY (machine_id, source, event_key)
);
"#;

/// 用量表的全部列（`usage` 与 `snapshot_usage` 相同）。
const USAGE_COLUMNS: &str = "machine_id, source, event_key, revision, op, vendor, model, ts_ms,
    requests, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
    cache_write_1h_tokens, reasoning_tokens";

/// 一页在采集端的位置：对方用量库 `store_id`，页内变更的 `seq` 落在 `(after, through]`。
/// 由导出端随页给出（单元 9 的传输信封），不进 v1 记录本身。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor {
    pub store_id: String,
    pub after: u64,
    pub through: u64,
}

/// 已确认的读取位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub store_id: String,
    pub seq: u64,
}

/// 对方用量库被重建过（`store_id` 变了），旧游标作废，要走完整快照。可 `downcast_ref` 识别。
#[derive(Debug)]
pub struct StoreReset {
    pub machine_id: String,
}

impl std::fmt::Display for StoreReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "采集端用量库已重建，旧游标作废，需要完整快照")
    }
}

impl std::error::Error for StoreReset {}

/// 进行中的完整快照：对方新库的 `store_id`、开始时固定的高水位、已暂存到的 `seq`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotState {
    pub store_id: String,
    pub high_water: u64,
    pub staged_through: u64,
}

impl SnapshotState {
    /// 已拉到开始时的高水位，可以 [`ImportStore::finish_snapshot`]。
    pub fn complete(&self) -> bool {
        self.staged_through >= self.high_water
    }
}

/// 快照替换的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotOutcome {
    /// 快照里的键数（写进 `usage`）。
    pub applied: usize,
    /// 导入库里有、快照里没有而保留的键（对方原件已清理的历史）。
    pub kept: usize,
    /// 替换后游标的位置（新库的 `seq`）。
    pub cursor_seq: u64,
}

/// 一页里每条记录的去向。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// 新键。
    pub inserted: usize,
    /// 收到更大的修订，整行替换。
    pub replaced: usize,
    /// 修订相同或更旧（重复 / 乱序 / 晚到），忽略。
    pub stale: usize,
    /// 游标是否前移。
    pub advanced: bool,
}

/// 一组用量的合计（撤回的不计）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub events: u64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub reasoning_tokens: u64,
}

pub struct ImportStore {
    conn: Connection,
    local_machine_id: String,
}

impl ImportStore {
    /// 打开（不存在则创建）导入库。`local` 是本机身份，用来拒绝把本机当远端汇入。
    pub fn open(path: &Path, local: &DeviceIdentity) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {}", parent.display()))?;
        }
        let conn = Connection::open(path).context("打开远端用量库")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA busy_timeout = 5000;",
        )?;
        Self::init(conn, local)
    }

    pub fn in_memory(local: &DeviceIdentity) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, local)
    }

    fn init(mut conn: Connection, local: &DeviceIdentity) -> Result<Self> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            bail!(
                "远端用量库版本 {version} 不认识（本程序是 {SCHEMA_VERSION}），请升级 AgentHome；不会删表重建"
            );
        }
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(Self {
            conn,
            local_machine_id: local.machine_id().to_string(),
        })
    }

    /// 配对：把连接名绑定到对方的 `machine_id`。本机、已配对过的机器、已占用的连接名都拒绝。
    pub fn pair(&mut self, label: &str, machine_id: &str, now_ms: i64) -> Result<()> {
        ensure!(is_label(label), "连接名需为 1–{MAX_LABEL_LEN} 个可见字符");
        ensure!(is_uuid(machine_id), "machine_id 不是 UUID");
        ensure!(
            machine_id != self.local_machine_id,
            "这是本机，不能当远端汇入"
        );
        let tx = self.conn.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT label FROM peers WHERE machine_id = ?1",
                [machine_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            bail!("这台机器已配对为「{existing}」，不能重复配对");
        }
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM peers WHERE label = ?1)",
            [label],
            |r| r.get(0),
        )?;
        ensure!(!taken, "连接名「{label}」已被另一台机器使用");
        tx.execute(
            "INSERT INTO peers (machine_id, label, paired_at_ms) VALUES (?1, ?2, ?3)",
            params![machine_id, label, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 连接名绑定的 `machine_id`；没配对过为 `None`。
    pub fn peer(&self, label: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT machine_id FROM peers WHERE label = ?1",
                [label],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 这台采集端已确认的位置；还没导入过为 `None`（从 `seq = 0` 拉起）。
    pub fn cursor(&self, machine_id: &str) -> Result<Option<Cursor>> {
        Ok(self
            .conn
            .query_row(
                "SELECT store_id, seq FROM cursors WHERE machine_id = ?1",
                [machine_id],
                |r| {
                    Ok(Cursor {
                        store_id: r.get(0)?,
                        seq: r.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .optional()?)
    }

    /// 导入从连接 `label` 收到的一页。先整页解码校验（未知字段、超限、机器不符整页拒收），
    /// 再在一个事务里按修订写入用量并前移游标。被拒时什么都不写。
    pub fn import_page(
        &mut self,
        label: &str,
        cursor: &PageCursor,
        bytes: &[u8],
    ) -> Result<ImportStats> {
        let page = self.check_page(label, cursor, bytes)?;
        ensure!(
            self.snapshot(&page.machine_id)?.is_none(),
            "这台机器正在做完整快照，先完成快照再增量导入"
        );

        let tx = self.conn.transaction()?;
        let confirmed = load_cursor(&tx, &page.machine_id)?;
        let advanced = match &confirmed {
            Some(c) if c.store_id != cursor.store_id => {
                return Err(StoreReset {
                    machine_id: page.machine_id,
                }
                .into());
            }
            Some(c) => {
                ensure!(
                    cursor.after <= c.seq,
                    "缺页：已确认到 {}，这一页从 {} 之后开始",
                    c.seq,
                    cursor.after
                );
                cursor.through > c.seq
            }
            None => {
                ensure!(
                    cursor.after == 0,
                    "首次导入要从头拉取，这一页从 {} 之后开始",
                    cursor.after
                );
                true
            }
        };
        let mut stats = ImportStats {
            advanced,
            ..ImportStats::default()
        };
        for record in &page.records {
            apply(&tx, "usage", record, &mut stats)?;
        }
        if advanced {
            tx.execute(
                "INSERT INTO cursors (machine_id, store_id, seq) VALUES (?1, ?2, ?3)
                 ON CONFLICT (machine_id) DO UPDATE SET store_id = excluded.store_id, seq = excluded.seq",
                params![page.machine_id, cursor.store_id, to_i64(cursor.through)?],
            )?;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// 这台采集端的游标是否已失效、要走完整快照。`store_id` / `high_water` 是对方导出时报告的：
    /// 库被重建（`store_id` 变了），或从旧备份恢复（高水位低于已确认的游标，之后新分配的 `seq`
    /// 会落在游标之内、增量永远读不到），都算失效；已有进行中的快照也返回 `true`。
    /// 还没导入过（没有游标）不算失效，从 `seq = 0` 增量拉起即可。
    pub fn needs_snapshot(
        &self,
        machine_id: &str,
        store_id: &str,
        high_water: u64,
    ) -> Result<bool> {
        if self.snapshot(machine_id)?.is_some() {
            return Ok(true);
        }
        Ok(match self.cursor(machine_id)? {
            Some(c) => c.store_id != store_id || c.seq > high_water,
            None => false,
        })
    }

    /// 进行中的快照；没有为 `None`。
    pub fn snapshot(&self, machine_id: &str) -> Result<Option<SnapshotState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT store_id, high_water, staged_through FROM snapshots WHERE machine_id = ?1",
                [machine_id],
                |r| {
                    Ok(SnapshotState {
                        store_id: r.get(0)?,
                        high_water: r.get::<_, i64>(1)? as u64,
                        staged_through: r.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?)
    }

    /// 开始（或续上）连接 `label` 那台机器的完整快照。`store_id` / `high_water` 取对方此刻报告的值，
    /// 高水位开始后固定，快照只需拉到它为止，之后的变更由增量补上。
    /// 已有同一 `store_id` 的快照且对方高水位没有倒退：续上，保留已暂存的页；
    /// 否则（对方又重建 / 又恢复了旧备份）清掉暂存从头来。返回该从哪里拉：`staged_through` 之后。
    pub fn begin_snapshot(
        &mut self,
        label: &str,
        store_id: &str,
        high_water: u64,
    ) -> Result<SnapshotState> {
        let Some(machine_id) = self.peer(label)? else {
            bail!("连接「{label}」还没配对");
        };
        ensure!(is_store_id(store_id), "store_id 不是 32 位 hex");
        if let Some(s) = self.snapshot(&machine_id)?
            && s.store_id == store_id
            && s.staged_through <= high_water
        {
            return Ok(s);
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM snapshot_usage WHERE machine_id = ?1",
            [&machine_id],
        )?;
        tx.execute(
            "INSERT INTO snapshots (machine_id, store_id, high_water, staged_through)
             VALUES (?1, ?2, ?3, 0)
             ON CONFLICT (machine_id) DO UPDATE SET store_id = excluded.store_id,
                 high_water = excluded.high_water, staged_through = 0",
            params![machine_id, store_id, to_i64(high_water)?],
        )?;
        tx.commit()?;
        Ok(SnapshotState {
            store_id: store_id.to_string(),
            high_water,
            staged_through: 0,
        })
    }

    /// 暂存快照的一页。校验和增量导入相同（未知字段、超限、机器不符整页拒收）；另外页必须属于
    /// 快照开始时的那个库（否则 [`StoreReset`]，重新 `begin_snapshot`），且不能跳页。
    /// 页和暂存进度同事务提交，重复、晚到的页按修订忽略。只写暂存表，`usage` 不变。
    pub fn stage_snapshot_page(
        &mut self,
        label: &str,
        cursor: &PageCursor,
        bytes: &[u8],
    ) -> Result<SnapshotState> {
        let page = self.check_page(label, cursor, bytes)?;
        let Some(mut state) = self.snapshot(&page.machine_id)? else {
            bail!("这台机器没有进行中的快照");
        };
        if state.store_id != cursor.store_id {
            return Err(StoreReset {
                machine_id: page.machine_id,
            }
            .into());
        }
        ensure!(
            cursor.after <= state.staged_through,
            "快照缺页：已暂存到 {}，这一页从 {} 之后开始",
            state.staged_through,
            cursor.after
        );
        let tx = self.conn.transaction()?;
        let mut stats = ImportStats::default();
        for record in &page.records {
            apply(&tx, "snapshot_usage", record, &mut stats)?;
        }
        if cursor.through > state.staged_through {
            state.staged_through = cursor.through;
            tx.execute(
                "UPDATE snapshots SET staged_through = ?2 WHERE machine_id = ?1",
                params![page.machine_id, to_i64(cursor.through)?],
            )?;
        }
        tx.commit()?;
        Ok(state)
    }

    /// 快照拉够后，在一个事务里替换这台机器的数据：快照里的键整行覆盖（不比修订，新库的修订从头编号），
    /// 快照里没有的旧键保留并把修订归 0（对方原件回来时新库的任何修订都能覆盖它）；
    /// 游标设到新库的暂存位置，清掉暂存。没拉够就报错，什么都不改。
    pub fn finish_snapshot(&mut self, label: &str) -> Result<SnapshotOutcome> {
        let Some(machine_id) = self.peer(label)? else {
            bail!("连接「{label}」还没配对");
        };
        let Some(state) = self.snapshot(&machine_id)? else {
            bail!("这台机器没有进行中的快照");
        };
        ensure!(
            state.complete(),
            "快照还没拉完：已暂存到 {}，需要到 {}",
            state.staged_through,
            state.high_water
        );
        let tx = self.conn.transaction()?;
        let kept = tx.execute(
            "UPDATE usage SET revision = 0 WHERE machine_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM snapshot_usage s WHERE s.machine_id = usage.machine_id
                     AND s.source = usage.source AND s.event_key = usage.event_key)",
            [&machine_id],
        )?;
        let applied = tx.execute(
            &format!(
                "INSERT INTO usage ({USAGE_COLUMNS})
                 SELECT {USAGE_COLUMNS} FROM snapshot_usage WHERE machine_id = ?1
                 ON CONFLICT (machine_id, source, event_key) DO UPDATE SET {UPDATE_ALL}"
            ),
            [&machine_id],
        )?;
        tx.execute(
            "INSERT INTO cursors (machine_id, store_id, seq) VALUES (?1, ?2, ?3)
             ON CONFLICT (machine_id) DO UPDATE SET store_id = excluded.store_id, seq = excluded.seq",
            params![machine_id, state.store_id, to_i64(state.staged_through)?],
        )?;
        tx.execute(
            "DELETE FROM snapshot_usage WHERE machine_id = ?1",
            [&machine_id],
        )?;
        tx.execute("DELETE FROM snapshots WHERE machine_id = ?1", [&machine_id])?;
        tx.commit()?;
        Ok(SnapshotOutcome {
            applied,
            kept,
            cursor_seq: state.staged_through,
        })
    }

    /// 整页解码并校验：不是本机、连接已配对且页来自配对的那台机器、游标格式有效。
    fn check_page(&self, label: &str, cursor: &PageCursor, bytes: &[u8]) -> Result<UsagePage> {
        let page = UsagePage::decode(bytes)?;
        ensure!(
            page.machine_id != self.local_machine_id,
            "收到本机的用量，拒绝把本机当远端汇入"
        );
        let Some(bound) = self.peer(label)? else {
            bail!("连接「{label}」还没配对");
        };
        ensure!(
            page.machine_id == bound,
            "连接「{label}」送来的不是配对时那台机器的用量"
        );
        ensure!(is_store_id(&cursor.store_id), "store_id 不是 32 位 hex");
        ensure!(cursor.through >= cursor.after, "页的游标区间无效");
        Ok(page)
    }

    /// 合计；`machine_id` 为 `None` 时是全部远端机器。撤回的事件不计。
    pub fn totals(&self, machine_id: Option<&str>) -> Result<Totals> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(requests), 0), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_5m_tokens), 0), COALESCE(SUM(cache_write_1h_tokens), 0),
                    COALESCE(SUM(reasoning_tokens), 0)
             FROM usage WHERE op = 'upsert' AND (?1 IS NULL OR machine_id = ?1)",
            [machine_id],
            |r| {
                let u = |i: usize| r.get::<_, i64>(i).map(|v| v as u64);
                Ok(Totals {
                    events: u(0)?,
                    requests: u(1)?,
                    input_tokens: u(2)?,
                    output_tokens: u(3)?,
                    cache_read_tokens: u(4)?,
                    cache_write_5m_tokens: u(5)?,
                    cache_write_1h_tokens: u(6)?,
                    reasoning_tokens: u(7)?,
                })
            },
        )?)
    }
}

/// 连接名：非空、不超长、没有控制字符。只存在本机，不进协议。
fn is_label(s: &str) -> bool {
    !s.trim().is_empty() && s.chars().count() <= MAX_LABEL_LEN && !s.chars().any(char::is_control)
}

fn is_store_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn to_i64(n: u64) -> Result<i64> {
    i64::try_from(n).context("数值超出 SQLite 整数范围")
}

fn load_cursor(tx: &Transaction<'_>, machine_id: &str) -> Result<Option<Cursor>> {
    Ok(tx
        .query_row(
            "SELECT store_id, seq FROM cursors WHERE machine_id = ?1",
            [machine_id],
            |r| {
                Ok(Cursor {
                    store_id: r.get(0)?,
                    seq: r.get::<_, i64>(1)? as u64,
                })
            },
        )
        .optional()?)
}

/// 冲突时整行覆盖的 `SET` 子句。
const UPDATE_ALL: &str = "revision = excluded.revision, op = excluded.op, vendor = excluded.vendor,
             model = excluded.model, ts_ms = excluded.ts_ms, requests = excluded.requests,
             input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens,
             cache_read_tokens = excluded.cache_read_tokens,
             cache_write_5m_tokens = excluded.cache_write_5m_tokens,
             cache_write_1h_tokens = excluded.cache_write_1h_tokens,
             reasoning_tokens = excluded.reasoning_tokens";

/// 同键只接受更大的修订，整行替换（撤回也是一次修订，用量为 0）。`table` 是 `usage` 或 `snapshot_usage`。
fn apply(
    tx: &Transaction<'_>,
    table: &str,
    r: &UsageRecord,
    stats: &mut ImportStats,
) -> Result<()> {
    let existing: Option<i64> = tx
        .query_row(
            &format!(
                "SELECT revision FROM {table} WHERE machine_id = ?1 AND source = ?2 AND event_key = ?3"
            ),
            params![r.machine_id, r.source, r.event_key],
            |row| row.get(0),
        )
        .optional()?;
    let revision = to_i64(r.revision)?;
    match existing {
        Some(old) if old >= revision => {
            stats.stale += 1;
            return Ok(());
        }
        Some(_) => stats.replaced += 1,
        None => stats.inserted += 1,
    }
    let op = match r.op {
        Op::Upsert => "upsert",
        Op::Retract => "retract",
    };
    tx.execute(
        &format!(
            "INSERT INTO {table} ({USAGE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT (machine_id, source, event_key) DO UPDATE SET {UPDATE_ALL}"
        ),
        params![
            r.machine_id,
            r.source,
            r.event_key,
            revision,
            op,
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
    Ok(())
}
