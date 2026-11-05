//! 列表与详情里用到的纯格式化函数，不依赖 GPUI。

use chrono::{Local, TimeZone as _};

/// 当前 Unix 毫秒。
pub fn now_ms() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// token 简写：812 / 12.3k / 1.2M。
pub fn short_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.)
    }
}

/// 相对时间：刚刚 / N 分钟前 / N 小时前 / N 天前，超过 30 天显示日期。
pub fn relative_time(ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - ms).max(0) / 1_000;
    if seconds < 60 {
        "刚刚".to_string()
    } else if seconds < 3_600 {
        format!("{} 分钟前", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3_600)
    } else if seconds < 30 * 86_400 {
        format!("{} 天前", seconds / 86_400)
    } else {
        absolute_date(ms)
    }
}

/// 本地日期 YYYY-MM-DD。
pub fn absolute_date(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => "无效时间".to_string(),
    }
}

/// 本地时间 YYYY-MM-DD HH:MM:SS，消息行用。
pub fn absolute_time(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "无效时间".to_string(),
    }
}

/// 按字符截断（不按字节，避免切坏中文），超长补省略号。
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}
