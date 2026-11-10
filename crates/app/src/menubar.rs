//! 菜单栏（状态栏）：常驻图标 + 点击弹出的各 Agent 使用概要（`docs/design/MenuBar.dc.html`）。
//!
//! - `MenuBarPanel`：弹窗内容（gpui 视图）。自带一个栖息地与一份 `LiveFeed`，主窗口隐藏时也照常更新；
//!   `--menubar-popup` 把它开成普通窗口，供截图与端到端核对。
//! - `controller`（仅 macOS）：持有 `NSStatusItem`，点击图标在其下方开 / 关 `WindowKind::PopUp` 窗口；
//!   主窗口的关闭改成隐藏，app 常驻菜单栏。gpui 没有状态栏 API，原生部分在 `menubar/status_item.rs`。

#[cfg(target_os = "macos")]
mod status_item;

use std::sync::Arc;
use std::time::Duration;

use agenthome_core::model::{Period, ProviderId, QuotaSnapshot, QuotaWindow, SessionQuery};
use agenthome_core::pressure::{PressureReading, worst_pressure};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, img, px,
};

use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS};
use crate::data::DataSource;
use crate::format::{
    countdown, now_ms, quota_window_label, relative_time, short_cost, short_tokens, thousands,
};
use crate::habitat::{HabitatSize, HabitatView, ResidentId, ResidentState};
use crate::live::{LiveFeed, LiveState, primary_window, resident_of, shown_state};
use crate::pressure_badge::PressureBadge;
use crate::sync::{SyncHandle, SyncState};

#[cfg(target_os = "macos")]
pub use controller::{install, keep_running_on_close, reopen};

/// 弹窗尺寸（逻辑像素）；设计稿 360×640，按实际内容略增高。
pub const POPUP_WIDTH: f32 = 360.;
pub const POPUP_HEIGHT: f32 = 700.;
/// 「最近」列出的会话数。
const RECENT_COUNT: usize = 3;
/// 弹窗可见时刷新倒计时与「N 分钟前」的间隔。
const TICK: Duration = Duration::from_secs(1);

/// 某个 Agent（或全部）今天的用量。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DayUsage {
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub sessions: u64,
}

/// 「最近」里的一条会话。
#[derive(Clone, Debug, PartialEq)]
pub struct RecentSession {
    pub id: String,
    pub title: String,
    pub who: Option<ResidentId>,
    pub updated_at_ms: i64,
}

/// 弹窗里来自索引库的部分：今日汇总、各 Agent 今日用量、最近会话。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub total: DayUsage,
    /// 按 `ResidentId::index()` 排列。
    pub agents: [DayUsage; 3],
    pub recent: Vec<RecentSession>,
}

fn provider_of(who: ResidentId) -> ProviderId {
    match who {
        ResidentId::Claude => ProviderId::ClaudeCode,
        ResidentId::Codex => ProviderId::Codex,
        ResidentId::Gemini => ProviderId::GeminiCli,
    }
}

/// 读今日汇总与最近会话（后台线程调用）。子代理会话不进「最近」，与归档列表的折叠一致。
pub fn load_overview(source: &dyn DataSource) -> anyhow::Result<Overview> {
    let today = |provider: Option<ProviderId>| {
        source
            .totals(Period::Today, provider)
            .map(|totals| DayUsage {
                output_tokens: totals.usage.output_tokens,
                cost_usd: totals.cost_usd,
                sessions: totals.sessions,
            })
    };
    let total = today(None)?;
    let mut agents = [DayUsage::default(); 3];
    for who in ResidentId::ALL {
        agents[who.index()] = today(Some(provider_of(who)))?;
    }
    let recent = source
        .sessions(&SessionQuery {
            limit: 20,
            ..Default::default()
        })?
        .into_iter()
        .filter(|session| session.parent_session_id.is_none())
        .take(RECENT_COUNT)
        .map(|session| RecentSession {
            who: resident_of(&session.provider),
            id: session.id,
            title: session.title,
            updated_at_ms: session.updated_at_ms,
        })
        .collect();
    Ok(Overview {
        total,
        agents,
        recent,
    })
}

/// 一个额度窗口的用量。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowUse {
    /// 短标签：5h / 7d（按窗口分钟数推出，不依赖来源文案的语言）。
    pub label: String,
    pub used_percent: f32,
    pub resets_in_ms: Option<i64>,
}

/// Codex 的信用额度与重置额度。
#[derive(Clone, Debug, PartialEq)]
pub struct CreditsLine {
    pub balance: Option<String>,
    pub unlimited: bool,
    /// 未用的重置额度张数（设置页手动查询后才有）。
    pub reset_credits: Option<u32>,
    pub expires_in_ms: Option<i64>,
}

/// 压力最大的那个窗口。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPressure {
    /// 不是主窗口时为它的短标签（「7d」），是主窗口时为 None（不必重复窗口名）。
    pub label: Option<String>,
    /// 完整窗口名（「7 天」），悬停说明用。
    pub window: String,
    pub reading: PressureReading,
}

/// 住户行要显示的额度：主窗口 + 最吃紧的另一个窗口 + 信用 + 使用压力。
#[derive(Clone, Debug, PartialEq)]
pub struct AgentQuota {
    pub primary: WindowUse,
    pub secondary: Option<WindowUse>,
    pub credits: Option<CreditsLine>,
    pub pressure: Option<WindowPressure>,
}

/// 窗口短标签：300 分钟 → 5h，10080 分钟 → 7d，单模型窗口带上模型名；没有分钟数时退回来源给的标签。
pub fn window_short_label(window: &QuotaWindow) -> String {
    // 「7 天 (Fable)」这类单模型窗口：括号里的模型名接在短标签后，免得和总量窗口同名。
    let model = window
        .label
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(model, _)| model.trim())
        .filter(|model| !model.is_empty());
    let short = match window.window_minutes {
        Some(minutes) if minutes > 0 && minutes.is_multiple_of(1_440) => {
            format!("{}d", minutes / 1_440)
        }
        Some(minutes) if minutes > 0 && minutes.is_multiple_of(60) => format!("{}h", minutes / 60),
        Some(minutes) if minutes > 0 => format!("{minutes}m"),
        _ => return window.label.clone(),
    };
    match model {
        Some(model) => format!("{short} {model}"),
        None => short,
    }
}

fn window_use(window: &QuotaWindow, now_ms: i64) -> WindowUse {
    WindowUse {
        label: window_short_label(window),
        used_percent: window.used_percent.clamp(0.0, 100.0),
        resets_in_ms: window.resets_at_ms.map(|at| at - now_ms),
    }
}

/// 从额度快照组装住户行的额度部分；没有任何窗口时为 None。
pub fn agent_quota(snapshot: &QuotaSnapshot, now_ms: i64) -> Option<AgentQuota> {
    let primary = primary_window(snapshot)?;
    // 其余窗口里取已用最多的那个（Claude 可能同时有 7 天总量与 7 天单模型）。
    let secondary = snapshot
        .windows
        .iter()
        .filter(|window| window.id != primary.id)
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
        .map(|window| window_use(window, now_ms));
    let credits = snapshot
        .credits
        .as_ref()
        .filter(|credits| {
            credits.has_credits || credits.unlimited || credits.reset_credits.is_some_and(|n| n > 0)
        })
        .map(|credits| CreditsLine {
            balance: credits.balance.clone(),
            unlimited: credits.unlimited,
            reset_credits: credits.reset_credits.filter(|n| *n > 0),
            expires_in_ms: credits.expires_at_ms.map(|at| at - now_ms),
        });
    let pressure =
        worst_pressure(&snapshot.windows, now_ms).map(|(window, reading)| WindowPressure {
            label: (window.id != primary.id).then(|| window_short_label(window)),
            window: quota_window_label(window),
            reading,
        });
    Some(AgentQuota {
        primary: window_use(primary, now_ms),
        secondary,
        credits,
        pressure,
    })
}

/// 用量档位，阈值与主窗口体力条一致（体力 < 50 琥珀、< 20 红）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageLevel {
    Ok,
    Warn,
    Danger,
}

pub fn usage_level(used_percent: f32) -> UsageLevel {
    if used_percent > 80.0 {
        UsageLevel::Danger
    } else if used_percent > 50.0 {
        UsageLevel::Warn
    } else {
        UsageLevel::Ok
    }
}

/// 10 格体力条里点亮几格：按剩余比例，向上取整，还剩一点就至少亮一格（与主窗口体力条同向）。
pub fn lit_cells(used_percent: f32) -> usize {
    let left = 100.0 - used_percent.clamp(0.0, 100.0);
    (left / 10.0).ceil() as usize
}

/// Cocoa 全局坐标里的矩形：原点在主屏左下角，y 向上（`NSWindow.frame` / `NSScreen.frame`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 弹窗离屏幕边缘至少留这么宽。
const SCREEN_MARGIN: f64 = 8.0;
/// 弹窗顶边与状态栏图标底边的间距。
const ITEM_GAP: f64 = 4.0;

/// 弹窗左上角，相对所在屏幕的左上角、y 向下——即 gpui 在指定 `display_id` 时 `Bounds` 的坐标系
/// （见 gpui-pre-macos `MacWindow::open`：`screen.frame.origin.y + (屏高 − bounds.y)`）。
/// 水平居中在图标下方，左右不出屏；竖直贴在图标下方，太矮时整体上移到屏内。
pub fn popup_origin(item: ScreenRect, screen: ScreenRect, width: f64, height: f64) -> (f64, f64) {
    let centered = item.x + item.width / 2.0 - width / 2.0 - screen.x;
    let max_left = (screen.width - width - SCREEN_MARGIN).max(SCREEN_MARGIN);
    let left = centered.clamp(SCREEN_MARGIN, max_left);
    // 图标底边离屏幕顶边的距离 = 屏幕顶边 y − 图标底边 y。
    let below_item = screen.y + screen.height - item.y + ITEM_GAP;
    let top = below_item.clamp(0.0, (screen.height - height).max(0.0));
    (left, top)
}

/// 弹窗里的交互，交给菜单栏控制器处理（`--menubar-popup` 下无人订阅，点了不生效）。
#[derive(Clone, Debug, PartialEq)]
pub enum MenuBarEvent {
    OpenMain,
    OpenSettings,
    OpenSession(String),
    /// 弹窗失焦后已自行关闭。
    Closed,
}

/// 弹窗内容。
pub struct MenuBarPanel {
    source: Arc<dyn DataSource>,
    live: Entity<LiveState>,
    habitat: Entity<HabitatView>,
    feed: LiveFeed,
    demo: bool,
    sync: Option<Entity<SyncState>>,
    sync_handle: Option<SyncHandle>,
    /// 已经处理过的（数据版本，/usage 成功时刻）。
    seen_sync: Option<(u64, Option<i64>)>,
    overview: Option<Overview>,
    error: Option<String>,
    visible: bool,
    _loading: Option<Task<()>>,
    _ticker: Option<Task<()>>,
    _window_activation: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MenuBarEvent> for MenuBarPanel {}

impl MenuBarPanel {
    /// `demo` 为真时栖息地跑演示剧本（mock 数据源）。
    pub fn new(source: Arc<dyn DataSource>, demo: bool, cx: &mut Context<Self>) -> Self {
        let habitat = cx.new(|cx| {
            let mut view = HabitatView::new(HabitatSize::Compact, cx);
            view.set_demo(demo, cx);
            view
        });
        let live = cx.new(|_| LiveState::default());
        let feed = LiveFeed::start(source.clone(), vec![habitat.clone()], live.clone(), cx);
        let subscriptions = vec![cx.observe(&live, |_, _, cx| cx.notify())];
        Self {
            source,
            live,
            habitat,
            feed,
            demo,
            sync: None,
            sync_handle: None,
            seen_sync: None,
            overview: None,
            error: None,
            visible: false,
            _loading: None,
            _ticker: None,
            _window_activation: None,
            _subscriptions: subscriptions,
        }
    }

    /// 接上主窗口的同步引擎：显示同步状态，新数据入库后重读额度与今日统计。
    pub fn attach_sync(
        &mut self,
        sync: Entity<SyncState>,
        handle: Option<SyncHandle>,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.observe(&sync, |this, sync, cx| {
            let state = sync.read(cx);
            let seen = (state.data_revision, state.claude_probe.last_ok_ms);
            if this.seen_sync != Some(seen) {
                this.seen_sync = Some(seen);
                this.feed.request_quota();
                if this.visible {
                    this.refresh(cx);
                }
            }
            cx.notify();
        });
        self._subscriptions.push(subscription);
        self.sync = Some(sync);
        self.sync_handle = handle;
        cx.notify();
    }

    /// 弹窗打开 / 关闭：打开时重读今日统计并每秒刷新倒计时，关闭时停掉。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.visible = visible;
        if !visible {
            self._ticker = None;
            self._window_activation = None;
            return;
        }
        self.feed.request_quota();
        self.refresh(cx);
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    /// 弹窗失焦（点了别的窗口或别的 app）时关掉自己并通知控制器。
    pub fn close_on_deactivate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._window_activation = Some(cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() {
                window.remove_window();
                cx.emit(MenuBarEvent::Closed);
            }
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        self._loading = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_overview(source.as_ref()) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(overview) => {
                        this.overview = Some(overview);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn sync_label(&self, now: i64, cx: &App) -> SharedString {
        let state = self.sync.as_ref().map(|sync| sync.read(cx));
        if let Some(scan) = state.and_then(|state| state.scanning.as_ref()) {
            return t!("menubar.syncing", done = scan.done, total = scan.total).into();
        }
        if let Some(at) = state.and_then(|state| state.last_sync_ms) {
            return t!("menubar.synced_ago", ago = relative_time(at, now)).into();
        }
        if self.demo {
            return t!("menubar.demo_data").into();
        }
        t!("menubar.not_synced").into()
    }

    fn render_header(&self, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let chip_bg = cx.theme().background.opacity(0.78);
        // 栖息地的事件气泡也在顶部居中，出现时让出位置（同左栏的在线人数）。
        let show_chips = !self.habitat.read(cx).showing_toast();
        div()
            .relative()
            .flex_none()
            .w_full()
            .h(px(164.))
            .rounded(px(4.))
            .border_2()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(self.habitat.clone())
            .when(show_chips, |frame| {
                frame
                    .child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .top(px(6.))
                            .px(px(7.))
                            .py(px(2.))
                            .rounded(px(2.))
                            .bg(chip_bg)
                            .font_family(FONT_PIXEL_SANS)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(13.))
                            .text_color(cx.theme().primary)
                            .child("AgentHome"),
                    )
                    .child(
                        div()
                            .absolute()
                            .right(px(8.))
                            .top(px(6.))
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(2.))
                            .bg(chip_bg)
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(self.sync_label(now, cx)),
                    )
            })
            .into_any_element()
    }

    fn render_tiles(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.overview.as_ref().map(|overview| overview.total);
        let value = |text: Option<String>| text.unwrap_or_else(|| "—".to_string());
        let tile = |label: SharedString, text: String, color: Hsla, cx: &App| {
            v_flex()
                .flex_1()
                .gap(px(2.))
                .px(px(10.))
                .py(px(8.))
                .rounded(px(3.))
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .font_family(FONT_PIXEL_MONO)
                        .text_size(px(22.))
                        .line_height(px(22.))
                        .text_color(color)
                        .child(text),
                )
        };
        h_flex()
            .flex_none()
            .gap(px(8.))
            .child(tile(
                t!("menubar.tile_cost").into(),
                value(total.map(|t| short_cost(t.cost_usd))),
                cx.theme().primary,
                cx,
            ))
            .child(tile(
                t!("menubar.tile_output").into(),
                value(total.map(|t| short_tokens(t.output_tokens))),
                cx.theme().foreground,
                cx,
            ))
            .child(tile(
                t!("menubar.tile_sessions").into(),
                value(total.map(|t| thousands(t.sessions))),
                cx.theme().foreground,
                cx,
            ))
            .into_any_element()
    }

    fn render_agent(&self, who: ResidentId, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let state = self.habitat.read(cx).residents()[who.index()].state;
        // 「是否接入」与主窗口左栏、仪表盘额度卡同一判定（`LiveState::connected`）。
        let shown = shown_state(self.live.read(cx).connected(who), state);
        let away = shown.is_none();
        let quota = if away {
            None
        } else {
            self.live
                .read(cx)
                .quota_for(who)
                .and_then(|snapshot| agent_quota(snapshot, now))
        };
        let today = self
            .overview
            .as_ref()
            .map(|overview| overview.agents[who.index()]);
        // 状态叫法与主窗口一致（`ResidentState::label`）。
        let status: SharedString = match shown {
            None => t!("menubar.status_away").into(),
            Some(state) => state.label().into(),
        };
        let status_color = match shown {
            Some(ResidentState::Working | ResidentState::Thinking) => cx.theme().success,
            Some(ResidentState::Tired) => cx.theme().danger,
            _ => cx.theme().muted_foreground,
        };
        let used = quota.as_ref().map(|q| q.primary.used_percent);
        let level_color = |used: f32, cx: &App| match usage_level(used) {
            UsageLevel::Ok => cx.theme().success,
            UsageLevel::Warn => cx.theme().warning,
            UsageLevel::Danger => cx.theme().danger,
        };
        let bar_color = used.map_or(cx.theme().border, |used| level_color(used, cx));
        let lit = used.map_or(0, lit_cells);

        let left: SharedString = match (&quota, away) {
            (_, true) => t!("menubar.not_connected_hint").into(),
            (Some(quota), false) => match quota.primary.resets_in_ms {
                Some(ms) => t!("menubar.resets_in", time = countdown(ms)).into(),
                None => SharedString::default(),
            },
            (None, false) => match who {
                ResidentId::Claude => t!("menubar.no_quota_claude"),
                ResidentId::Codex => t!("menubar.no_quota_codex"),
                ResidentId::Gemini => t!("menubar.no_quota_gemini"),
            }
            .into(),
        };
        let today_text: Option<SharedString> = today.filter(|_| !away).map(|today| {
            t!(
                "menubar.today_line",
                tokens = short_tokens(today.output_tokens),
                cost = short_cost(today.cost_usd)
            )
            .into()
        });
        // 重置额度 3 天内过期：抢在第二个窗口前面显示，并着色提醒（1 天内红、3 天内琥珀）。
        let credit_alert = quota
            .as_ref()
            .and_then(|q| q.credits.as_ref())
            .filter(|credits| credits.reset_credits.is_some())
            .and_then(|credits| credits.expires_in_ms)
            .and_then(|ms| {
                if ms < 86_400_000 {
                    Some(cx.theme().danger)
                } else if ms < 3 * 86_400_000 {
                    Some(cx.theme().warning)
                } else {
                    None
                }
            });
        let right: SharedString = match &quota {
            Some(AgentQuota {
                secondary: Some(window),
                ..
            }) if credit_alert.is_none() => {
                let percent = (100.0 - window.used_percent).round();
                match window.resets_in_ms {
                    Some(ms) => t!(
                        "menubar.secondary_window",
                        window = window.label,
                        percent = percent,
                        time = countdown(ms)
                    ),
                    None => t!(
                        "menubar.secondary_window_no_reset",
                        window = window.label,
                        percent = percent
                    ),
                }
                .into()
            }
            Some(AgentQuota {
                credits: Some(credits),
                ..
            }) if credits.reset_credits.is_some() => {
                let count = credits.reset_credits.unwrap_or_default();
                let amount: String = t!("menubar.reset_credits", count = count).into();
                match credits.expires_in_ms {
                    Some(ms) if ms > 0 => format!(
                        "{amount} · {}",
                        t!("menubar.credits_expire", time = countdown(ms))
                    )
                    .into(),
                    _ => amount.into(),
                }
            }
            Some(AgentQuota {
                credits: Some(credits),
                ..
            }) => {
                let amount: String = if credits.unlimited {
                    t!("menubar.credits_unlimited").into()
                } else {
                    t!(
                        "menubar.credits",
                        balance = credits.balance.clone().unwrap_or_default()
                    )
                    .into()
                };
                match credits.expires_in_ms {
                    Some(ms) if ms > 0 => format!(
                        "{amount} · {}",
                        t!("menubar.credits_expire", time = countdown(ms))
                    )
                    .into(),
                    _ => amount.into(),
                }
            }
            _ => SharedString::default(),
        };

        h_flex()
            .gap(px(10.))
            .py(px(7.))
            .when(who != ResidentId::Claude, |row| {
                row.border_t_1().border_color(cx.theme().border)
            })
            .when(away, |row| row.opacity(0.55))
            .child(
                img(format!("avatars/avatar-{}.png", who.slug()))
                    .flex_none()
                    .size(px(30.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(cx.theme().border),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_baseline()
                            .gap(px(6.))
                            .text_size(px(12.5))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(who.name()))
                            .when_some(quota.as_ref(), |line, quota| {
                                line.child(
                                    div()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(quota.primary.label.clone()),
                                )
                            })
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(status_color)
                                    .child(status),
                            )
                            .when_some(used, |line, used| {
                                // 模板按 %{percent} 切开：英文「58% left」、中文「剩余 58%」。
                                // 文字用正文字体（VT323 没有中文字形），数字用像素字体。
                                let template = t!("menubar.left", percent = "\u{0}").to_string();
                                let (before, after) =
                                    template.split_once('\u{0}').unwrap_or(("", &template));
                                let word = |text: &str| {
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(text.trim().to_string())
                                };
                                line.child(
                                    h_flex()
                                        .ml_auto()
                                        .items_baseline()
                                        .gap(px(4.))
                                        .when(!before.trim().is_empty(), |row| {
                                            row.child(word(before))
                                        })
                                        .child(
                                            div()
                                                .font_family(FONT_PIXEL_MONO)
                                                .text_size(px(17.))
                                                .text_color(level_color(used, cx))
                                                .child(format!("{}%", (100.0 - used).round())),
                                        )
                                        .when(!after.trim().is_empty(), |row| {
                                            row.child(word(after))
                                        }),
                                )
                            }),
                    )
                    // 没有额度信号时不画空条，今日用量并到第二行右侧，行更矮。
                    .when(quota.is_some(), |column| {
                        column.child(h_flex().mt(px(3.)).gap(px(2.)).children((0..10).map(|ix| {
                            div().flex_1().h(px(8.)).rounded(px(1.)).bg(if ix < lit {
                                bar_color
                            } else {
                                cx.theme().border
                            })
                        })))
                    })
                    .child(
                        h_flex()
                            .mt(px(3.))
                            .gap(px(8.))
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap(px(6.))
                                    .child(div().truncate().child(left))
                                    .items_center()
                                    .when_some(
                                        quota.as_ref().and_then(|q| q.pressure.clone()),
                                        |line, pressure| {
                                            line.child(
                                                PressureBadge::new(
                                                    SharedString::from(format!(
                                                        "menubar-pressure-{}",
                                                        who.slug()
                                                    )),
                                                    pressure.reading,
                                                    pressure.window,
                                                )
                                                .prefix(pressure.label),
                                            )
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .when_some(credit_alert, |d, color| d.text_color(color))
                                    .child(if quota.is_some() {
                                        right
                                    } else {
                                        today_text.clone().unwrap_or_default()
                                    }),
                            ),
                    )
                    .when_some(today_text.filter(|_| quota.is_some()), |column, text| {
                        column.child(
                            div()
                                .mt(px(1.))
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(text),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_recent(&self, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let recent = self
            .overview
            .as_ref()
            .map(|overview| overview.recent.clone())
            .unwrap_or_default();
        v_flex()
            .flex_none()
            .pt(px(6.))
            .gap(px(1.))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px(px(8.))
                    .pb(px(2.))
                    .font_family(FONT_PIXEL_SANS)
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("menubar.recent")),
            )
            .when(recent.is_empty(), |list| {
                list.child(
                    div()
                        .px(px(8.))
                        .h(px(26.))
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("menubar.recent_empty")),
                )
            })
            .children(recent.into_iter().map(|session| {
                let id = session.id.clone();
                h_flex()
                    .id(SharedString::from(format!("menubar-recent-{}", session.id)))
                    .h(px(26.))
                    .px(px(8.))
                    .gap(px(8.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().list_hover))
                    .when_some(session.who, |row, who| {
                        row.child(
                            img(format!("avatars/avatar-{}.png", who.slug()))
                                .flex_none()
                                .size(px(18.))
                                .rounded(px(3.))
                                .border_1()
                                .border_color(cx.theme().border),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .child(session.title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(relative_time(session.updated_at_ms, now)),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(MenuBarEvent::OpenSession(id.clone()))
                    }))
            }))
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let can_sync = self.sync_handle.is_some();
        let item = |id: &'static str, label: SharedString, cx: &App| {
            h_flex()
                .id(id)
                .h(px(28.))
                .px(px(8.))
                .rounded(px(3.))
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().list_hover))
                .child(label)
        };
        v_flex()
            .flex_none()
            .mt_auto()
            .pt(px(6.))
            .gap(px(1.))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                item("menubar-open", t!("menubar.open_app").into(), cx)
                    .font_weight(FontWeight::MEDIUM)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(MenuBarEvent::OpenMain))),
            )
            .child(
                h_flex()
                    .gap(px(2.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        item("menubar-sync", t!("menubar.sync_now").into(), cx)
                            .when(!can_sync, |button| button.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(handle) = &this.sync_handle {
                                    handle.scan_all();
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        item("menubar-settings", t!("menubar.settings").into(), cx).on_click(
                            cx.listener(|_, _, _, cx| cx.emit(MenuBarEvent::OpenSettings)),
                        ),
                    )
                    .child(
                        item("menubar-quit", t!("menubar.quit").into(), cx)
                            .ml_auto()
                            .on_click(|_, _, cx| cx.quit()),
                    ),
            )
            .into_any_element()
    }
}

impl Render for MenuBarPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_ms();
        let error = self
            .error
            .clone()
            .or_else(|| self.live.read(cx).error.clone());
        v_flex()
            .id("menubar-panel")
            .size_full()
            .px(px(14.))
            .py(px(12.))
            .gap(px(8.))
            .bg(cx.theme().popover)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .child(self.render_header(now, cx))
            .child(self.render_tiles(cx))
            .when_some(error, |panel, error| {
                panel.child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().danger)
                        .truncate()
                        .child(t!("menubar.load_failed", error = error)),
                )
            })
            .child(
                v_flex()
                    .flex_none()
                    .children(ResidentId::ALL.map(|who| self.render_agent(who, now, cx))),
            )
            .child(self.render_recent(now, cx))
            .child(self.render_footer(cx))
    }
}

/// 菜单栏控制器：状态项、弹窗开关、主窗口的隐藏与唤回。
#[cfg(target_os = "macos")]
mod controller {
    use std::time::Instant;

    use futures::StreamExt as _;
    use gpui_kit::{
        AnyWindowHandle, Bounds, DisplayId, Global, WindowBounds, WindowKind, WindowOptions, point,
        size,
    };

    use super::status_item::{self, StatusItem, TrayEvent};
    use super::*;
    use crate::workspace::Workspace;

    /// 弹窗因失焦关掉后这段时间内的图标点击视为「关」：点图标本身也可能先让弹窗失焦。
    const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(300);

    struct MenuBarGlobal(Entity<MenuBarController>);

    impl Global for MenuBarGlobal {}

    pub struct MenuBarController {
        item: StatusItem,
        panel: Entity<MenuBarPanel>,
        popup: Option<AnyWindowHandle>,
        closed_at: Option<Instant>,
        main_window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        _events: Task<()>,
        _subscription: Subscription,
    }

    /// 装上菜单栏图标。失败（拿不到主线程或状态栏按钮）时只打日志，主窗口照常用。
    pub fn install(
        source: Arc<dyn DataSource>,
        demo: bool,
        main_window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        cx: &mut App,
    ) {
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let Some(item) = StatusItem::install(sender, &t!("menubar.tooltip")) else {
            eprintln!("Could not create the menu bar item; the menu bar summary is unavailable.");
            return;
        };
        let (sync, sync_handle) = workspace.read(cx).sync_link();
        let panel = cx.new(|cx| {
            let mut panel = MenuBarPanel::new(source, demo, cx);
            panel.attach_sync(sync, sync_handle, cx);
            panel
        });
        let controller = cx.new(|cx| {
            // 状态项的点击与全局鼠标监听都从 AppKit 回调里经 channel 送到前台。
            let events = cx.spawn(async move |this, cx| {
                while let Some(event) = receiver.next().await {
                    if this
                        .update(cx, |this: &mut MenuBarController, cx| {
                            this.handle(event, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let subscription = cx.subscribe(&panel, |this, _, event: &MenuBarEvent, cx| {
                this.on_panel_event(event, cx)
            });
            MenuBarController {
                item,
                panel,
                popup: None,
                closed_at: None,
                main_window,
                workspace,
                _events: events,
                _subscription: subscription,
            }
        });
        cx.set_global(MenuBarGlobal(controller));
    }

    /// Dock 图标被点（`applicationShouldHandleReopen`）：把主窗口带回来。
    pub fn reopen(cx: &mut App) {
        if let Some(global) = cx.try_global::<MenuBarGlobal>() {
            global
                .0
                .clone()
                .update(cx, |this, cx| this.show_main(None, None, cx));
        }
    }

    /// 主窗口点关闭时只隐藏：菜单栏常驻，同步引擎与实时信号都挂在主窗口的外壳上，不能丢。
    /// 退出走弹窗里的「退出」或 Dock 菜单。
    pub fn keep_running_on_close(window: &mut Window, cx: &App) {
        window.on_window_should_close(cx, |window, _| {
            status_item::order_out(window);
            false
        });
    }

    impl MenuBarController {
        fn handle(&mut self, event: TrayEvent, cx: &mut Context<Self>) {
            match event {
                TrayEvent::Clicked => {
                    if self.popup.is_some() {
                        self.close_popup(cx);
                    } else if self
                        .closed_at
                        .is_some_and(|at| at.elapsed() < TOGGLE_DEBOUNCE)
                    {
                        // 刚因失焦关掉：这次点击本意就是「关」。
                    } else {
                        self.open_popup(cx);
                    }
                }
                TrayEvent::OutsideClick => self.close_popup(cx),
            }
        }

        fn on_panel_event(&mut self, event: &MenuBarEvent, cx: &mut Context<Self>) {
            match event {
                MenuBarEvent::Closed => self.forget_popup(cx),
                MenuBarEvent::OpenMain => {
                    self.close_popup(cx);
                    self.show_main(None, None, cx);
                }
                MenuBarEvent::OpenSettings => {
                    self.close_popup(cx);
                    self.show_main(Some("settings"), None, cx);
                }
                MenuBarEvent::OpenSession(id) => {
                    self.close_popup(cx);
                    self.show_main(None, Some(id.clone()), cx);
                }
            }
        }

        fn open_popup(&mut self, cx: &mut Context<Self>) {
            let Some(anchor) = self.item.anchor() else {
                eprintln!("Could not locate the menu bar item on screen.");
                return;
            };
            let (left, top) = popup_origin(
                anchor.item,
                anchor.screen,
                f64::from(POPUP_WIDTH),
                f64::from(POPUP_HEIGHT),
            );
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(left as f32), px(top as f32)),
                    size: size(px(POPUP_WIDTH), px(POPUP_HEIGHT)),
                })),
                titlebar: None,
                focus: true,
                show: true,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                display_id: Some(DisplayId::new(u64::from(anchor.display_id))),
                app_id: Some("agenthome".into()),
                ..Default::default()
            };
            let panel = self.panel.clone();
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.close_on_deactivate(window, cx);
                    panel.set_visible(true, cx);
                });
                panel
            });
            match opened {
                Ok((handle, _)) => {
                    self.popup = Some(handle);
                    self.item.set_highlighted(true);
                    self.item.watch_outside_clicks(true);
                }
                Err(error) => eprintln!("Could not open the menu bar popup: {error:#}"),
            }
        }

        fn close_popup(&mut self, cx: &mut Context<Self>) {
            if let Some(handle) = self.popup.take() {
                // 推迟到本轮事件之后：关闭可能由弹窗自己的回调触发，那时窗口正被借用。
                cx.defer(move |cx| {
                    handle
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                });
            }
            self.forget_popup(cx);
        }

        /// 弹窗已经（或即将）关闭后的收尾。
        fn forget_popup(&mut self, cx: &mut Context<Self>) {
            self.popup = None;
            self.closed_at = Some(Instant::now());
            self.item.set_highlighted(false);
            self.item.watch_outside_clicks(false);
            self.panel
                .update(cx, |panel, cx| panel.set_visible(false, cx));
        }

        /// 把主窗口带到前台；`page` / `session` 与启动参数同义（打开设置页、定位会话）。
        fn show_main(
            &mut self,
            page: Option<&'static str>,
            session: Option<String>,
            cx: &mut Context<Self>,
        ) {
            let workspace = self.workspace.clone();
            let shown = self.main_window.update(cx, |_, window, cx| {
                status_item::order_front(window);
                window.activate_window();
                if page.is_some() || session.is_some() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.apply_startup(page, None, session, window, cx)
                    });
                }
            });
            if let Err(error) = shown {
                eprintln!("The main window is gone: {error:#}");
            }
            cx.activate(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::MockDataSource;
    use agenthome_core::model::{Credits, Freshness};

    fn window(id: &str, used: f32, minutes: Option<u32>, resets_at_ms: Option<i64>) -> QuotaWindow {
        QuotaWindow {
            id: id.into(),
            label: format!("label-{id}"),
            used_percent: used,
            resets_at_ms,
            window_minutes: minutes,
        }
    }

    fn snapshot(provider: ProviderId, windows: Vec<QuotaWindow>) -> QuotaSnapshot {
        QuotaSnapshot {
            provider,
            plan: None,
            windows,
            credits: None,
            fetched_at_ms: 0,
            source: String::new(),
            freshness: Freshness::Live,
        }
    }

    const MAIN: ScreenRect = ScreenRect {
        x: 0.0,
        y: 0.0,
        width: 1512.0,
        height: 982.0,
    };

    #[test]
    fn popup_sits_under_the_item_on_the_main_screen() {
        // 菜单栏高 24：状态项窗口底边在 y = 982 − 24。
        let item = ScreenRect {
            x: 1000.0,
            y: 958.0,
            width: 30.0,
            height: 24.0,
        };
        let (left, top) = popup_origin(item, MAIN, 360.0, 652.0);
        assert_eq!(left, 1015.0 - 180.0);
        assert_eq!(top, 24.0 + ITEM_GAP);
    }

    #[test]
    fn popup_stays_inside_the_right_edge() {
        // 最右侧的状态项：居中会出屏，贴右边留 8。
        let item = ScreenRect {
            x: 1480.0,
            y: 958.0,
            width: 30.0,
            height: 24.0,
        };
        let (left, _) = popup_origin(item, MAIN, 360.0, 652.0);
        assert_eq!(left, 1512.0 - 360.0 - SCREEN_MARGIN);
    }

    #[test]
    fn popup_is_relative_to_a_secondary_screen() {
        // 副屏在主屏右侧且更高（Cocoa y 向上，副屏原点 y = −300）：坐标要减去副屏原点。
        let screen = ScreenRect {
            x: 1512.0,
            y: -300.0,
            width: 2560.0,
            height: 1440.0,
        };
        let item = ScreenRect {
            x: 1512.0 + 2000.0,
            y: -300.0 + 1440.0 - 25.0,
            width: 40.0,
            height: 25.0,
        };
        let (left, top) = popup_origin(item, screen, 360.0, 652.0);
        assert_eq!(left, 2000.0 + 20.0 - 180.0);
        assert_eq!(top, 25.0 + ITEM_GAP);
    }

    #[test]
    fn popup_moves_up_on_a_short_screen() {
        let screen = ScreenRect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let item = ScreenRect {
            x: 10.0,
            y: 576.0,
            width: 30.0,
            height: 24.0,
        };
        let (left, top) = popup_origin(item, screen, 360.0, 652.0);
        assert_eq!(left, SCREEN_MARGIN);
        assert_eq!(top, 0.0);
    }

    #[test]
    fn window_labels_come_from_minutes() {
        assert_eq!(window_short_label(&window("a", 0.0, Some(300), None)), "5h");
        assert_eq!(
            window_short_label(&window("a", 0.0, Some(10_080), None)),
            "7d"
        );
        assert_eq!(window_short_label(&window("a", 0.0, Some(45), None)), "45m");
        let fable = QuotaWindow {
            label: "7 天 (Fable)".into(),
            ..window("seven_day_fable", 0.0, Some(10_080), None)
        };
        assert_eq!(window_short_label(&fable), "7d Fable");
        // 没有分钟数时退回来源标签。
        assert_eq!(window_short_label(&window("a", 0.0, None, None)), "label-a");
    }

    #[test]
    fn quota_picks_primary_and_tightest_other_window() {
        let snapshot = snapshot(
            ProviderId::ClaudeCode,
            vec![
                window("seven_day", 40.0, Some(10_080), Some(10_000)),
                window("five_hour", 58.0, Some(300), Some(5_000)),
                window("seven_day_opus", 78.0, Some(10_080), None),
            ],
        );
        let quota = agent_quota(&snapshot, 1_000).expect("has windows");
        assert_eq!(quota.primary.label, "5h");
        assert_eq!(quota.primary.used_percent, 58.0);
        assert_eq!(quota.primary.resets_in_ms, Some(4_000));
        let secondary = quota.secondary.expect("has a second window");
        assert_eq!(secondary.used_percent, 78.0);
        assert_eq!(secondary.resets_in_ms, None);
        assert_eq!(quota.credits, None);
    }

    #[test]
    fn quota_without_windows_is_none_and_credits_need_a_balance() {
        assert!(agent_quota(&snapshot(ProviderId::Codex, vec![]), 0).is_none());
        let mut codex = snapshot(
            ProviderId::Codex,
            vec![window("primary", 120.0, Some(300), None)],
        );
        codex.credits = Some(Credits {
            balance: Some("3".into()),
            unlimited: false,
            has_credits: true,
            reset_credits: None,
            expires_at_ms: Some(86_400_000),
            reset_credit_expiries_ms: Vec::new(),
        });
        let quota = agent_quota(&codex, 0).expect("has a window");
        // 超过 100% 的读数夹到 100，避免条与文字溢出。
        assert_eq!(quota.primary.used_percent, 100.0);
        assert_eq!(
            quota.credits,
            Some(CreditsLine {
                balance: Some("3".into()),
                unlimited: false,
                reset_credits: None,
                expires_in_ms: Some(86_400_000),
            })
        );
        codex.credits = Some(Credits {
            balance: None,
            unlimited: false,
            has_credits: false,
            reset_credits: None,
            expires_at_ms: None,
            reset_credit_expiries_ms: Vec::new(),
        });
        assert_eq!(agent_quota(&codex, 0).and_then(|q| q.credits), None);
        // 余额为 0 但查到了未用的重置额度：照样显示，否则临期提醒没地方出现。
        codex.credits = Some(Credits {
            balance: Some("0".into()),
            unlimited: false,
            has_credits: false,
            reset_credits: Some(2),
            expires_at_ms: Some(2 * 86_400_000),
            reset_credit_expiries_ms: vec![2 * 86_400_000, 3 * 86_400_000],
        });
        let credits = agent_quota(&codex, 0).and_then(|q| q.credits);
        assert_eq!(credits.map(|c| c.reset_credits), Some(Some(2)));
    }

    #[test]
    fn levels_and_cells_match_the_main_window_thresholds() {
        assert_eq!(usage_level(50.0), UsageLevel::Ok);
        assert_eq!(usage_level(50.5), UsageLevel::Warn);
        assert_eq!(usage_level(80.5), UsageLevel::Danger);
        // 体力条按剩余点亮：用满才全灭，还剩一点也亮一格。
        assert_eq!(lit_cells(0.0), 10);
        assert_eq!(lit_cells(58.0), 5);
        assert_eq!(lit_cells(99.5), 1);
        assert_eq!(lit_cells(100.0), 0);
        assert_eq!(lit_cells(150.0), 0);
    }

    #[test]
    fn overview_adds_up_per_agent_and_skips_subagents() {
        let source = MockDataSource::new();
        let overview = load_overview(&source).expect("mock data loads");
        let sum_cost: f64 = overview.agents.iter().map(|a| a.cost_usd).sum();
        let sum_output: u64 = overview.agents.iter().map(|a| a.output_tokens).sum();
        // mock 只有三位住户的数据：各 Agent 之和等于总计。
        assert!((sum_cost - overview.total.cost_usd).abs() < 1e-6);
        assert_eq!(sum_output, overview.total.output_tokens);
        assert!(overview.recent.len() <= RECENT_COUNT);
        let all = source
            .sessions(&SessionQuery {
                limit: 20,
                ..Default::default()
            })
            .expect("mock sessions");
        for recent in &overview.recent {
            let session = all.iter().find(|s| s.id == recent.id).expect("listed");
            assert!(session.parent_session_id.is_none());
        }
        // 按更新时间倒序，与数据源一致。
        assert!(
            overview
                .recent
                .windows(2)
                .all(|pair| pair[0].updated_at_ms >= pair[1].updated_at_ms)
        );
    }
}
