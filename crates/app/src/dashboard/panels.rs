//! 仪表盘各数据卡片的渲染（累计 token / 趋势 / 今天 / 按模型 / 按项目 / 归档健康）。
//! 都是 `DashboardView` 的只读渲染方法，状态在 `dashboard.rs`。

use agenthome_core::model::{CostSource, Period, ProviderHealth, ProviderId};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, IntoElement, ParentElement as _, Styled as _, div, px, relative, rgb,
};

use super::cards::{BarRow, Tag, TagKind, Tile, avatar, card, card_title, legend, skeleton_rows};
use super::fmt::{
    AMBER, BLUE, CLAUDE, CODEX, GEMINI, GREEN, LINE, MUTED, day_label, hour_minute, month_day,
    percent_change, provider_color, provider_name,
};
use super::model::{DayPoint, TREND_DAYS};
use super::{DashboardView, TrendMetric, segmented};
use crate::format::{now_ms, relative_time, short_cost, short_tokens};

/// 页头周期的中文名。
pub fn period_label(period: Period) -> &'static str {
    match period {
        Period::Today => "今天",
        Period::Week => "本周",
        Period::Month => "本月",
        Period::Days(90) => "90 天",
        Period::Days(_) => "30 天",
        Period::All => "全部",
    }
}

/// 周期副标题：`本月 · 11-01 至今` / `全部时间`。
fn period_subtitle(period: Period) -> String {
    match period.since_ms() {
        Some(since) => format!("{} · {} 至今", period_label(period), month_day(since)),
        None => "全部时间".to_string(),
    }
}

impl DashboardView {
    /// 「累计 token」卡。
    pub(super) fn render_token_card(&self, cx: &mut Context<Self>) -> Div {
        let selected = Self::TOKEN_PERIODS
            .iter()
            .position(|p| *p == self.token_period)
            .unwrap_or(0);
        let header = h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(card_title("累计 token", period_subtitle(self.token_period)))
            .child(div().ml_auto().child(segmented(
                "token-period",
                &["今天", "本周", "本月", "全部"],
                selected,
                cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(period) = Self::TOKEN_PERIODS.get(*ix) {
                        this.set_token_period(*period, cx);
                    }
                }),
            )));
        let mut panel = card(cx).child(header);
        let Some(tokens) = self.tokens.as_deref() else {
            return panel.child(skeleton_rows(4));
        };
        let all = &tokens.all.usage;
        let (claude, codex, gemini) = (
            &tokens.claude.usage,
            &tokens.codex.usage,
            &tokens.gemini.usage,
        );
        // 按住户拆分；Gemini 没有用量时不占位置。
        let split = |a: u64, b: u64, c: u64| {
            let mut parts = vec![
                format!("Claude {}", short_tokens(a)),
                format!("Codex {}", short_tokens(b)),
            ];
            if c > 0 {
                parts.push(format!("Gemini {}", short_tokens(c)));
            }
            parts.join(" · ")
        };
        let hit_rate = {
            let denominator = all.cache_read_tokens + all.input_tokens;
            if denominator == 0 {
                "无缓存命中".to_string()
            } else {
                format!(
                    "命中率 {:.0}%",
                    all.cache_read_tokens as f64 / denominator as f64 * 100.0
                )
            }
        };
        panel = panel.child(
            h_flex()
                .gap(px(10.))
                // 来源拆分换行时四个格子仍等高、顶边对齐。
                .items_stretch()
                .child(Tile::new("输入", short_tokens(all.input_tokens)).note(
                    split(claude.input_tokens, codex.input_tokens, gemini.input_tokens),
                    Some(GREEN),
                ))
                .child(Tile::new("输出", short_tokens(all.output_tokens)).note(
                    split(
                        claude.output_tokens,
                        codex.output_tokens,
                        gemini.output_tokens,
                    ),
                    Some(GREEN),
                ))
                // 缓存读是数字最大、最容易被误读的一项：每轮请求都把整段上下文从缓存重读一遍计数。
                .child(
                    Tile::new("缓存读", short_tokens(all.cache_read_tokens))
                        .note("每轮重读上下文 · 约 1/10 价", None),
                )
                .child(
                    Tile::new("缓存写", short_tokens(all.cache_write_tokens)).note(
                        format!(
                            "5m {} · 1h {}",
                            short_tokens(all.cache_write_5m_tokens),
                            short_tokens(all.cache_write_1h_tokens)
                        ),
                        None,
                    ),
                ),
        );
        panel = panel.child(
            div()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "缓存读为什么这么大：长会话里每一轮请求都会把整段对话上下文从缓存重读一遍并计入 token，\
                     同一段上下文被反复计数（{hit_rate}）。它按约 1/10 的输入单价计费，\
                     所以 token 数看着惊人、花费并不按比例增长——实际花费以下方「{}」为准。",
                    match tokens.all.cost_source {
                        CostSource::Recorded => "记录成本",
                        CostSource::Unknown => "成本",
                        CostSource::Estimated | CostSource::Mixed => "估算成本",
                    }
                )),
        );
        let cost_label = match tokens.all.cost_source {
            CostSource::Recorded => "记录成本",
            CostSource::Unknown => "成本未知",
            CostSource::Estimated | CostSource::Mixed => "估算成本",
        };
        let foreground = cx.theme().foreground;
        let stat = |label: &'static str, value: String| {
            h_flex()
                .items_baseline()
                .gap(px(6.))
                .child(label)
                .child(super::cards::vt(20.).text_color(foreground).child(value))
        };
        panel = panel.child(
            h_flex()
                .flex_wrap()
                .gap(px(22.))
                .pt(px(10.))
                .border_t_1()
                .border_color(cx.theme().muted)
                .items_end()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                // 估算成本是这张卡真正该看的数：放大放在最前，并按住户拆开。
                .child(
                    v_flex().gap(px(4.)).child(cost_label).child(
                        h_flex()
                            .items_baseline()
                            .gap(px(8.))
                            .child(
                                super::cards::vt(30.)
                                    .text_color(rgb(AMBER))
                                    .child(short_cost(tokens.all.cost_usd)),
                            )
                            .child({
                                let mut parts = vec![
                                    format!("Claude {}", short_cost(tokens.claude.cost_usd)),
                                    format!("Codex {}", short_cost(tokens.codex.cost_usd)),
                                ];
                                if tokens.gemini.usage.output_tokens > 0 {
                                    parts.push(format!(
                                        "Gemini {}",
                                        short_cost(tokens.gemini.cost_usd)
                                    ));
                                }
                                parts.join(" · ")
                            }),
                    ),
                )
                .child(stat("会话", tokens.all.sessions.to_string()))
                .child(stat("消息", tokens.all.messages.to_string())),
        );
        let max_output = claude
            .output_tokens
            .max(codex.output_tokens)
            .max(gemini.output_tokens)
            .max(1) as f32;
        let show_gemini = gemini.output_tokens > 0;
        panel.child(
            v_flex()
                .gap(px(8.))
                .pt(px(10.))
                .border_t_1()
                .border_color(cx.theme().muted)
                .child(
                    div()
                        .font_family(crate::assets::FONT_PIXEL_SANS)
                        .text_size(px(12.))
                        .child("按住户 · 输出 token"),
                )
                .child(
                    BarRow::new("Claude", short_tokens(claude.output_tokens))
                        .avatar(ProviderId::ClaudeCode)
                        .label_width(60.)
                        .segment(claude.output_tokens as f32 / max_output, CLAUDE),
                )
                .child(
                    BarRow::new("Codex", short_tokens(codex.output_tokens))
                        .avatar(ProviderId::Codex)
                        .label_width(60.)
                        .segment(codex.output_tokens as f32 / max_output, CODEX),
                )
                .when(show_gemini, |rows| {
                    rows.child(
                        BarRow::new("Gemini", short_tokens(gemini.output_tokens))
                            .avatar(ProviderId::GeminiCli)
                            .label_width(60.)
                            .segment(gemini.output_tokens as f32 / max_output, GEMINI),
                    )
                })
                .child(if show_gemini {
                    legend(&[(CLAUDE, "Claude"), (CODEX, "Codex"), (GEMINI, "Gemini")])
                } else {
                    legend(&[(CLAUDE, "Claude"), (CODEX, "Codex")])
                }),
        )
    }

    /// 「token 量趋势」卡：按天堆叠柱，自绘 div。
    pub(super) fn render_trend_card(&self, cx: &mut Context<Self>) -> Div {
        let days = self.trend_days;
        let metric = self.trend_metric;
        let days_ix = TREND_DAYS.iter().position(|d| *d == days).unwrap_or(0);
        let selected = TrendMetric::ALL
            .iter()
            .position(|m| *m == metric)
            .unwrap_or(0);
        let header = h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(card_title(
                "token 量趋势",
                format!("最近 {days} 天 · {}", metric.subtitle()),
            ))
            .child(
                h_flex()
                    .ml_auto()
                    .gap(px(8.))
                    .child(segmented(
                        "trend-days",
                        &["30 天", "90 天"],
                        days_ix,
                        cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(days) = TREND_DAYS.get(*ix) {
                                this.set_trend_days(*days, cx);
                            }
                        }),
                    ))
                    .child(segmented(
                        "trend-metric",
                        &["token", "成本", "消息"],
                        selected,
                        cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(metric) = TrendMetric::ALL.get(*ix) {
                                this.trend_metric = *metric;
                                cx.notify();
                            }
                        }),
                    )),
            );
        let panel = card(cx).child(header);
        let Some(overview) = self.overview.as_deref() else {
            return panel.child(skeleton_rows(5));
        };
        let points = &overview.daily;
        let max = points
            .iter()
            .map(|p| metric.total(p))
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        let frac = |v: f64| (v / max) as f32;
        let columns = points.iter().enumerate().map(|(ix, p)| {
            let column = v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .justify_end()
                .gap(px(1.));
            let is_last = ix + 1 == points.len();
            match metric {
                TrendMetric::Tokens => column
                    .child(bar(
                        frac(p.input as f64),
                        if is_last { 0xFFD08A } else { AMBER },
                    ))
                    .child(bar(
                        frac(p.output as f64),
                        if is_last { 0x9CC3FF } else { BLUE },
                    ))
                    .child(bar(
                        frac(p.cache_write as f64),
                        if is_last { 0x3A4766 } else { LINE },
                    )),
                TrendMetric::Cost => column.child(bar(frac(p.cost_usd), AMBER)),
                TrendMetric::Messages => column.child(bar(frac(p.messages as f64), BLUE)),
            }
        });
        let axis = [0usize, 1, 2, 3, 4]
            .into_iter()
            .filter_map(|k| {
                let n = points.len();
                if n == 0 {
                    return None;
                }
                let ix = if k == 4 { n - 1 } else { k * (n - 1) / 4 };
                points.get(ix).map(|p| day_label(&p.day).to_string())
            })
            .collect::<Vec<_>>();
        let legend_items: &[(u32, &'static str)] = match metric {
            TrendMetric::Tokens => &[(AMBER, "输入"), (BLUE, "输出"), (LINE, "缓存写")],
            TrendMetric::Cost => &[(AMBER, "估算成本")],
            TrendMetric::Messages => &[(BLUE, "消息数")],
        };
        panel
            .child(
                h_flex()
                    .h(px(170.))
                    .items_end()
                    .gap(px(4.))
                    .pt(px(8.))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(columns),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        legend(legend_items).when(metric == TrendMetric::Tokens, |l| {
                            l.child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("· 缓存读单独看，量级差两位"),
                            )
                        }),
                    )
                    .child(
                        h_flex()
                            .w(relative(0.56))
                            .justify_between()
                            .font_family(crate::assets::FONT_PIXEL_MONO)
                            .text_size(px(15.))
                            .text_color(cx.theme().muted_foreground)
                            .children(axis),
                    ),
            )
    }

    /// 「今天」卡：四个 tile + 进行中的来源。
    pub(super) fn render_today_card(&self, cx: &mut Context<Self>) -> Div {
        let now = now_ms();
        let panel = card(cx).child(card_title(
            "今天",
            format!("{} · 截至 {}", month_day(now), hour_minute(now)),
        ));
        let Some(overview) = self.overview.as_deref() else {
            return panel.child(skeleton_rows(4));
        };
        let today = &overview.today;
        let totals = &today.totals;
        let active: u32 = overview.activity.iter().map(|a| a.active_sessions).sum();
        let cost_change = percent_change(totals.cost_usd, today.yesterday_cost_usd);
        let output_change = percent_change(
            totals.usage.output_tokens as f64,
            today.yesterday_output as f64,
        );
        let change_note = |change: Option<String>, up_color: u32| match change {
            Some(text) => (
                format!("较昨日 {text}"),
                Some(if text.starts_with('-') {
                    MUTED
                } else {
                    up_color
                }),
            ),
            None => ("昨日无数据".to_string(), None),
        };
        let (cost_note, cost_color) = change_note(cost_change, AMBER);
        let (output_note, output_color) = change_note(output_change, GREEN);
        let ongoing: Vec<AnyElement> = overview
            .activity
            .iter()
            .filter(|a| a.active_sessions > 0)
            .map(|a| {
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.5))
                    .child(avatar(&a.provider, 18.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!(
                                "{} · {} 个会话活跃",
                                provider_name(&a.provider),
                                a.active_sessions
                            )),
                    )
                    .child(
                        super::cards::vt(15.)
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                a.last_activity_ms
                                    .map(|t| relative_time(t, now))
                                    .unwrap_or_else(|| "--".to_string()),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        panel
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(
                        Tile::new("估算成本", short_cost(totals.cost_usd))
                            .note(cost_note, cost_color),
                    )
                    .child(
                        Tile::new("输出 token", short_tokens(totals.usage.output_tokens))
                            .note(output_note, output_color),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(
                        Tile::new("会话", totals.sessions.to_string())
                            .note(format!("{active} 个进行中"), Some(GREEN)),
                    )
                    .child(
                        Tile::new("缓存读", short_tokens(totals.usage.cache_read_tokens)).note(
                            format!("写 {}", short_tokens(totals.usage.cache_write_tokens)),
                            None,
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(cx.theme().muted)
                    .child(
                        div()
                            .font_family(crate::assets::FONT_PIXEL_SANS)
                            .text_size(px(12.))
                            .child("进行中"),
                    )
                    .when(ongoing.is_empty(), |list| {
                        list.child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child("最近 10 分钟没有活跃会话"),
                        )
                    })
                    .children(ongoing),
            )
    }

    /// 「按模型」卡：成本横条。
    pub(super) fn render_model_card(&self, cx: &mut Context<Self>) -> Div {
        let panel = card(cx).child(card_title(
            "按模型",
            format!(
                "{}成本 · 跟随累计 token 周期",
                period_label(self.token_period)
            ),
        ));
        let Some(overview) = self.overview.as_deref() else {
            return panel.child(skeleton_rows(5));
        };
        let mut rows = overview.by_model.clone();
        rows.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
        let max = rows
            .iter()
            .map(|r| r.cost_usd)
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        panel
            .when(rows.is_empty(), |p| {
                p.child(empty_hint("这个周期没有可计价的用量", cx))
            })
            .children(rows.iter().map(|row| {
                BarRow::new(row.key.clone(), short_cost(row.cost_usd))
                    .segment((row.cost_usd / max) as f32, provider_color(&row.provider))
            }))
    }

    /// 「按项目」卡：Top 5，同一项目按来源分段。
    pub(super) fn render_project_card(&self, cx: &mut Context<Self>) -> Div {
        let panel = card(cx).child(card_title(
            "按项目",
            format!("{} · Top 5 · 按住户分段", period_label(self.token_period)),
        ));
        let Some(overview) = self.overview.as_deref() else {
            return panel.child(skeleton_rows(5));
        };
        let max = overview
            .projects
            .iter()
            .map(|p| p.total_usd)
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        panel
            .when(overview.projects.is_empty(), |p| {
                p.child(empty_hint("这个周期没有项目用量", cx))
            })
            .children(overview.projects.iter().map(|project| {
                project.parts.iter().fold(
                    BarRow::new(project.name.clone(), short_cost(project.total_usd)),
                    |row, (provider, cost)| {
                        row.segment((cost / max) as f32, provider_color(provider))
                    },
                )
            }))
            .child(legend(&[(CLAUDE, "Claude"), (CODEX, "Codex")]).mt(px(4.)))
    }

    /// 「归档健康」卡。
    pub(super) fn render_health_card(&self, cx: &mut Context<Self>) -> Div {
        let panel = card(cx).child(card_title("归档健康", "来源 → 仓库"));
        let Some(overview) = self.overview.as_deref() else {
            return panel.child(skeleton_rows(5));
        };
        let repo = agenthome_core::paths::default_data_dir();
        let repo_text = dirs_home_short(&repo.display().to_string());
        panel
            .children(overview.health.iter().flat_map(health_rows))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(cx.theme().muted)
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("仓库")
                            .child(
                                div()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().foreground)
                                    .child(repo_text),
                            ),
                    )
                    // TODO: 接 `agenthome scan` 重建索引，现在只是占位。
                    .child(
                        Button::new("rebuild-index")
                            .link()
                            .xsmall()
                            .label("重建索引"),
                    ),
            )
    }
}

/// 一根柱里的一段。
fn bar(fraction: f32, color: u32) -> Div {
    div()
        .w_full()
        .h(relative(fraction.clamp(0.0, 1.0)))
        .bg(rgb(color))
}

fn empty_hint(text: &'static str, cx: &mut Context<DashboardView>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// 一个来源的归档健康：总数行 + 非零的子项行。
fn health_rows(health: &ProviderHealth) -> Vec<AnyElement> {
    let mut rows = vec![
        h_flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(12.5))
            .child(avatar(&health.provider, 18.))
            .child(
                div()
                    .flex_1()
                    .child(provider_name(&health.provider).to_string()),
            )
            .child(super::cards::vt(18.).child(health.sessions.to_string()))
            .into_any_element(),
    ];
    let sub = |text: &'static str, count: u64, kind: TagKind| {
        h_flex()
            .items_center()
            .gap(px(10.))
            .pl(px(28.))
            .text_size(px(12.5))
            .child(div().flex_1().text_color(rgb(MUTED)).child(text))
            .child(Tag::new(count.to_string(), kind))
            .into_any_element()
    };
    if health.archive_only > 0 {
        rows.push(sub(
            "仅归档存在（原件已被清理）",
            health.archive_only,
            TagKind::Warn,
        ));
    }
    if health.expiring_within_7d > 0 {
        rows.push(sub(
            "7 天内将被清理",
            health.expiring_within_7d,
            TagKind::Err,
        ));
    }
    if health.compressed > 0 {
        rows.push(sub(
            "已压缩 .zst（解析时自动解压）",
            health.compressed,
            TagKind::Dim,
        ));
    }
    rows
}

/// `/Users/x/AgentHome` → `~/AgentHome`。
fn dirs_home_short(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

impl TrendMetric {
    fn total(self, p: &DayPoint) -> f64 {
        match self {
            TrendMetric::Tokens => (p.input + p.output + p.cache_write) as f64,
            TrendMetric::Cost => p.cost_usd,
            TrendMetric::Messages => p.messages as f64,
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            TrendMetric::Tokens => "输入 / 输出 / 缓存写分层",
            TrendMetric::Cost => "按天估算成本",
            TrendMetric::Messages => "按天消息数",
        }
    }
}
