//! OpenCode 用量的只读适配（CARM-284 方案单元 5）。目前**只接入用量采集**，不进本地归档。
//!
//! 格式按 OpenCode 1.19.5 实测（CARM-289）：数据在 `$XDG_DATA_HOME/opencode/opencode.db`
//! （默认 `~/.local/share/opencode/opencode.db`），SQLite + WAL。
//! - `message(id, session_id, data)`、`part(id, message_id, session_id, time_created, data)`，`data` 是 JSON 文本。
//! - 每条 `part.data.type = "step-finish"` 是一次落盘的模型请求：`tokens{input, output, reasoning, cache{read, write}}`，
//!   `input` 已扣掉缓存读写，`output` 已扣掉推理。模型取所属 assistant 消息的 `modelID`
//!   （`providerID` 是 OpenCode 的路由 / 网关名，如 `opencode-go`，不代表厂商，不用）。
//!
//! 入库口径同 Gemini CLI：input = 新鲜输入，output = 输出 + 推理（推理按输出计费），reasoning = 推理，
//! cache_read = 缓存读，cache_write = 缓存写（源不分 5m / 1h，投影时整体按 5m，与 `pricing::estimate_usd` 一致）。
//!
//! 读法：只读打开、`readonly_shm=1`，看得到 WAL 里已提交、还没合并回主文件的请求，同时不写源目录
//! （不建 `-wal` / `-shm`，也不往 OpenCode 正在用的 `-shm` 里写读标记）。只取用量需要的字段，
//! 正文、标题、路径都不读出来。
//!
//! 事件键依据：fork 会用新 id 复制整段消息和步骤（时间也重写），但所属消息的 `time.created` 和步骤内容原样保留。
//! 所以依据取「消息创建时间 + 该消息内第几个步骤 + 五项 token」，fork 出来的副本与原请求同键，只计一次；
//! 消息缺创建时间时退回 `part.id`（只保证同一行重读不重复）。

use crate::model::{ProviderId, Usage};
use crate::providers::{Discovery, FileUsage, Parsed, SourceProvider, UsageBasis, UsageEvent};
use anyhow::{Result, bail};
use rusqlite::types::Value;
use std::path::{Path, PathBuf};

const DB_FILE: &str = "opencode.db";

pub struct OpenCodeProvider {
    root: PathBuf,
}

impl OpenCodeProvider {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join(DB_FILE)
    }
}

impl SourceProvider for OpenCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCode
    }

    fn detect(&self) -> bool {
        self.db_path().is_file()
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self) -> Discovery {
        let db = self.db_path();
        Discovery {
            files: if db.is_file() { vec![db] } else { vec![] },
            skipped_superseded: 0,
        }
    }

    fn parse_file(&self, path: &Path) -> Result<Parsed> {
        bail!(
            "OpenCode 目前只接入用量采集，还不进本地归档：{}",
            path.display()
        )
    }

    fn usage_events(&self, path: &Path) -> Result<Option<FileUsage>> {
        read_usage(path).map(Some)
    }

    fn accepts(&self, path: &Path) -> Option<bool> {
        Some(path == self.db_path())
    }

    /// 整库会被反复改写，按版本归档只会无限增长；用量已由本适配读出。
    fn archive_raw(&self, _path: &Path) -> bool {
        false
    }
}

/// 所有完成步骤，按完成时间排序。`k` 是步骤在所属消息里的序号（按 part id，即写入顺序）。
const STEPS_SQL: &str = "
SELECT p.id,
       p.time_created,
       p.session_id = m.session_id,
       json_extract(m.data, '$.role'),
       json_extract(m.data, '$.modelID'),
       json_extract(m.data, '$.time.created'),
       row_number() OVER (PARTITION BY p.message_id ORDER BY p.id),
       json_extract(p.data, '$.tokens') IS NOT NULL,
       json_extract(p.data, '$.tokens.input'),
       json_extract(p.data, '$.tokens.output'),
       json_extract(p.data, '$.tokens.reasoning'),
       json_extract(p.data, '$.tokens.cache.read'),
       json_extract(p.data, '$.tokens.cache.write')
FROM part p
LEFT JOIN message m ON m.id = p.message_id
WHERE json_extract(p.data, '$.type') = 'step-finish'
ORDER BY p.time_created, p.id";

fn read_usage(path: &Path) -> Result<FileUsage> {
    let conn = crate::providers::open_sqlite_readonly(path)?;
    // 一个读事务里读完，OpenCode 同时在写也拿到同一时刻的一致视图。
    let tx = conn.unchecked_transaction()?;
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    {
        let mut stmt = tx.prepare(STEPS_SQL)?;
        let mut rows = stmt.query([])?;
        let mut n = 0usize;
        while let Some(r) = rows.next()? {
            n += 1;
            let part_id: String = r.get(0)?;
            let ts_ms: i64 = r.get::<_, Option<i64>>(1)?.unwrap_or(0);
            let same_session: Option<bool> = r.get(2)?;
            let role: Option<String> = r.get(3)?;
            if role.as_deref() != Some("assistant") || same_session != Some(true) {
                warnings.push(format!(
                    "第 {n} 个完成步骤找不到同会话的 assistant 消息，未计入"
                ));
                continue;
            }
            let model: Option<String> = r.get::<_, Option<String>>(4)?.filter(|m| !m.is_empty());
            let msg_created = token(r.get(5)?);
            let ordinal: i64 = r.get(6)?;
            let has_tokens: bool = r.get(7)?;
            let t: Vec<Option<u64>> = (8..=12)
                .map(|i| r.get::<_, Value>(i).map(token))
                .collect::<rusqlite::Result<_>>()?;
            // 缺单项按 0（OpenCode 缺推理明细时本来就存 0）；有值但不是非负整数的整条不计，报出来。
            if !has_tokens
                || t.iter()
                    .zip(8..)
                    .any(|(v, i)| v.is_none() && !is_null(r, i))
            {
                warnings.push(format!("第 {n} 个完成步骤的 token 不是非负整数，未计入"));
                continue;
            }
            let [input, output, reasoning, cache_read, cache_write] =
                [0, 1, 2, 3, 4].map(|i| t[i].unwrap_or(0));
            let basis = match msg_created {
                Some(created) => UsageBasis::Request(format!(
                    "{created}:{ordinal}:{input}:{output}:{reasoning}:{cache_read}:{cache_write}"
                )),
                None => UsageBasis::Request(part_id),
            };
            events.push(UsageEvent {
                basis,
                ts_ms,
                model,
                usage: Usage {
                    input_tokens: input,
                    output_tokens: output + reasoning,
                    reasoning_tokens: reasoning,
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                    requests: 1,
                    ..Usage::default()
                },
            });
        }
    }
    tx.finish()?;
    Ok(FileUsage {
        native_session_id: DB_FILE.to_string(),
        events,
        warnings,
    })
}

/// 非负整数才算数（JSON 里的 `12.0` 之类也不认，免得静默截断）。
fn token(v: Value) -> Option<u64> {
    match v {
        Value::Integer(n) => u64::try_from(n).ok(),
        _ => None,
    }
}

fn is_null(r: &rusqlite::Row<'_>, i: usize) -> bool {
    matches!(r.get::<_, Value>(i), Ok(Value::Null))
}
