//! 仪表盘页：页头工具行 + 四行卡片。
//!
//! 数据分三路加载，各带 revision 防过期：页头周期（趋势 / 按模型 / 按项目 / 归档 / 今天）、
//! 累计 token 卡自己的周期、额度快照（`QuotaState`，含每秒倒计时）。

mod cards;
mod fmt;
mod model;
mod panels;
mod quota;

use std::sync::Arc;

use agenthome_core::model::Period;
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, px, rgb,
};

use crate::data::DataSource;
use crate::habitat::HabitatView;
use cards::card;
use model::{Overview, TokenTotals};
use quota::{DueCard, QuotaState, ResidentsCard};

/// 趋势图显示的指标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrendMetric {
    Tokens,
    Cost,
    Messages,
}

impl TrendMetric {
    pub const ALL: [TrendMetric; 3] = [
        TrendMetric::Tokens,
        TrendMetric::Cost,
        TrendMetric::Messages,
    ];
}

pub struct DashboardView {
    source: Arc<dyn DataSource>,
    habitat: Entity<HabitatView>,
    quota: Entity<QuotaState>,
    residents: Entity<ResidentsCard>,
    due: Entity<DueCard>,
    /// 页头周期：今天 / 30 天 / 90 天 / 全部。
    period: Period,
    /// 累计 token 卡的周期：今天 / 本周 / 本月 / 全部。
    token_period: Period,
    trend_metric: TrendMetric,
    overview: Option<Arc<Overview>>,
    overview_error: Option<SharedString>,
    overview_revision: u64,
    tokens: Option<Arc<TokenTotals>>,
    tokens_revision: u64,
    _overview_task: Task<()>,
    _tokens_task: Task<()>,
    _quota_task: Task<()>,
}

impl DashboardView {
    const HEADER_PERIODS: [Period; 4] = [
        Period::Today,
        Period::Days(30),
        Period::Days(90),
        Period::All,
    ];
    const TOKEN_PERIODS: [Period; 4] = [Period::Today, Period::Week, Period::Month, Period::All];

    /// `habitat` 是外壳创建并持续喂事件的 Hero 尺寸栖息地，仪表盘只负责摆放。
    pub fn new(
        source: Arc<dyn DataSource>,
        habitat: Entity<HabitatView>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let quota = cx.new(QuotaState::new);
        let residents = cx.new(|cx| ResidentsCard::new(quota.clone(), habitat.clone(), cx));
        let due = cx.new(|cx| DueCard::new(quota.clone(), cx));
        let mut this = Self {
            source,
            habitat,
            quota,
            residents,
            due,
            period: Period::Days(30),
            token_period: Period::Month,
            trend_metric: TrendMetric::Tokens,
            overview: None,
            overview_error: None,
            overview_revision: 0,
            tokens: None,
            tokens_revision: 0,
            _overview_task: Task::ready(()),
            _tokens_task: Task::ready(()),
            _quota_task: Task::ready(()),
        };
        this.load_overview(cx);
        this.load_tokens(cx);
        this.refresh_quota(cx);
        this
    }

    fn set_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.period == period {
            return;
        }
        self.period = period;
        self.load_overview(cx);
    }

    fn set_token_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.token_period == period {
            return;
        }
        self.token_period = period;
        self.load_tokens(cx);
    }

    /// 页头周期驱动的所有查询一次拉齐；刷新期间保留旧数据。
    fn load_overview(&mut self, cx: &mut Context<Self>) {
        self.overview_revision += 1;
        let revision = self.overview_revision;
        self.overview_error = None;
        let source = self.source.clone();
        let period = self.period;
        self._overview_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { model::load_overview(source.as_ref(), period) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.overview_revision != revision {
                    return;
                }
                match result {
                    Ok(overview) => this.overview = Some(Arc::new(overview)),
                    Err(error) => {
                        this.overview_error = Some(format!("读取仪表盘数据失败：{error:#}").into())
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn load_tokens(&mut self, cx: &mut Context<Self>) {
        self.tokens_revision += 1;
        let revision = self.tokens_revision;
        let source = self.source.clone();
        let period = self.token_period;
        self._tokens_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { model::load_tokens(source.as_ref(), period) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.tokens_revision != revision {
                    return;
                }
                match result {
                    Ok(tokens) => this.tokens = Some(Arc::new(tokens)),
                    Err(error) => {
                        this.overview_error = Some(format!("读取 token 汇总失败：{error:#}").into())
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 重新拉额度快照，结果写进 `QuotaState`，由住户卡与到期卡自行重绘。
    fn refresh_quota(&mut self, cx: &mut Context<Self>) {
        let quota = self.quota.clone();
        let source = self.source.clone();
        quota.update(cx, |state, cx| {
            state.loading = true;
            cx.notify();
        });
        self._quota_task = cx.spawn(async move |_, cx| {
            let result = cx.background_spawn(async move { source.quota() }).await;
            quota.update(cx, |state, cx| {
                state.loading = false;
                match result {
                    Ok(snapshots) => {
                        state.snapshots = snapshots;
                        state.error = None;
                    }
                    Err(error) => state.error = Some(format!("读取额度失败：{error:#}").into()),
                }
                state.now_ms = crate::format::now_ms();
                cx.notify();
            });
        });
        // 按钮的 loading 态来自 quota 实体，这里也要重绘一次工具行。
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = Self::HEADER_PERIODS
            .iter()
            .position(|p| *p == self.period)
            .unwrap_or(1);
        let loading = self.quota.read(cx).loading;
        h_flex()
            .h(px(48.))
            .flex_shrink_0()
            .items_center()
            .justify_end()
            .gap(px(10.))
            .px(px(14.))
            .border_b_1()
            .border_color(cx.theme().muted)
            .child(segmented(
                "dashboard-period",
                &["今天", "30 天", "90 天", "全部"],
                selected,
                cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(period) = Self::HEADER_PERIODS.get(*ix) {
                        this.set_period(*period, cx);
                    }
                }),
            ))
            .child(
                Button::new("refresh-quota")
                    .outline()
                    .small()
                    .icon(IconName::RefreshCw)
                    .label("刷新额度")
                    .loading(loading)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_quota(cx))),
            )
    }
}

/// 分段控件：设计稿里的 `.seg`，用 gpui-kit 的 segmented TabBar。
fn segmented(
    id: &'static str,
    labels: &[&'static str],
    selected: usize,
    on_click: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> TabBar {
    TabBar::new(id)
        .segmented()
        .xsmall()
        .selected_index(selected)
        .on_click(on_click)
        .children(labels.iter().map(|label| Tab::new().label(*label)))
}

impl Render for DashboardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let habitat_card = card(cx)
            .flex_shrink_0()
            .w(px(640.))
            .p(px(6.))
            .gap_0()
            .child(div().w(px(628.)).h(px(314.)).child(self.habitat.clone()));
        // 2:1 的两列：flex_grow 配 0 基准，gap 不破坏比例。
        let wide = |panel: gpui_kit::Div| panel.flex_grow(2.).flex_basis(px(0.)).min_w_0();
        let narrow = |panel: gpui_kit::Div| panel.flex_grow(1.).flex_basis(px(0.)).min_w_0();
        let row = || h_flex().items_stretch().gap(px(14.));

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("dashboard-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(1400.))
                            .mx_auto()
                            .px(px(24.))
                            .pt(px(18.))
                            .pb(px(32.))
                            .gap(px(14.))
                            .when_some(self.overview_error.clone(), |page, error| {
                                page.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(fmt::RED_TEXT))
                                        .child(error),
                                )
                            })
                            .child(
                                row()
                                    .child(habitat_card)
                                    .child(div().flex_1().min_w_0().child(self.residents.clone())),
                            )
                            .child(
                                row()
                                    .child(wide(self.render_token_card(cx)))
                                    .child(narrow(div().flex().child(self.due.clone()))),
                            )
                            .child(
                                row()
                                    .child(wide(self.render_trend_card(cx)))
                                    .child(narrow(self.render_today_card(cx))),
                            )
                            .child(
                                row()
                                    .child(narrow(self.render_model_card(cx)))
                                    .child(narrow(self.render_project_card(cx)))
                                    .child(narrow(self.render_health_card(cx))),
                            ),
                    ),
            )
    }
}
