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
    StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    kbd::Kbd,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, KeyBinding, Keystroke, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, actions, div, img,
    px,
};

use crate::archive::{ArchiveView, side_item};
use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS, PixelMono as _};
use crate::dashboard::DashboardView;
use crate::data::DataSource;
use crate::format::{countdown, now_ms, relative_time};
use crate::habitat::{HabitatSize, HabitatView, ResidentId, ResidentState};
use crate::live::{LiveFeed, LiveState, primary_window};
use crate::message_row::{provider_avatar, secondary};
use crate::search_palette::{PaletteEvent, SearchPalette};
use crate::settings::{SettingsEvent, SettingsView};
use crate::sync::{SyncEngine, SyncState};

actions!(agenthome, [OpenSearch]);

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

    fn label(self) -> String {
        match self {
            Page::Dashboard => t!("workspace.page_dashboard").into(),
            Page::Archive => t!("workspace.page_archive").into(),
            Page::Settings => t!("workspace.page_settings").into(),
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
        // 「接入」只看本机有没有这个来源的数据，与仪表盘额度卡、菜单栏弹窗同一判定（`LiveState::connected`）。
        let away = !live.connected(who);
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
            t!("workspace.plan_not_connected").to_string().into()
        } else {
            quota
                .map(|q| match (&q.plan, q.windows.is_empty()) {
                    (Some(plan), _) => plan.clone(),
                    // 有额度窗口但不知道套餐名：不能再说「未接入」。
                    (None, false) => t!("workspace.plan_subscription").to_string(),
                    (None, true) => t!("workspace.plan_no_quota").to_string(),
                })
                .unwrap_or_else(|| match who {
                    ResidentId::Gemini => t!("workspace.plan_connected").to_string(),
                    _ => t!("workspace.plan_no_quota").to_string(),
                })
                .into()
        };
        let (sub_label, sub_value): (SharedString, SharedString) = match (away, window) {
            (true, _) => (
                t!("workspace.connect_in_settings").to_string().into(),
                "".into(),
            ),
            // 各来源的额度信号不同：Claude 靠后台 /usage，Codex 靠 rollout 快照，Gemini 没有本地额度信号。
            (false, None) => (
                match who {
                    ResidentId::Claude => t!("workspace.waiting_claude"),
                    ResidentId::Codex => t!("workspace.waiting_codex"),
                    ResidentId::Gemini => t!("workspace.gemini_no_quota"),
                }
                .to_string()
                .into(),
                "".into(),
            ),
            (false, Some(w)) => (
                t!(
                    "workspace.window_resets",
                    window = crate::format::quota_window_label(w)
                )
                .to_string()
                .into(),
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
                            .child(div().pixel_mono(&sub_value, 14., cx).child(sub_value)),
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
        let caption = t!(
            "workspace.residents_caption",
            online = residents.len() - away,
            away = away
        )
        .to_string();
        // 事件气泡和在线人数都在小屋顶部，气泡出现时先让出位置。
        let show_caption = !self.habitat.read(cx).showing_toast();
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
                    .when(show_caption, |frame| {
                        frame.child(
                            div()
                                .absolute()
                                .left_2()
                                .top(px(6.))
                                .px(px(5.))
                                .py(px(1.))
                                .rounded(px(2.))
                                .bg(cx.theme().background.opacity(0.78))
                                .pixel_mono(&caption, 11., cx)
                                .text_color(cx.theme().primary)
                                .child(caption),
                        )
                    }),
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
    palette: Entity<SearchPalette>,
    focus_handle: FocusHandle,
    /// 同步引擎状态；mock 模式下没有引擎，保持默认值。
    sync: Entity<SyncState>,
    sync_engine: Option<SyncEngine>,
    /// 已经让页面重拉过的数据版本。
    seen_data_revision: u64,
    /// 已经让额度重读过的后台 /usage 成功时刻。
    seen_probe_ok_ms: Option<i64>,
    /// 顶行按下鼠标后、首次移动时开始拖窗口（同 gpui-kit TitleBar 的做法）。
    drag_pending: bool,
    feed: LiveFeed,
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
        cx.bind_keys([KeyBinding::new("cmd-k", OpenSearch, None)]);

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
        let palette = cx.new(|cx| SearchPalette::new(source.clone(), window, cx));
        let dashboard =
            cx.new(|cx| DashboardView::new(source, habitat_compact, live.clone(), window, cx));
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
            // ⌘K 面板选中一条：切到归档页，打开会话并定位到命中的消息。
            cx.subscribe_in(
                &palette,
                window,
                |this, _, event: &PaletteEvent, window, cx| match event {
                    PaletteEvent::Open {
                        session_id,
                        message_id,
                    } => {
                        this.page = Page::Archive;
                        this.archive.update(cx, |archive, cx| {
                            archive.open_session_at(
                                session_id.clone(),
                                message_id.clone(),
                                window,
                                cx,
                            )
                        });
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &settings,
                |this, _, event: &SettingsEvent, cx| match event {
                    SettingsEvent::LanguageChanged => {
                        this.dashboard
                            .update(cx, |dashboard, cx| dashboard.refresh_data(cx));
                        this.archive
                            .update(cx, |archive, cx| archive.refresh_data(cx));
                        cx.notify();
                    }
                },
            ),
            cx.observe(&live, |this, live, cx| {
                // 仪表盘的额度卡与左栏共用同一份快照（`set_quota` 内部去重，没变不重绘）。
                let quota = live.read(cx).quota.clone();
                this.dashboard
                    .update(cx, |dashboard, cx| dashboard.set_quota(quota, cx));
                cx.notify();
            }),
            // 左栏的「视图 / 项目」选中态随归档页状态变化，归档页重绘时外壳跟着重绘。
            cx.observe(&archive, |_, _, cx| cx.notify()),
            cx.observe(&sync, |this, sync, cx| {
                let revision = sync.read(cx).data_revision;
                let probe_ok = sync.read(cx).claude_probe.last_ok_ms;
                // 后台 /usage 刚读完新额度：不等 60 秒的额度轮询。
                if probe_ok != this.seen_probe_ok_ms {
                    this.seen_probe_ok_ms = probe_ok;
                    this.feed.request_quota();
                }
                if revision != this.seen_data_revision {
                    this.seen_data_revision = revision;
                    // 新会话数据可能带来 Codex rollout 里的新额度快照。
                    this.feed.request_quota();
                    this.dashboard
                        .update(cx, |dashboard, cx| dashboard.refresh_data(cx));
                    this.archive
                        .update(cx, |archive, cx| archive.refresh_data(cx));
                }
                cx.notify();
            }),
        ];
        // 启动即让外壳持有焦点，⌘K 等外壳级动作才有派发路径。
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            page: Page::Archive,
            live,
            residents,
            archive,
            dashboard,
            settings,
            palette,
            focus_handle,
            sync,
            sync_engine,
            seen_data_revision: 0,
            seen_probe_ok_ms: None,
            drag_pending: false,
            feed,
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
                t!(
                    "workspace.ingesting",
                    provider = scan.provider.as_str(),
                    done = scan.done,
                    total = scan.total
                )
                .to_string(),
            )
        } else if let Some(error) = sync.error.as_ref().or(live.error.as_ref()) {
            (cx.theme().danger, error.clone())
        } else if self.sync_engine.is_some() {
            match sync.last_sync_ms {
                Some(at) => (cx.theme().success, {
                    let count = sync.watching_count();
                    let time = relative_time(at, now);
                    if count == 1 {
                        t!("workspace.watching_one", count = count, time = time)
                    } else {
                        t!("workspace.watching_other", count = count, time = time)
                    }
                    .to_string()
                }),
                None => (
                    cx.theme().muted_foreground,
                    t!("workspace.sync_starting").to_string(),
                ),
            }
        } else {
            match live.refreshed_at_ms {
                Some(at) => (cx.theme().success, {
                    let count = live.detected_count();
                    let time = relative_time(at, now);
                    if count == 1 {
                        t!("workspace.demo_one", count = count, time = time)
                    } else {
                        t!("workspace.demo_other", count = count, time = time)
                    }
                    .to_string()
                }),
                None => (
                    cx.theme().muted_foreground,
                    t!("workspace.connecting").to_string(),
                ),
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
                        t!("workspace.sync_now").to_string()
                    } else {
                        t!("workspace.sync_demo").to_string()
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

    /// 弹出 ⌘K 全局搜索面板；已经开着浮层时不重复弹。
    /// 启动参数 `--page` / `--search` / `--session`：打开指定页面、弹出带查询的 ⌘K 面板，或打开某个会话。
    pub fn apply_startup(
        &mut self,
        page: Option<&str>,
        search: Option<String>,
        session: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = session {
            self.page = Page::Archive;
            self.archive.update(cx, |archive, cx| {
                archive.open_session_at(id, None, window, cx)
            });
        }
        match page {
            Some("dashboard") => self.page = Page::Dashboard,
            Some("settings") => self.page = Page::Settings,
            Some("archive") => self.page = Page::Archive,
            _ => {}
        }
        if let Some(query) = search {
            let (recent, label) = self.archive.read(cx).recent_for_palette();
            self.palette.update(cx, |palette, cx| {
                palette.show_with_query(recent, label, &query, window, cx)
            });
        }
        cx.notify();
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let (recent, label) = self.archive.read(cx).recent_for_palette();
        self.palette
            .update(cx, |palette, cx| palette.show(recent, label, window, cx));
    }

    /// 左栏搜索入口：长得像输入框，点击（或 ⌘K）弹出全局搜索面板。
    /// 图标、占位文字与 ⌘K 徽标放在同一行、统一垂直居中。
    fn render_search(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .px_3()
            .pb_2()
            .child(
                h_flex()
                    .id("sidebar-search")
                    .h(px(28.))
                    .px(px(8.))
                    .gap(px(6.))
                    .items_center()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().input)
                    .bg(cx.theme().background)
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().secondary))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        Icon::new(IconName::Search)
                            .small()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(t!("workspace.search").to_string()),
                    )
                    // 用 gpui-kit 的 Kbd：⌘ 与 K 同一种界面字体。像素字体 VT323 没有 ⌘ 字形，
                    // 会回退到系统字体，两个字的基线对不齐。
                    .children(
                        Keystroke::parse("cmd-k")
                            .ok()
                            .map(|stroke| Kbd::new(stroke).outline().flex_none()),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_search(window, cx))),
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
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
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

impl Workspace {
    /// 菜单栏弹窗用：同步状态与「立即同步」句柄（mock 下没有同步引擎，句柄为 None）。
    pub fn sync_link(&self) -> (Entity<SyncState>, Option<crate::sync::SyncHandle>) {
        (
            self.sync.clone(),
            self.sync_engine.as_ref().map(SyncEngine::handle),
        )
    }

    /// 菜单栏原生菜单用：实时信号与左栏栖息地（住户状态与左栏同源）。
    pub fn live_link(&self, cx: &gpui_kit::App) -> (Entity<LiveState>, Entity<HabitatView>) {
        let residents = self.residents.read(cx);
        (residents.live.clone(), residents.habitat.clone())
    }
}
