//! 首页花费卡：汇总按模型厂商展示，口径和周期独立于其他卡片。

use std::collections::BTreeMap;

use agenthome_core::cost::{CostSummary, ModelCost, Vendor, VendorCost, prorated_fee};
use agenthome_core::diagnostics::ResultStep;
use agenthome_core::model::Period;
use agenthome_core::providers::registry::display_name;
use agenthome_core::usage_report::MachineFilter;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Div, FontWeight, ParentElement as _, SharedString, Styled as _,
    Task, div, px, relative, rgb,
};
use serde_json::json;

use super::cards::{card, card_title, mono_num, skeleton_rows};
use super::fmt::{CLAUDE, CODEX, GEMINI, MUTED, amber_text};
use super::panels::{period_label, period_subtitle};
use super::{DashboardEvent, DashboardView, segmented};
use crate::config::{SUBSCRIPTION_VENDORS, subscription_fee};
use crate::format::{short_cost, short_tokens};
use crate::telemetry;

const RANKED_MODELS: usize = 8;
const RANGES: [Period; 3] = [Period::Today, Period::Days(7), Period::Days(30)];

pub(super) struct CostCard {
    range: usize,
    /// CARM-301：只作用于花费卡。
    machine: MachineFilter,
    /// 已配对的远端 `(machine_id, 连接名)`，随花费一起刷新；为空时不显示机器筛选。
    machines: Vec<(String, String)>,
    /// 调试 / 截图用：`AGENTHOME_COST_MACHINE=local|<连接名>` 指定启动时的机器筛选，首次加载后清掉。
    pending_machine: Option<String>,
    /// 调试 / 截图用：`AGENTHOME_COST_PAID=1` 启动时选「实付」（CARM-303，本机没有辅助功能权限，没法模拟点击）。
    paid: bool,
    expanded: bool,
    fees: BTreeMap<Vendor, f64>,
    summary: Option<anyhow::Result<CostSummary>>,
    revision: u64,
    _task: Task<()>,
}

impl CostCard {
    pub(super) fn new() -> Self {
        Self {
            range: 2,
            machine: MachineFilter::All,
            machines: Vec::new(),
            pending_machine: std::env::var("AGENTHOME_COST_MACHINE").ok(),
            paid: std::env::var("AGENTHOME_COST_PAID").is_ok_and(|v| v == "1"),
            expanded: false,
            fees: SUBSCRIPTION_VENDORS
                .into_iter()
                .filter_map(|vendor| subscription_fee(vendor).map(|fee| (vendor, fee)))
                .collect(),
            summary: None,
            revision: 0,
            _task: Task::ready(()),
        }
    }
}

impl DashboardView {
    pub(super) fn load_cost(&mut self, cx: &mut Context<Self>) {
        self.cost.revision += 1;
        let revision = self.cost.revision;
        let period = RANGES[self.cost.range];
        let machine = self.cost.machine.clone();
        let pending = self.cost.pending_machine.take();
        let source = self.source.clone();
        self.cost._task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let machines = source
                        .remote_machines()
                        .step("cost.list_remote_machines")
                        .map_err(|error| (telemetry::machine_filter_kind(&machine), error))?;
                    let machine = match pending.as_deref() {
                        Some("local") => MachineFilter::Local,
                        Some(label) => machines
                            .iter()
                            .find(|(_, l)| l == label)
                            .map_or(machine, |(id, _)| MachineFilter::Remote(id.clone())),
                        None => machine,
                    };
                    // 选中的远端已取消配对：退回全部机器，而不是报错或显示空表。
                    let machine = match machine {
                        MachineFilter::Remote(id) if !machines.iter().any(|(m, _)| *m == id) => {
                            MachineFilter::All
                        }
                        machine => machine,
                    };
                    let rows = source
                        .cost_by_model(period, &machine)
                        .step("cost.read_models")
                        .map_err(|error| (telemetry::machine_filter_kind(&machine), error))?;
                    Ok::<_, (&str, anyhow::Error)>((
                        machines,
                        machine,
                        CostSummary::from_breakdown(&rows),
                    ))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.cost.revision != revision {
                    return;
                }
                let result = match loaded {
                    Ok((machines, machine, summary)) => {
                        this.cost.machines = machines;
                        this.cost.machine = machine;
                        telemetry::report_unpriced_models(&summary);
                        Ok(summary)
                    }
                    Err((machine_kind, error)) => {
                        telemetry::report_cost_failure(period, machine_kind, &error);
                        Err(error)
                    }
                };
                this.cost.summary = Some(result);
                cx.notify();
            });
        });
        cx.notify();
    }

    pub fn set_subscription_fee(
        &mut self,
        vendor: Vendor,
        fee: Option<f64>,
        cx: &mut Context<Self>,
    ) {
        if let Some(fee) = fee {
            self.cost.fees.insert(vendor, fee);
        } else {
            self.cost.fees.remove(&vendor);
        }
        cx.notify();
    }

    fn set_cost_range(&mut self, range: usize, cx: &mut Context<Self>) {
        if self.cost.range == range {
            return;
        }
        self.cost.range = range;
        self.cost.summary = None;
        self.cost.expanded = false;
        telemetry::event(
            "home_cost_range_change",
            json!({"range": RANGES[range].as_str()}),
        );
        self.load_cost(cx);
    }

    fn set_cost_machine(&mut self, ix: usize, cx: &mut Context<Self>) {
        let machine = match ix {
            0 => MachineFilter::All,
            1 => MachineFilter::Local,
            n => match self.cost.machines.get(n - 2) {
                Some((id, _)) => MachineFilter::Remote(id.clone()),
                None => return,
            },
        };
        if self.cost.machine == machine {
            return;
        }
        // 只上报类别：不带机器标识、连接名和金额。
        let kind = telemetry::machine_filter_kind(&machine);
        telemetry::event("home_cost_machine_change", json!({"machine": kind}));
        self.cost.machine = machine;
        self.cost.summary = None;
        self.cost.expanded = false;
        self.load_cost(cx);
    }

    fn render_machine_filter(&self, cx: &mut Context<Self>) -> Option<TabBar> {
        if self.cost.machines.is_empty() {
            return None;
        }
        let labels = [
            t!("dashboard.cost_machine_all").into(),
            t!("dashboard.cost_machine_local").into(),
        ]
        .into_iter()
        .chain(
            self.cost
                .machines
                .iter()
                .map(|(_, label)| SharedString::from(label.clone())),
        )
        .collect();
        let selected = match &self.cost.machine {
            MachineFilter::All => 0,
            MachineFilter::Local => 1,
            MachineFilter::Remote(id) => self
                .cost
                .machines
                .iter()
                .position(|(m, _)| m == id)
                .map_or(0, |i| i + 2),
        };
        Some(segmented(
            "cost-machine",
            labels,
            selected,
            cx.listener(|this, ix: &usize, _, cx| this.set_cost_machine(*ix, cx)),
        ))
    }

    fn set_cost_basis(&mut self, paid: bool, cx: &mut Context<Self>) {
        if self.cost.paid == paid {
            return;
        }
        self.cost.paid = paid;
        let basis = if paid { "paid" } else { "equivalent" };
        telemetry::event("home_cost_basis_change", json!({"basis": basis}));
        cx.notify();
    }

    pub(super) fn render_cost_card(&self, cx: &mut Context<Self>) -> Div {
        let panel = card(cx).w_full().child(
            h_flex()
                .flex_wrap()
                .gap(px(10.))
                .child(card_title(
                    t!("dashboard.cost_title"),
                    period_subtitle(RANGES[self.cost.range]),
                ))
                .child(
                    h_flex()
                        .flex_wrap()
                        .ml_auto()
                        .gap(px(8.))
                        .child(segmented(
                            "cost-basis",
                            vec![
                                t!("dashboard.cost_equivalent").into(),
                                t!("dashboard.cost_paid").into(),
                            ],
                            usize::from(self.cost.paid),
                            cx.listener(|this, ix: &usize, _, cx| {
                                this.set_cost_basis(*ix == 1, cx)
                            }),
                        ))
                        // CARM-301：机器筛选在时间范围前，只作用于花费卡；没有配对远端时不显示。
                        .children(self.render_machine_filter(cx))
                        .child(segmented(
                            "cost-range",
                            RANGES.into_iter().map(period_label).collect(),
                            self.cost.range,
                            cx.listener(|this, ix: &usize, _, cx| this.set_cost_range(*ix, cx)),
                        )),
                ),
        );
        match &self.cost.summary {
            None => panel.child(skeleton_rows(6)),
            Some(Err(_)) => panel.child(
                div()
                    .text_color(cx.theme().danger)
                    .child(t!("dashboard.cost_load_failed")),
            ),
            Some(Ok(summary)) => panel.child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap(px(24.))
                    .child(
                        self.render_cost_vendors(summary, cx)
                            .flex_grow(2.)
                            .flex_basis(px(295.)),
                    )
                    .child(
                        self.render_cost_models(summary, cx)
                            .flex_grow(3.)
                            .flex_basis(px(380.)),
                    ),
            ),
        }
    }

    fn render_cost_vendors(&self, summary: &CostSummary, cx: &mut Context<Self>) -> Div {
        let paid = self.cost.paid;
        // 订阅按账号计费：单看一台机器时没法拆，金额一律显示「—」。
        let per_machine = paid && self.cost.machine != MachineFilter::All;
        let (rows, total, caption) = if paid {
            let days = match RANGES[self.cost.range] {
                Period::Days(days) => days,
                _ => 1,
            };
            let rows: Vec<_> = SUBSCRIPTION_VENDORS
                .into_iter()
                .map(|vendor| {
                    let usage = summary.vendors.iter().find(|row| row.vendor == vendor);
                    let amount = self
                        .cost
                        .fees
                        .get(&vendor)
                        .filter(|_| !per_machine)
                        .map(|fee| prorated_fee(*fee, days));
                    (vendor, usage, amount)
                })
                .collect();
            let total = rows
                .iter()
                .filter_map(|(_, _, amount)| *amount)
                .reduce(|a, b| a + b);
            let caption = if per_machine {
                t!("dashboard.cost_paid_per_machine")
            } else {
                t!("dashboard.cost_paid_hint")
            };
            (rows, total, caption)
        } else {
            let rows = summary
                .vendors
                .iter()
                .map(|row| {
                    let has_price = row.cost_usd > 0.0 || row.tokens > row.unpriced_tokens;
                    (row.vendor, Some(row), has_price.then_some(row.cost_usd))
                })
                .collect();
            (
                rows,
                Some(summary.total_usd),
                t!("dashboard.cost_equivalent_hint"),
            )
        };
        let amount = |value: Option<f64>| value.map_or_else(|| "—".into(), short_cost);
        let denominator = total.unwrap_or_default();
        let share = |cost: f64| {
            if denominator > 0.0 {
                cost / denominator
            } else {
                0.0
            }
        };
        let stacked_bar = h_flex()
            .w_full()
            .h(px(12.))
            .rounded(px(3.))
            .overflow_hidden()
            .bg(cx.theme().muted)
            .children(rows.iter().filter_map(|(vendor, _, cost)| {
                let fraction = share((*cost)?);
                (fraction > 0.0).then(|| {
                    div()
                        .h_full()
                        .w(relative(fraction as f32))
                        .flex_shrink_0()
                        .bg(rgb(vendor_color(*vendor)))
                })
            }));

        v_flex()
            .min_w_0()
            .gap(px(8.))
            .child(cost_note(caption, cx))
            .child(
                mono_num(34., cx)
                    .font_weight(FontWeight::BOLD)
                    .text_color(amber_text(cx))
                    .child(amount(total)),
            )
            .child(stacked_bar)
            .children(rows.into_iter().map(|(vendor, usage, cost)| {
                v_flex()
                    .w_full()
                    .gap(px(2.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(cx.theme().muted)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .size(px(9.))
                                    .rounded(px(2.))
                                    .bg(rgb(vendor_color(vendor))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(vendor_label(vendor)),
                            )
                            .when(paid && cost.is_none() && !per_machine, |row| {
                                row.child(
                                    Button::new(format!("cost-missing-fee-{}", vendor.as_str()))
                                        .link()
                                        .xsmall()
                                        .label(t!("dashboard.cost_missing_fee"))
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(DashboardEvent::OpenSubscriptionSettings);
                                        })),
                                )
                            })
                            .when(!paid || cost.is_some() || per_machine, |row| {
                                row.child(
                                    mono_num(17., cx)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(amount(cost)),
                                )
                                .child(
                                    cost_note(
                                        cost.map_or_else(
                                            || "—".into(),
                                            |cost| {
                                                let percent = share(cost) * 100.;
                                                if percent >= 10.0 {
                                                    format!("{percent:.0}%")
                                                } else {
                                                    format!("{percent:.1}%")
                                                }
                                            },
                                        ),
                                        cx,
                                    )
                                    .w(px(47.))
                                    .text_right(),
                                )
                            }),
                    )
                    .when_some(usage, |row, usage| {
                        row.child(cost_note(vendor_sources(usage), cx).pl(px(17.)))
                    })
            }))
            .when(!paid && summary.unpriced_tokens > 0, |column| {
                let names: Vec<_> = summary
                    .models
                    .iter()
                    .filter(|model| model.unpriced_tokens > 0)
                    .map(|model| model.model.as_str())
                    .collect();
                column.child(
                    div()
                        .w_full()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(amber_text(cx).opacity(0.35))
                        .bg(amber_text(cx).opacity(0.08))
                        .text_color(amber_text(cx))
                        .text_size(px(12.))
                        .child(t!(
                            "dashboard.cost_unpriced_note",
                            count = names.len(),
                            tokens = cost_tokens(summary.unpriced_tokens),
                            models = names.join(", ")
                        )),
                )
            })
    }

    fn render_cost_models(&self, summary: &CostSummary, cx: &mut Context<Self>) -> Div {
        let paid = self.cost.paid;
        // 汇总已经把整条未计价的模型放在末尾。
        let priced_count = summary.models.len() - summary.unpriced_models().count();
        let (priced, unpriced) = summary.models.split_at(priced_count);
        let (shown_priced, shown_unpriced) = if self.cost.expanded {
            (priced, unpriced)
        } else {
            (
                &priced[..priced.len().min(RANKED_MODELS)],
                &unpriced[..unpriced.len().min(1)],
            )
        };
        let hidden = priced.len().saturating_sub(RANKED_MODELS) + unpriced.len().saturating_sub(1);
        let max = priced
            .first()
            .map_or(0.0, |model| model.cost_usd)
            .max(f64::EPSILON);

        v_flex()
            .min_w_0()
            .gap(px(6.))
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("dashboard.model_title")),
                    )
                    .when(
                        !paid && priced.len() > RANKED_MODELS && !self.cost.expanded,
                        |heading| heading.child(cost_note(t!("dashboard.cost_model_top"), cx)),
                    ),
            )
            .when(paid, |column| {
                column.child(cost_note(t!("dashboard.cost_paid_models_hint"), cx))
            })
            .when(summary.models.is_empty(), |column| {
                column.child(cost_note(t!("dashboard.model_empty"), cx))
            })
            .children(
                shown_priced
                    .iter()
                    .enumerate()
                    .map(|(i, model)| cost_model_row(model, Some(i + 1), max, paid, cx)),
            )
            .children(
                shown_unpriced
                    .iter()
                    .map(|model| cost_model_row(model, None, max, paid, cx)),
            )
            .when(hidden > 0, |column| {
                column.child(
                    Button::new("cost-expand-models")
                        .ghost()
                        .xsmall()
                        .self_start()
                        .label(if self.cost.expanded {
                            t!("dashboard.cost_models_collapse")
                        } else {
                            t!("dashboard.cost_models_expand", count = hidden)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cost.expanded = !this.cost.expanded;
                            if this.cost.expanded {
                                telemetry::event("home_cost_models_expand", json!({}));
                            }
                            cx.notify();
                        })),
                )
            })
    }
}

fn cost_model_row(model: &ModelCost, rank: Option<usize>, max: f64, paid: bool, cx: &App) -> Div {
    let unpriced = model.is_unpriced();
    let muted = cx.theme().muted_foreground;
    let (fraction, color, value) = if unpriced {
        (
            1.0,
            rgb(MUTED),
            format!("{} tok", cost_tokens(model.tokens)),
        )
    } else {
        let color = if paid {
            rgb(MUTED)
        } else {
            rgb(vendor_color(model.vendor))
        };
        let value = if paid {
            "—".into()
        } else {
            short_cost(model.cost_usd)
        };
        ((model.cost_usd / max) as f32, color, value)
    };
    h_flex()
        .w_full()
        .min_h(px(21.))
        .gap(px(8.))
        .text_color(if paid || unpriced {
            muted
        } else {
            cx.theme().foreground
        })
        .child(
            cost_note(rank.map(|n| n.to_string()).unwrap_or_default(), cx)
                .w(px(18.))
                .flex_none(),
        )
        .child(
            h_flex()
                .gap(px(5.))
                .w(relative(0.43))
                .min_w_0()
                .child(div().min_w_0().truncate().child(model.model.clone()))
                .when(model.unpriced_tokens > 0, |label| {
                    label.child(
                        cost_note(
                            if unpriced {
                                t!("dashboard.cost_unpriced")
                            } else {
                                t!("dashboard.cost_partly_unpriced")
                            },
                            cx,
                        )
                        .px(px(4.))
                        .rounded(px(3.))
                        .bg(cx.theme().muted)
                        .flex_none()
                        .whitespace_nowrap(),
                    )
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(8.))
                .rounded(px(2.))
                .overflow_hidden()
                .bg(cx.theme().muted)
                .child(div().h_full().w(relative(fraction)).bg(color)),
        )
        .child(
            mono_num(12., cx)
                .w(px(80.))
                .flex_none()
                .text_right()
                .child(value),
        )
}

fn cost_note(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn vendor_sources(usage: &VendorCost) -> String {
    usage
        .sources
        .iter()
        .map(|(provider, tokens)| {
            format!("{} · {} tok", display_name(provider), cost_tokens(*tokens))
        })
        .collect::<Vec<_>>()
        .join("  /  ")
}

fn cost_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        format!("{:.2}B", tokens as f64 / 1_000_000_000.0)
    } else {
        short_tokens(tokens)
    }
}

fn vendor_label(vendor: Vendor) -> SharedString {
    match vendor {
        Vendor::Anthropic => "Anthropic".into(),
        Vendor::OpenAI => "OpenAI".into(),
        Vendor::Google => "Google".into(),
        Vendor::Other => t!("dashboard.cost_other_vendor").into(),
    }
}

fn vendor_color(vendor: Vendor) -> u32 {
    match vendor {
        Vendor::Anthropic => CLAUDE,
        Vendor::OpenAI => CODEX,
        Vendor::Google => GEMINI,
        Vendor::Other => MUTED,
    }
}
