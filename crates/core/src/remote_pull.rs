//! Mac 端后台拉取远端用量（CARM-284 方案单元 12）。
//!
//! 一次拉取 [`pull`]：从导入库里这台远端已确认的位置往后，按 SSH 导出的信封
//! （[`ExportPage`]）一页一页拉，交给导入库（单元 7 增量 / 单元 8 完整快照）。每页的用量和游标在
//! 导入库的**同一个事务**里提交，所以在任意两页之间停下（App 退出、休眠、断网、超时）都不会留下
//! 半页；下次从已确认的游标续拉，重复收到的页按修订忽略，结果只计一次。
//!
//! - 第一次拉到一台远端：把连接名绑定到对方报告的 `machine_id`（SSH 主机密钥在配对时已固定校验）。
//! - 游标失效（对方库重建 / 从旧备份恢复）：转完整快照，按快照的暂存位置续拉，拉够后替换。
//! - 一次最多拉 `max_pages` 页，剩下的算积压，由调度 [`PullSchedule`] 很快再来一轮。
//!
//! 同一远端同时只能有一个拉取：[`lock_remote`] 在远端目录里拿独占文件锁，跨进程也成立。
//!
//! 失败类别（[`PullFailure`]）按阶段区分：拉取用 [`RemoteErrorKind`]，导入用 [`FailureKind`]。
//! 上报（Sentry）发阶段、类别和拒收响应的安全摘要；`detail` 只在本机显示。

use crate::device::DeviceIdentity;
use crate::remote_ssh::{self, ExportPage, RemoteError, RemoteErrorKind, ResponseDiagnostic};
use crate::usage_import::{ImportStore, StoreReset};
use crate::usage_store::FailureKind;
use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

/// 常态两次拉取之间的间隔。采集端常态每 60 秒一轮，Mac 不必更勤。
pub const PULL_INTERVAL_MS: i64 = 5 * 60 * 1000;
/// 有积压（一轮没拉完）时，下一轮隔多久。
pub const BACKLOG_DELAY_MS: i64 = 2 * 1000;
/// 失败后的第一次重试间隔，之后每次翻倍，最多到 [`PULL_INTERVAL_MS`]。
pub const RETRY_BASE_MS: i64 = 30 * 1000;
/// 调度器多久看一次钟。Mac 休眠时单调时钟停走，所以按墙钟判断到点和唤醒。
pub const TICK_MS: i64 = 15 * 1000;
/// 两次看钟之间墙钟走了超过这么久，就当作刚从休眠里醒来（或系统时间被调过），立即补拉。
pub const WAKE_GAP_MS: i64 = 3 * TICK_MS;
/// 一轮最多拉多少页（每页 ≤ 256 KiB），避免历史回填一次占住太久。
pub const MAX_PAGES_PER_PULL: usize = 32;

/// 一次拉取失败的类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullFailure {
    /// 连接 / 拉取阶段（SSH、采集端导出、返回内容）。
    Sync(RemoteErrorKind),
    /// 写进导入库阶段（含快照）。
    Import(FailureKind),
    /// 这台远端已有别的拉取在进行（另一个进程）。
    Busy,
}

impl PullFailure {
    /// 阶段：`sync` / `import`。
    pub fn stage(self) -> &'static str {
        match self {
            Self::Sync(_) | Self::Busy => "sync",
            Self::Import(_) => "import",
        }
    }

    /// 类别：`RemoteErrorKind` / `FailureKind` 的字符串，或 `busy`。
    pub fn kind(self) -> &'static str {
        match self {
            Self::Sync(kind) => kind.as_str(),
            Self::Import(kind) => kind.as_str(),
            Self::Busy => "busy",
        }
    }

    /// 拉取阶段的错误：`remote_ssh` 打了类别的取类别，其余（读配置等本地问题）算 `local`。
    pub fn of_fetch(error: &anyhow::Error) -> Self {
        Self::Sync(
            error
                .downcast_ref::<RemoteError>()
                .map(|e| e.kind)
                .unwrap_or(RemoteErrorKind::Local),
        )
    }

    /// 导入阶段的错误：磁盘满、无权限按 I/O 原因分，其余（拒收的页、库写失败）都算 `store`。
    pub fn of_import(error: &anyhow::Error) -> Self {
        Self::Import(match FailureKind::of(error) {
            kind @ (FailureKind::DiskFull | FailureKind::PermissionDenied) => kind,
            _ => FailureKind::Store,
        })
    }
}

/// 一次拉取失败：类别 + 本机展示用的详情（可能含地址，不上报）+ 失败前已提交的页数。
#[derive(Debug)]
pub struct PullError {
    pub failure: PullFailure,
    pub detail: String,
    pub pages: usize,
    pub response: Option<ResponseDiagnostic>,
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}/{}] {}",
            self.failure.stage(),
            self.failure.kind(),
            self.detail
        )
    }
}

impl std::error::Error for PullError {}

/// 一次拉取的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PullOutcome {
    /// 拉了几页（含快照页）。
    pub pages: usize,
    /// 新键 / 被更大修订替换的键。
    pub inserted: usize,
    pub replaced: usize,
    /// 完成了几次完整快照替换。
    pub snapshots: usize,
    /// 结束时对方还有多少变更没拉（按 `seq` 计）；进行中的快照按还差多少暂存计。
    pub backlog: u64,
    /// 还有没拉完的（到了页数上限或被叫停）。
    pub more: bool,
    /// 结束时这台远端在做完整快照。
    pub in_snapshot: bool,
}

/// 从连接 `label` 拉取，直到对方没有新变更、拉满 `max_pages` 页或 `stop()` 为真。
/// `fetch(after)` 取 `after` 之后的一页（真实环境是 [`remote_ssh::fetch`]）。
pub fn pull(
    store: &mut ImportStore,
    label: &str,
    fetch: &mut dyn FnMut(u64) -> Result<ExportPage>,
    max_pages: usize,
    stop: &dyn Fn() -> bool,
    now_ms: i64,
) -> Result<PullOutcome, PullError> {
    let mut out = PullOutcome::default();
    let import_err = |out: &PullOutcome, e: anyhow::Error| PullError {
        failure: PullFailure::of_import(&e),
        detail: format!("{e:#}"),
        pages: out.pages,
        response: None,
    };
    let mut fetch_page = |out: &mut PullOutcome, after: u64| match fetch(after) {
        Ok(page) => {
            out.pages += 1;
            Ok(page)
        }
        Err(e) => Err(PullError {
            failure: PullFailure::of_fetch(&e),
            detail: format!("{e:#}"),
            pages: out.pages,
            response: e.downcast_ref::<RemoteError>().and_then(|e| e.response),
        }),
    };
    loop {
        let machine_id = store.peer(label).map_err(|e| import_err(&out, e))?;
        let snapshot = match &machine_id {
            Some(m) => store.snapshot(m).map_err(|e| import_err(&out, e))?,
            None => None,
        };
        out.in_snapshot = snapshot.is_some();
        if let Some(s) = &snapshot {
            out.backlog = s.high_water.saturating_sub(s.staged_through);
            if s.complete() {
                store
                    .finish_snapshot(label)
                    .map_err(|e| import_err(&out, e))?;
                out.snapshots += 1;
                continue;
            }
        }
        if stop() || out.pages >= max_pages {
            out.more = true;
            return Ok(out);
        }

        if let Some(s) = snapshot {
            let page = fetch_page(&mut out, s.staged_through)?;
            // 快照期间对方又重建了库：从头开始新的快照。
            if page.store_id != s.store_id {
                store
                    .begin_snapshot(label, &page.store_id, page.high_water)
                    .map_err(|e| import_err(&out, e))?;
                continue;
            }
            let bytes = page_bytes(&page).map_err(|e| import_err(&out, e))?;
            let state = match store.stage_snapshot_page(label, &page.cursor(), &bytes) {
                Ok(state) => state,
                Err(e) if e.is::<StoreReset>() => {
                    store
                        .begin_snapshot(label, &page.store_id, page.high_water)
                        .map_err(|e| import_err(&out, e))?;
                    continue;
                }
                Err(e) => return Err(import_err(&out, e)),
            };
            // 对方已经没有更多变更，暂存位置却还没到快照开始时的高水位：对方高水位倒退了，
            // 这份快照永远拉不够。不空转，报错交给下一轮（届时游标判断会重新开始快照）。
            if !state.complete() && !page.more {
                return Err(PullError {
                    failure: PullFailure::Import(FailureKind::Store),
                    detail: "完整快照期间采集端高水位倒退".into(),
                    pages: out.pages,
                    response: None,
                });
            }
            continue;
        }

        let after = match &machine_id {
            Some(m) => store
                .cursor(m)
                .map_err(|e| import_err(&out, e))?
                .map_or(0, |c| c.seq),
            None => 0,
        };
        let page = fetch_page(&mut out, after)?;
        let machine_id = match machine_id {
            Some(m) => m,
            None => {
                let m = page.page.machine_id.clone();
                store
                    .pair(label, &m, now_ms)
                    .map_err(|e| import_err(&out, e))?;
                m
            }
        };
        let stale = store
            .needs_snapshot(&machine_id, &page.store_id, page.high_water)
            .map_err(|e| import_err(&out, e))?;
        if stale {
            store
                .begin_snapshot(label, &page.store_id, page.high_water)
                .map_err(|e| import_err(&out, e))?;
            continue;
        }
        let bytes = page_bytes(&page).map_err(|e| import_err(&out, e))?;
        match store.import_page(label, &page.cursor(), &bytes) {
            Ok(stats) => {
                out.inserted += stats.inserted;
                out.replaced += stats.replaced;
            }
            Err(e) if e.is::<StoreReset>() => {
                store
                    .begin_snapshot(label, &page.store_id, page.high_water)
                    .map_err(|e| import_err(&out, e))?;
                continue;
            }
            Err(e) => return Err(import_err(&out, e)),
        }
        out.backlog = page.high_water.saturating_sub(page.next);
        if !page.more {
            out.more = false;
            return Ok(out);
        }
    }
}

/// 导入库收的是信封里的 v1 页本身。
fn page_bytes(page: &ExportPage) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&page.page)?)
}

/// 已配对的远端名（`<data>/remote/<名字>/remote.json` 存在的目录），按名字排序。
pub fn list_remotes(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("remote")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("remote.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| remote_ssh::is_valid_name(n))
        .collect();
    names.sort();
    names
}

/// 远端目录里的独占锁。拿不到说明别的进程正在拉这台远端。锁随返回值 drop 释放（进程退出也释放）。
pub fn lock_remote(remote_dir: &Path) -> Result<File, PullError> {
    let busy = |detail: String| PullError {
        failure: PullFailure::Busy,
        detail,
        pages: 0,
        response: None,
    };
    let file = File::create(remote_dir.join("pull.lock"))
        .map_err(|e| busy(format!("创建锁文件失败：{e}")))?;
    file.try_lock()
        .map_err(|_| busy("这台远端正在被另一个拉取使用".into()))?;
    Ok(file)
}

/// 真实环境的一次拉取：拿锁，经受限 SSH 拉页，导入 `store`。
pub fn pull_ssh(
    data_dir: &Path,
    store: &mut ImportStore,
    local: &DeviceIdentity,
    label: &str,
    stop: &dyn Fn() -> bool,
    now_ms: i64,
) -> Result<PullOutcome, PullError> {
    let dir: PathBuf = remote_ssh::remote_dir(data_dir, label)
        .context("远端名不合规")
        .map_err(|e| PullError {
            failure: PullFailure::Sync(RemoteErrorKind::Local),
            detail: format!("{e:#}"),
            pages: 0,
            response: None,
        })?;
    let _lock = lock_remote(&dir)?;
    let local_id = local.machine_id().to_string();
    let mut fetch = |after| remote_ssh::fetch(&dir, after, Some(&local_id));
    pull(store, label, &mut fetch, MAX_PAGES_PER_PULL, stop, now_ms)
}

/// 上次成功拉取的时刻，存在 `<远端目录>/last_pull.json`，App 重开后（还没连上之前）首页也能显示。
pub fn load_last_ok(remote_dir: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(remote_dir.join(LAST_PULL_FILE)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("last_ok_ms")?
        .as_i64()
}

pub fn save_last_ok(remote_dir: &Path, at_ms: i64) -> Result<()> {
    remote_ssh::write_private(
        &remote_dir.join(LAST_PULL_FILE),
        &serde_json::json!({ "last_ok_ms": at_ms }).to_string(),
    )
}

const LAST_PULL_FILE: &str = "last_pull.json";

/// 一台远端的拉取节奏（按墙钟毫秒）：打开即拉；成功后常态间隔、有积压很快再来；
/// 失败按 30 秒起翻倍退避到常态间隔；从休眠醒来（两次看钟间隔过长）立即补拉并清掉退避。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSchedule {
    next_due_ms: i64,
    failures: u32,
    last_tick_ms: Option<i64>,
}

impl Default for PullSchedule {
    fn default() -> Self {
        Self::new()
    }
}

impl PullSchedule {
    /// 新的远端（含 App 刚打开）：立即到点。
    pub fn new() -> Self {
        Self {
            next_due_ms: i64::MIN,
            failures: 0,
            last_tick_ms: None,
        }
    }

    /// 每次看钟时调用：返回现在要不要拉。
    pub fn due(&mut self, now_ms: i64) -> bool {
        if let Some(last) = self.last_tick_ms
            && (now_ms - last > WAKE_GAP_MS || now_ms < last)
        {
            // 休眠醒来或系统时间被往回调：网络环境多半变了，之前的退避不再有意义。
            self.next_due_ms = now_ms;
            self.failures = 0;
        }
        self.last_tick_ms = Some(now_ms);
        now_ms >= self.next_due_ms
    }

    pub fn on_success(&mut self, now_ms: i64, outcome: &PullOutcome) {
        self.failures = 0;
        self.next_due_ms = now_ms
            + if outcome.more {
                BACKLOG_DELAY_MS
            } else {
                PULL_INTERVAL_MS
            };
    }

    pub fn on_failure(&mut self, now_ms: i64) {
        let delay = RETRY_BASE_MS
            .saturating_mul(1i64 << self.failures.min(16))
            .min(PULL_INTERVAL_MS);
        self.failures = self.failures.saturating_add(1);
        self.next_due_ms = now_ms + delay;
    }

    /// 下一次到点的墙钟时刻。
    pub fn next_due_ms(&self) -> i64 {
        self.next_due_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_due_then_waits_interval() {
        let mut s = PullSchedule::new();
        assert!(s.due(1_000));
        s.on_success(1_000, &PullOutcome::default());
        assert!(!s.due(1_000 + TICK_MS));
        assert_eq!(s.next_due_ms(), 1_000 + PULL_INTERVAL_MS);
    }

    /// 有积压时不等 5 分钟，很快接着拉；拉完回到常态间隔。
    #[test]
    fn backlog_pulls_again_soon() {
        let mut s = PullSchedule::new();
        let more = PullOutcome {
            more: true,
            ..PullOutcome::default()
        };
        s.on_success(0, &more);
        assert_eq!(s.next_due_ms(), BACKLOG_DELAY_MS);
    }

    /// 断网 / 换网时失败退避：30 秒起翻倍，封顶常态间隔；成功后清零。
    #[test]
    fn failures_back_off_and_cap() {
        let mut s = PullSchedule::new();
        let mut delays = Vec::new();
        for _ in 0..6 {
            s.on_failure(0);
            delays.push(s.next_due_ms());
        }
        assert_eq!(
            delays,
            vec![30_000, 60_000, 120_000, 240_000, 300_000, 300_000]
        );
        s.on_success(0, &PullOutcome::default());
        s.on_failure(0);
        assert_eq!(s.next_due_ms(), RETRY_BASE_MS);
    }

    /// Mac 休眠时单调时钟停走；醒来后墙钟跳了一大截，要立即补拉，并清掉之前的退避。
    #[test]
    fn wake_from_sleep_pulls_immediately() {
        let mut s = PullSchedule::new();
        assert!(s.due(0));
        for _ in 0..5 {
            s.on_failure(0);
        }
        assert!(!s.due(TICK_MS));
        // 合上盖子 2 小时。
        let woke = TICK_MS + 2 * 3600 * 1000;
        assert!(s.due(woke));
        s.on_failure(woke);
        assert_eq!(s.next_due_ms(), woke + RETRY_BASE_MS, "醒来后退避从头算");
    }

    #[test]
    fn regular_ticks_are_not_mistaken_for_wake() {
        let mut s = PullSchedule::new();
        assert!(s.due(0));
        s.on_success(0, &PullOutcome::default());
        let mut t = 0;
        while t + TICK_MS < PULL_INTERVAL_MS {
            t += TICK_MS;
            assert!(!s.due(t), "第 {t} 毫秒不该到点");
        }
        assert!(s.due(PULL_INTERVAL_MS));
    }

    #[test]
    fn last_ok_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_last_ok(dir.path()), None);
        save_last_ok(dir.path(), 1_763_500_000_000).unwrap();
        assert_eq!(load_last_ok(dir.path()), Some(1_763_500_000_000));
    }

    #[test]
    fn failure_categories() {
        let remote: anyhow::Error = RemoteError {
            kind: RemoteErrorKind::Unreachable,
            detail: "connect to 10.0.0.2 port 22: refused".into(),
            response: None,
        }
        .into();
        let f = PullFailure::of_fetch(&remote);
        assert_eq!((f.stage(), f.kind()), ("sync", "unreachable"));
        let other = anyhow::anyhow!("没有这个远端（先 pair）");
        assert_eq!(PullFailure::of_fetch(&other).kind(), "local");
        let rejected = anyhow::anyhow!("缺页：已确认到 5，这一页从 9 之后开始");
        let f = PullFailure::of_import(&rejected);
        assert_eq!((f.stage(), f.kind()), ("import", "store"));
        let full = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert_eq!(PullFailure::of_import(&full).kind(), "disk_full");
    }
}
