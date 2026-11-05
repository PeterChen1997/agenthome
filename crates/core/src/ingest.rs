//! 扫描 + 解析 + 入库。P0 只做 index 层，原件只读，不拷贝。

use crate::providers::claude_code::ClaudeCodeProvider;
use crate::store::Store;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 每处理完一个文件回调一次。
#[derive(Clone, Debug)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub path: PathBuf,
}

/// 一次 ingest 的结果。任何被跳过的东西都在这里有计数。
#[derive(Debug, Default)]
pub struct Report {
    /// 发现的转录文件数（不含 superseded/orphaned）。
    pub scanned: usize,
    /// 实际重新解析并入库的文件数。
    pub parsed: usize,
    /// size + mtime 未变而跳过。
    pub skipped_unchanged: usize,
    /// `.superseded-*` / `.orphaned-*` 旧副本。
    pub skipped_superseded: usize,
    /// 解析或入库失败：(路径, 错误)。
    pub failed: Vec<(PathBuf, String)>,
    /// 各文件 warnings 总条数（坏行、缺字段）。
    pub warnings: usize,
    /// 最近几条 warning 原文（带路径），便于排查；全部保留会太长。
    pub warning_samples: Vec<String>,
    /// 非对话记录按 type 计数（attachment / queue-operation ...），P0 不入库。
    pub ignored_records: BTreeMap<String, usize>,
    /// 同一次扫描里两个文件算出同一个 session id：(后来者路径, 先到者路径)。后来者覆盖先到者。
    pub id_collisions: Vec<(PathBuf, PathBuf)>,
    pub elapsed: Duration,
}

const WARNING_SAMPLE_LIMIT: usize = 20;

/// 扫 Claude Code 根目录并入库。未变文件按 `sources` 表 size+mtime 跳过。
pub fn ingest_claude(
    store: &mut Store,
    provider: &ClaudeCodeProvider,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let start = Instant::now();
    let mut report = Report::default();
    let discovery = provider.discover();
    report.scanned = discovery.files.len();
    report.skipped_superseded = discovery.skipped_superseded;
    let total = discovery.files.len();
    let mut seen_ids: HashMap<String, PathBuf> = HashMap::new();

    for (i, path) in discovery.files.into_iter().enumerate() {
        match process_one(store, provider, &path, &mut report, &mut seen_ids) {
            Ok(()) => {}
            Err(e) => report.failed.push((path.clone(), format!("{e:#}"))),
        }
        progress(Progress {
            done: i + 1,
            total,
            path,
        });
    }
    report.elapsed = start.elapsed();
    report
}

fn process_one(
    store: &mut Store,
    provider: &ClaudeCodeProvider,
    path: &std::path::Path,
    report: &mut Report,
    seen_ids: &mut HashMap<String, PathBuf>,
) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path)?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Some(prev) = store.source_state(path)?
        && prev.size_bytes == meta.len()
        && prev.mtime_ms == mtime_ms
    {
        report.skipped_unchanged += 1;
        return Ok(());
    }

    let parsed = provider.parse_file(path)?;
    if let Some(first) = seen_ids.insert(parsed.session.id.clone(), path.to_path_buf()) {
        report.id_collisions.push((path.to_path_buf(), first));
    }
    report.warnings += parsed.warnings.len();
    for w in &parsed.warnings {
        if report.warning_samples.len() < WARNING_SAMPLE_LIMIT {
            report
                .warning_samples
                .push(format!("{}: {w}", path.display()));
        }
    }
    for (k, n) in parsed.ignored_records {
        *report.ignored_records.entry(k).or_default() += n;
    }
    store.store_parsed(&parsed.session, &parsed.messages)?;
    report.parsed += 1;
    Ok(())
}
