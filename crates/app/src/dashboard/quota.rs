//! 额度快照 + 每秒倒计时：`QuotaState` 持有数据与时钟，
//! 「住户」卡和「到期与重置」卡各自观察它，秒针只让这两块重绘。

use std::time::Duration;

use agenthome_core::model::{Freshness, ProviderId, QuotaSnapshot, QuotaWindow};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, px, rgb,
};

use super::cards::{Tag, TagKind, avatar, card, card_title, hp_bar, hp_tier, vt};
use super::fmt::{self, AMBER, GREEN, MUTED, RED_TEXT, countdown};
use super::model::{DueItem, due_items};
use crate::assets::FONT_PIXEL_MONO;
use crate::format::{now_ms, quota_window_label};
use crate::habitat::{HabitatView, ResidentId};

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

fn provider_of(who: ResidentId) -> ProviderId {
    match who {
        ResidentId::Claude => ProviderId::ClaudeCode,
        ResidentId::Codex => ProviderId::Codex,
        ResidentId::Gemini => ProviderId::GeminiCli,
    }
}

/// 「额度详情」卡：三位住户各一列，状态文案来自栖息地状态机，体力条来自额度窗口。
/// 左栏只显示主窗口一条，这里列出全部窗口、重置时间、重置信用与合规来源。
pub struct ResidentsCard {
    quota: Entity<QuotaState>,
    habitat: Entity<HabitatView>,
    _subscriptions: Vec<Subscription>,
}

impl ResidentsCard {
    pub fn new(
        quota: Entity<QuotaState>,
        habitat: Entity<HabitatView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&quota, |_, _, cx| cx.notify()),
            cx.observe(&habitat, |_, _, cx| cx.notify()),
        ];
        Self {
            quota,
            habitat,
            _subscriptions: subscriptions,
        }
    }

    fn render_resident(
        &self,
        who: ResidentId,
        status: SharedString,
        snapshots: &[QuotaSnapshot],
        now_ms: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let provider = provider_of(who);
        let snapshot = snapshots.iter().find(|s| s.provider == provider);
        let plan: SharedString = snapshot
            .and_then(|s| s.plan.clone())
            .unwrap_or_else(|| match who {
                // Gemini 没有本地额度信号，但会话与 token 已接入。
                ResidentId::Gemini => t!("dashboard.plan_connected").into_owned(),
                _ => t!("dashboard.plan_not_connected").into_owned(),
            })
            .into();
        let header = h_flex()
            .items_baseline()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::MEDIUM)
                    .child(who.name()),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .child(format!("{plan} · {status}")),
            )
            .child(div().ml_auto().child(match snapshot {
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
                body = body.children(snapshot.windows.iter().map(|w| window_row(w, cx)));
                body = body.child(reset_line(snapshot, now_ms, cx));
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
            .child(avatar(&provider, 40.))
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
        let rows: Vec<AnyElement> = ResidentId::ALL
            .into_iter()
            .map(|who| {
                let status: SharedString = residents
                    .get(who.index())
                    .map(|r| r.state.label().to_string().into())
                    .unwrap_or_else(|| t!("dashboard.status_away").into());
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

/// 一个窗口：标签 + 剩余百分比 + 10 格体力条。
fn window_row(window: &QuotaWindow, cx: &mut Context<ResidentsCard>) -> AnyElement {
    let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let tier = hp_tier(left);
    v_flex()
        .gap(px(4.))
        .child(
            h_flex()
                .justify_between()
                .items_baseline()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(quota_window_label(window))
                .child(
                    vt(22.)
                        .text_color(if tier == GREEN {
                            cx.theme().foreground
                        } else {
                            rgb(tier).into()
                        })
                        .child(format!("{left:.0}%")),
                ),
        )
        .child(hp_bar(Some(left)))
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
                .child(vt(22.).text_color(cx.theme().muted_foreground).child("--")),
        )
        .child(hp_bar(None))
        .into_any_element()
}

/// 底部一行：各窗口重置倒计时 + Codex 重置信用。
fn reset_line(snapshot: &QuotaSnapshot, now_ms: i64, cx: &App) -> AnyElement {
    let mut line = h_flex()
        .flex_wrap()
        .gap(px(14.))
        .text_size(px(11.5))
        .text_color(rgb(MUTED));
    for window in &snapshot.windows {
        let Some(at) = window.resets_at_ms else {
            continue;
        };
        let remaining = at - now_ms;
        line = line.child(
            h_flex()
                .gap(px(4.))
                .items_baseline()
                .child(t!(
                    "dashboard.window_reset",
                    window = quota_window_label(window)
                ))
                .child(
                    vt(16.)
                        .text_color(due_color(remaining, Some(100.0 - window.used_percent), cx))
                        .child(countdown(remaining)),
                ),
        );
    }
    if let Some(credits) = &snapshot.credits {
        let mut item = h_flex()
            .gap(px(4.))
            .items_baseline()
            .child(t!("dashboard.reset_credits"));
        match credits.reset_credits {
            Some(n) => {
                let text = if n == 1 {
                    t!("dashboard.credits_one")
                } else {
                    t!("dashboard.credits_other", count = n)
                };
                item = item.child(vt(16.).text_color(rgb(GREEN)).child(text));
            }
            None if credits.unlimited => item = item.child(t!("dashboard.credits_unlimited")),
            None => item = item.child(t!("dashboard.credits_none")),
        }
        if let Some(expires) = credits.expires_at_ms {
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
                    vt(16.)
                        .text_color(due_color(expires - now_ms, None, cx))
                        .child(countdown(expires - now_ms)),
                )
                .when(!after.trim().is_empty(), |item| {
                    item.child(after.trim().to_string())
                });
        }
        line = line.child(item);
    }
    line.into_any_element()
}

/// 倒计时颜色：不足 1 小时红；不足 6 小时或窗口剩余不足 1/4 琥珀；其余用主题正文色
/// （写死深色主题的浅字，浅色主题下会看不见）。
fn due_color(remaining_ms: i64, quota_left: Option<f32>, cx: &App) -> Hsla {
    if remaining_ms < 3_600_000 {
        rgb(RED_TEXT).into()
    } else if remaining_ms < 6 * 3_600_000 || quota_left.is_some_and(|left| left < 25.0) {
        rgb(AMBER).into()
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
            .child(
                div()
                    .font_family(FONT_PIXEL_MONO)
                    .text_size(px(22.))
                    .line_height(px(22.))
                    .text_color(due_color(remaining, item.quota_left, cx))
                    .child(countdown(remaining)),
            )
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
