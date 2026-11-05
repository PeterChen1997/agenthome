//! 消息流中单条消息的渲染。纯渲染辅助：读取消息与折叠状态，不持有状态。
//!
//! 折叠 / 展开通过 `Workspace::toggle_block` 回写，并触发该行重新测量。

use std::collections::HashSet;

use agenthome_core::model::{ContentBlock, Message, Role};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    text::TextView,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, IntoElement, ParentElement as _, SharedString,
    Styled as _, WeakEntity, Window, div, prelude::FluentBuilder as _,
};

use crate::format::{absolute_time, preview};
use crate::workspace::{BlockKey, Workspace};

/// 工具结果折叠态显示的字符数。
const RESULT_PREVIEW_CHARS: usize = 200;

pub fn render_message(
    message_ix: usize,
    message: &Message,
    expanded: &HashSet<BlockKey>,
    owner: &WeakEntity<Workspace>,
    cx: &mut App,
) -> AnyElement {
    let is_user = matches!(message.role, Role::User);
    let role_label = match message.role {
        Role::User => "用户",
        Role::Assistant => "助手",
        Role::System => "系统",
        Role::Tool => "工具",
    };
    let message_id: SharedString = message.id.clone().into();
    let blocks = message
        .blocks
        .iter()
        .enumerate()
        .map(|(block_ix, block)| {
            let key = (message.id.clone(), block_ix);
            let open = expanded.contains(&key);
            let owner = owner.clone();
            let on_toggle = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let key = key.clone();
                let _ = owner.update(cx, |this, cx| this.toggle_block(message_ix, key, cx));
            };
            render_block(
                ElementId::from((message_id.clone(), block_ix)),
                block,
                open,
                on_toggle,
                cx,
            )
        })
        .collect::<Vec<_>>();
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .p_3()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .bg(if is_user {
            cx.theme().secondary
        } else {
            cx.theme().group_box
        })
        .child(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(div().font_medium().child(role_label))
                .when_some(message.model.clone(), |this, model| this.child(model))
                .child(absolute_time(message.timestamp_ms))
                .when(message.is_meta, |this| this.child("元信息")),
        )
        .children(blocks)
        .into_any_element()
}

fn render_block(
    id: ElementId,
    block: &ContentBlock,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &mut App,
) -> AnyElement {
    match block {
        ContentBlock::Text { text } => TextView::markdown((id, "text"), text.clone())
            .selectable(true)
            .into_any_element(),
        ContentBlock::Thinking { text } => {
            let text = text.clone();
            disclosure(
                id,
                "💭 思考过程",
                open,
                on_toggle,
                cx,
                move |id, cx| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(TextView::markdown((id, "thinking"), text).selectable(true))
                        .into_any_element()
                },
            )
        }
        ContentBlock::ToolCall { name, input, .. } => {
            let pretty = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
            disclosure(
                id,
                format!("🔧 {name}"),
                open,
                on_toggle,
                cx,
                move |_, cx| code_box(pretty, cx),
            )
        }
        ContentBlock::ToolResult {
            output, is_error, ..
        } => {
            let label = if *is_error {
                "✗ 工具结果（错误）"
            } else {
                "工具结果"
            };
            let short = preview(output, RESULT_PREVIEW_CHARS);
            let full = output.clone();
            v_flex()
                .gap_1()
                .child(toggle_button(id.clone(), label, open, on_toggle))
                .child(if open {
                    code_box(full, cx)
                } else {
                    div()
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(short)
                        .into_any_element()
                })
                .into_any_element()
        }
        ContentBlock::Image { media_type } => muted_line(format!("[图片 {media_type}]"), cx),
        ContentBlock::Compaction { summary } => {
            let summary = summary.clone();
            disclosure(id, "压缩摘要", open, on_toggle, cx, move |id, cx| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(TextView::markdown((id, "compaction"), summary).selectable(true))
                    .into_any_element()
            })
        }
        ContentBlock::Unknown { native_type, .. } => {
            muted_line(format!("[未知块 {native_type}]"), cx)
        }
    }
}

/// 折叠行：一个切换按钮 + 展开时才构建的内容。
fn disclosure(
    id: ElementId,
    label: impl Into<SharedString>,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &mut App,
    content: impl FnOnce(ElementId, &mut App) -> AnyElement,
) -> AnyElement {
    v_flex()
        .gap_1()
        .child(toggle_button(id.clone(), label, open, on_toggle))
        .when(open, |this| this.child(content(id, cx)))
        .into_any_element()
}

fn toggle_button(
    id: ElementId,
    label: impl Into<SharedString>,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new((id, "toggle"))
        .ghost()
        .small()
        .icon(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .label(label)
        .on_click(on_toggle)
}

/// 等宽文本块，用于工具入参 JSON 与工具输出原文。
fn code_box(text: String, cx: &mut App) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .p_2()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .text_xs()
        .font_family(cx.theme().mono_font_family.clone())
        .child(text)
        .into_any_element()
}

fn muted_line(text: String, cx: &mut App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}
