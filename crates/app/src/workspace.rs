//! 应用外壳：Notion 式两栏，不再有整条标题栏。
//!
//! 左栏从窗口顶端开始（参照 Notion）：顶行只有红绿灯与右侧「立即同步」图标（空白处可拖动窗口、
//! 双击缩放），下面固定搜索框，再往下是栖息地与住户、导航、页面专属区；最底部是品牌 + 同步状态行。
//! 右侧内容区从窗口顶端开始。
//! 外壳创建并持有数据源、两个栖息地、实时信号与各页实体；页本身不互相引用。
//! 归档页专属的「视图 / 项目」由 `ArchiveView::render_sidebar_sections` 提供，点击仍由归档页处理。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Sizable as _,
    StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    actions, div, img, px,
};

use crate::archive::{ArchiveView, side_item};
use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS};
use crate::dashboard::DashboardView;
use crate::data::DataSource;
use crate::format::{countdown, now_ms, relative_time};
use crate::habitat::{HabitatSize, HabitatView, ResidentId, ResidentState};
use crate::live::{LiveFeed, LiveState, primary_window};
use crate::message_row::{provider_avatar, secondary};
use crate::settings::SettingsView;
use crate::sync::{SyncEngine, SyncState};

actions!(agenthome, [FocusSearch]);

/// 左栏宽，所有页面共用。
pub(crate) const SIDEBAR_WIDTH: f32 = 240.;
/// 左栏顶行高：只放红绿灯与右侧图标。红绿灯位置见 `main.rs` 的 `TRAFFIC_LIGHT_POSITION`。
pub(crate) const TOP_ROW_HEIGHT: f32 = 44.;

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
                .map(|q| match (&q.plan, q.windows.is_empty()) {
                    (Some(plan), _) => plan.clone(),
                    // 有额度窗口但不知道套餐名：不能再说「未接入」。
                    (None, false) => "订阅".to_string(),
                    (None, true) => "未接入额度".to_string(),
                })
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
    settings: Entity<SettingsView>,
    search: Entity<InputState>,
    focus_handle: FocusHandle,
    /// 同步引擎状态；mock 模式下没有引擎，保持默认值。
    sync: Entity<SyncState>,
    sync_engine: Option<SyncEngine>,
    /// 已经让页面重拉过的数据版本。
    seen_data_revision: u64,
    /// 顶行按下鼠标后、首次移动时开始拖窗口（同 gpui-kit TitleBar 的做法）。
    drag_pending: bool,
    _feed: LiveFeed,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// `demo` 为真时栖息地跑演示剧本（mock 数据源），否则只吃真实信号。
    /// `sync_db` 为索引库路径时启动同步引擎，把来源目录的新内容实时入库。
    pub fn new(
        source: Arc<dyn DataSource>,
        demo: bool,
        sync_db: Option<PathBuf>,
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
        let live = cx.new(|_| LiveState::default());
        let feed = LiveFeed::start(
            source.clone(),
            vec![habitat_compact.clone()],
            live.clone(),
            cx,
        );
        let residents = cx.new(|cx| ResidentPanel::new(live.clone(), habitat_compact.clone(), cx));
        let archive = cx.new(|cx| ArchiveView::new(source.clone(), window, cx));
        let dashboard = cx.new(|cx| DashboardView::new(source, habitat_compact, window, cx));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索会话"));
        let sync = cx.new(|_| SyncState::default());
        let sync_engine = sync_db
            .clone()
            .map(|db| SyncEngine::start(db, sync.clone(), cx));
        let settings = cx.new(|cx| {
            SettingsView::new(
                sync.clone(),
                sync_engine.as_ref().map(SyncEngine::handle),
                live.clone(),
                sync_db,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, state, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value().to_string();
                    // 搜索只作用于归档：在别的页输入时自动切过去，结果立刻可见。
                    if !text.trim().is_empty() && this.page != Page::Archive {
                        this.page = Page::Archive;
                    }
                    this.archive
                        .update(cx, |archive, cx| archive.set_search(text, cx));
                    cx.notify();
                }
            }),
            cx.observe(&live, |this, live, cx| {
                // 仪表盘的额度卡与左栏共用这一份 5 秒一拉的快照。
                let quota = live.read(cx).quota.clone();
                this.dashboard
                    .update(cx, |dashboard, cx| dashboard.set_quota(quota, cx));
                cx.notify();
            }),
            // 左栏的「视图 / 项目」选中态随归档页状态变化，归档页重绘时外壳跟着重绘。
            cx.observe(&archive, |_, _, cx| cx.notify()),
            cx.observe(&sync, |this, sync, cx| {
                let revision = sync.read(cx).data_revision;
                if revision != this.seen_data_revision {
                    this.seen_data_revision = revision;
                    this.dashboard
                        .update(cx, |dashboard, cx| dashboard.refresh_data(cx));
                    this.archive
                        .update(cx, |archive, cx| archive.refresh_data(cx));
                }
                cx.notify();
            }),
        ];
        Self {
            page: Page::Archive,
            live,
            residents,
            archive,
            dashboard,
            settings,
            search,
            focus_handle: cx.focus_handle(),
            sync,
            sync_engine,
            seen_data_revision: 0,
            drag_pending: false,
            _feed: feed,
            _subscriptions: subscriptions,
        }
    }

    /// 左栏底部状态：扫描进度 > 同步 / 信号错误 > 监听中。
    fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let live = self.live.read(cx);
        let sync = self.sync.read(cx);
        let now = now_ms();
        let (dot, text) = if let Some(scan) = &sync.scanning {
            (
                cx.theme().warning,
                format!(
                    "正在入库 {} {}/{}",
                    scan.provider.as_str(),
                    scan.done,
                    scan.total
                ),
            )
        } else if let Some(error) = sync.error.as_ref().or(live.error.as_ref()) {
            (cx.theme().danger, error.clone())
        } else if self.sync_engine.is_some() {
            match sync.last_sync_ms {
                Some(at) => (
                    cx.theme().success,
                    format!(
                        "监听中 · {} 个来源 · {}同步",
                        sync.watching_count(),
                        relative_time(at, now)
                    ),
                ),
                None => (cx.theme().muted_foreground, "正在启动同步…".to_string()),
            }
        } else {
            match live.refreshed_at_ms {
                Some(at) => (
                    cx.theme().success,
                    format!(
                        "演示数据 · {} 个来源 · {}",
                        live.detected_count(),
                        relative_time(at, now)
                    ),
                ),
                None => (cx.theme().muted_foreground, "正在接入信号…".to_string()),
            }
        };
        // 底部品牌行（同 Notion 左下角的工作区行）：图标 + 名字，名字下一行是同步状态。
        h_flex()
            .id("sidebar-footer")
            .flex_none()
            .gap(px(10.))
            .px_3()
            .h(px(52.))
            .border_t_1()
            .border_color(cx.theme().secondary)
            .child(
                img("avatars/app-icon.png")
                    .flex_none()
                    .size(px(26.))
                    .rounded(cx.theme().radius),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_family(FONT_PIXEL_SANS)
                            .text_size(px(14.))
                            .font_semibold()
                            .text_color(cx.theme().primary)
                            .child("AgentHome"),
                    )
                    .child(
                        h_flex()
                            .gap(px(5.))
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(div().flex_none().size(px(6.)).bg(dot))
                            .child(div().flex_1().min_w_0().truncate().child(text.clone())),
                    ),
            )
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
            })
            .into_any_element()
    }

    /// 左栏顶行：红绿灯 + 右侧「立即同步」图标。按钮以外的空白可拖动窗口、双击缩放；
    /// 拖动区域不包住按钮，免得点按钮时被当成拖窗口。
    fn render_top_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let sync = self.sync_engine.as_ref().map(SyncEngine::handle);
        let scanning = self.sync.read(cx).scanning.is_some();
        h_flex()
            .flex_none()
            .h(px(TOP_ROW_HEIGHT))
            .pr(px(10.))
            .child(
                div()
                    .id("sidebar-drag-area")
                    .flex_1()
                    .h_full()
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.drag_pending = true),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.drag_pending = false),
                    )
                    .on_mouse_down_out(cx.listener(|this, _, _, _| this.drag_pending = false))
                    .on_mouse_move(cx.listener(|this, _, window, _| {
                        if this.drag_pending {
                            this.drag_pending = false;
                            window.start_window_move();
                        }
                    })),
            )
            .child(
                Button::new("sync-now")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip(if sync.is_some() {
                        "立即同步全部来源"
                    } else {
                        "演示数据没有同步"
                    })
                    .loading(scanning)
                    .disabled(sync.is_none())
                    .on_click(move |_, _, _| {
                        if let Some(sync) = &sync {
                            sync.scan_all();
                        }
                    }),
            )
            .into_any_element()
    }

    /// 左栏搜索框：搜标题、正文与工具输出，结果在归档页；⌘K 聚焦。
    fn render_search(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .px_3()
            .pb_2()
            .child(
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
            )
            .into_any_element()
    }

    /// 左栏：顶行（红绿灯 + 立即同步）→ 搜索 → 栖息地与住户 → 导航 → 归档页的「视图 / 项目」，
    /// 底部固定品牌与同步状态。顶行、搜索与底部固定，中间可滚动。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let page_sections = (self.page == Page::Archive).then(|| {
            self.archive
                .update(cx, |archive, cx| archive.render_sidebar_sections(cx))
        });
        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().secondary)
            .child(self.render_top_row(cx))
            .child(self.render_search(cx))
            .child(
                v_flex()
                    .id("workspace-sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pt_1()
                    .pb_3()
                    .child(self.residents.clone())
                    .child(v_flex().pt_3().pb_2().children(Page::ALL.map(|page| {
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
                    .when_some(page_sections, |col, sections| col.child(sections)),
            )
            .child(self.render_status(cx))
            .into_any_element()
    }

    fn render_page(&self) -> AnyElement {
        match self.page {
            Page::Archive => self.archive.clone().into_any_element(),
            Page::Dashboard => self.dashboard.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
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
            .items_stretch()
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .child(self.render_page()),
            )
    }
}
