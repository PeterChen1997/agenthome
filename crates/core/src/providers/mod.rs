//! 各 agent 的会话发现与解析。`SourceProvider` 是统一入口，`ingest::ingest_all` 按它调度。

pub mod antigravity_cli;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub(crate) mod proto;
pub mod registry;

use crate::model::{Message, ProviderId, Session};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一个转录文件的解析结果。
#[derive(Debug)]
pub struct Parsed {
    pub session: Session,
    pub messages: Vec<Message>,
    /// 解析失败的行、缺字段等，调用方必须展示或计数，不能丢。
    pub warnings: Vec<String>,
    /// 非对话记录按 `type` 计数（attachment / queue-operation / world_state ...），
    /// 不入库但要让上层知道跳过了什么。
    pub ignored_records: BTreeMap<String, usize>,
}

/// 一次计费请求的用量（远程用量投影用）：只有依据、时间、模型和 token，不带正文。
#[derive(Clone, Debug, PartialEq)]
pub struct UsageEvent {
    pub basis: UsageBasis,
    /// 与本地统计同口径：用量所在消息的时间（UTC 毫秒）。
    pub ts_ms: i64,
    pub model: Option<String>,
    /// 解析器归一后的用量：`input_tokens` 是新鲜输入（`input_includes_cache = false`）。
    pub usage: crate::model::Usage,
}

/// 事件键的依据，见 `usage_v1::EventBasis`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageBasis {
    /// 来源自带的请求标识（Claude `message.id`、Codex `response_id`）。
    Request(String),
    /// 没有请求标识时：本会话内第几条用量（从 0 起，按原件顺序）。改名、压缩、重读都不变。
    Ordinal(u64),
}

/// 一个转录文件的用量投影。
#[derive(Debug)]
pub struct FileUsage {
    /// 会话原生 id（不含主机名），`UsageBasis::Ordinal` 用它区分会话。
    pub native_session_id: String,
    pub events: Vec<UsageEvent>,
    /// 解析警告，调用方要计数或展示，不能丢。
    pub warnings: Vec<String>,
}

/// 发现结果。
#[derive(Debug, Default)]
pub struct Discovery {
    /// 待解析的转录文件（已排序）。
    pub files: Vec<PathBuf>,
    /// 跳过的旧副本数：Claude 的 `.superseded-*` / `.orphaned-*`，
    /// Codex 的「同一会话 `.jsonl` 与 `.jsonl.zst` 并存时的 `.zst`」。
    pub skipped_superseded: usize,
}

/// 一种会话来源。实现只管「找文件」和「解析一个文件」，增量跳过与入库在 `ingest`。
pub trait SourceProvider {
    fn id(&self) -> ProviderId;
    /// 本机是否存在该 agent 的数据目录。
    fn detect(&self) -> bool;
    /// 来源根目录；raw 层用原件相对它的路径作归档 key。
    fn root(&self) -> &Path;
    fn discover(&self) -> Discovery;
    fn parse_file(&self, path: &Path) -> Result<Parsed>;

    /// 只产出用量的路径（远程用量采集用）：复用 `parse_file` 的归一与文件内去重，
    /// 每次计费请求一条。`None` 表示该来源还没接入。
    fn usage_events(&self, _path: &Path) -> Result<Option<FileUsage>> {
        Ok(None)
    }

    /// 能按行续读的来源返回一个「什么都还没喂」的解析器，`ingest` 用它按字节偏移增量入库；
    /// `None` 表示只能整文件解析（`parse_file`）。
    fn line_parser(&self, _path: &Path) -> Result<Option<Box<dyn LineParser>>> {
        Ok(None)
    }

    /// `path` 属于本来源的哪个根目录（有的来源不止一个，如 Codex 的 `archived_sessions`）；
    /// raw 层用它算归档 key，`ingest_paths` / 文件监听用它判断路径归属。
    fn source_root(&self, path: &Path) -> Option<&Path> {
        path.starts_with(self.root()).then(|| self.root())
    }

    /// `path` 是不是 `discover` 会返回的转录文件（文件监听只给出变化的路径，按它决定入不入库）。
    /// `None` 表示判断不了，调用方对整个来源跑一次 `discover`。
    fn accepts(&self, _path: &Path) -> Option<bool> {
        None
    }

    /// 这个原件要不要拷进 raw 层。默认要；解不开、又会被整份改写的文件（如 Antigravity 的
    /// 加密 `.pb`）不拷：每次改写都会另存一个新版本，磁盘只增不减，换不来任何可读内容。
    fn archive_raw(&self, _path: &Path) -> bool {
        true
    }

    /// 文件监听只把这些后缀的文件变化交给入库；锁文件、临时文件、SQLite 的 `-wal` 等不触发，
    /// 否则 agent 活跃时会空转。
    fn watch_suffixes(&self) -> &'static [&'static str] {
        &[".jsonl"]
    }

    /// 要监听的目录。默认只有根目录；不止一个根目录的来源（Codex 的 `archived_sessions`）在这里补上。
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root().to_path_buf()]
    }
}

/// 续读时按消息 id 取已入库的消息（同一 `message.id` 的续行、迟到的 usage 要改它）。
pub type LoadMessage<'a> = dyn FnMut(&str) -> Option<Message> + 'a;

/// 增量解析接不上（要改的旧消息不在库里、usage 口径切换等），调用方改为整文件重解析。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart(pub String);

/// 按行喂入的解析器。整文件解析与增量续读走同一份状态机，保证两条路径的结果一致。
///
/// 解析器只在内存里保留「后续行还可能用到」的状态（已见 uuid、assistant 合并键、会话累计量），
/// 消息正文在 `take_changes` 后交出、不再持有；后续行要改旧消息时用 `LoadMessage` 从库里取。
pub trait LineParser: Send {
    /// 喂一行（不含换行符）。`line_no` 从 1 起，与整文件逐行解析的行号一致（空行也占一个号）。
    fn feed(
        &mut self,
        line_no: usize,
        line: &[u8],
        load: &mut LoadMessage<'_>,
    ) -> Result<(), Restart>;
    /// 交出自上次调用以来新增或改动的消息，顺序与整文件解析的输出一致。
    fn take_changes(&mut self) -> Vec<Message>;
    /// 以目前喂过的全部行为准的会话快照（usage、消息数、标题等都是全量值，不是增量）。
    fn session(&mut self, source: crate::model::SourceInfo) -> Session;
    /// 交出自上次调用以来的警告。
    fn take_warnings(&mut self) -> Vec<String>;
    /// 交出自上次调用以来的非对话记录计数。
    fn take_ignored(&mut self) -> BTreeMap<String, usize>;
}

/// `Usage::add` 的逆运算（model.rs 没有），会话累计量在消息被改时先减旧值再加新值。
pub(crate) fn usage_sub(total: &mut crate::model::Usage, u: &crate::model::Usage) {
    total.input_tokens = total.input_tokens.saturating_sub(u.input_tokens);
    total.output_tokens = total.output_tokens.saturating_sub(u.output_tokens);
    total.cache_read_tokens = total.cache_read_tokens.saturating_sub(u.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_sub(u.cache_write_tokens);
    total.cache_write_5m_tokens = total
        .cache_write_5m_tokens
        .saturating_sub(u.cache_write_5m_tokens);
    total.cache_write_1h_tokens = total
        .cache_write_1h_tokens
        .saturating_sub(u.cache_write_1h_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_sub(u.reasoning_tokens);
    total.requests = total.requests.saturating_sub(u.requests);
}

/// 会话级估算成本的累计：`pricing::estimate_messages` 的增量版本（无可计价消息时为 None）。
#[derive(Debug, Default, Clone)]
pub(crate) struct CostSum {
    priced: usize,
    sum: f64,
}

impl CostSum {
    /// 一条消息从 (旧模型, 旧 usage) 变成 (新模型, 新 usage)；新消息的旧值传 None。
    pub(crate) fn replace(
        &mut self,
        before: (Option<&str>, Option<&crate::model::Usage>),
        after: (Option<&str>, Option<&crate::model::Usage>),
    ) {
        let est = |(model, usage): (Option<&str>, Option<&crate::model::Usage>)| {
            crate::pricing::estimate_usd(model?, usage?)
        };
        if let Some(c) = est(before) {
            self.priced = self.priced.saturating_sub(1);
            self.sum -= c;
        }
        if let Some(c) = est(after) {
            self.priced += 1;
            self.sum += c;
        }
    }

    pub(crate) fn value(&self) -> Option<f64> {
        (self.priced > 0).then_some(self.sum)
    }
}

/// 逐行跑完整个文件（`parse_file` 用）。`stop_on_read_error` 对应各来源原来的读错处理（Claude 继续、Codex 停）。
pub(crate) fn feed_whole(
    parser: &mut dyn LineParser,
    reader: impl std::io::BufRead,
    stop_on_read_error: bool,
) -> Result<Vec<String>> {
    let mut read_errors = Vec::new();
    let mut no_load = |_: &str| None;
    for (idx, line) in reader.split(b'\n').enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                read_errors.push(format!("第 {} 行读取失败: {e}", idx + 1));
                if stop_on_read_error {
                    break;
                }
                continue;
            }
        };
        if let Err(Restart(why)) = parser.feed(idx + 1, &line, &mut no_load) {
            anyhow::bail!("整文件解析不应要求重解析：{why}");
        }
    }
    Ok(read_errors)
}

/// 会话 id 里的 `machine_id` 段：本机身份文件里固定的会话标签，改主机名后不变（`device.rs`）。
pub(crate) fn machine_id() -> String {
    crate::device::local_session_label()
}

/// 读文件元数据生成 `SourceInfo`。
pub(crate) fn source_info(path: &Path, compressed: bool) -> Result<crate::model::SourceInfo> {
    use anyhow::Context;
    let meta = std::fs::metadata(path).with_context(|| format!("读取元数据 {}", path.display()))?;
    Ok(crate::model::SourceInfo {
        path: path.to_path_buf(),
        size_bytes: meta.len(),
        mtime_ms: mtime_ms(&meta),
        compressed,
    })
}

/// 只读打开别的工具的 SQLite（OpenCode、agy 对话库、Antigravity 摘要库），读到已提交的 WAL 又不改源目录里的任何文件：
/// - 有 `-wal` 或 `-shm`（对方在跑，或没正常退出）：`mode=ro&readonly_shm=1`，`-shm` 只读映射，
///   SQLite 在内存里核对 WAL 索引，不写读标记。缺 `-shm` 时会打不开，报错、下一轮重试，不静默少读。
/// - 都没有（对方正常退出时已全部合并进主文件）：`immutable=1`，否则只读连接也会新建 `-shm`。
///   读的瞬间对方恰好启动，新写入先进 `-wal`、主文件不变，下一轮会重读（OpenCode 的 `SourceProgress` 把 `-wal` 算进进度）。
///
/// 注意 SQLite 在同一进程内共用 `-shm` 映射：本进程另有读写连接时 `readonly_shm` 不起作用。AgentHome 不会写别的工具的库。
pub(crate) fn open_sqlite_readonly(path: &Path) -> Result<rusqlite::Connection> {
    use anyhow::Context;
    use rusqlite::OpenFlags;
    let sidecar = |suffix: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        Path::new(&p).exists()
    };
    let mode = if sidecar("-wal") || sidecar("-shm") {
        "mode=ro&readonly_shm=1"
    } else {
        "mode=ro&immutable=1"
    };
    let uri = format!("file:{}?{mode}", uri_path(path)?);
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("只读打开 {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(conn)
}

/// SQLite URI 的路径部分：`?`、`#`、`%` 等要百分号转义。
fn uri_path(path: &Path) -> Result<String> {
    use anyhow::Context;
    let s = path.to_str().context("数据库路径不是 UTF-8")?;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
