//! 额度快照 + 每秒倒计时：`QuotaState` 持有数据与时钟，
//! 「住户」卡和「到期与重置」卡各自观察它，秒针只让这两块重绘。

use std::time::Duration;

use agenthome_core::model::{Freshness, ProviderId, QuotaSnapshot, QuotaWindow};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, px, rgb,
};

use super::cards::{Tag, TagKind, avatar, card, card_title, hp_bar, hp_tier, vt};
use super::fmt::{self, AMBER, GREEN, MUTED, RED_TEXT, countdown};
use super::model::{DueItem, due_items};
use crate::assets::FONT_PIXEL_MONO;
use crate::format::now_ms;
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

/// 「住户」卡：三行，状态文案来自栖息地状态机，体力条来自额度窗口。
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
        status: &'static str,
        snapshots: &[QuotaSnapshot],
        now_ms: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let provider = provider_of(who);
        let snapshot = snapshots.iter().find(|s| s.provider == provider);
        let plan: SharedString = snapshot
            .and_then(|s| s.plan.clone())
            .unwrap_or_else(|| "未接入".to_string())
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
                None => Tag::new("未接入额度", TagKind::Dim),
            }));

        let mut body = v_flex().flex_1().min_w_0().gap(px(6.)).child(header);
        match snapshot {
            Some(snapshot) => {
                body = body.children(snapshot.windows.iter().map(|w| window_row(w, cx)));
                body = body.child(reset_line(snapshot, now_ms));
            }
            None => {
                body = body.child(window_row_empty(cx)).child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED))
                        .child("本地没有这个来源的额度快照，先在设置里接入。"),
                );
            }
        }

        h_flex()
            .items_start()
            .gap(px(10.))
            .py(px(10.))
            .when(who != ResidentId::Claude, |row| {
                row.border_t_1().border_color(cx.theme().muted)
            })
            .when(snapshot.is_none(), |row| row.opacity(0.6))
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
                let status = residents
                    .get(who.index())
                    .map(|r| r.state.label())
                    .unwrap_or("外出");
                self.render_resident(who, status, &snapshots, now_ms, cx)
            })
            .collect();
        card(cx)
            .gap_0()
            .child(card_title("住户", "状态由数据层事件驱动 · 体力条 = 窗口剩余").pb(px(6.)))
            .children(rows)
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
            format!("合规 · {}", source_label(&snapshot.source)),
            TagKind::Ok,
        ),
        Freshness::Cached => Tag::new(
            format!("合规 · {}", source_label(&snapshot.source)),
            TagKind::Dim,
        ),
        Freshness::Stale => Tag::new(
            format!("可能过期 · {}", source_label(&snapshot.source)),
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
                .text_color(rgb(MUTED))
                .child(window.label.clone())
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
                .text_color(rgb(MUTED))
                .child("配额")
                .child(vt(22.).text_color(cx.theme().muted_foreground).child("--")),
        )
        .child(hp_bar(None))
        .into_any_element()
}

/// 底部一行：各窗口重置倒计时 + Codex 重置信用。
fn reset_line(snapshot: &QuotaSnapshot, now_ms: i64) -> AnyElement {
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
                .child(format!("{} 重置", window.label))
                .child(
                    vt(16.)
                        .text_color(rgb(due_color(remaining, Some(100.0 - window.used_percent))))
                        .child(countdown(remaining)),
                ),
        );
    }
    if let Some(credits) = &snapshot.credits {
        let mut item = h_flex().gap(px(4.)).items_baseline().child("重置信用");
        match credits.reset_credits {
            Some(n) => item = item.child(vt(16.).text_color(rgb(GREEN)).child(format!("{n} 张"))),
            None if credits.unlimited => item = item.child("无限"),
            None => item = item.child("无"),
        }
        if let Some(expires) = credits.expires_at_ms {
            item = item
                .child("·")
                .child(
                    vt(16.)
                        .text_color(rgb(due_color(expires - now_ms, None)))
                        .child(countdown(expires - now_ms)),
                )
                .child("后过期");
        }
        line = line.child(item);
    }
    line.into_any_element()
}

/// 倒计时颜色：不足 1 小时红；不足 6 小时或窗口剩余不足 1/4 琥珀；其余正文色。
fn due_color(remaining_ms: i64, quota_left: Option<f32>) -> u32 {
    if remaining_ms < 3_600_000 {
        RED_TEXT
    } else if remaining_ms < 6 * 3_600_000 || quota_left.is_some_and(|left| left < 25.0) {
        AMBER
    } else {
        fmt::TEXT
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
                    .text_color(rgb(due_color(remaining, item.quota_left)))
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
                    "读取额度中…"
                } else {
                    "没有额度快照，接入后这里按先后列出重置与过期。"
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
            .child(card_title("到期与重置", "按先后"))
            .child(body)
    }
}

/// 快照 `source` 是文件路径，界面只显示来源种类。
fn source_label(source: &str) -> &'static str {
    if source.contains("/usage") {
        "官方 /usage"
    } else if source.contains("rollout") {
        "rollout 快照"
    } else if source.contains("statusline") {
        "statusline"
    } else {
        "本地文件"
    }
}
