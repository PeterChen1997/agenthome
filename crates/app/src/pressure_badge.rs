//! 额度使用压力徽标：带底色的档位词，悬停显示推算依据与五档各自的含义。
//! 左栏住户行、仪表盘额度详情、菜单栏面板共用。

use agenthome_core::pressure::{Pressure, PressureReading};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};

use crate::format::{pressure_label, pressure_short_label};

const LEVELS: [Pressure; 5] = [
    Pressure::Relaxed,
    Pressure::Steady,
    Pressure::Tight,
    Pressure::Critical,
    Pressure::Exhausted,
];

/// 档位颜色：宽裕绿、正常蓝、偏紧琥珀、紧张与用尽红。
pub fn pressure_color(level: Pressure, cx: &App) -> Hsla {
    match level {
        Pressure::Relaxed => cx.theme().success,
        Pressure::Steady => cx.theme().info,
        Pressure::Tight => cx.theme().warning,
        Pressure::Critical | Pressure::Exhausted => cx.theme().danger,
    }
}

fn level_description(level: Pressure) -> String {
    match level {
        Pressure::Relaxed => t!("format.pressure_desc_relaxed"),
        Pressure::Steady => t!("format.pressure_desc_steady"),
        Pressure::Tight => t!("format.pressure_desc_tight"),
        Pressure::Critical => t!("format.pressure_desc_critical"),
        Pressure::Exhausted => t!("format.pressure_desc_exhausted"),
    }
    .into()
}

#[derive(IntoElement)]
pub struct PressureBadge {
    id: ElementId,
    reading: PressureReading,
    /// 徽标里档位词前的短窗口名（「7d」）；最紧的就是主窗口时不写。
    prefix: Option<String>,
    /// 悬停说明里的完整窗口名（「7 天」）。
    window: String,
    /// 用短档位词（左栏住户行）；悬停说明仍是完整说法。
    short: bool,
}

impl PressureBadge {
    pub fn new(id: impl Into<ElementId>, reading: PressureReading, window: String) -> Self {
        Self {
            id: id.into(),
            reading,
            prefix: None,
            window,
            short: false,
        }
    }

    pub fn prefix(mut self, prefix: Option<String>) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn short(mut self) -> Self {
        self.short = true;
        self
    }
}

impl RenderOnce for PressureBadge {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let level = self.reading.level;
        let color = pressure_color(level, cx);
        let label = if self.short {
            pressure_short_label(level)
        } else {
            pressure_label(level)
        };
        let text = match &self.prefix {
            Some(prefix) => format!("{prefix} {label}"),
            None => label,
        };
        let reading = self.reading;
        let window = self.window;
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .h(px(17.))
            .px(px(6.))
            .rounded(px(3.))
            .bg(color.opacity(0.18))
            .border_1()
            .border_color(color.opacity(0.45))
            .text_color(color)
            .text_size(px(11.))
            .line_height(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .child(text)
            .tooltip(move |window_cx, cx| {
                let window = window.clone();
                Tooltip::element(move |_, cx| tip(&window, reading, cx)).build(window_cx, cx)
            })
    }
}

/// 悬停说明：标题（窗口 · 档位）、这一档的推算结果、五档图例（当前档高亮）。
fn tip(window: &str, reading: PressureReading, cx: &App) -> AnyElement {
    let level = reading.level;
    let detail: SharedString = match level {
        Pressure::Exhausted => t!("format.pressure_tip_exhausted").into(),
        _ => {
            let projected = t!(
                "format.pressure_tip_projected",
                percent = (reading.projected * 100.0).round()
            );
            match reading.runs_out_in_ms {
                Some(ms) if ms > 0 => format!(
                    "{projected}{}",
                    t!(
                        "format.pressure_tip_runs_out",
                        time = crate::format::countdown(ms)
                    )
                )
                .into(),
                _ => projected.into(),
            }
        }
    };
    v_flex()
        .max_w(px(280.))
        .py(px(4.))
        .gap(px(6.))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(pressure_color(level, cx))
                .child(format!("{window} · {}", pressure_label(level))),
        )
        .child(div().text_size(px(12.)).child(detail))
        .child(
            div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(t!("format.pressure_tip_how")),
        )
        .child(
            v_flex()
                .pt(px(6.))
                .gap(px(3.))
                .border_t_1()
                .border_color(cx.theme().border)
                .children(LEVELS.into_iter().map(|each| {
                    let current = each == level;
                    h_flex()
                        .gap(px(8.))
                        .text_size(px(11.5))
                        .child(
                            div()
                                .w(px(58.))
                                .whitespace_nowrap()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(pressure_color(each, cx))
                                .child(pressure_label(each)),
                        )
                        .child(
                            div()
                                .text_color(if current {
                                    cx.theme().popover_foreground
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .when(current, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(level_description(each)),
                        )
                })),
        )
        .into_any_element()
}
