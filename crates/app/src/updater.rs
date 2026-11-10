//! 检查更新：自动（启动后与每 24 小时一次，设置页可关）+ 手动（设置页「检查更新」）。
//! 有新版本时用户点「立即更新」：后台下载并验证（`agenthome_core::update::download_and_verify`），
//! 通过后交给安装脚本，应用退出、脚本替换 `.app` 并重新打开。没法替换自身时退回「前往下载」。

use std::path::PathBuf;
use std::time::Duration;

use agenthome_core::update::{
    Release, can_replace, download_and_verify, fetch_latest_release, is_newer, running_app_bundle,
    spawn_replace_and_relaunch,
};
use gpui_kit::{App, AsyncApp, Entity, Task};

use crate::config;
use crate::format::now_ms;

/// 当前版本（编进程序的 Cargo 版本号）。调试构建可用环境变量 `AGENTHOME_PRETEND_VERSION`
/// 假装成旧版本，用来核对「有新版本」的提醒；正式包里不读它。
pub fn current_version() -> String {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("AGENTHOME_PRETEND_VERSION") {
        return v;
    }
    env!("CARGO_PKG_VERSION").to_string()
}
/// 启动后等一会儿再查，不和首次入库抢启动时间。
const FIRST_DELAY: Duration = Duration::from_secs(8);
/// 自动检查的间隔。
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateState {
    /// 最近一次查到的最新正式版（可能不比当前新）。
    pub latest: Option<Release>,
    pub checking: bool,
    pub last_checked_ms: Option<i64>,
    /// 最近一次失败原因；成功后清空。
    pub last_error: Option<String>,
    pub install: InstallPhase,
}

/// 应用内更新的进度。
#[derive(Clone, Debug, Default, PartialEq)]
pub enum InstallPhase {
    #[default]
    Idle,
    /// 下载并验证中（几分钟，取决于网速；公证校验要联网查苹果）。
    Downloading,
    /// 验证通过，正在退出交给安装脚本。
    Installing,
    Failed(String),
}

/// 应用内更新要替换的 `.app`：正在运行的正式包且所在目录可写；否则为 None（退回「前往下载」）。
/// 调试构建可用 `AGENTHOME_UPDATE_TARGET` 指向一份副本来演练整套流程。
pub fn install_target() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("AGENTHOME_UPDATE_TARGET") {
        return Some(PathBuf::from(path));
    }
    running_app_bundle().filter(|bundle| can_replace(bundle))
}

/// 下载、验证，通过后交给安装脚本并退出应用（脚本会重新打开新版）。
pub fn install_now(state: &Entity<UpdateState>, cx: &mut App) {
    let Some(release) = state.read(cx).available().cloned() else {
        return;
    };
    if matches!(
        state.read(cx).install,
        InstallPhase::Downloading | InstallPhase::Installing
    ) {
        return;
    }
    let Some(target) = install_target() else {
        cx.open_url(&release.url);
        return;
    };
    state.update(cx, |s, cx| {
        s.install = InstallPhase::Downloading;
        cx.notify();
    });
    let state = state.clone();
    let work_dir = std::env::temp_dir().join("agenthome-update");
    let job = cx.background_executor().spawn(async move {
        let new_app = download_and_verify(&release, &work_dir)?;
        let backup = agenthome_core::paths::default_data_dir()
            .join("backup")
            .join(format!("AgentHome-{}.app", current_version()));
        // 调试演练时可以不重新打开（AGENTHOME_UPDATE_NO_RELAUNCH）。
        let relaunch =
            !cfg!(debug_assertions) || std::env::var_os("AGENTHOME_UPDATE_NO_RELAUNCH").is_none();
        spawn_replace_and_relaunch(&new_app, &target, &backup, relaunch)
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = job.await;
        let ok = result.is_ok();
        state.update(cx, |s, cx| {
            s.install = match result {
                Ok(()) => InstallPhase::Installing,
                Err(error) => InstallPhase::Failed(format!("{error:#}")),
            };
            cx.notify();
        });
        if ok {
            // 让「正在安装」先画出来，再退出交给脚本。
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            cx.update(|cx| cx.quit());
        }
    })
    .detach();
}

impl UpdateState {
    /// 比当前版本新的那一版；没有就是 None。
    pub fn available(&self) -> Option<&Release> {
        self.latest
            .as_ref()
            .filter(|r| is_newer(&r.version, &current_version()))
    }
}

/// 立即查一次（已经在查就不重复发请求）。
pub fn check_now(state: &Entity<UpdateState>, cx: &mut App) {
    if state.read(cx).checking {
        return;
    }
    state.update(cx, |s, cx| {
        s.checking = true;
        cx.notify();
    });
    let state = state.clone();
    let request = cx
        .background_executor()
        .spawn(async move { fetch_latest_release(&current_version()) });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = request.await;
        state.update(cx, |s, cx| {
            s.checking = false;
            s.last_checked_ms = Some(now_ms());
            match result {
                Ok(release) => {
                    s.latest = Some(release);
                    s.last_error = None;
                }
                Err(error) => s.last_error = Some(format!("{error:#}")),
            }
            cx.notify();
        });
    })
    .detach();
}

/// 自动检查：启动后 [`FIRST_DELAY`] 查一次，之后每 [`INTERVAL`] 一次；设置里关掉时跳过。
/// 返回的任务被丢弃即停止。
pub fn start_auto(state: Entity<UpdateState>, cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor().timer(FIRST_DELAY).await;
        loop {
            if config::auto_update_check() {
                cx.update(|cx| check_now(&state, cx));
            }
            cx.background_executor().timer(INTERVAL).await;
        }
    })
}
