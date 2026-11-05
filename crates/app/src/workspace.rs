//! 应用外壳：标题栏（导航 / 监听状态 / 搜索）+ 按当前页切换归档 / 仪表盘 / 设置。
//!
//! 外壳创建并持有数据源、两个栖息地、实时信号与各页实体；页本身不互相引用。

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex,
    input::{Input, InputEvent, InputState},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, Styled as _,
    Subscription, Window, actions, div, px,
};

use crate::archive::ArchiveView;
use crate::assets::FONT_PIXEL_MONO;
use crate::dashboard::DashboardView;
use crate::data::DataSource;
use crate::format::{now_ms, relative_time};
use crate::habitat::{HabitatSize, HabitatView};
use crate::live::{LiveFeed, LiveState};

actions!(agenthome, [FocusSearch]);

/// 设计稿标题栏高 48。
const TITLE_BAR_HEIGHT: f32 = 48.;
/// 左栏宽；导航从中栏左缘起。
/// gpui-kit 的 TitleBar 在 macOS 上已为红绿灯留出的左内边距。
const TRAFFIC_LIGHT_INSET: f32 = if cfg!(target_os = "macos") { 80. } else { 12. };

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
}

pub struct Workspace {
    page: Page,
    live: Entity<LiveState>,
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
        let archive = cx
            .new(|cx| ArchiveView::new(source.clone(), live.clone(), habitat_compact, window, cx));
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
        ];
        Self {
            page: Page::Archive,
            live,
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
        let page_ix = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(1);
        TitleBar::new()
            .h(px(TITLE_BAR_HEIGHT))
            .child(
                h_flex()
                    .size_full()
                    .relative()
                    .pr(px(14.))
                    .gap_3()
                    // 导航放在一个向左扩出红绿灯内边距的绝对定位叠层里，按整个窗口宽度居中；
                    // 状态条与搜索框是后绘制的兄弟元素，命中优先级更高，不会被叠层挡住。
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(-TRAFFIC_LIGHT_INSET))
                            .right_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                TabBar::new("main-nav")
                                    .segmented()
                                    .selected_index(page_ix)
                                    .children(Page::ALL.map(|page| Tab::new().label(page.label())))
                                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                        if let Some(page) = Page::ALL.get(*ix) {
                                            this.page = *page;
                                            cx.notify();
                                        }
                                    })),
                            ),
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
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_page(cx)),
            )
    }
}
