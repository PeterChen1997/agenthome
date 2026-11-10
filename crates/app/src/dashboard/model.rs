//! 仪表盘的数据整形：把 `DataSource` 的查询结果变成各卡片直接可画的结构。
//! 全是纯函数，在后台线程里跑，不碰 GPUI。

use std::collections::BTreeMap;

use agenthome_core::model::{
    CostBreakdown, DailyUsage, Period, ProviderActivity, ProviderHealth, ProviderId, QuotaSnapshot,
    UsageTotals,
};

use crate::data::DataSource;
use crate::format::{absolute_date, now_ms};

/// 页头周期（今天 / 30 天 / 90 天 / 全部）驱动的一整页数据。
pub struct Overview {
    pub daily: Vec<DayPoint>,
    pub by_model: Vec<CostBreakdown>,
    pub projects: Vec<ProjectCost>,
    pub health: Vec<ProviderHealth>,
    pub activity: Vec<ProviderActivity>,
    pub today: TodayStats,
}

/// 趋势图的一根柱：当天所有来源 / 模型合计。
#[derive(Clone, Debug, PartialEq)]
pub struct DayPoint {
    pub day: String,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
    pub messages: u64,
}

/// 「按项目」一行：同一项目按来源分段。
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectCost {
    pub name: String,
    pub parts: Vec<(ProviderId, f64)>,
    pub total_usd: f64,
}

/// 「今天」卡：今日汇总 + 昨日对照 + 进行中的会话数。
pub struct TodayStats {
    pub totals: UsageTotals,
    pub yesterday_cost_usd: f64,
    pub yesterday_output: u64,
}

/// 「累计 token」卡：全部 + 三位住户各自的汇总。
pub struct TokenTotals {
    pub all: UsageTotals,
    pub claude: UsageTotals,
    pub codex: UsageTotals,
    pub gemini: UsageTotals,
}

/// 「到期与重置」一行。
#[derive(Clone, Debug, PartialEq)]
pub struct DueItem {
    pub provider: ProviderId,
    pub title: String,
    pub note: String,
    pub at_ms: i64,
    /// 窗口剩余额度（0–100），信用过期没有。
    pub quota_left: Option<f32>,
}

/// 趋势卡可选的天数。
pub const TREND_DAYS: [u32; 2] = [30, 90];

/// `trend_days` 只管趋势图；`breakdown` 是「按模型 / 按项目」的周期，跟随累计 token 卡。
pub fn load_overview(
    source: &dyn DataSource,
    trend_days: u32,
    breakdown: Period,
) -> anyhow::Result<Overview> {
    let today = source.totals(Period::Today, None)?;
    let recent = source.daily(2)?;
    let yesterday = absolute_date(now_ms() - 86_400_000);
    let (yesterday_cost_usd, yesterday_output) = recent
        .iter()
        .filter(|row| row.day == yesterday)
        .fold((0.0, 0), |(cost, output), row| {
            (cost + row.cost_usd, output + row.usage.output_tokens)
        });
    Ok(Overview {
        daily: aggregate_daily(source.daily(trend_days)?),
        by_model: source.by_model(breakdown)?,
        projects: group_projects(source.by_project(breakdown, 5)?, 5),
        health: source.archive_health()?,
        activity: source.activity()?,
        today: TodayStats {
            totals: today,
            yesterday_cost_usd,
            yesterday_output,
        },
    })
}

pub fn load_tokens(source: &dyn DataSource, period: Period) -> anyhow::Result<TokenTotals> {
    Ok(TokenTotals {
        all: source.totals(period, None)?,
        claude: source.totals(period, Some(ProviderId::ClaudeCode))?,
        codex: source.totals(period, Some(ProviderId::Codex))?,
        gemini: source.totals(period, Some(ProviderId::GeminiCli))?,
    })
}

/// 按天合并所有来源 / 模型，按日期升序。
pub fn aggregate_daily(rows: Vec<DailyUsage>) -> Vec<DayPoint> {
    let mut days: BTreeMap<String, DayPoint> = BTreeMap::new();
    for row in rows {
        let point = days.entry(row.day.clone()).or_insert_with(|| DayPoint {
            day: row.day.clone(),
            input: 0,
            output: 0,
            cache_write: 0,
            cost_usd: 0.0,
            messages: 0,
        });
        point.input += row.usage.input_tokens;
        point.output += row.usage.output_tokens;
        point.cache_write += row.usage.cache_write_tokens;
        point.cost_usd += row.cost_usd;
        point.messages += row.message_count;
    }
    days.into_values().collect()
}

/// 同名项目合并成一行，按总成本降序取前 `limit` 个。
pub fn group_projects(rows: Vec<CostBreakdown>, limit: usize) -> Vec<ProjectCost> {
    let mut projects: Vec<ProjectCost> = Vec::new();
    for row in rows {
        match projects.iter_mut().find(|p| p.name == row.key) {
            Some(project) => {
                project.parts.push((row.provider, row.cost_usd));
                project.total_usd += row.cost_usd;
            }
            None => projects.push(ProjectCost {
                name: row.key,
                total_usd: row.cost_usd,
                parts: vec![(row.provider, row.cost_usd)],
            }),
        }
    }
    projects.sort_by(|a, b| b.total_usd.total_cmp(&a.total_usd));
    projects.truncate(limit);
    projects
}

/// 所有窗口的重置时刻与信用过期，按时间先后排序。
pub fn due_items(snapshots: &[QuotaSnapshot]) -> Vec<DueItem> {
    let mut items = Vec::new();
    for snapshot in snapshots {
        let name = super::fmt::provider_name(&snapshot.provider);
        for window in &snapshot.windows {
            let Some(at_ms) = window.resets_at_ms else {
                continue;
            };
            let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
            items.push(DueItem {
                provider: snapshot.provider.clone(),
                title: t!(
                    "dashboard.due_window_reset",
                    name = name,
                    window = crate::format::quota_window_label(window)
                )
                .into_owned(),
                note: if left < 25.0 {
                    t!("dashboard.due_low_quota").into_owned()
                } else {
                    String::new()
                },
                at_ms,
                quota_left: Some(left),
            });
        }
        if let Some(credits) = &snapshot.credits
            && let Some(at_ms) = credits.expires_at_ms
        {
            items.push(DueItem {
                provider: snapshot.provider.clone(),
                title: t!("dashboard.due_credit_expiry", name = name).into_owned(),
                note: credits
                    .reset_credits
                    .map(|n| t!("dashboard.due_credits_unused", count = n).into_owned())
                    .unwrap_or_default(),
                at_ms,
                quota_left: None,
            });
        }
    }
    items.sort_by_key(|item| item.at_ms);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::MockDataSource;

    #[test]
    fn overview_shapes_mock_data_for_every_card() {
        let source = MockDataSource::new();
        let overview = load_overview(&source, 30, Period::Month).expect("mock 不该失败");
        assert_eq!(overview.daily.len(), 30, "30 天周期应有 30 根柱");
        assert!(
            overview.daily.windows(2).all(|w| w[0].day < w[1].day),
            "柱按日期升序"
        );
        assert_eq!(overview.projects.len(), 5, "按项目只取 Top 5");
        let first = overview.projects.first().expect("至少一个项目");
        assert_eq!(first.parts.len(), 2, "同一项目按来源分段");
        assert!(
            overview
                .projects
                .windows(2)
                .all(|w| w[0].total_usd >= w[1].total_usd),
            "项目按成本降序"
        );
        assert_eq!(overview.by_model.len(), 5);
        assert!(!overview.health.is_empty());
        assert!(overview.today.totals.cost_usd > 0.0);
    }

    #[test]
    fn tokens_split_by_resident_add_up() {
        let source = MockDataSource::new();
        let tokens = load_tokens(&source, Period::Month).expect("mock 不该失败");
        let split = tokens.claude.usage.output_tokens + tokens.codex.usage.output_tokens;
        assert!(split <= tokens.all.usage.output_tokens);
        assert!(tokens.claude.usage.output_tokens > tokens.codex.usage.output_tokens);
    }

    #[test]
    fn due_items_are_sorted_and_include_credit_expiry() {
        let source = MockDataSource::new();
        let items = due_items(&source.quota().expect("mock 不该失败"));
        assert_eq!(items.len(), 5, "4 个窗口 + 1 个信用过期");
        assert!(items.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
        let credit = items
            .iter()
            .find(|i| i.quota_left.is_none())
            .expect("应有信用过期项");
        assert_eq!(credit.provider, ProviderId::Codex);
        // 测试不切 locale（全局、并行），断言默认的英文。
        assert_eq!(credit.note, "3 unused");
        let seven_day = items
            .iter()
            .find(|i| {
                i.provider == ProviderId::ClaudeCode && i.quota_left.is_some_and(|l| l < 25.0)
            })
            .expect("Claude 7 天窗口只剩 22%");
        assert_eq!(seven_day.note, "Less than 1/4 left");
        // core 给的窗口标签是中文，英文界面里不能漏出来。
        assert!(
            seven_day.title.starts_with("Claude ") && seven_day.title.ends_with(" window reset")
        );
        assert!(items.iter().all(|i| i.title.is_ascii()), "{items:?}");
    }
}
