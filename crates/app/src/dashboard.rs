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

use agenthome_core::model::{Period, QuotaSnapshot};
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
    quota: Entity<QuotaState>,
    residents: Entity<ResidentsCard>,
    due: Entity<DueCard>,
    /// 趋势卡天数：30 / 90。
    trend_days: u32,
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
    const TOKEN_PERIODS: [Period; 4] = [Period::Today, Period::Week, Period::Month, Period::All];

    /// `habitat` 是外壳持有、常驻左栏的栖息地；仪表盘不再重复渲染它，只读住户状态文案。
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
            quota,
            residents,
            due,
            trend_days: 30,
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

    /// 索引库有新数据入库后重拉所有查询（同步引擎触发）；刷新期间保留旧数据，不闪。
    pub fn refresh_data(&mut self, cx: &mut Context<Self>) {
        self.load_overview(cx);
        self.load_tokens(cx);
    }

    /// 外壳每 5 秒拉到的额度快照直接写进 `QuotaState`，不再各拉一份，也不显示加载态。
    pub fn set_quota(&mut self, snapshots: Vec<QuotaSnapshot>, cx: &mut Context<Self>) {
        self.quota.update(cx, |state, cx| {
            if state.snapshots == snapshots && state.error.is_none() {
                return;
            }
            state.snapshots = snapshots;
            state.error = None;
            state.loading = false;
            state.now_ms = crate::format::now_ms();
            cx.notify();
        });
    }

    fn set_trend_days(&mut self, days: u32, cx: &mut Context<Self>) {
        if self.trend_days == days {
            return;
        }
        self.trend_days = days;
        self.load_overview(cx);
    }

    fn set_token_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.token_period == period {
            return;
        }
        self.token_period = period;
        // 「按模型 / 按项目」跟随这个周期，一起重拉。
        self.load_tokens(cx);
        self.load_overview(cx);
    }

    /// 趋势、分项、今天、归档健康一次拉齐；刷新期间保留旧数据。
    fn load_overview(&mut self, cx: &mut Context<Self>) {
        self.overview_revision += 1;
        let revision = self.overview_revision;
        self.overview_error = None;
        let source = self.source.clone();
        let trend_days = self.trend_days;
        let breakdown = self.token_period;
        self._overview_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    model::load_overview(source.as_ref(), trend_days, breakdown)
                })
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

    /// 工具行只留「刷新额度」。周期控件都放在各自卡片上，只管所在的卡片。
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            // 栖息地常驻左栏，这里不再重复；第一行是额度详情（三位住户各一列）。
                            .child(self.residents.clone())
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
