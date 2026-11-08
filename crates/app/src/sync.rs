//! 实时数据通道（落地清单 10.10 / 10.11 / 10.12）：app 自己把来源目录的新内容入库，
//! 不再依赖手动 `agenthome scan`。
//!
//! - 启动即增量扫描一次（10.11）；索引库不存在时这一次就是首次全量建库。
//! - notify 监听各来源根目录（10.10），变化后防抖 [`DEBOUNCE`] 再只扫变了的来源。
//! - 每 [`FALLBACK_SCAN`] 兜底扫一次全部来源（10.12），监听丢事件也能追上。
//!
//! 入库在独立线程上跑，持有自己的 `Store` 连接（WAL 下与 UI 的只读查询互不阻塞）。
//! 线程把状态写进共享快照，前台每 [`UI_POLL`] 看一眼版本号，有变化才写回 `SyncState` 并重绘。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agenthome_core::model::ProviderId;
use agenthome_core::{
    ClaudeCodeProvider, CodexProvider, Report, SourceProvider, Store, WatchEvent, ingest_all,
    paths, watch_roots,
};
use gpui_kit::{App, AsyncApp, Entity, Task};

use crate::format::now_ms;

/// 文件变化后等这么久没有新变化再入库：活跃会话每秒都在追加，没必要每次都扫。
const DEBOUNCE: Duration = Duration::from_secs(2);
/// 兜底全量扫描间隔。
const FALLBACK_SCAN: Duration = Duration::from_secs(3_600);
/// 后台跑官方 `claude /usage` 刷新 Claude 订阅额度的间隔（每次约 10 秒、不耗 token）。
const CLAUDE_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// 终端 statusline 在这段时间内更新过，就跳过定时的 /usage（数据已足够新）。
const STATUSLINE_FRESH_MS: i64 = 10 * 60_000;
/// 前台检查后台快照的间隔。
const UI_POLL: Duration = Duration::from_millis(500);

/// 发给入库线程的指令。
#[derive(Clone, Debug)]
enum Command {
    /// 某来源目录有变化（来自文件监听）。
    Changed(ProviderId),
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
    /// 最近一次因终端 statusline 已足够新而跳过的时刻。
    pub skipped_at_ms: Option<i64>,
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

    /// 立即跑一次官方 `/usage` 刷新 Claude 额度（不受「statusline 足够新」的跳过规则限制）。
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
                s.error = Some(format!("打开索引库失败，无法同步：{error:#}"));
            });
            return;
        }
    };
    let claude = ClaudeCodeProvider::new(paths::claude_projects_dir());
    let codex = CodexProvider::new(paths::codex_sessions_dir());

    // 先挂监听再扫：扫描期间发生的变化会排进通道，扫完立刻补一轮。
    let error_sender = watch_sender.clone();
    let watcher = watch_roots(&roots, move |event| match event {
        WatchEvent::Changed(provider) => {
            let _ = watch_sender.send(Command::Changed(provider));
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
            for (provider, _, reason) in &watcher.unwatched {
                if let Some(p) = s.provider_mut(provider) {
                    p.watching = false;
                    p.watch_note = Some(reason.clone());
                }
            }
        }),
        Err(error) => update(&shared, |s| {
            s.error = Some(format!("文件监听启动失败，只靠每小时兜底扫描：{error:#}"));
        }),
    }

    let all: HashSet<ProviderId> = roots.iter().map(|(p, _)| p.clone()).collect();
    scan(&mut store, &all, &claude, &codex, &shared);
    let mut next_full = Instant::now() + FALLBACK_SCAN;

    loop {
        let wait = next_full.saturating_duration_since(Instant::now());
        let first = match receiver.recv_timeout(wait) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => Command::ScanAll,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // 防抖：收齐这一阵子的所有变化，再一次性扫。
        let mut pending: HashSet<ProviderId> = HashSet::new();
        let absorb = |command: Command, pending: &mut HashSet<ProviderId>| match command {
            Command::Changed(provider) => {
                pending.insert(provider);
            }
            Command::ScanAll => pending.extend(all.iter().cloned()),
        };
        absorb(first, &mut pending);
        loop {
            match receiver.recv_timeout(DEBOUNCE) {
                Ok(command) => absorb(command, &mut pending),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        scan(&mut store, &pending, &claude, &codex, &shared);
        if pending == all {
            next_full = Instant::now() + FALLBACK_SCAN;
        }
    }
    drop(watcher);
}

/// 对选中的来源跑一次增量入库，并把报告写进共享状态。
fn scan(
    store: &mut Store,
    which: &HashSet<ProviderId>,
    claude: &ClaudeCodeProvider,
    codex: &CodexProvider,
    shared: &Mutex<SyncState>,
) {
    let mut providers: Vec<&dyn SourceProvider> = Vec::new();
    if which.contains(&ProviderId::ClaudeCode) {
        providers.push(claude);
    }
    if which.contains(&ProviderId::Codex) {
        providers.push(codex);
    }
    // 进度回调每个文件一次；只在跨过 1% 或换来源时才写共享状态，避免锁竞争。
    let mut last_reported: Option<(ProviderId, usize)> = None;
    let report = ingest_all(store, &providers, &mut |progress| {
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
    });
    let now = now_ms();
    let full = providers.len() == 2;
    update(shared, |s| {
        s.scanning = None;
        s.last_sync_ms = Some(now);
        if full {
            s.last_full_scan_ms = Some(now);
        }
        if report.parsed > 0 {
            s.data_revision += 1;
        }
        for provider in which {
            if let Some(p) = s.provider_mut(provider) {
                p.detected = p.root.is_dir();
            }
        }
        for (provider, sub) in &report.per_provider {
            if let Some(p) = s.provider_mut(provider) {
                apply_report(p, sub, now);
            }
        }
    });
}

fn apply_report(p: &mut ProviderSync, report: &Report, now: i64) {
    p.last_scan_ms = Some(now);
    p.files = report.scanned;
    p.parsed = report.parsed;
    p.failed = report.failed.len();
    p.warnings = report.warnings;
    p.last_failure = report
        .failed
        .last()
        .map(|(path, error)| format!("{}：{error}", path.display()));
}

/// Claude 额度线程：启动即跑一次，之后每 [`CLAUDE_PROBE_INTERVAL`] 一次；`true` 指令强制立即跑。
fn run_claude_probe(shared: Arc<Mutex<SyncState>>, receiver: mpsc::Receiver<bool>) {
    use agenthome_core::signals::{claude_statusline, claude_usage, find_claude_binary};

    let mut force = false;
    loop {
        let binary = find_claude_binary();
        let statusline_fresh = claude_statusline(None).ok().flatten().is_some_and(|s| {
            !s.windows.is_empty() && now_ms() - s.fetched_at_ms <= STATUSLINE_FRESH_MS
        });
        match &binary {
            None => update(&shared, |s| {
                s.claude_probe.binary = None;
                s.claude_probe.last_error = Some(
                    "找不到官方 claude 命令（~/.local/bin、/opt/homebrew/bin、/usr/local/bin）"
                        .into(),
                );
            }),
            Some(_) if statusline_fresh && !force => update(&shared, |s| {
                s.claude_probe.binary = binary.clone();
                s.claude_probe.skipped_at_ms = Some(now_ms());
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
        force = match receiver.recv_timeout(CLAUDE_PROBE_INTERVAL) {
            Ok(forced) => forced,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => break,
        };
    }
}
