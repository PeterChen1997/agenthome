//! 各 agent 的会话发现与解析。`SourceProvider` 是统一入口，`ingest::ingest_all` 按它调度。

pub mod claude_code;
pub mod codex;
pub mod gemini;

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

/// 本机主机名，作为 `machine_id`。
pub(crate) fn machine_id() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
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

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
