//! 额度快照 + 每秒倒计时：`QuotaState` 持有数据与时钟，
//! 「住户」卡和「到期与重置」卡各自观察它，秒针只让这两块重绘。

use std::time::Duration;

use agenthome_core::model::{Freshness, ProviderId, QuotaSnapshot, QuotaWindow};
use agenthome_core::pressure::{Pressure, pressure};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, px, rgb,
};

use super::cards::{Tag, TagKind, avatar, card, card_title, hp_bar, hp_color, mono_num};
use super::fmt::{self, GREEN, MUTED, RED_TEXT, amber_text, countdown};
use super::model::{DueItem, due_items};
use crate::assets::MonoText as _;
use crate::format::{now_ms, quota_window_label};
use crate::habitat::{HabitatView, ResidentId};
use crate::live::{LiveState, shown_state};
use crate::pressure_badge::PressureBadge;

/// 额度快照与倒计时时钟。
pub struct QuotaState {
    pub snapshots: Vec<QuotaSnapshot>,
    pub now_ms: i64,
    pub loading: bool,
    pub error: Option<SharedString>,
    _ticker: Task<()>,
}

impl QuotaState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |state, cx| {
                    state.now_ms = now_ms();
                    // 没有快照就没有倒计时，不必重绘。
                    if !state.snapshots.is_empty() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            snapshots: Vec::new(),
            now_ms: now_ms(),
            loading: false,
            error: None,
            _ticker: ticker,
        }
    }
}

/// 头像用：住户名下登记的第一个来源（头像按住户取，哪个来源都一样）。
fn resident_provider(who: ResidentId) -> ProviderId {
    crate::live::providers_of(who)
        .into_iter()
        .next()
        .unwrap_or(ProviderId::Other(String::new()))
}

/// 「额度详情」卡：三位住户各一列，状态文案来自栖息地状态机，体力条来自额度窗口。
/// 左栏只显示主窗口一条，这里列出全部窗口、重置时间、重置信用与合规来源。
pub struct ResidentsCard {
    quota: Entity<QuotaState>,
    habitat: Entity<HabitatView>,
    live: Entity<LiveState>,
    _subscriptions: Vec<Subscription>,
}

impl ResidentsCard {
    pub fn new(
        quota: Entity<QuotaState>,
        habitat: Entity<HabitatView>,
        live: Entity<LiveState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&quota, |_, _, cx| cx.notify()),
            cx.observe(&habitat, |_, _, cx| cx.notify()),
            cx.observe(&live, |_, _, cx| cx.notify()),
        ];
        Self {
            quota,
            habitat,
            live,
            _subscriptions: subscriptions,
        }
    }

    fn render_resident(
        &self,
        who: ResidentId,
        status: Option<SharedString>,
        snapshots: &[QuotaSnapshot],
        now_ms: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 按住户找快照：猫的额度来自 Antigravity CLI（Gemini CLI 没有额度信号）。
        let snapshot = snapshots
            .iter()
            .find(|s| crate::live::resident_of(&s.provider) == Some(who));
        // 未接入（`status` 为 None）只写「未接入」；已接入时套餐名优先，没有套餐名写「已接入」。
        let plan: SharedString = match (&status, snapshot.and_then(|s| s.plan.clone())) {
            (None, _) => t!("dashboard.plan_not_connected").into_owned(),
            (Some(_), Some(plan)) => plan,
            (Some(_), None) => t!("dashboard.plan_connected").into_owned(),
        }
        .into();
        let summary: SharedString = match &status {
            Some(status) => format!("{plan} · {status}").into(),
            None => plan,
        };
        // 标题行放不下时（窄窗口、英文长状态如「Low battery」）标签整体换到下一行，
        // 不截断、不压到右边一列；名字与状态各自不换行。
        let header = h_flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(8.))
            .gap_y(px(4.))
            .child(
                h_flex()
                    .min_w_0()
                    .items_baseline()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(who.name()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .child(summary),
                    ),
            )
            .child(div().flex_none().ml_auto().child(match snapshot {
                Some(s) => compliance_tag(s),
                None => Tag::new(
                    if who == ResidentId::Gemini {
                        t!("dashboard.no_quota_signal")
                    } else {
                        t!("dashboard.quota_not_connected")
                    },
                    TagKind::Dim,
                ),
            }));

        let mut body = v_flex().flex_1().min_w_0().gap(px(6.)).child(header);
        match snapshot {
            Some(snapshot) => {
                body = body.children(
                    snapshot
                        .windows
                        .iter()
                        .map(|w| window_row(who.slug(), w, now_ms, cx)),
                );
                if let Some(credits) = credits_line(snapshot, now_ms, cx) {
                    body = body.child(credits);
                }
            }
            None => {
                body = body.child(window_row_empty(cx)).child(
                    div()
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(match who {
                            ResidentId::Gemini => t!("dashboard.gemini_quota_hint"),
                            _ => t!("dashboard.missing_quota_hint"),
                        }),
                );
            }
        }

        // 三列等宽；列间用左边线分隔。
        h_flex()
            .flex_1()
            .flex_basis(px(0.))
            .min_w_0()
            .items_start()
            .gap(px(10.))
            .py(px(4.))
            .when(who != ResidentId::Claude, |col| {
                col.pl(px(16.)).border_l_1().border_color(cx.theme().muted)
            })
            .when(who != ResidentId::Gemini, |col| col.pr(px(16.)))
            .when(snapshot.is_none(), |col| col.opacity(0.6))
            .child(avatar(&resident_provider(who), 40.))
            .child(body)
            .into_any_element()
    }
}

impl Render for ResidentsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 先把实体里的数据拷出来，避免 read 借用与 listener 的 cx 冲突。
        let (snapshots, now_ms, error) = {
            let quota = self.quota.read(cx);
            (quota.snapshots.clone(), quota.now_ms, quota.error.clone())
        };
        let residents = self.habitat.read(cx).residents().clone();
        let live = self.live.read(cx);
        let connected = ResidentId::ALL.map(|who| live.connected(who));
        let rows: Vec<AnyElement> = ResidentId::ALL
            .into_iter()
            .map(|who| {
                // 是否接入看本机数据（与左栏、菜单栏同一判定），状态文案看栖息地。
                let status: Option<SharedString> = residents.get(who.index()).and_then(|r| {
                    shown_state(connected[who.index()], r.state).map(|s| s.label().into())
                });
                self.render_resident(who, status, &snapshots, now_ms, cx)
            })
            .collect();
        card(cx)
            .gap_0()
            .child(
                card_title(t!("dashboard.quota_title"), t!("dashboard.quota_subtitle")).pb(px(10.)),
            )
            .child(h_flex().items_stretch().children(rows))
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .pt(px(6.))
                        .text_size(px(11.5))
                        .text_color(rgb(RED_TEXT))
                        .child(error),
                )
            })
    }
}

/// 合规标签：数据来自本地文件，新鲜度决定颜色。
fn compliance_tag(snapshot: &QuotaSnapshot) -> Tag {
    match snapshot.freshness {
        Freshness::Live => Tag::new(
            t!(
                "dashboard.compliant",
                source = source_label(&snapshot.source)
            ),
            TagKind::Ok,
        ),
        Freshness::Cached => Tag::new(
            t!(
                "dashboard.compliant",
                source = source_label(&snapshot.source)
            ),
            TagKind::Dim,
        ),
        Freshness::Stale => Tag::new(
            t!(
                "dashboard.maybe_stale",
                source = source_label(&snapshot.source)
            ),
            TagKind::Warn,
        ),
    }
}

/// 一个窗口：标签 + 使用压力 + 剩余百分比 + 10 格体力条；照当前速度会在重置前用完时，下面写多久后用完。
fn window_row(
    provider_key: &str,
    window: &QuotaWindow,
    now_ms: i64,
    cx: &mut Context<ResidentsCard>,
) -> AnyElement {
    let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let reading = pressure(window, now_ms);
    let runs_out = reading
        .filter(|r| matches!(r.level, Pressure::Tight | Pressure::Critical))
        .and_then(|r| r.runs_out_in_ms.map(|ms| (r.level, ms)));
    v_flex()
        .gap(px(4.))
        .child(
            h_flex()
                .justify_between()
                .items_baseline()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(
                    // 窗口名可截断（「5h (Claude/GPT)」较长）；徽标与右侧的重置、百分比固定不缩。
                    h_flex()
                        .min_w_0()
                        .items_center()
                        .gap(px(6.))
                        .child(div().min_w_0().truncate().child(quota_window_label(window)))
                        .when_some(reading, |row, reading| {
                            row.child(PressureBadge::new(
                                SharedString::from(format!(
                                    "dashboard-pressure-{}-{}",
                                    provider_key, window.id
                                )),
                                reading,
                                quota_window_label(window),
                            ))
                        }),
                )
                .child(
                    // 右侧：重置倒计时 + 剩余百分比，跟着各自的条走（不再在卡片底部另列一串）。
                    h_flex()
                        .flex_none()
                        .items_baseline()
                        .gap(px(10.))
                        .when_some(window.resets_at_ms, |row, at| {
                            let remaining = at - now_ms;
                            let text = countdown(remaining);
                            row.child(
                                h_flex()
                                    .items_baseline()
                                    .gap(px(4.))
                                    .text_size(px(11.5))
                                    .child(t!("dashboard.reset_short"))
                                    .child(
                                        mono_num(12., cx)
                                            .text_color(due_color(remaining, Some(left), cx))
                                            .child(text),
                                    ),
                            )
                        })
                        .child(
                            mono_num(13., cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if left > 50.0 {
                                    cx.theme().foreground
                                } else {
                                    hp_color(left, cx)
                                })
                                .child(format!("{left:.0}%")),
                        ),
                ),
        )
        .child(hp_bar(Some(left), cx))
        .when_some(runs_out, |row, (level, ms)| {
            row.child(
                div()
                    .text_size(px(11.5))
                    .text_color(match level {
                        Pressure::Tight => amber_text(cx),
                        _ => rgb(RED_TEXT).into(),
                    })
                    .child(t!("format.pressure_runs_out", time = countdown(ms))),
            )
        })
        .into_any_element()
}

fn window_row_empty(cx: &mut Context<ResidentsCard>) -> AnyElement {
    v_flex()
        .gap(px(4.))
        .child(
            h_flex()
                .justify_between()
                .items_baseline()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(t!("dashboard.quota"))
                .child(
                    mono_num(16., cx)
                        .text_color(cx.theme().muted_foreground)
                        .child("--"),
                ),
        )
        .child(hp_bar(None, cx))
        .into_any_element()
}

/// 底部一行：Codex 重置额度（张数与最近一张的过期倒计时）；没有就不画。
fn credits_line(snapshot: &QuotaSnapshot, now_ms: i64, cx: &App) -> Option<AnyElement> {
    let mut line = h_flex()
        .flex_wrap()
        .gap(px(14.))
        .text_size(px(11.5))
        .text_color(rgb(MUTED));
    let credits = snapshot.credits.as_ref()?;
    {
        let mut item = h_flex()
            .gap(px(4.))
            .items_baseline()
            .child(t!("dashboard.reset_credits"));
        match credits.reset_credits {
            Some(n) => {
                // 数字用 VT323，量词（「张」/「credits」）按语言决定字体：中文退回正文字体，
                // 否则「张」会按 VT323 的大字号画。按 `%{count}` 切模板，中英文语序都能放。
                let template = if n == 1 {
                    t!("dashboard.credits_one")
                } else {
                    t!("dashboard.credits_other")
                };
                let (before, after) = template
                    .split_once("%{count}")
                    .unwrap_or(("", template.as_ref()));
                let unit = |text: &str| {
                    let text = text.trim().to_string();
                    mono_num(12., cx)
                        .mono_text(&text, 12., cx)
                        .text_color(rgb(GREEN))
                        .child(text)
                };
                item = item
                    .when(!before.trim().is_empty(), |item| item.child(unit(before)))
                    .child(
                        mono_num(12., cx)
                            .text_color(rgb(GREEN))
                            .child(n.to_string()),
                    )
                    .when(!after.trim().is_empty(), |item| item.child(unit(after)));
            }
            None if credits.unlimited => item = item.child(t!("dashboard.credits_unlimited")),
            None => item = item.child(t!("dashboard.credits_none")),
        }
        if let Some(expires) = credits.expires_at_ms {
            let text = countdown(expires - now_ms);
            // 倒计时单独着色，按 `%{t}` 把文案切成前后两段，中英文语序都能放。
            let template = t!("dashboard.credits_expire");
            let (before, after) = template
                .split_once("%{t}")
                .unwrap_or((template.as_ref(), ""));
            item = item
                .when(!before.trim().is_empty(), |item| {
                    item.child(before.trim().to_string())
                })
                .child(
                    mono_num(12., cx)
                        .mono_text(&text, 12., cx)
                        .text_color(due_color(expires - now_ms, None, cx))
                        .child(text),
                )
                .when(!after.trim().is_empty(), |item| {
                    item.child(after.trim().to_string())
                });
        }
        line = line.child(item);
    }
    Some(line.into_any_element())
}

/// 倒计时颜色：不足 1 小时红；不足 6 小时或窗口剩余不足 1/4 琥珀；其余用主题正文色
/// （写死深色主题的浅字，浅色主题下会看不见）。
/// 重置额度过期（`quota_left` 为 None）要提前提醒：3 天内琥珀、1 天内红。
fn due_color(remaining_ms: i64, quota_left: Option<f32>, cx: &App) -> Hsla {
    const DAY: i64 = 86_400_000;
    let (red, amber) = if quota_left.is_none() {
        (DAY, 3 * DAY)
    } else {
        (3_600_000, 6 * 3_600_000)
    };
    if remaining_ms < red {
        rgb(RED_TEXT).into()
    } else if remaining_ms < amber || quota_left.is_some_and(|left| left < 25.0) {
        amber_text(cx)
    } else {
        cx.theme().foreground
    }
}

/// 「到期与重置」卡。
pub struct DueCard {
    quota: Entity<QuotaState>,
    _subscription: Subscription,
}

impl DueCard {
    pub fn new(quota: Entity<QuotaState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&quota, |_, _, cx| cx.notify());
        Self {
            quota,
            _subscription: subscription,
        }
    }

    fn render_item(
        &self,
        ix: usize,
        item: &DueItem,
        now_ms: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remaining = item.at_ms - now_ms;
        let mut note = fmt::due_time(item.at_ms, now_ms);
        if !item.note.is_empty() {
            note = format!("{} · {note}", item.note);
        }
        h_flex()
            .items_center()
            .gap(px(10.))
            .py(px(8.))
            .when(ix > 0, |row| {
                row.border_t_1().border_color(cx.theme().muted)
            })
            .child(avatar(&item.provider, 22.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(item.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(note),
                    ),
            )
            .child({
                let text = countdown(remaining);
                div()
                    .flex_none()
                    .mono_text(&text, 16., cx)
                    .line_height(px(22.))
                    .text_color(due_color(remaining, item.quota_left, cx))
                    .child(text)
            })
            .into_any_element()
    }
}

impl Render for DueCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let quota = self.quota.read(cx);
        let now_ms = quota.now_ms;
        let items = due_items(&quota.snapshots);
        let body: AnyElement = if items.is_empty() {
            div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(if quota.loading {
                    t!("dashboard.due_loading")
                } else {
                    t!("dashboard.due_empty")
                })
                .into_any_element()
        } else {
            v_flex()
                .children(
                    items
                        .iter()
                        .enumerate()
                        .map(|(ix, item)| self.render_item(ix, item, now_ms, cx)),
                )
                .into_any_element()
        };
        card(cx)
            .flex_1()
            .min_w_0()
            .child(card_title(
                t!("dashboard.due_title"),
                t!("dashboard.due_subtitle"),
            ))
            .child(body)
    }
}

/// 快照 `source` 是文件路径，界面只显示来源种类。
fn source_label(source: &str) -> SharedString {
    if source.contains("/usage") {
        t!("dashboard.source_usage").into()
    } else if source.contains("rollout") {
        t!("dashboard.source_rollout").into()
    } else if source.contains("statusline") {
        "statusline".into()
    } else {
        t!("dashboard.source_local_file").into()
    }
}
