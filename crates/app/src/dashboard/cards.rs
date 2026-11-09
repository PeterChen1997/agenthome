//! 仪表盘的无状态小件：卡片壳、标题、VT323 数字、体力条、标签、tile、横条行。

use agenthome_core::model::ProviderId;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Div, FontWeight, Hsla, IntoElement, ParentElement as _, RenderOnce,
    SharedString, Styled as _, Window, div, img, px, relative, rgb, rgba,
};

use super::fmt::{AMBER, GREEN, LINE, MUTED, RED, RED_TEXT, avatar_path, provider_color};
use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS};

/// 卡片壳：面板底 + 1px 线 + 4px 圆角，内容纵向排列。
pub fn card(cx: &App) -> Div {
    v_flex()
        .min_w_0()
        .gap(px(10.))
        .px(px(18.))
        .py(px(16.))
        .rounded(px(4.))
        .bg(cx.theme().group_box)
        .border_1()
        .border_color(cx.theme().border)
}

/// 卡片标题行：Pixelify Sans 标题 + 次要说明。
pub fn card_title(title: impl Into<SharedString>, sub: impl Into<SharedString>) -> Div {
    h_flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .font_family(FONT_PIXEL_SANS)
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(14.))
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(MUTED))
                .child(sub.into()),
        )
}

/// VT323 数字容器。
pub fn vt(size: f32) -> Div {
    div()
        .font_family(FONT_PIXEL_MONO)
        .text_size(px(size))
        .line_height(px(size))
        .whitespace_nowrap()
}

/// 住户头像；非内置来源用首字母方块代替。
pub fn avatar(provider: &ProviderId, size: f32) -> AnyElement {
    match avatar_path(provider) {
        Some(path) => img(path)
            .flex_shrink_0()
            .size(px(size))
            .rounded(px(3.))
            .border_1()
            .border_color(rgb(LINE))
            .into_any_element(),
        None => div()
            .flex_shrink_0()
            .size(px(size))
            .rounded(px(3.))
            .bg(rgb(provider_color(provider)))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(size * 0.5))
            .child(
                provider
                    .as_str()
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string())
                    .unwrap_or_default(),
            )
            .into_any_element(),
    }
}

/// 体力条颜色：剩余 >50% 绿，20–50% 琥珀，<20% 红。
pub fn hp_tier(left: f32) -> u32 {
    if left > 50.0 {
        GREEN
    } else if left >= 20.0 {
        AMBER
    } else {
        RED
    }
}

/// 10 格体力条；None 表示没有数据，全灰。
pub fn hp_bar(left: Option<f32>) -> Div {
    let filled = left.map_or(0, |l| (l / 10.0).round().clamp(0.0, 10.0) as usize);
    let color = left.map(hp_tier);
    h_flex().gap(px(3.)).children((0..10).map(move |i| {
        div().flex_1().h(px(10.)).rounded(px(1.)).bg(match color {
            Some(c) if i < filled => rgb(c),
            _ => rgb(LINE),
        })
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    Ok,
    Warn,
    Err,
    Dim,
}

/// VT323 小标签（合规 / 告警 / 计数）。
#[derive(IntoElement)]
pub struct Tag {
    text: SharedString,
    kind: TagKind,
}

impl Tag {
    pub fn new(text: impl Into<SharedString>, kind: TagKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

impl RenderOnce for Tag {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (bg, fg): (Hsla, Hsla) = match self.kind {
            TagKind::Ok => (rgba((GREEN << 8) | 0x29).into(), rgb(GREEN).into()),
            TagKind::Warn => (rgba((AMBER << 8) | 0x29).into(), rgb(AMBER).into()),
            TagKind::Err => (rgba((RED << 8) | 0x29).into(), rgb(RED_TEXT).into()),
            TagKind::Dim => (cx.theme().muted, rgb(MUTED).into()),
        };
        div()
            .flex()
            .items_center()
            .h(px(18.))
            .px(px(6.))
            .rounded(px(2.))
            .bg(bg)
            .text_color(fg)
            .font_family(FONT_PIXEL_MONO)
            .text_size(px(14.))
            .line_height(px(14.))
            .whitespace_nowrap()
            .child(self.text)
    }
}

/// 数字 tile：小标签 + VT323 大数字 + 一行说明。
#[derive(IntoElement)]
pub struct Tile {
    label: SharedString,
    value: SharedString,
    note: Option<SharedString>,
    /// 说明文字颜色，None 用弱化色。
    note_color: Option<u32>,
    value_size: f32,
}

impl Tile {
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            note: None,
            note_color: None,
            value_size: 30.,
        }
    }

    pub fn note(mut self, note: impl Into<SharedString>, color: Option<u32>) -> Self {
        self.note = Some(note.into());
        self.note_color = color;
        self
    }
}

impl RenderOnce for Tile {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(4.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(3.))
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(self.label),
            )
            .child(vt(self.value_size).child(self.value))
            .when_some(self.note, |tile, note| {
                tile.child(
                    // 窄窗口下来源拆分放不下一行：换行而不是截断，数字不能被省略号吃掉。
                    div()
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(match self.note_color {
                            Some(c) => rgb(c).into(),
                            None => cx.theme().muted_foreground,
                        })
                        .child(note),
                )
            })
    }
}

/// 横条行：可选头像 + 标签 + 分段横条 + 右侧 VT323 数值。
#[derive(IntoElement)]
pub struct BarRow {
    avatar: Option<ProviderId>,
    label: SharedString,
    label_width: f32,
    /// (占满宽的比例 0–1, 颜色)
    segments: Vec<(f32, u32)>,
    value: SharedString,
}

impl BarRow {
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            avatar: None,
            label: label.into(),
            label_width: 118.,
            segments: Vec::new(),
            value: value.into(),
        }
    }

    pub fn avatar(mut self, provider: ProviderId) -> Self {
        self.avatar = Some(provider);
        self
    }

    pub fn label_width(mut self, width: f32) -> Self {
        self.label_width = width;
        self
    }

    pub fn segment(mut self, fraction: f32, color: u32) -> Self {
        if fraction > 0.0 {
            self.segments.push((fraction.min(1.0), color));
        }
        self
    }
}

impl RenderOnce for BarRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(12.5))
            .when_some(self.avatar, |row, provider| {
                row.child(avatar(&provider, 18.))
            })
            .child(
                div()
                    .w(px(self.label_width))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.label),
            )
            .child(
                h_flex()
                    .flex_1()
                    .h(px(10.))
                    .gap(px(2.))
                    .rounded(px(1.))
                    .overflow_hidden()
                    .bg(cx.theme().border)
                    .children(self.segments.into_iter().map(|(fraction, color)| {
                        div().h_full().w(relative(fraction)).bg(rgb(color))
                    })),
            )
            .child(
                vt(18.)
                    .w(px(72.))
                    .flex_shrink_0()
                    .text_right()
                    .child(self.value),
            )
    }
}

/// 图例：色块 + 文案。
pub fn legend(items: &[(u32, &'static str)]) -> Div {
    h_flex()
        .flex_wrap()
        .gap(px(14.))
        .text_size(px(11.5))
        .text_color(rgb(MUTED))
        .children(items.iter().map(|(color, text)| {
            h_flex()
                .items_center()
                .gap(px(5.))
                .child(div().size(px(10.)).bg(rgb(*color)))
                .child(*text)
        }))
}

/// 加载占位：几根骨架条。
pub fn skeleton_rows(count: usize) -> Div {
    v_flex()
        .gap(px(10.))
        .py(px(4.))
        .children((0..count).map(|i| {
            Skeleton::new()
                .h(px(12.))
                .w(relative(if i % 2 == 0 { 0.8 } else { 0.55 }))
        }))
}
