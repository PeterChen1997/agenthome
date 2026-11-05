//! 应用外壳：标题栏（当前页名 / 监听状态 / 搜索）+ 左侧固定导航栏 + 按当前页切换的内容区。
//!
//! 外壳创建并持有数据源、两个栖息地、实时信号与各页实体；页本身不互相引用。
//! 左栏公共部分（品牌、导航、栖息地与住户）归外壳；归档页专属的「视图 / 项目」由
//! `ArchiveView::render_sidebar_sections` 提供，点击仍由归档页处理。

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, TitleBar, h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, actions, div, img,
    px,
};

use crate::archive::{ArchiveView, side_item};
use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS};
use crate::dashboard::DashboardView;
use crate::data::DataSource;
use crate::format::{countdown, now_ms, relative_time};
use crate::habitat::{HabitatSize, HabitatView, ResidentId, ResidentState};
use crate::live::{LiveFeed, LiveState, primary_window};
use crate::message_row::{provider_avatar, secondary};

actions!(agenthome, [FocusSearch]);

/// 设计稿标题栏高 48。
const TITLE_BAR_HEIGHT: f32 = 48.;
/// 左栏宽，所有页面共用。
const SIDEBAR_WIDTH: f32 = 240.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Dashboard,
    Archive,
    Settings,
}

impl Page {
    const ALL: [Page; 3] = [Page::Dashboard, Page::Archive, Page::Settings];

    fn label(self) -> &'static str {
        match self {
            Page::Dashboard => "仪表盘",
            Page::Archive => "归档",
            Page::Settings => "设置",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::Dashboard => IconName::LayoutDashboard,
            Page::Archive => IconName::Inbox,
            Page::Settings => IconName::Settings,
        }
    }
}

/// 左栏的栖息地盒 + 三行住户：自己每秒刷新倒计时，不牵动整页。
pub struct ResidentPanel {
    live: Entity<LiveState>,
    habitat: Entity<HabitatView>,
    _ticker: Task<()>,
    _subscription: Subscription,
}

impl ResidentPanel {
    fn new(live: Entity<LiveState>, habitat: Entity<HabitatView>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&live, |_, _, cx| cx.notify());
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            live,
            habitat,
            _ticker: ticker,
            _subscription: subscription,
        }
    }

    fn render_resident(&self, who: ResidentId, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let live = self.live.read(cx);
        let residents = self.habitat.read(cx).residents();
        let away = residents[who.index()].state == ResidentState::Away;
        let quota = live.quota_for(who);
        let window = quota.and_then(primary_window);
        let hp = window
            .map(|w| (100.0 - w.used_percent).round().clamp(0.0, 100.0) as u8)
            .unwrap_or(0);
        let lit = usize::from(hp.div_ceil(10));
        let bar_color = if hp < 20 {
            cx.theme().danger
        } else if hp < 50 {
            cx.theme().warning
        } else {
            cx.theme().success
        };
        let plan: SharedString = if away {
            "未接入".into()
        } else {
            quota
                .and_then(|q| q.plan.clone())
                .unwrap_or_else(|| "未接入额度".to_string())
                .into()
        };
        let (sub_label, sub_value): (SharedString, SharedString) = match (away, window) {
            (true, _) => ("在设置里接入".into(), "".into()),
            (false, None) => ("装 statusline 后显示额度".into(), "".into()),
            (false, Some(w)) => (
                format!("{} 重置", w.label).into(),
                w.resets_at_ms
                    .map(|at| countdown(at - now))
                    .unwrap_or_else(|| "—".to_string())
                    .into(),
            ),
        };
        h_flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .when(away, |row| row.opacity(0.55))
            .child(
                div()
                    .flex_none()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(provider_avatar(&who_provider(who), 28.)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_baseline()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child(who.name()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(plan),
                            )
                            .when(window.is_some(), |row| {
                                row.child(
                                    div()
                                        .ml_auto()
                                        .font_family(FONT_PIXEL_MONO)
                                        .text_size(px(15.))
                                        .child(format!("{hp}%")),
                                )
                            }),
                    )
                    .child(h_flex().mt_1().gap(px(2.)).children((0..10).map(|ix| {
                        div().flex_1().h(px(7.)).rounded(px(1.)).bg(if ix < lit {
                            bar_color
                        } else {
                            cx.theme().border
                        })
                    })))
                    .child(
                        h_flex()
                            .mt(px(3.))
                            .justify_between()
                            .text_xs()
                            .text_color(secondary(cx))
                            .child(sub_label)
                            .child(
                                div()
                                    .font_family(FONT_PIXEL_MONO)
                                    .text_size(px(14.))
                                    .child(sub_value),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn who_provider(who: ResidentId) -> agenthome_core::model::ProviderId {
    use agenthome_core::model::ProviderId;
    match who {
        ResidentId::Claude => ProviderId::ClaudeCode,
        ResidentId::Codex => ProviderId::Codex,
        ResidentId::Gemini => ProviderId::GeminiCli,
    }
}

impl Render for ResidentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_ms();
        let residents = self.habitat.read(cx).residents();
        let away = residents
            .iter()
            .filter(|r| r.state == ResidentState::Away)
            .count();
        let caption = format!("{} 位在线 · {} 外出", residents.len() - away, away);
        v_flex()
            .w_full()
            .child(
                div()
                    .relative()
                    .mx_3()
                    .h(px(108.))
                    .rounded(cx.theme().radius)
                    .border_2()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .child(self.habitat.clone())
                    .child(
                        div()
                            .absolute()
                            .left_2()
                            .top(px(6.))
                            .px(px(5.))
                            .py(px(1.))
                            .rounded(px(2.))
                            .bg(cx.theme().background.opacity(0.78))
                            .font_family(FONT_PIXEL_MONO)
                            .text_size(px(11.))
                            .text_color(cx.theme().primary)
                            .child(caption),
                    ),
            )
            .child(
                v_flex()
                    .mt_2()
                    .children(ResidentId::ALL.map(|who| self.render_resident(who, now, cx))),
            )
    }
}

pub struct Workspace {
    page: Page,
    live: Entity<LiveState>,
    residents: Entity<ResidentPanel>,
    archive: Entity<ArchiveView>,
    dashboard: Entity<DashboardView>,
    search: Entity<InputState>,
    focus_handle: FocusHandle,
    _feed: LiveFeed,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// `demo` 为真时栖息地跑演示剧本（mock 数据源），否则只吃真实信号。
    pub fn new(
        source: Arc<dyn DataSource>,
        demo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title("AgentHome");
        cx.bind_keys([KeyBinding::new("cmd-k", FocusSearch, None)]);

        let habitat_compact = cx.new(|cx| {
            let mut view = HabitatView::new(HabitatSize::Compact, cx);
            view.set_demo(demo, cx);
            view
        });
        let habitat_hero = cx.new(|cx| {
            let mut view = HabitatView::new(HabitatSize::Hero, cx);
            view.set_demo(demo, cx);
            view
        });
        let live = cx.new(|_| LiveState::default());
        let feed = LiveFeed::start(
            source.clone(),
            vec![habitat_compact.clone(), habitat_hero.clone()],
            live.clone(),
            cx,
        );
        let residents = cx.new(|cx| ResidentPanel::new(live.clone(), habitat_compact, cx));
        let archive = cx.new(|cx| ArchiveView::new(source.clone(), window, cx));
        let dashboard = cx.new(|cx| DashboardView::new(source, habitat_hero, window, cx));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索标题、正文、工具输出…"));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, state, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value().to_string();
                    this.archive
                        .update(cx, |archive, cx| archive.set_search(text, cx));
                }
            }),
            cx.observe(&live, |_, _, cx| cx.notify()),
            // 左栏的「视图 / 项目」选中态随归档页状态变化，归档页重绘时外壳跟着重绘。
            cx.observe(&archive, |_, _, cx| cx.notify()),
        ];
        Self {
            page: Page::Archive,
            live,
            residents,
            archive,
            dashboard,
            search,
            focus_handle: cx.focus_handle(),
            _feed: feed,
            _subscriptions: subscriptions,
        }
    }

    fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let live = self.live.read(cx);
        let (dot, text) = match (&live.error, live.refreshed_at_ms) {
            (Some(error), _) => (cx.theme().danger, error.clone()),
            (None, Some(at)) => (
                cx.theme().success,
                format!(
                    "监听中 · {} 个来源 · {}",
                    live.detected_count(),
                    relative_time(at, now_ms())
                ),
            ),
            (None, None) => (cx.theme().muted_foreground, "正在接入信号…".to_string()),
        };
        h_flex()
            .gap(px(6.))
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(div().size_2().bg(dot))
            .child(div().max_w(px(320.)).truncate().child(text))
            .into_any_element()
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        TitleBar::new()
            .h(px(TITLE_BAR_HEIGHT))
            .child(
                h_flex()
                    .size_full()
                    .pr(px(14.))
                    .gap_3()
                    // 红绿灯右侧放当前页名，弱化色小字，填掉导航移走后的空缺。
                    .child(
                        div()
                            .font_family(FONT_PIXEL_SANS)
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.page.label()),
                    )
                    .child(div().flex_1())
                    .child(self.render_status(cx))
                    .child(
                        div().w(px(300.)).child(
                            Input::new(&self.search)
                                .small()
                                .prefix(Icon::new(IconName::Search).small())
                                .suffix(
                                    div()
                                        .px(px(5.))
                                        .h(px(18.))
                                        .flex()
                                        .items_center()
                                        .rounded(px(2.))
                                        .border_1()
                                        .border_color(cx.theme().border)
                                        .bg(cx.theme().group_box)
                                        .font_family(FONT_PIXEL_MONO)
                                        .text_size(px(14.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("⌘K"),
                                )
                                .cleanable(true),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 左栏：品牌行、导航、栖息地与住户，归档页再接「视图 / 项目」。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let page_sections = (self.page == Page::Archive).then(|| {
            self.archive
                .update(cx, |archive, cx| archive.render_sidebar_sections(cx))
        });
        v_flex()
            .id("workspace-sidebar")
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().secondary)
            .pb_3()
            .child(
                h_flex()
                    .gap_2()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(
                        img("avatars/app-icon.png")
                            .size(px(22.))
                            .rounded(cx.theme().radius),
                    )
                    .child(
                        div()
                            .font_family(FONT_PIXEL_SANS)
                            .text_size(px(16.))
                            .font_semibold()
                            .text_color(cx.theme().primary)
                            .child("AgentHome"),
                    ),
            )
            .child(v_flex().pb_2().children(Page::ALL.map(|page| {
                side_item(
                    ("nav", page as usize),
                    Some(page.icon()),
                    page.label(),
                    None,
                    self.page == page,
                    cx.listener(move |this, _, _, cx| {
                        if this.page != page {
                            this.page = page;
                            cx.notify();
                        }
                    }),
                    cx,
                )
            })))
            .child(self.residents.clone())
            .when_some(page_sections, |col, sections| col.child(sections))
            .into_any_element()
    }

    fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Archive => self.archive.clone().into_any_element(),
            Page::Dashboard => self.dashboard.clone().into_any_element(),
            Page::Settings => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child("设置（待实现）")
                .child(
                    div()
                        .text_sm()
                        .child("来源接入、statusline 脚本安装与清理策略将放在这里。"),
                )
                .into_any_element(),
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(self.render_page(cx)),
                    ),
            )
    }
}
