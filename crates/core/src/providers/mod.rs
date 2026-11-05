//! 各 agent 的会话发现与解析。`SourceProvider` 是统一入口，`ingest::ingest_all` 按它调度。

pub mod claude_code;
pub mod codex;

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
    fn discover(&self) -> Discovery;
    fn parse_file(&self, path: &Path) -> Result<Parsed>;
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
