//! 扫描 + 解析 + 入库。原件只读；开了 raw 层时先把原件归档（`raw.rs`），再解析进 index 层。
//!
//! 增量规则（append-only 的 jsonl）：
//! - `sources` 表记着每个原件已解析到的字节偏移、行数、首尾指纹与 inode。原件变长、inode 与指纹都没变时，
//!   只读偏移之后新增的完整行，喂给按行解析器（`LineParser`），只 upsert 新增或被续行改动的消息。
//! - 原件变短、inode 变了、已解析部分首尾指纹不符、`.zst` 压缩件、解析器说接不上（`Restart`）时整文件重解析。
//! - 续读状态（已见 uuid、assistant 合并键、会话累计量）在 `IngestCache` 里；缓存没有时（app 刚启动、CLI）
//!   先把偏移之前的部分在内存里重放一遍重建状态，不写库。
//! - 末尾没有换行的一段可能是写了一半的行：是完整 JSON 才消费，否则留到下次。

use crate::model::{ProviderId, SourceInfo};
use crate::providers::claude_code::ClaudeCodeProvider;
use crate::providers::{LineParser, LoadMessage, Restart, SourceProvider};
use crate::raw::{RawArchive, RawReport};
use crate::store::{ResumePoint, SourceRecord, Store, WriteStats};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
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
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// 发现的转录文件数（不含旧副本）。
    pub scanned: usize,
    /// 实际重新解析并入库的文件数（含增量续读的）。
    pub parsed: usize,
    /// 其中按字节偏移增量续读的文件数。
    pub incremental: usize,
    /// 有续读位置却只能整文件重解析的文件与原因（原件被截断 / 替换 / 改写、解析器接不上）。
    pub fallbacks: Vec<(PathBuf, String)>,
    /// 写库改动的行数。
    pub writes: WriteStats,
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
    /// 把另一次 ingest 的报告并进来（含按来源拆开的子报告）。
    pub fn merge_from(&mut self, other: &Report) {
        self.merge(other);
        self.per_provider.extend(
            other
                .per_provider
                .iter()
                .map(|(p, r)| (p.clone(), r.clone())),
        );
    }

    fn merge(&mut self, other: &Report) {
        self.scanned += other.scanned;
        self.parsed += other.parsed;
        self.incremental += other.incremental;
        self.fallbacks.extend(other.fallbacks.iter().cloned());
        self.writes.add(&other.writes);
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

/// 续读状态缓存。app 的同步线程持有一份跨扫描复用；没有缓存时增量照样正确，只是要先重放偏移之前的部分。
///
/// 只缓存最近 [`CACHE_ACTIVE_MS`] 内还在写的原件，最多 [`CACHE_CAPACITY`] 个（按最近使用淘汰），
/// 全量扫描时冷文件不占内存。
#[derive(Default)]
pub struct IngestCache {
    entries: HashMap<PathBuf, CacheEntry>,
    tick: u64,
}

struct CacheEntry {
    parser: Box<dyn LineParser>,
    offset: u64,
    lines: u64,
    session_id: String,
    used: u64,
}

const CACHE_CAPACITY: usize = 32;
const CACHE_ACTIVE_MS: i64 = 6 * 3_600_000;

impl IngestCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 缓存着续读状态的原件数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn take(&mut self, path: &Path) -> Option<CacheEntry> {
        self.entries.remove(path)
    }

    fn put(&mut self, path: &Path, mut entry: CacheEntry, mtime_ms: i64) {
        if chrono::Utc::now().timestamp_millis() - mtime_ms > CACHE_ACTIVE_MS {
            return;
        }
        self.tick += 1;
        entry.used = self.tick;
        self.entries.insert(path.to_path_buf(), entry);
        while self.entries.len() > CACHE_CAPACITY {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(p, _)| p.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
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
    ingest_all_cached(store, providers, raw, &mut IngestCache::new(), progress)
}

/// 同 [`ingest_all`]，续读状态放进调用方持有的 `cache`（app 同步线程用）。
pub fn ingest_all_cached(
    store: &mut Store,
    providers: &[&dyn SourceProvider],
    raw: Option<&RawArchive>,
    cache: &mut IngestCache,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let start = Instant::now();
    let mut total = Report::default();
    for p in providers {
        if !p.detect() {
            continue;
        }
        let r = ingest_one(store, *p, raw, cache, progress);
        total.merge(&r);
        total.per_provider.push((p.id(), r));
    }
    total.elapsed = start.elapsed();
    total
}

/// 只处理给定的路径（文件监听报告的变化），不遍历来源目录。
///
/// 路径按 `SourceProvider::accepts` 过滤（锁文件、旧副本、已删除的文件不入库）；
/// 某来源判断不了（`accepts` 返回 None）时对它退回一次完整的 `discover`。
pub fn ingest_paths(
    store: &mut Store,
    providers: &[&dyn SourceProvider],
    paths: &[PathBuf],
    raw: Option<&RawArchive>,
    cache: &mut IngestCache,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let start = Instant::now();
    let mut total = Report::default();
    for p in providers {
        let mine: BTreeSet<&PathBuf> = paths
            .iter()
            .filter(|x| p.source_root(x).is_some())
            .collect();
        if mine.is_empty() || !p.detect() {
            continue;
        }
        let accepted: Option<Vec<PathBuf>> = mine
            .iter()
            .map(|x| p.accepts(x).map(|ok| ok.then(|| (*x).clone())))
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().flatten().collect());
        let r = match accepted {
            Some(files) => ingest_files(store, *p, files, 0, raw, cache, progress),
            None => ingest_one(store, *p, raw, cache, progress),
        };
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
    ingest_one(store, provider, None, &mut IngestCache::new(), progress)
}

fn ingest_one(
    store: &mut Store,
    provider: &dyn SourceProvider,
    raw: Option<&RawArchive>,
    cache: &mut IngestCache,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let discovery = provider.discover();
    ingest_files(
        store,
        provider,
        discovery.files,
        discovery.skipped_superseded,
        raw,
        cache,
        progress,
    )
}

fn ingest_files(
    store: &mut Store,
    provider: &dyn SourceProvider,
    files: Vec<PathBuf>,
    skipped_superseded: usize,
    raw: Option<&RawArchive>,
    cache: &mut IngestCache,
    progress: &mut dyn FnMut(Progress),
) -> Report {
    let start = Instant::now();
    let mut report = Report {
        scanned: files.len(),
        skipped_superseded,
        ..Report::default()
    };
    let total = files.len();
    let mut seen_ids: HashMap<String, PathBuf> = HashMap::new();

    for (i, path) in files.into_iter().enumerate() {
        // 先归档：与 index 是否已最新无关（老用户的库早已入库，原件却从没拷过），解析失败的原件也要留。
        if let Some(raw) = raw.filter(|_| provider.archive_raw(&path)) {
            let root = provider.source_root(&path).unwrap_or(provider.root());
            match raw.archive(&provider.id(), root, &path) {
                Ok(outcome) => report.raw.record(&outcome),
                Err(e) => report.raw.failed.push((path.clone(), format!("{e:#}"))),
            }
        }
        let mut ctx = Ctx {
            store: &mut *store,
            provider,
            report: &mut report,
            seen_ids: &mut seen_ids,
            cache: &mut *cache,
        };
        if let Err(e) = ctx.process_one(&path) {
            report.failed.push((path.clone(), format!("{e:#}")));
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

struct Ctx<'a> {
    store: &'a mut Store,
    provider: &'a dyn SourceProvider,
    report: &'a mut Report,
    seen_ids: &'a mut HashMap<String, PathBuf>,
    cache: &'a mut IngestCache,
}

/// 续读的结果。
enum Resumed {
    Done,
    /// 接不上，原因记进 `Report::fallbacks` 后整文件重解析。
    Fallback(String),
}

impl Ctx<'_> {
    fn process_one(&mut self, path: &Path) -> anyhow::Result<()> {
        let meta = std::fs::metadata(path)?;
        let mtime_ms = crate::providers::mtime_ms(&meta);
        if self.provider.id() == ProviderId::Codex {
            crate::signals::codex_rollout::note_rollout(self.provider.root(), path, mtime_ms);
        }
        let prev = self.store.source_record(path)?;
        if let Some(prev) = &prev
            && prev.size_bytes == meta.len()
            && prev.mtime_ms == mtime_ms
        {
            self.report.skipped_unchanged += 1;
            return Ok(());
        }
        if is_zst(path) {
            return self.parse_whole_legacy(path);
        }
        if let Some(prev) = prev.as_ref().filter(|p| p.resume.offset > 0) {
            match self.resume(path, &meta, prev)? {
                Resumed::Done => return Ok(()),
                Resumed::Fallback(reason) => {
                    self.report.fallbacks.push((path.to_path_buf(), reason));
                }
            }
        }
        // 走到这里的缓存状态都不能再用了。
        self.cache.take(path);
        match self.provider.line_parser(path)? {
            Some(parser) => self.parse_whole(path, &meta, parser),
            None => self.parse_whole_legacy(path),
        }
    }

    /// 从上次的偏移续读。
    fn resume(
        &mut self,
        path: &Path,
        meta: &std::fs::Metadata,
        prev: &SourceRecord,
    ) -> anyhow::Result<Resumed> {
        let r = &prev.resume;
        let size = meta.len();
        if size < r.offset {
            return Ok(Resumed::Fallback("原件变短了（被截断或替换）".into()));
        }
        if r.inode.is_some() && inode(meta) != r.inode {
            return Ok(Resumed::Fallback("原件被替换（inode 变了）".into()));
        }
        if r.fingerprint.as_deref() != Some(fingerprint(path, r.offset)?.as_str()) {
            return Ok(Resumed::Fallback("已解析部分的首尾内容变了".into()));
        }
        let mut file = File::open(path).with_context(|| format!("打开 {}", path.display()))?;

        let cached = self.cache.take(path).filter(|e| {
            e.offset == r.offset && e.lines == r.lines && e.session_id == prev.session_id
        });
        let mut parser = match cached {
            Some(entry) => entry.parser,
            None => {
                let Some(mut parser) = self.provider.line_parser(path)? else {
                    return Ok(Resumed::Fallback("来源不支持按行续读".into()));
                };
                // 重放偏移之前的部分，只为重建续读状态；消息、警告都已在上次入库时处理过，丢弃。
                let reader = BufReader::with_capacity(1 << 20, (&file).take(r.offset));
                let fed = match feed_lines(reader, parser.as_mut(), 1, &mut |_| None)? {
                    Ok(fed) => fed,
                    Err(Restart(why)) => return Ok(Resumed::Fallback(why)),
                };
                if fed.bytes != r.offset || fed.lines != r.lines {
                    return Ok(Resumed::Fallback(format!(
                        "重放到偏移 {} 时行数 {} 与记录的 {} 不符",
                        fed.bytes, fed.lines, r.lines
                    )));
                }
                parser.take_changes();
                parser.take_warnings();
                parser.take_ignored();
                parser
            }
        };

        // 上次最后一行没有换行（末尾是完整 JSON 就先消费了）：这次开头应是补上的那个换行。
        let mut start = r.offset;
        file.seek(SeekFrom::Start(r.offset - 1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' && size > r.offset {
            let mut next = [0u8; 1];
            file.read_exact(&mut next)?;
            if next[0] != b'\n' {
                return Ok(Resumed::Fallback("上次的末行之后又续写了同一行".into()));
            }
            start += 1;
        }
        file.seek(SeekFrom::Start(start))?;
        let reader = BufReader::with_capacity(1 << 20, (&file).take(size - start));
        let session_id = prev.session_id.clone();
        let store = &*self.store;
        let mut load = |id: &str| store.get_message(&session_id, id).ok().flatten();
        let fed = match feed_lines(reader, parser.as_mut(), r.lines as usize + 1, &mut load)? {
            Ok(fed) => fed,
            Err(Restart(why)) => return Ok(Resumed::Fallback(why)),
        };

        let changes = parser.take_changes();
        let session = parser.session(SourceInfo {
            path: path.to_path_buf(),
            size_bytes: size,
            mtime_ms: crate::providers::mtime_ms(meta),
            compressed: false,
        });
        if session.id != prev.session_id {
            return Ok(Resumed::Fallback(format!(
                "会话 id 从 {} 变成了 {}",
                prev.session_id, session.id
            )));
        }
        self.absorb(
            path,
            &session.id,
            parser.take_warnings(),
            parser.take_ignored(),
        );
        let offset = start + fed.bytes;
        let resume = ResumePoint {
            offset,
            lines: r.lines + fed.lines,
            fingerprint: Some(fingerprint(path, offset)?),
            inode: inode(meta),
        };
        let writes = self.store.store_increment(&session, &changes, &resume)?;
        self.report.writes.add(&writes);
        self.report.parsed += 1;
        self.report.incremental += 1;
        self.cache.put(
            path,
            CacheEntry {
                parser,
                offset,
                lines: resume.lines,
                session_id: session.id,
                used: 0,
            },
            crate::providers::mtime_ms(meta),
        );
        Ok(Resumed::Done)
    }

    /// 整文件解析，记下续读位置。
    fn parse_whole(
        &mut self,
        path: &Path,
        meta: &std::fs::Metadata,
        mut parser: Box<dyn LineParser>,
    ) -> anyhow::Result<()> {
        let size = meta.len();
        let file = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
        let reader = BufReader::with_capacity(1 << 20, (&file).take(size));
        let fed = match feed_lines(reader, parser.as_mut(), 1, &mut |_| None)? {
            Ok(fed) => fed,
            Err(Restart(why)) => anyhow::bail!("整文件解析不应要求重解析：{why}"),
        };
        let messages = parser.take_changes();
        let mut session = parser.session(SourceInfo {
            path: path.to_path_buf(),
            size_bytes: size,
            mtime_ms: crate::providers::mtime_ms(meta),
            compressed: false,
        });
        // 与 `parse_file` 同口径：整文件时按消息逐条求和。
        session.estimated_cost_usd = crate::pricing::estimate_messages(&messages);
        self.absorb(
            path,
            &session.id,
            parser.take_warnings(),
            parser.take_ignored(),
        );
        let resume = ResumePoint {
            offset: fed.bytes,
            lines: fed.lines,
            fingerprint: (fed.bytes > 0)
                .then(|| fingerprint(path, fed.bytes))
                .transpose()?,
            inode: inode(meta),
        };
        let writes = self.store.store_full(&session, &messages, Some(&resume))?;
        self.report.writes.add(&writes);
        self.report.parsed += 1;
        let session_id = session.id;
        self.cache.put(
            path,
            CacheEntry {
                parser,
                offset: resume.offset,
                lines: resume.lines,
                session_id,
                used: 0,
            },
            crate::providers::mtime_ms(meta),
        );
        Ok(())
    }

    /// 只能整文件解析的原件（`.zst`、不支持按行续读的来源）：老路径，不记续读位置。
    fn parse_whole_legacy(&mut self, path: &Path) -> anyhow::Result<()> {
        let parsed = self.provider.parse_file(path)?;
        self.absorb(
            path,
            &parsed.session.id,
            parsed.warnings,
            parsed.ignored_records,
        );
        let writes = self
            .store
            .store_full(&parsed.session, &parsed.messages, None)?;
        self.report.writes.add(&writes);
        self.report.parsed += 1;
        Ok(())
    }

    /// 警告、跳过的记录、同 id 冲突计入报告。
    fn absorb(
        &mut self,
        path: &Path,
        session_id: &str,
        warnings: Vec<String>,
        ignored: BTreeMap<String, usize>,
    ) {
        if let Some(first) = self
            .seen_ids
            .insert(session_id.to_string(), path.to_path_buf())
        {
            self.report.id_collisions.push((path.to_path_buf(), first));
        }
        self.report.warnings += warnings.len();
        for w in &warnings {
            if self.report.warning_samples.len() < WARNING_SAMPLE_LIMIT {
                self.report
                    .warning_samples
                    .push(format!("{}: {w}", path.display()));
            }
        }
        for (k, n) in ignored {
            *self.report.ignored_records.entry(k).or_default() += n;
        }
    }
}

/// 喂了多少。
#[derive(Debug, Default)]
struct Fed {
    bytes: u64,
    lines: u64,
}

/// 逐行喂给解析器，行号从 `first_line` 起。以换行结尾的行都消费；末尾没有换行的一段可能是写了一半的行，
/// 只有是完整 JSON 时才消费（否则留到下次，免得把半行当坏行）。
fn feed_lines(
    mut reader: impl BufRead,
    parser: &mut dyn LineParser,
    first_line: usize,
    load: &mut LoadMessage<'_>,
) -> std::io::Result<Result<Fed, Restart>> {
    let mut fed = Fed::default();
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        let complete = buf.last() == Some(&b'\n');
        let line = if complete { &buf[..n - 1] } else { &buf[..] };
        if !complete
            && (line.iter().all(u8::is_ascii_whitespace)
                || serde_json::from_slice::<serde::de::IgnoredAny>(line).is_err())
        {
            break;
        }
        if let Err(restart) = parser.feed(first_line + fed.lines as usize, line, load) {
            return Ok(Err(restart));
        }
        fed.bytes += n as u64;
        fed.lines += 1;
    }
    Ok(Ok(fed))
}

/// 已解析部分 `[0, offset)` 的首尾各 4 KB 的 sha256（前 16 字节十六进制）。
/// 原件只会在末尾追加；首尾任一处变了就说明被改写或替换过，不能续读。
fn fingerprint(path: &Path, offset: u64) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    const SPAN: u64 = 4096;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let head_len = offset.min(SPAN) as usize;
    let mut buf = vec![0u8; head_len];
    read_exact_at(&mut file, 0, &mut buf)?;
    hasher.update(&buf);
    let tail_start = offset.saturating_sub(SPAN);
    let mut tail = vec![0u8; (offset - tail_start) as usize];
    read_exact_at(&mut file, tail_start, &mut tail)?;
    hasher.update(&tail);
    hasher.update(offset.to_le_bytes());
    let digest = hasher.finalize();
    Ok(digest[..16].iter().map(|b| format!("{b:02x}")).collect())
}

fn read_exact_at(file: &mut File, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(buf)
}

fn is_zst(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}
