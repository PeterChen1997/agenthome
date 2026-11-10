//! 额度使用压力：已用比例对照窗口已经过去的时间，判断照现在的速度到重置前够不够用。
//!
//! 例：7 天窗口刚开始一天就用了 50%，照这个速度两天就会用完，压力很大；
//! 同样用了 50%，若窗口已过去六天，离重置只剩一天，则绰绰有余。

use crate::model::QuotaWindow;

/// 压力档位，从轻到重。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    /// 照现在的速度，重置时还剩三成以上。
    Relaxed,
    /// 照现在的速度，重置前刚好用得完或略有剩余。
    Steady,
    /// 照现在的速度，会比重置早一点用完。
    Tight,
    /// 照现在的速度，离重置还早就会用完。
    Critical,
    /// 已经用完，等重置。
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureReading {
    pub level: Pressure,
    /// 照当前速度，到重置时会用掉窗口的多少倍（1.0 = 刚好用完）。
    pub projected: f32,
    /// 照当前速度还要多久用完；重置前用不完时为 None，已用完为 Some(0)。
    pub runs_out_in_ms: Option<i64>,
}

/// 窗口开头已用时间按至少一成计：刚重置几分钟时速度没有意义，
/// 否则用掉 1% 也会被外推成「马上用完」。
const MIN_ELAPSED: f64 = 0.10;
/// 已用到这个比例就算用完（来源给的百分比是取整的）。
const EXHAUSTED: f64 = 0.995;

impl Pressure {
    fn from_projected(projected: f64) -> Self {
        if projected < 0.7 {
            Self::Relaxed
        } else if projected <= 1.0 {
            Self::Steady
        } else if projected < 1.5 {
            Self::Tight
        } else {
            Self::Critical
        }
    }
}

/// 计算一个窗口的使用压力。需要窗口长度与重置时刻；重置时刻已过（快照过期）时为 None。
pub fn pressure(window: &QuotaWindow, now_ms: i64) -> Option<PressureReading> {
    let minutes = window.window_minutes.filter(|m| *m > 0)?;
    let length_ms = f64::from(minutes) * 60_000.0;
    let remaining_ms = (window.resets_at_ms? - now_ms) as f64;
    if remaining_ms <= 0.0 {
        return None;
    }
    let remaining_ms = remaining_ms.min(length_ms);
    let used = (f64::from(window.used_percent) / 100.0).clamp(0.0, 1.0);
    let elapsed = (1.0 - remaining_ms / length_ms).max(MIN_ELAPSED);
    let projected = used / elapsed;
    if used >= EXHAUSTED {
        return Some(PressureReading {
            level: Pressure::Exhausted,
            projected: projected as f32,
            runs_out_in_ms: Some(0),
        });
    }
    // 速度 = 已用 / 已过时间；剩下的按同样速度用完要多久。
    let runs_out_ms = if used > 0.0 {
        (1.0 - used) * elapsed * length_ms / used
    } else {
        f64::INFINITY
    };
    Some(PressureReading {
        level: Pressure::from_projected(projected),
        projected: projected as f32,
        runs_out_in_ms: (runs_out_ms < remaining_ms).then_some(runs_out_ms.round() as i64),
    })
}

/// 一组窗口里压力最大的那个（同档取预计用量更高的）。
pub fn worst_pressure<'a>(
    windows: impl IntoIterator<Item = &'a QuotaWindow>,
    now_ms: i64,
) -> Option<(&'a QuotaWindow, PressureReading)> {
    windows
        .into_iter()
        .filter_map(|window| pressure(window, now_ms).map(|reading| (window, reading)))
        .max_by(|(_, a), (_, b)| {
            a.level
                .cmp(&b.level)
                .then(a.projected.total_cmp(&b.projected))
        })
}

/// 住户行 / 菜单栏只放一个徽标时显示哪个窗口：主窗口，除非别的窗口**档位**更紧。
/// 同档时不换成别的窗口：五个窗口都「宽裕」时把「7d Argon 宽裕」挤进窄行既占地方又没有信息。
pub fn pressure_to_show<'a>(
    windows: &'a [QuotaWindow],
    primary_id: &str,
    now_ms: i64,
) -> Option<(&'a QuotaWindow, PressureReading)> {
    let primary = windows
        .iter()
        .find(|w| w.id == primary_id)
        .and_then(|w| pressure(w, now_ms).map(|r| (w, r)));
    let worst = worst_pressure(windows, now_ms);
    match (primary, worst) {
        (Some(p), Some(w)) if w.1.level > p.1.level => Some(w),
        (Some(p), _) => Some(p),
        (None, w) => w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;
    const NOW: i64 = 1_800_000_000_000;

    fn week(used: f32, resets_in_ms: i64) -> QuotaWindow {
        QuotaWindow {
            id: "seven_day".into(),
            label: "7 天".into(),
            used_percent: used,
            resets_at_ms: Some(NOW + resets_in_ms),
            window_minutes: Some(10_080),
        }
    }

    fn level(window: &QuotaWindow) -> Option<Pressure> {
        pressure(window, NOW).map(|r| r.level)
    }

    #[test]
    fn same_usage_reads_differently_depending_on_time_left() {
        // 用户举的例子：离重置还有一周（窗口刚开始）已用 50%，远不够用。
        assert_eq!(level(&week(50.0, 7 * DAY - HOUR)), Some(Pressure::Critical));
        // 同样 50%，窗口过半：刚好用到重置附近。
        assert_eq!(
            level(&week(50.0, 3 * DAY + 12 * HOUR)),
            Some(Pressure::Steady)
        );
        // 同样 50%，只剩一天就重置：很宽裕。
        assert_eq!(level(&week(50.0, DAY)), Some(Pressure::Relaxed));
    }

    #[test]
    fn tight_means_running_out_a_little_before_reset() {
        // 过去 4 天用了 70%：照这个速度 5.7 天用完，比重置早 1.3 天。
        let reading = pressure(&week(70.0, 3 * DAY), NOW).unwrap();
        assert_eq!(reading.level, Pressure::Tight);
        let eta = reading.runs_out_in_ms.unwrap();
        assert!((DAY + 17 * HOUR..DAY + 18 * HOUR).contains(&eta), "{eta}");
    }

    #[test]
    fn no_run_out_time_when_quota_lasts_until_reset() {
        let reading = pressure(&week(30.0, 3 * DAY), NOW).unwrap();
        assert_eq!(reading.level, Pressure::Relaxed);
        assert_eq!(reading.runs_out_in_ms, None);
    }

    #[test]
    fn a_fresh_window_does_not_extrapolate_tiny_usage() {
        // 5 小时窗口刚开始 5 分钟用了 2%：不该因为速度外推就报「紧张」。
        let window = QuotaWindow {
            id: "five_hour".into(),
            label: "5 小时".into(),
            used_percent: 2.0,
            resets_at_ms: Some(NOW + 5 * HOUR - 5 * 60_000),
            window_minutes: Some(300),
        };
        assert_eq!(level(&window), Some(Pressure::Relaxed));
    }

    #[test]
    fn exhausted_window_is_its_own_state() {
        let reading = pressure(&week(100.0, 2 * DAY), NOW).unwrap();
        assert_eq!(reading.level, Pressure::Exhausted);
        assert_eq!(reading.runs_out_in_ms, Some(0));
    }

    #[test]
    fn unknown_without_length_or_reset_or_after_reset() {
        let mut window = week(50.0, DAY);
        window.window_minutes = None;
        assert_eq!(level(&window), None);
        let mut window = week(50.0, DAY);
        window.resets_at_ms = None;
        assert_eq!(level(&window), None);
        // 重置时刻已过：快照是上个窗口的，不能拿来判断。
        assert_eq!(level(&week(50.0, -HOUR)), None);
    }

    #[test]
    fn badge_stays_on_primary_unless_another_window_is_a_level_worse() {
        let five_hour = QuotaWindow {
            id: "five_hour".into(),
            label: "5 小时".into(),
            used_percent: 0.0,
            resets_at_ms: Some(NOW + 5 * HOUR),
            window_minutes: Some(300),
        };
        // 同为宽裕，周窗口预计用量略高：仍显示主窗口。
        let calm = [five_hour.clone(), week(5.0, 6 * DAY)];
        let (w, _) = pressure_to_show(&calm, "five_hour", NOW).unwrap();
        assert_eq!(w.id, "five_hour");
        // 周窗口紧张：换成它。
        let tight = [five_hour, week(60.0, 6 * DAY)];
        let (w, r) = pressure_to_show(&tight, "five_hour", NOW).unwrap();
        assert_eq!((w.id.as_str(), r.level), ("seven_day", Pressure::Critical));
    }

    #[test]
    fn worst_picks_the_most_constrained_window() {
        let five_hour = QuotaWindow {
            id: "five_hour".into(),
            label: "5 小时".into(),
            used_percent: 10.0,
            resets_at_ms: Some(NOW + 2 * HOUR),
            window_minutes: Some(300),
        };
        let seven_day = week(60.0, 6 * DAY);
        let windows = [five_hour, seven_day];
        let (window, reading) = worst_pressure(&windows, NOW).unwrap();
        assert_eq!(window.id, "seven_day");
        assert_eq!(reading.level, Pressure::Critical);
    }
}
