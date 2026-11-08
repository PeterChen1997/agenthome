//! 额度信号：都来自本机文件，不走网络、不碰鉴权。
//!
//! - `claude_statusline`：statusline 包装脚本写下的最近一次 JSON（见 docs/06-statusline-安装.md）。
//! - `claude_usage`：后台在伪终端跑官方 `claude` 的 `/usage` 读屏得到的快照（只用桌面版时的来源）。
//! - `codex_rollout_quota`：最新 rollout 里最后一条 `token_count.rate_limits`。

pub mod claude_statusline;
pub mod claude_usage;
pub mod codex_rollout;

pub use claude_statusline::claude_statusline;
pub use claude_usage::{
    claude_usage_snapshot, find_claude_binary, merge_claude, refresh_claude_usage,
};
pub use codex_rollout::codex_rollout_quota;

use crate::model::Freshness;

/// 超过 10 分钟视为过期；Codex 窗口按天计，24 小时内还算「缓存可用」。
pub(crate) fn freshness(mtime_ms: i64, now_ms: i64, allow_cached: bool) -> Freshness {
    let age = now_ms - mtime_ms;
    if age <= 10 * 60_000 {
        Freshness::Live
    } else if allow_cached && age <= 24 * 3_600_000 {
        Freshness::Cached
    } else {
        Freshness::Stale
    }
}

/// 按 window_minutes 给窗口一个人类可读标签。
pub(crate) fn window_label(minutes: Option<u32>) -> String {
    match minutes {
        Some(300) => "5 小时".to_string(),
        Some(10080) => "7 天".to_string(),
        Some(m) if m.is_multiple_of(1440) => format!("{} 天", m / 1440),
        Some(m) if m.is_multiple_of(60) => format!("{} 小时", m / 60),
        Some(m) => format!("{m} 分钟"),
        None => "窗口".to_string(),
    }
}
