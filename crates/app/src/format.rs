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
        t!("format.just_now").into()
    } else if seconds < 3_600 {
        t!("format.minutes_ago", count = seconds / 60).into()
    } else if seconds < 86_400 {
        t!("format.hours_ago", count = seconds / 3_600).into()
    } else if seconds < 30 * 86_400 {
        let days = seconds / 86_400;
        if days == 1 {
            t!("format.day_ago").into()
        } else {
            t!("format.days_ago", count = days).into()
        }
    } else {
        absolute_date(ms)
    }
}

/// 本地日期 YYYY-MM-DD。
pub fn absolute_date(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d").to_string(),
        None => t!("format.invalid_time").into(),
    }
}

/// 本地时间 YYYY-MM-DD HH:MM:SS，消息行用。
pub fn absolute_time(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => t!("format.invalid_time").into(),
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

/// 成本简写，三位有效数字：$0.41 / $7.32 / $10.2 / $241 / $1,255。
pub fn short_cost(usd: f64) -> String {
    if usd < 10. {
        format!("${usd:.2}")
    } else if usd < 100. {
        format!("${usd:.1}")
    } else {
        let whole = usd.round() as u64;
        let digits = whole.to_string();
        let mut out = String::with_capacity(digits.len() + 2);
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        format!("${out}")
    }
}

/// 倒计时 HH:MM:SS；已过期显示「已重置」。
pub fn countdown(remaining_ms: i64) -> String {
    if remaining_ms <= 0 {
        return t!("format.reset").into();
    }
    let seconds = remaining_ms / 1_000;
    if seconds >= 86_400 {
        return format!("{}d {:02}h", seconds / 86_400, seconds % 86_400 / 3_600);
    }
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// 本地时刻 HH:MM:SS，消息行用。
pub fn clock_time(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%H:%M:%S").to_string(),
        None => "--:--:--".to_string(),
    }
}

/// 本地 MM-DD HH:MM，详情头部的起止时间用。
pub fn short_datetime(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%m-%d %H:%M").to_string(),
        None => t!("format.invalid_time").into(),
    }
}

/// 耗时简写：0.3s / 12.4s / 3m10s / 1h02m。
pub fn short_duration(ms: i64) -> String {
    let seconds = ms.max(0) as f64 / 1_000.;
    if seconds < 60. {
        format!("{seconds:.1}s")
    } else if seconds < 3_600. {
        format!("{}m{:02}s", (seconds / 60.) as u64, (seconds % 60.) as u64)
    } else {
        format!(
            "{}h{:02}m",
            (seconds / 3_600.) as u64,
            (seconds % 3_600. / 60.) as u64
        )
    }
}

/// 额度窗口名（「5 小时」「7 天 (Opus)」）。core 给的 `label` 是中文，这里按窗口长度换成当前语言，
/// 按模型的周窗口保留 label 末尾的「 (模型)」；不知道长度的窗口按 id 认，认不出就原样显示。
pub fn quota_window_label(window: &agenthome_core::model::QuotaWindow) -> String {
    let base: String = match window.window_minutes {
        Some(m) if m > 0 && m.is_multiple_of(1440) => {
            t!("format.window_days", count = m / 1440).into()
        }
        Some(m) if m > 0 && m.is_multiple_of(60) => {
            t!("format.window_hours", count = m / 60).into()
        }
        Some(m) => t!("format.window_minutes", count = m).into(),
        None if window.id == "spend_limit" => return t!("format.window_spend_limit").into(),
        None => return window.label.clone(),
    };
    match window.label.find(" (") {
        Some(at) => format!("{base}{}", &window.label[at..]),
        None => base,
    }
}

/// 会话列表的日期分组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DayGroup {
    Today,
    Yesterday,
    ThisWeek,
    Earlier,
}

impl DayGroup {
    pub fn label(self) -> String {
        match self {
            DayGroup::Today => t!("format.today").into(),
            DayGroup::Yesterday => t!("format.yesterday").into(),
            DayGroup::ThisWeek => t!("format.this_week").into(),
            DayGroup::Earlier => t!("format.earlier").into(),
        }
    }

    /// 以本地日期判断：今天 / 昨天 / 本周（周一起）/ 更早。
    pub fn of(ms: i64, now_ms: i64) -> Self {
        use chrono::Datelike as _;
        let (Some(t), Some(now)) = (
            Local.timestamp_millis_opt(ms).single(),
            Local.timestamp_millis_opt(now_ms).single(),
        ) else {
            return DayGroup::Earlier;
        };
        let day = t.date_naive();
        let today = now.date_naive();
        let monday =
            today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
        if day >= today {
            DayGroup::Today
        } else if day == today - chrono::Duration::days(1) {
            DayGroup::Yesterday
        } else if day >= monday {
            DayGroup::ThisWeek
        } else {
            DayGroup::Earlier
        }
    }
}

/// 路径里的家目录换成 `~`：界面更短，截图也不会带出本机用户名。
pub fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_keeps_three_significant_digits() {
        assert_eq!(short_cost(0.41), "$0.41");
        assert_eq!(short_cost(10.24), "$10.2");
        assert_eq!(short_cost(1255.4), "$1,255");
    }

    #[test]
    fn countdown_formats_hours() {
        assert_eq!(countdown(2 * 3_600_000 + 10 * 60_000 + 33_000), "02:10:33");
        assert_eq!(countdown(-5), "Reset");
    }

    #[test]
    fn quota_window_label_follows_language_not_core_text() {
        // core 的窗口名写死中文；界面要按窗口长度重新取文案，按模型的后缀不能丢。
        let window = |id: &str, label: &str, minutes| agenthome_core::model::QuotaWindow {
            id: id.into(),
            label: label.into(),
            used_percent: 0.0,
            resets_at_ms: None,
            window_minutes: minutes,
        };
        assert_eq!(
            quota_window_label(&window("five_hour", "5 小时", Some(300))),
            "5h"
        );
        assert_eq!(
            quota_window_label(&window("seven_day_opus", "7 天 (Opus)", Some(10_080))),
            "7d (Opus)"
        );
        assert_eq!(
            quota_window_label(&window("spend_limit", "消费上限", None)),
            "Spend limit"
        );
        assert_eq!(quota_window_label(&window("other", "窗口", None)), "窗口");
    }

    #[test]
    fn relative_time_has_no_plural_mistakes() {
        let now = 100 * 86_400_000;
        assert_eq!(relative_time(now - 86_400_000, now), "1 day ago");
        assert_eq!(relative_time(now - 3 * 86_400_000, now), "3 days ago");
        assert_eq!(
            relative_time(now - 5 * 60_000, now),
            t!("format.minutes_ago", count = 5, locale = "en")
        );
    }

    #[test]
    fn tilde_hides_home_directory() {
        // 截图与界面不能带出本机用户名。
        let home = dirs::home_dir().unwrap();
        assert_eq!(super::tilde(&home.join("AgentHome")), "~/AgentHome");
        assert_eq!(super::tilde(&home), "~");
        assert_eq!(super::tilde(std::path::Path::new("/opt/x")), "/opt/x");
    }
}
