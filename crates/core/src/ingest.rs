//! 扫描 + 解析 + 入库。原件只读；开了 raw 层时先把原件归档（`raw.rs`），再解析进 index 层。

use crate::model::ProviderId;
use crate::providers::SourceProvider;
use crate::providers::claude_code::ClaudeCodeProvider;
use crate::raw::{RawArchive, RawReport};
use crate::store::Store;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 每处理完一个文件回调一次。
#[derive(Clone, Debug)]
pub struct Progress {
    pub provider: ProviderId,
    pub done: usize,
    pub total: usize,
    pub path: PathBuf,
}

/// 一次 ingest 的结果。任何被跳过的东西都在这里有计数。
#[derive(Debug, Default)]
pub struct Report {
    /// 发现的转录文件数（不含旧副本）。
    pub scanned: usize,
    /// 实际重新解析并入库的文件数。
    pub parsed: usize,
    /// size + mtime 未变而跳过。
    pub skipped_unchanged: usize,
    /// 旧副本：Claude `.superseded-*` / `.orphaned-*`，Codex 与 `.jsonl` 并存的 `.zst`。
    pub skipped_superseded: usize,
    /// 解析或入库失败：(路径, 错误)。
    pub failed: Vec<(PathBuf, String)>,
    /// 各文件 warnings 总条数（坏行、缺字段）。
    pub warnings: usize,
    /// 最近几条 warning 原文（带路径），便于排查；全部保留会太长。
    pub warning_samples: Vec<String>,
    /// 非对话记录按 type 计数，不入库。
    pub ignored_records: BTreeMap<String, usize>,
    /// 同一次扫描里两个文件算出同一个 session id：(后来者路径, 先到者路径)。后来者覆盖先到者。
    pub id_collisions: Vec<(PathBuf, PathBuf)>,
    pub elapsed: Duration,
    /// raw 层归档结果；没开 raw 层时全是 0。
    pub raw: RawReport,
    /// `ingest_all` 时按来源拆开的子报告（子报告自身的 `per_provider` 为空）。
    pub per_provider: Vec<(ProviderId, Report)>,
}

const WARNING_SAMPLE_LIMIT: usize = 20;

impl Report {
    fn merge(&mut self, other: &Report) {
        self.scanned += other.scanned;
        self.parsed += other.parsed;
        self.skipped_unchanged += other.skipped_unchanged;
        self.skipped_superseded += other.skipped_superseded;
        self.failed.extend(other.failed.iter().cloned());
        self.warnings += other.warnings;
        for w in &other.warning_samples {
            if self.warning_samples.len() < WARNING_SAMPLE_LIMIT {
                self.warning_samples.push(w.clone());
            }
        }
        for (k, n) in &other.ignored_records {
            *self.ignored_records.entry(k.clone()).or_default() += n;
        }
        self.id_collisions
            .extend(other.id_collisions.iter().cloned());
        self.raw.merge(&other.raw);
        self.elapsed += other.elapsed;
    }
}

/// 顺序跑每个来源并合并报告。来源目录不存在（`detect() == false`）的跳过，不算失败。
/// `raw` 为 `Some` 时每个原件先归档再入库；归档失败记进 `report.raw.failed`，不挡入库。
pub fn ingest_all(
    store: &mut Store,
    providers: &[&dyn SourceProvider],
    raw: Option<&RawArchive>,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let start = Instant::now();
    let mut total = Report::default();
    for p in providers {
        if !p.detect() {
            continue;
        }
        let r = ingest_one(store, *p, raw, progress);
        total.merge(&r);
        total.per_provider.push((p.id(), r));
    }
    total.elapsed = start.elapsed();
    total
}

/// 只扫 Claude Code（P0 接口，保留作薄封装）。
pub fn ingest_claude(
    store: &mut Store,
    provider: &ClaudeCodeProvider,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    ingest_one(store, provider, None, progress)
}

fn ingest_one(
    store: &mut Store,
    provider: &dyn SourceProvider,
    raw: Option<&RawArchive>,
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
        // 先归档：与 index 是否已最新无关（老用户的库早已入库，原件却从没拷过），解析失败的原件也要留。
        if let Some(raw) = raw {
            match raw.archive(&provider.id(), provider.root(), &path) {
                Ok(outcome) => report.raw.record(&outcome),
                Err(e) => report.raw.failed.push((path.clone(), format!("{e:#}"))),
            }
        }
        match process_one(store, provider, &path, &mut report, &mut seen_ids) {
            Ok(()) => {}
            Err(e) => report.failed.push((path.clone(), format!("{e:#}"))),
        }
        progress(Progress {
            provider: provider.id(),
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
    provider: &dyn SourceProvider,
    path: &std::path::Path,
    report: &mut Report,
    seen_ids: &mut HashMap<String, PathBuf>,
) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path)?;
    let mtime_ms = crate::providers::mtime_ms(&meta);
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
