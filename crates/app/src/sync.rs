//! 实时数据通道（落地清单 10.10 / 10.11 / 10.12）：app 自己把来源目录的新内容入库，
//! 不再依赖手动 `agenthome scan`。
//!
//! - 启动即增量扫描一次（10.11）；索引库不存在时这一次就是首次全量建库。
//! - notify 监听各来源根目录（10.10），只处理变了的那几个文件（`ingest_paths`，按字节偏移增量续读），
//!   不为每次变化遍历来源目录。合并策略见 [`QUIET`] / [`MAX_DELAY`]。
//! - 每 [`FALLBACK_SCAN`] 兜底扫一次全部来源（10.12），监听丢事件也能追上；监听报错时立即补一次全量。
//!
//! 每个原件先归档到 raw 层（10.1–10.3，`agenthome_core::raw`）再入库，归档失败不挡入库但会展示。
//!
//! 入库在独立线程上跑，持有自己的 `Store` 连接（WAL 下与 UI 的只读查询互不阻塞）。
//! 线程把状态写进共享快照，前台每 [`UI_POLL`] 看一眼版本号，有变化才写回 `SyncState` 并重绘。

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agenthome_core::model::ProviderId;
use agenthome_core::{
    ClaudeCodeProvider, CodexProvider, GeminiProvider, IngestCache, RawArchive, RawStats, Report,
    SourceProvider, Store, WatchEvent, ingest_all_cached, ingest_paths, paths, watch_roots,
};
use gpui_kit::{App, AsyncApp, Entity, Task};

use crate::format::now_ms;

/// 合并文件变化：首个事件后至少等这么久，期间安静了就入库。活跃会话每秒都在追加，没必要每行都入一次库。
const QUIET: Duration = Duration::from_secs(2);
/// 持续写入时最多攒这么久就入库一次。原来的「安静 2 秒才入库」在 agent 连续输出时会一直等下去，
/// 界面迟迟不更新；增量入库每次只写新增的几行，10 秒一次的代价很小。
const MAX_DELAY: Duration = Duration::from_secs(10);
/// 兜底全量扫描间隔。
const FALLBACK_SCAN: Duration = Duration::from_secs(3_600);
/// 后台跑官方 `claude /usage` 刷新 Claude 订阅额度的间隔（每次约 10 秒、不耗 token）。
const CLAUDE_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// 前台检查后台快照的间隔。
const UI_POLL: Duration = Duration::from_millis(500);

/// 发给入库线程的指令。
#[derive(Clone, Debug)]
enum Command {
    /// 某来源目录下这些文件有变化（来自文件监听）。
    Changed(ProviderId, Vec<PathBuf>),
    /// 扫全部来源（设置页「立即扫描」、监听报错后的补救）。
    ScanAll,
}

/// 正在进行的扫描。
#[derive(Clone, Debug, PartialEq)]
pub struct ScanProgress {
    pub provider: ProviderId,
    pub done: usize,
    pub total: usize,
}

/// 单个来源的同步状况，供标题栏与设置页展示。
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSync {
    pub provider: ProviderId,
    pub root: PathBuf,
    /// 根目录存在（该 agent 装过）。
    pub detected: bool,
    /// 文件监听已挂上。
    pub watching: bool,
    /// 没挂上监听的原因（目录不存在、FSEvents 报错）。
    pub watch_note: Option<String>,
    /// 最近一次扫描完成的时刻（Unix 毫秒）。
    pub last_scan_ms: Option<i64>,
    /// 最近一次扫描：发现文件数 / 重新解析数 / 失败数 / 解析警告数。
    pub files: usize,
    pub parsed: usize,
    pub failed: usize,
    pub warnings: usize,
    /// 最近一次失败的路径与原因，展示用。
    pub last_failure: Option<String>,
}

/// 后台 `/usage` 刷新 Claude 额度的状况。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeProbe {
    /// 找到的官方 claude 可执行文件；None 表示没装或不在常见位置。
    pub binary: Option<PathBuf>,
    pub running: bool,
    /// 最近一次成功的时刻（Unix 毫秒）。
    pub last_ok_ms: Option<i64>,
    /// 最近一次失败原因；成功后清空。
    pub last_error: Option<String>,
}

/// raw 层原件归档的状况。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawSync {
    pub root: Option<PathBuf>,
    /// 打开 raw 层失败的原因；此时只入索引、不归档。
    pub open_error: Option<String>,
    /// 最近一次统计（每次有写入后重算）。
    pub stats: Option<RawStats>,
    /// 最近一次扫描里归档失败的文件数与最后一条原因。
    pub failed: usize,
    pub last_failure: Option<String>,
}

/// 同步状态快照。`SyncState` 实体里放的就是它。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncState {
    pub scanning: Option<ScanProgress>,
    pub providers: Vec<ProviderSync>,
    /// 最近一次任何扫描完成的时刻。
    pub last_sync_ms: Option<i64>,
    /// 最近一次全部来源扫描完成的时刻。
    pub last_full_scan_ms: Option<i64>,
    /// 每次有文件被重新解析入库就加一；页面据此重拉数据。
    pub data_revision: u64,
    /// 打开索引库失败、监听后端报错等；UI 以文字展示。
    pub error: Option<String>,
    pub claude_probe: ClaudeProbe,
    pub raw: RawSync,
    /// 内部版本号：快照任何字段变化都加一，前台据此判断要不要重绘。
    version: u64,
}

impl SyncState {
    pub fn watching_count(&self) -> usize {
        self.providers.iter().filter(|p| p.watching).count()
    }

    fn provider_mut(&mut self, id: &ProviderId) -> Option<&mut ProviderSync> {
        self.providers.iter_mut().find(|p| &p.provider == id)
    }

    fn touch(&mut self) {
        self.version += 1;
    }
}

/// 同步引擎句柄。drop 后前台不再接收状态；入库线程随进程退出。
pub struct SyncEngine {
    commands: Sender<Command>,
    probe: Sender<bool>,
    _poll: Task<()>,
}

/// 发指令用的轻量句柄，设置页持有一份。
#[derive(Clone)]
pub struct SyncHandle {
    commands: Sender<Command>,
    probe: Sender<bool>,
}

impl SyncHandle {
    /// 立即扫描全部来源（设置页「立即扫描」）。
    pub fn scan_all(&self) {
        let _ = self.commands.send(Command::ScanAll);
    }

    /// 立即跑一次官方 `/usage` 刷新 Claude 额度（不等 15 分钟的定时）。
    pub fn refresh_claude_quota(&self) {
        let _ = self.probe.send(true);
    }
}

impl SyncEngine {
    /// 启动入库线程与文件监听，状态写进 `state`。
    pub fn start(db_path: PathBuf, state: Entity<SyncState>, cx: &mut App) -> Self {
        let roots: Vec<(ProviderId, PathBuf)> = vec![
            (ProviderId::ClaudeCode, paths::claude_projects_dir()),
            (ProviderId::Codex, paths::codex_sessions_dir()),
            (ProviderId::GeminiCli, paths::gemini_dir()),
        ];
        let shared = Arc::new(Mutex::new(SyncState {
            providers: roots
                .iter()
                .map(|(provider, root)| ProviderSync {
                    provider: provider.clone(),
                    root: root.clone(),
                    detected: root.is_dir(),
                    watching: false,
                    watch_note: None,
                    last_scan_ms: None,
                    files: 0,
                    parsed: 0,
                    failed: 0,
                    warnings: 0,
                    last_failure: None,
                })
                .collect(),
            ..SyncState::default()
        }));
        let (commands, receiver) = mpsc::channel::<Command>();

        let worker_shared = shared.clone();
        let watch_sender = commands.clone();
        std::thread::Builder::new()
            .name("agenthome-sync".into())
            .spawn(move || run_worker(db_path, roots, worker_shared, watch_sender, receiver))
            .expect("启动同步线程失败");

        let (probe, probe_receiver) = mpsc::channel::<bool>();
        let probe_shared = shared.clone();
        std::thread::Builder::new()
            .name("agenthome-claude-usage".into())
            .spawn(move || run_claude_probe(probe_shared, probe_receiver))
            .expect("启动 Claude 额度线程失败");

        // 弱引用：外壳释放状态实体后轮询自行退出。
        let state = state.downgrade();
        let poll = cx.spawn(async move |cx: &mut AsyncApp| {
            let mut seen = u64::MAX;
            loop {
                let snapshot = shared.lock().ok().map(|s| s.clone());
                if let Some(snapshot) = snapshot
                    && snapshot.version != seen
                {
                    seen = snapshot.version;
                    let applied = state.update(cx, |state, cx| {
                        *state = snapshot;
                        cx.notify();
                    });
                    if applied.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(UI_POLL).await;
            }
        });
        Self {
            commands,
            probe,
            _poll: poll,
        }
    }

    pub fn handle(&self) -> SyncHandle {
        SyncHandle {
            commands: self.commands.clone(),
            probe: self.probe.clone(),
        }
    }
}

fn update(shared: &Mutex<SyncState>, f: impl FnOnce(&mut SyncState)) {
    if let Ok(mut state) = shared.lock() {
        f(&mut state);
        state.touch();
    }
}

fn run_worker(
    db_path: PathBuf,
    roots: Vec<(ProviderId, PathBuf)>,
    shared: Arc<Mutex<SyncState>>,
    watch_sender: Sender<Command>,
    receiver: mpsc::Receiver<Command>,
) {
    let mut store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            update(&shared, |s| {
                s.error =
                    Some(t!("sync.open_index_failed", error = format!("{error:#}")).to_string());
            });
            return;
        }
    };
    let claude = ClaudeCodeProvider::new(paths::claude_projects_dir());
    let codex = CodexProvider::new(paths::codex_sessions_dir());
    let gemini = GeminiProvider::new(paths::gemini_dir());
    let sources = Sources {
        claude: &claude,
        codex: &codex,
        gemini: &gemini,
    };
    let raw_root = paths::raw_dir_for_db(&db_path);
    let raw = match RawArchive::open(raw_root.clone()) {
        Ok(raw) => Some(raw),
        Err(error) => {
            update(&shared, |s| {
                s.raw.open_error = Some(format!("{error:#}"));
            });
            None
        }
    };
    update(&shared, |s| s.raw.root = Some(raw_root));

    // 先挂监听再扫：扫描期间发生的变化会排进通道，扫完立刻补一轮。
    // 除各来源主目录外还监听 Codex 的归档目录（会话被归档时 rollout 挪到那里）；它不存在属正常，不算没挂上。
    let mut watch_list = roots.clone();
    if let Some(archive) = codex.archive_root() {
        watch_list.push((ProviderId::Codex, archive.to_path_buf()));
    }
    let error_sender = watch_sender.clone();
    let watcher = watch_roots(&watch_list, move |event| match event {
        WatchEvent::Changed { provider, paths } => {
            let _ = watch_sender.send(Command::Changed(provider, paths));
        }
        WatchEvent::Error(_) => {
            // FSEvents 可能丢事件；补一次全量扫描追上，不让数据悄悄落后。
            let _ = error_sender.send(Command::ScanAll);
        }
    });
    match &watcher {
        Ok(watcher) => update(&shared, |s| {
            for (provider, _) in &watcher.watched {
                if let Some(p) = s.provider_mut(provider) {
                    p.watching = true;
                    p.watch_note = None;
                }
            }
            for (provider, root, reason) in &watcher.unwatched {
                if !roots.iter().any(|(_, r)| r == root) {
                    continue;
                }
                if let Some(p) = s.provider_mut(provider) {
                    p.watching = false;
                    p.watch_note = Some(reason.clone());
                }
            }
        }),
        Err(error) => update(&shared, |s| {
            s.error = Some(t!("sync.watch_failed", error = format!("{error:#}")).to_string());
        }),
    }

    let all: HashSet<ProviderId> = roots.iter().map(|(p, _)| p.clone()).collect();
    // 续读状态（各活跃原件的解析器）跨扫描复用，增量入库不必每次重放已解析的部分。
    let mut cache = IngestCache::new();
    let mut batch = Batch::default();
    batch.full.extend(all.iter().cloned());
    scan(
        &mut store,
        raw.as_ref(),
        &batch,
        &sources,
        &mut cache,
        &shared,
    );
    let mut next_full = Instant::now() + FALLBACK_SCAN;

    loop {
        let wait = next_full.saturating_duration_since(Instant::now());
        let first = match receiver.recv_timeout(wait) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => Command::ScanAll,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // 合并：首个事件后至少等 QUIET；期间有新事件就再等（每次最多 QUIET），但总共不超过 MAX_DELAY。
        let mut batch = Batch::default();
        batch.absorb(first, &all);
        let started = Instant::now();
        loop {
            let left = MAX_DELAY.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match receiver.recv_timeout(QUIET.min(left)) {
                Ok(command) => batch.absorb(command, &all),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        scan(
            &mut store,
            raw.as_ref(),
            &batch,
            &sources,
            &mut cache,
            &shared,
        );
        if batch.full == all {
            next_full = Instant::now() + FALLBACK_SCAN;
        }
    }
    drop(watcher);
}

/// 同步引擎管的三个来源。
struct Sources<'a> {
    claude: &'a ClaudeCodeProvider,
    codex: &'a CodexProvider,
    gemini: &'a GeminiProvider,
}

/// 一次要处理的变化：要全量扫描的来源 + 其余来源里变了的文件。
#[derive(Default)]
struct Batch {
    full: HashSet<ProviderId>,
    paths: BTreeSet<PathBuf>,
}

impl Batch {
    fn absorb(&mut self, command: Command, all: &HashSet<ProviderId>) {
        match command {
            Command::Changed(provider, paths) => {
                if !self.full.contains(&provider) {
                    self.paths.extend(paths);
                }
            }
            Command::ScanAll => self.full.extend(all.iter().cloned()),
        }
    }
}

/// 全量扫描 `batch.full` 里的来源（遍历目录，未变文件按 size + mtime 跳过），
/// 其余来源只处理 `batch.paths`；两者都按字节偏移增量入库。报告写进共享状态。
fn scan(
    store: &mut Store,
    raw: Option<&RawArchive>,
    batch: &Batch,
    sources: &Sources<'_>,
    cache: &mut IngestCache,
    shared: &Mutex<SyncState>,
) {
    let both: [(&ProviderId, &dyn SourceProvider); 3] = [
        (&ProviderId::ClaudeCode, sources.claude),
        (&ProviderId::Codex, sources.codex),
        (&ProviderId::GeminiCli, sources.gemini),
    ];
    let full: Vec<&dyn SourceProvider> = both
        .iter()
        .filter(|(id, _)| batch.full.contains(id))
        .map(|(_, p)| *p)
        .collect();
    let partial: Vec<&dyn SourceProvider> = both
        .iter()
        .filter(|(id, _)| !batch.full.contains(id))
        .map(|(_, p)| *p)
        .collect();
    let paths: Vec<PathBuf> = batch
        .paths
        .iter()
        .filter(|p| {
            partial
                .iter()
                .any(|provider| provider.source_root(p).is_some())
        })
        .cloned()
        .collect();
    // 进度回调每个文件一次；只在跨过 1% 或换来源时才写共享状态，避免锁竞争。
    let mut last_reported: Option<(ProviderId, usize)> = None;
    let mut on_progress = |progress: agenthome_core::Progress| {
        let step = (progress.total / 100).max(1);
        let changed = last_reported
            .as_ref()
            .is_none_or(|(p, done)| *p != progress.provider || progress.done >= done + step);
        if changed || progress.done == progress.total {
            last_reported = Some((progress.provider.clone(), progress.done));
            update(shared, |s| {
                s.scanning = Some(ScanProgress {
                    provider: progress.provider.clone(),
                    done: progress.done,
                    total: progress.total,
                });
            });
        }
    };
    let full_report =
        (!full.is_empty()).then(|| ingest_all_cached(store, &full, raw, cache, &mut on_progress));
    let path_report = (!paths.is_empty())
        .then(|| ingest_paths(store, &partial, &paths, raw, cache, &mut on_progress));
    let mut report = Report::default();
    for r in full_report.iter().chain(path_report.iter()) {
        report.merge_from(r);
    }
    let which: HashSet<ProviderId> = report
        .per_provider
        .iter()
        .map(|(p, _)| p.clone())
        .chain(batch.full.iter().cloned())
        .collect();
    let now = now_ms();
    let full = batch.full.len() == both.len();
    // 统计要遍历全部清单，只在有写入或还没统计过时重算。
    let stats_stale = shared.lock().map(|s| s.raw.stats.is_none()).unwrap_or(true);
    let raw_stats = raw
        .filter(|_| stats_stale || report.raw.wrote_anything())
        .map(RawArchive::stats);
    update(shared, |s| {
        if raw.is_some() {
            if let Some(stats) = raw_stats {
                s.raw.stats = Some(stats);
            }
            s.raw.failed = report.raw.failed.len();
            s.raw.last_failure = report.raw.failed.last().map(|(path, error)| {
                t!("sync.path_error", path = path.display(), error = error).to_string()
            });
        }
        s.scanning = None;
        s.last_sync_ms = Some(now);
        if full {
            s.last_full_scan_ms = Some(now);
        }
        if report.parsed > 0 {
            s.data_revision += 1;
        }
        for provider in &which {
            if let Some(p) = s.provider_mut(provider) {
                p.detected = p.root.is_dir();
            }
        }
        for (provider, sub) in &report.per_provider {
            if let Some(p) = s.provider_mut(provider) {
                apply_report(p, sub, now, batch.full.contains(provider));
            }
        }
    });
}

/// `full`：这次遍历了来源目录（文件数才是全量的）；只处理变化路径时保留上次全量扫描的文件数。
fn apply_report(p: &mut ProviderSync, report: &Report, now: i64, full: bool) {
    p.last_scan_ms = Some(now);
    if full {
        p.files = report.scanned;
    }
    p.parsed = report.parsed;
    p.failed = report.failed.len();
    p.warnings = report.warnings;
    p.last_failure = report.failed.last().map(|(path, error)| {
        t!("sync.path_error", path = path.display(), error = error).to_string()
    });
}

/// Claude 额度线程：启动即跑一次，之后每 [`CLAUDE_PROBE_INTERVAL`] 一次；`true` 指令强制立即跑。
fn run_claude_probe(shared: Arc<Mutex<SyncState>>, receiver: mpsc::Receiver<bool>) {
    use agenthome_core::signals::{claude_usage, find_claude_binary};

    loop {
        let binary = find_claude_binary();
        match &binary {
            None => update(&shared, |s| {
                s.claude_probe.binary = None;
                s.claude_probe.last_error = Some(t!("sync.claude_not_found").to_string());
            }),
            Some(bin) => {
                update(&shared, |s| {
                    s.claude_probe.binary = binary.clone();
                    s.claude_probe.running = true;
                });
                let result = claude_usage::refresh_claude_usage(
                    bin,
                    &paths::default_data_dir(),
                    &claude_usage::claude_usage_path(),
                );
                update(&shared, |s| {
                    s.claude_probe.running = false;
                    match result {
                        Ok(snapshot) => {
                            s.claude_probe.last_ok_ms = Some(snapshot.fetched_at_ms);
                            s.claude_probe.last_error = None;
                        }
                        Err(error) => s.claude_probe.last_error = Some(format!("{error:#}")),
                    }
                });
            }
        }
        // 定时到点或收到「立即刷新」都直接再跑一次。
        if let Err(RecvTimeoutError::Disconnected) = receiver.recv_timeout(CLAUDE_PROBE_INTERVAL) {
            break;
        }
    }
}
