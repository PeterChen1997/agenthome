//! 检查更新：自动（启动后与每 24 小时一次，设置页可关）+ 手动（设置页「检查更新」）。
//! 只查 GitHub 上的最新正式版并提醒，不下载、不安装；请求本身在 `agenthome_core::update`。

use std::time::Duration;

use agenthome_core::update::{Release, fetch_latest_release, is_newer};
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
