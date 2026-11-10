//! 仪表盘专用的格式化与设计令牌色；通用格式化见 `crate::format`。

use crate::habitat::ResidentId;
use crate::live::resident_of;
use agenthome_core::model::ProviderId;
use chrono::{Datelike as _, Local, NaiveDate, TimeZone as _};

/// 设计令牌（docs/design/canvas.json notes.tokens）。
pub const CLAUDE: u32 = 0xE8824F;
/// 原设计稿 #D9DEEA 在浅色主题的白底上几乎看不见，取明暗两套背景上都可辨的灰。
pub const CODEX: u32 = 0xA3ADC2;
pub const GEMINI: u32 = 0x8E7CFF;
pub const AMBER: u32 = 0xF5B556;
pub const GREEN: u32 = 0x8BD16A;
pub const RED: u32 = 0xFF6B6B;
pub const RED_TEXT: u32 = 0xFF8A8A;
pub const BLUE: u32 = 0x6FA9FF;
/// 次要文字 #9AA6C0，主题里没有对应语义字段。
pub const MUTED: u32 = 0x9AA6C0;
pub const LINE: u32 = 0x26304A;
/// 浅色主题白底上的琥珀字：#F5B556 / 主题 warning #D9931F 在白底对比度不足 3:1，文字改用更深的一档。
pub const AMBER_TEXT_LIGHT: u32 = 0x9A6100;

/// 琥珀色文字（倒计时、告警标签）：深色主题沿用设计令牌，浅色主题用加深的琥珀。
pub fn amber_text(cx: &gpui_kit::App) -> gpui_kit::Hsla {
    use gpui_kit::component::ActiveTheme as _;
    if cx.theme().is_dark() {
        gpui_kit::rgb(AMBER).into()
    } else {
        gpui_kit::rgb(AMBER_TEXT_LIGHT).into()
    }
}

/// 住户色：Claude 赭陶、Codex 黑白、Gemini 蓝紫。
/// 按来源注册表里登记的住户取色；没有住户的来源用中性色。
pub fn provider_color(provider: &ProviderId) -> u32 {
    match resident_of(provider) {
        Some(ResidentId::Claude) => CLAUDE,
        Some(ResidentId::Codex) => CODEX,
        Some(ResidentId::Gemini) => GEMINI,
        None => MUTED,
    }
}

/// 住户显示名；没有住户的来源用注册表里的名字。
pub fn provider_name(provider: &ProviderId) -> String {
    match resident_of(provider) {
        Some(ResidentId::Claude) => "Claude".into(),
        Some(ResidentId::Codex) => "Codex".into(),
        Some(ResidentId::Gemini) => "Gemini".into(),
        None => agenthome_core::providers::registry::display_name(provider),
    }
}

/// 头像素材路径（按住户）；没有住户的来源返回 None。
pub fn avatar_path(provider: &ProviderId) -> Option<&'static str> {
    resident_of(provider).map(|who| match who {
        ResidentId::Claude => "avatars/avatar-claude.png",
        ResidentId::Codex => "avatars/avatar-codex.png",
        ResidentId::Gemini => "avatars/avatar-gemini.png",
    })
}

/// 倒计时：不足一天沿用 `format::countdown` 的 `HH:MM:SS`，否则 `Nd HHh`。
pub fn countdown(remaining_ms: i64) -> String {
    let seconds = remaining_ms / 1_000;
    if seconds < 86_400 {
        crate::format::countdown(remaining_ms)
    } else {
        format!("{}d {:02}h", seconds / 86_400, seconds % 86_400 / 3_600)
    }
}

/// 到期时刻：当天 `17:52`，一周内 `周日 09:00` / `Sun 09:00`，更远 `11-12 06:00` / `Nov 12 06:00`。
pub fn due_time(ms: i64, now_ms: i64) -> String {
    let Some(t) = Local.timestamp_millis_opt(ms).single() else {
        return t!("dashboard.invalid_time").into_owned();
    };
    let Some(now) = Local.timestamp_millis_opt(now_ms).single() else {
        return t.format(&t!("dashboard.fmt_month_day_time")).to_string();
    };
    let days = (t.date_naive() - now.date_naive()).num_days();
    if days == 0 {
        t.format("%H:%M").to_string()
    } else if (0..7).contains(&days) {
        let weekdays = t!("dashboard.weekdays");
        let weekday = weekdays
            .split(',')
            .nth(t.weekday().num_days_from_monday() as usize)
            .unwrap_or("");
        t.format(&t!("dashboard.fmt_weekday_time", weekday = weekday))
            .to_string()
    } else {
        t.format(&t!("dashboard.fmt_month_day_time")).to_string()
    }
}

/// 较前值的百分比变化文案（`+32%`）；前值为 0 时无法比较。
pub fn percent_change(now: f64, before: f64) -> Option<String> {
    if before <= 0.0 {
        return None;
    }
    let delta = (now - before) / before * 100.0;
    Some(format!("{delta:+.0}%"))
}

/// `YYYY-MM-DD` → `MM-DD`（英文 `Nov 12`）。
pub fn day_label(day: &str) -> String {
    match NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(date) => date.format(&t!("dashboard.fmt_month_day")).to_string(),
        Err(_) => day.get(5..).unwrap_or(day).to_string(),
    }
}

/// 本地 `MM-DD`（英文 `Nov 12`）。
pub fn month_day(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format(&t!("dashboard.fmt_month_day")).to_string(),
        None => "--".to_string(),
    }
}

/// 本地 `HH:MM`。
pub fn hour_minute(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%H:%M").to_string(),
        None => "--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_switches_unit_at_one_day() {
        assert_eq!(countdown(0), crate::format::countdown(0));
        assert_eq!(countdown(2 * 3_600_000 + 10 * 60_000 + 33_000), "02:10:33");
        assert_eq!(countdown(2 * 86_400_000 + 17 * 3_600_000), "2d 17h");
    }

    #[test]
    fn percent_change_needs_a_baseline() {
        assert_eq!(percent_change(13.2, 10.0).as_deref(), Some("+32%"));
        assert_eq!(percent_change(5.0, 0.0), None);
    }

    #[test]
    fn day_label_is_english_by_default() {
        assert_eq!(day_label("2026-11-09"), "Nov 9");
        assert_eq!(day_label("bad"), "bad");
    }
}
