//! 会话消息流：按「轮次」组织的行渲染。
//!
//! `Transcript` 在后台把消息预处理成轮次与工具调用配对；`render_row` 只读它与折叠状态，
//! 折叠 / 展开通过 `ArchiveView::toggle_block` 回写并触发该行重新测量。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agenthome_core::model::{ContentBlock, Message, ProviderId, Role};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, tag::Tag,
    text::TextView, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, div, img, px,
};

use crate::archive::ArchiveView;
use crate::assets::FONT_PIXEL_MONO;
use crate::format::{clock_time, preview, short_duration, short_tokens};

/// 消息流里一个可折叠块的标识：(消息 id, 块下标)。
pub type BlockKey = (String, usize);

/// Trace 行参数摘要的最大字符数。
const SUMMARY_CHARS: usize = 120;
/// 轮次编号列宽（设计稿 28px）。
const TURN_COLUMN: f32 = 28.;

/// 一次工具调用配对到的结果位置。
#[derive(Clone, Copy, Debug)]
struct PairedResult {
    message_ix: usize,
    block_ix: usize,
    is_error: bool,
    duration_ms: Option<i64>,
}

/// 预处理后的消息流：轮次划分与工具调用配对，构建在后台线程。
pub struct Transcript {
    pub messages: Arc<Vec<Message>>,
    /// 每条消息所属轮次（从 1 起）；首个用户消息之前的消息为 0。
    turn_of: Vec<u32>,
    /// 每个轮次的首条消息下标。
    pub turn_starts: Vec<usize>,
    /// tool_call_id → 配对的结果。
    results: HashMap<String, PairedResult>,
    /// tool_call_id → 调用所在消息下标；搜索定位到「只含已配对结果」的消息时改停在调用行。
    call_ix: HashMap<String, usize>,
    pub tool_calls: usize,
    pub tool_failures: usize,
    pub estimated_cost_usd: Option<f64>,
}

impl Transcript {
    pub fn build(messages: Vec<Message>) -> Self {
        let mut turn_of = Vec::with_capacity(messages.len());
        let mut turn_starts = Vec::new();
        let mut call_at: HashMap<&str, i64> = HashMap::new();
        let mut results = HashMap::new();
        let mut call_ix = HashMap::new();
        let mut tool_calls = 0;
        let mut tool_failures = 0;
        for (ix, message) in messages.iter().enumerate() {
            if message.role == Role::User && !message.is_meta {
                turn_starts.push(ix);
            }
            turn_of.push(u32::try_from(turn_starts.len()).unwrap_or(u32::MAX));
            for (block_ix, block) in message.blocks.iter().enumerate() {
                match block {
                    ContentBlock::ToolCall { tool_call_id, .. } => {
                        tool_calls += 1;
                        call_at.insert(tool_call_id, message.timestamp_ms);
                        call_ix.insert(tool_call_id.clone(), ix);
                    }
                    ContentBlock::ToolResult {
                        tool_call_id,
                        is_error,
                        ..
                    } => {
                        if *is_error {
                            tool_failures += 1;
                        }
                        let duration_ms = call_at
                            .get(tool_call_id.as_str())
                            .map(|at| message.timestamp_ms - at);
                        results.insert(
                            tool_call_id.clone(),
                            PairedResult {
                                message_ix: ix,
                                block_ix,
                                is_error: *is_error,
                                duration_ms,
                            },
                        );
                    }
                    _ => {}
                }
            }
        }
        let estimated_cost_usd = agenthome_core::pricing::estimate_messages(&messages);
        Self {
            messages: Arc::new(messages),
            turn_of,
            turn_starts,
            results,
            call_ix,
            tool_calls,
            tool_failures,
            estimated_cost_usd,
        }
    }

    /// 某条消息所属轮次（从 1 起）。
    pub fn turn_of(&self, message_ix: usize) -> u32 {
        self.turn_of.get(message_ix).copied().unwrap_or(0)
    }

    /// 定位到第 `message_ix` 条消息时实际停靠的行。已配对的工具结果并在调用行里显示、自身零高，
    /// 只含这类结果的消息改停在（最早的）调用行，否则高亮会落在看不见的行上。
    pub fn anchor_of(&self, message_ix: usize) -> usize {
        let Some(message) = self.messages.get(message_ix) else {
            return message_ix;
        };
        let mut anchor: Option<usize> = None;
        for block in &message.blocks {
            let ContentBlock::ToolResult { tool_call_id, .. } = block else {
                return message_ix;
            };
            match self.call_ix.get(tool_call_id) {
                Some(&call) if call < message_ix => {
                    anchor = Some(anchor.map_or(call, |a| a.min(call)));
                }
                _ => return message_ix,
            }
        }
        anchor.unwrap_or(message_ix)
    }

    fn is_turn_start(&self, message_ix: usize) -> bool {
        self.turn_of(message_ix) > 0
            && self
                .turn_starts
                .get(self.turn_of(message_ix) as usize - 1)
                .is_some_and(|start| *start == message_ix)
    }

    fn paired_output(&self, tool_call_id: &str) -> Option<&str> {
        let paired = self.results.get(tool_call_id)?;
        match self
            .messages
            .get(paired.message_ix)?
            .blocks
            .get(paired.block_ix)?
        {
            ContentBlock::ToolResult { output, .. } => Some(output.as_str()),
            _ => None,
        }
    }
}

/// 一条 Trace 行的内容；展开正文只在需要时构建。
struct TraceRow {
    name: SharedString,
    summary: String,
    /// (文字, 颜色) 的状态标签。
    status: Option<(SharedString, TagKind)>,
    trailing: Option<String>,
    body: String,
}

#[derive(Clone, Copy)]
enum TagKind {
    Ok,
    Err,
    Dim,
}

/// 来源头像；没有头像素材的来源用机器人图标。
pub fn provider_avatar(provider: &ProviderId, size: f32) -> AnyElement {
    let slug = match provider {
        ProviderId::ClaudeCode => Some("claude"),
        ProviderId::Codex => Some("codex"),
        ProviderId::GeminiCli => Some("gemini"),
        _ => None,
    };
    match slug {
        Some(slug) => img(format!("avatars/avatar-{slug}.png"))
            .size(px(size))
            .rounded(px(3.))
            .into_any_element(),
        None => Icon::new(IconName::Bot)
            .with_size(px(size))
            .into_any_element(),
    }
}

/// 住户名（头像旁用），不是产品全名。
pub fn provider_short_name(provider: &ProviderId) -> &str {
    match provider {
        ProviderId::ClaudeCode => "Claude",
        ProviderId::Codex => "Codex",
        ProviderId::GeminiCli => "Gemini",
        other => other.as_str(),
    }
}

/// 产品全名，详情头部用。
pub fn provider_display_name(provider: &ProviderId) -> &str {
    match provider {
        ProviderId::ClaudeCode => "Claude Code",
        ProviderId::Codex => "Codex",
        ProviderId::GeminiCli => "Gemini CLI",
        other => other.as_str(),
    }
}

/// 渲染消息流的第 `ix` 行。配对过的工具结果已并入调用行，自身渲染为零高。
/// `highlighted` 为真时整行铺一层强调色底（从搜索跳过来时短暂标出命中的消息）。
pub fn render_row(
    ix: usize,
    transcript: &Transcript,
    provider: &ProviderId,
    expanded: &HashSet<BlockKey>,
    highlighted: bool,
    owner: &WeakEntity<ArchiveView>,
    cx: &mut App,
) -> AnyElement {
    let Some(message) = transcript.messages.get(ix) else {
        return div().into_any_element();
    };
    let turn_start = transcript.is_turn_start(ix);
    let mut body: Vec<AnyElement> = Vec::new();
    let mut traces: Vec<(usize, TraceRow)> = Vec::new();
    let message_id: SharedString = message.id.clone().into();
    let flush = |traces: &mut Vec<(usize, TraceRow)>, body: &mut Vec<AnyElement>, cx: &mut App| {
        if traces.is_empty() {
            return;
        }
        let rows = std::mem::take(traces);
        body.push(render_trace_group(
            ix,
            &message_id,
            rows,
            expanded,
            owner,
            cx,
        ));
    };

    match message.role {
        Role::User if !message.is_meta => {
            body.push(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(secondary(cx))
                    .child(
                        div()
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child(t!("message.you").to_string()),
                    )
                    .child(clock_time(message.timestamp_ms))
                    .into_any_element(),
            );
            let text = message.text();
            if !text.is_empty() {
                body.push(
                    div()
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .py_3()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().group_box)
                        .text_sm()
                        .child(
                            TextView::markdown((message_id.clone(), usize::MAX), text)
                                .selectable(true),
                        )
                        .into_any_element(),
                );
            }
        }
        Role::Assistant => {
            body.push(render_assistant_header(ix, transcript, provider, cx));
        }
        _ => {}
    }

    for (block_ix, block) in message.blocks.iter().enumerate() {
        match block {
            ContentBlock::Text { text } if message.role == Role::Assistant => {
                flush(&mut traces, &mut body, cx);
                body.push(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_sm()
                        .child(
                            TextView::markdown((message_id.clone(), block_ix), text.clone())
                                .selectable(true),
                        )
                        .into_any_element(),
                );
            }
            // 用户文本已整体渲染成一个盒子；其余用户块按 Trace 行处理。
            ContentBlock::Text { .. } if message.role == Role::User && !message.is_meta => {}
            ContentBlock::Text { text } => traces.push((
                block_ix,
                TraceRow {
                    name: role_label(message.role, message.is_meta).into(),
                    summary: first_line(text),
                    status: None,
                    trailing: None,
                    body: text.clone(),
                },
            )),
            ContentBlock::Thinking { text } => traces.push((
                block_ix,
                TraceRow {
                    name: t!("message.thinking").to_string().into(),
                    summary: first_line(text),
                    status: None,
                    trailing: Some(
                        t!(
                            "message.chars",
                            count = short_tokens(text.chars().count() as u64)
                        )
                        .to_string(),
                    ),
                    body: text.clone(),
                },
            )),
            ContentBlock::ToolCall {
                tool_call_id,
                name,
                input,
            } => {
                let paired = transcript.results.get(tool_call_id).copied();
                let status = paired.map(|p| {
                    if p.is_error {
                        (SharedString::from("ERR"), TagKind::Err)
                    } else {
                        (SharedString::from("OK"), TagKind::Ok)
                    }
                });
                // TODO: Bash 的 EXIT N 与 Edit 的 +a −b 需要 core 解析输出后提供。
                let mut body_text =
                    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                if let Some(output) = transcript.paired_output(tool_call_id) {
                    body_text.push_str(&format!("\n\n{}\n", t!("message.result_divider")));
                    body_text.push_str(output);
                }
                traces.push((
                    block_ix,
                    TraceRow {
                        name: name.clone().into(),
                        summary: summarize_input(input),
                        status,
                        trailing: paired.and_then(|p| p.duration_ms).map(short_duration),
                        body: body_text,
                    },
                ));
            }
            ContentBlock::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => {
                // 已配对的结果显示在调用行里（duration 有值即见过调用）；孤儿结果单独成行。
                let paired_here = transcript.results.get(tool_call_id).is_some_and(|p| {
                    p.message_ix == ix && p.block_ix == block_ix && p.duration_ms.is_some()
                });
                if paired_here {
                    continue;
                }
                traces.push((
                    block_ix,
                    TraceRow {
                        name: t!("message.tool_result").to_string().into(),
                        summary: first_line(output),
                        status: Some(if *is_error {
                            ("ERR".into(), TagKind::Err)
                        } else {
                            ("OK".into(), TagKind::Ok)
                        }),
                        trailing: None,
                        body: output.clone(),
                    },
                ));
            }
            ContentBlock::Image { media_type } => traces.push((
                block_ix,
                TraceRow {
                    name: t!("message.image").to_string().into(),
                    summary: media_type.clone(),
                    status: None,
                    trailing: None,
                    body: t!("message.image_body", media_type = media_type).to_string(),
                },
            )),
            ContentBlock::Compaction { summary } => traces.push((
                block_ix,
                TraceRow {
                    name: t!("message.compaction").to_string().into(),
                    summary: first_line(summary),
                    status: Some(("COMPACT".into(), TagKind::Dim)),
                    trailing: None,
                    body: summary.clone(),
                },
            )),
            ContentBlock::Unknown { native_type, raw } => traces.push((
                block_ix,
                TraceRow {
                    name: native_type.clone().into(),
                    summary: summarize_input(raw),
                    status: Some(("RAW".into(), TagKind::Dim)),
                    trailing: None,
                    body: serde_json::to_string_pretty(raw).unwrap_or_else(|_| raw.to_string()),
                },
            )),
        }
    }
    flush(&mut traces, &mut body, cx);

    if body.is_empty() {
        return div().into_any_element();
    }
    let turn_number = transcript.turn_of(ix);
    h_flex()
        .items_start()
        .w_full()
        .min_w_0()
        .gap_3()
        .px_6()
        .when(turn_start, |row| {
            row.pt_4().border_t_1().border_color(cx.theme().secondary)
        })
        .when(highlighted, |row| row.bg(cx.theme().primary.opacity(0.16)))
        .pb_3()
        .child(
            div()
                .flex_none()
                .w(px(TURN_COLUMN))
                .font_family(FONT_PIXEL_MONO)
                .text_size(px(18.))
                .text_color(cx.theme().muted_foreground)
                .when(turn_start, |col| col.child(format!("{turn_number:02}"))),
        )
        .child(v_flex().flex_1().min_w_0().gap_2().children(body))
        .into_any_element()
}

/// assistant 行头：头像 + 名字 + 模型 + 时间 + 右侧「+N 输出 · 耗时」。
fn render_assistant_header(
    ix: usize,
    transcript: &Transcript,
    provider: &ProviderId,
    cx: &mut App,
) -> AnyElement {
    let message = &transcript.messages[ix];
    let output = message.usage.map(|u| u.output_tokens).unwrap_or(0);
    // 没有响应耗时字段，用到下一条消息的间隔近似。
    let elapsed = transcript
        .messages
        .get(ix + 1)
        .map(|next| next.timestamp_ms - message.timestamp_ms)
        .filter(|ms| *ms > 0);
    let mut trailing = t!("message.output", tokens = short_tokens(output)).to_string();
    if let Some(ms) = elapsed {
        trailing.push_str(&format!(" · {}", short_duration(ms)));
    }
    h_flex()
        .gap_2()
        .text_xs()
        .text_color(secondary(cx))
        .child(provider_avatar(provider, 20.))
        .child(
            div()
                .font_semibold()
                .text_color(cx.theme().foreground)
                .child(provider_short_name(provider).to_string()),
        )
        .when_some(message.model.clone(), |row, model| row.child(model))
        .child(clock_time(message.timestamp_ms))
        .child(
            div()
                .ml_auto()
                .font_family(cx.theme().mono_font_family.clone())
                .child(trailing),
        )
        .into_any_element()
}

/// 一组相邻 Trace 行：共用一个边框盒，点击行切换展开。
fn render_trace_group(
    message_ix: usize,
    message_id: &SharedString,
    rows: Vec<(usize, TraceRow)>,
    expanded: &HashSet<BlockKey>,
    owner: &WeakEntity<ArchiveView>,
    cx: &mut App,
) -> AnyElement {
    let border = cx.theme().border;
    let items = rows.into_iter().enumerate().map(|(n, (block_ix, row))| {
        let key = (message_id.to_string(), block_ix);
        let open = expanded.contains(&key);
        let owner = owner.clone();
        let id = ElementId::from((message_id.clone(), block_ix));
        let status = row.status.map(|(text, kind)| {
            let tag = match kind {
                TagKind::Ok => Tag::success(),
                TagKind::Err => Tag::danger(),
                TagKind::Dim => Tag::secondary(),
            };
            tag.small().font_family(FONT_PIXEL_MONO).child(text)
        });
        v_flex()
            .w_full()
            .when(n > 0, |block| {
                block.border_t_1().border_color(cx.theme().secondary)
            })
            .child(
                h_flex()
                    .id(id)
                    .w_full()
                    .h(px(34.))
                    .px_3()
                    .gap_2()
                    .bg(if open {
                        cx.theme().secondary
                    } else {
                        cx.theme().group_box
                    })
                    .hover(|style| style.bg(cx.theme().secondary))
                    .cursor_pointer()
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().flex_none().text_sm().font_medium().child(row.name))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(secondary(cx))
                            .child(row.summary),
                    )
                    .children(status)
                    .when_some(row.trailing, |line, trailing| {
                        line.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().muted_foreground)
                                .child(trailing),
                        )
                    })
                    .on_click(move |_, _, cx| {
                        let key = key.clone();
                        let _ = owner.update(cx, |this, cx| this.toggle_block(message_ix, key, cx));
                    }),
            )
            .when(open, |block| {
                block.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().secondary)
                        .bg(cx.theme().background)
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(secondary(cx))
                        .child(row.body),
                )
            })
    });
    v_flex()
        .w_full()
        .min_w_0()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(border)
        .overflow_hidden()
        .children(items)
        .into_any_element()
}

fn role_label(role: Role, is_meta: bool) -> String {
    match (role, is_meta) {
        (Role::User, true) => t!("message.role_meta").into(),
        (Role::User, false) => t!("message.role_user").into(),
        (Role::Assistant, _) => t!("message.role_assistant").into(),
        (Role::System, _) => t!("message.role_system").into(),
        (Role::Tool, _) => t!("message.role_tool").into(),
    }
}

fn first_line(text: &str) -> String {
    preview(text.lines().next().unwrap_or_default(), SUMMARY_CHARS)
}

/// 工具入参摘要：挑最能说明意图的字段，没有就压成一行 JSON。
fn summarize_input(input: &serde_json::Value) -> String {
    const KEYS: [&str; 8] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ];
    let picked = KEYS
        .iter()
        .find_map(|key| input.get(key).and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    preview(picked.lines().next().unwrap_or_default(), SUMMARY_CHARS)
}

/// 设计稿的「次要」文字色（#9AA6C0）：主题没有单独角色，用前景降透明度得到。
pub fn secondary(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.65)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{DataSource as _, MockDataSource};
    use agenthome_core::model::SessionQuery;

    fn sample() -> Transcript {
        let source = MockDataSource::new();
        let sessions = source.sessions(&SessionQuery::default()).unwrap();
        // 第二个会话消息少，够覆盖六种块的循环。
        let session = &sessions[1];
        Transcript::build(source.messages(&session.id).unwrap())
    }

    #[test]
    fn user_messages_open_turns_and_tool_calls_pair_with_results() {
        let transcript = sample();
        // mock 每 6 条一轮：用户 / 助手 / 调用 / 结果 / 思考 / 补充。
        assert_eq!(transcript.turn_starts.first(), Some(&0));
        assert_eq!(transcript.turn_of(0), 1);
        assert_eq!(transcript.turn_of(5), 1);
        assert_eq!(transcript.turn_of(6), 2);
        assert_eq!(transcript.tool_calls, transcript.results.len());
        let paired = transcript.results.values().next().unwrap();
        assert_eq!(paired.block_ix, 0);
        assert!(paired.duration_ms.is_some(), "结果应配对到之前的调用");
        assert!(transcript.estimated_cost_usd.is_some());
    }

    #[test]
    fn search_jump_lands_on_visible_row() {
        // 搜索命中可能落在「只含已配对工具结果」的消息上，它自身零高；必须改停在调用行，
        // 否则跳转后高亮落在看不见的行上，用户还是不知道命中在哪。
        let transcript = sample();
        // mock 第 3 条（下标 2）是工具调用，第 4 条（下标 3）是它的结果。
        assert_eq!(transcript.anchor_of(3), 2);
        // 普通消息停在自己身上；越界下标原样返回，不 panic。
        assert_eq!(transcript.anchor_of(0), 0);
        assert_eq!(transcript.anchor_of(1), 1);
        assert_eq!(transcript.anchor_of(usize::MAX), usize::MAX);
    }

    #[test]
    fn input_summary_prefers_intent_fields() {
        let input = serde_json::json!({ "description": "跑测试", "command": "cargo test" });
        assert_eq!(summarize_input(&input), "cargo test");
        let raw = serde_json::json!({ "x": 1 });
        assert_eq!(summarize_input(&raw), "{\"x\":1}");
    }
}
