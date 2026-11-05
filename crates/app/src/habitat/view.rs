//! 栖息地视图：背景 + 三个槽位的精灵动画 + 壁炉 / 星星 / 气泡 / 标签。
//!
//! 状态推导在 `state.rs`，这里只负责定时推 Tick 与画画。
//! 场景本身是一张像素画，叠在上面的小牌子沿用画里的深色底，不跟随主题明暗。

use std::time::{Duration, Instant};

use chrono::Timelike as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, IntoElement, ObjectFit,
    ParentElement as _, Render, SharedString, Styled as _, StyledImage as _, Task, Window, bounce,
    div, ease_in_out, img, px, relative, rgb, rgba,
};

use super::state::{
    ActivityKind, HabitatEvent, HabitatState, Resident, ResidentId, ResidentState, TimeOfDay,
};
use crate::assets::FONT_PIXEL_MONO;

/// 背景交叉淡入时长。
const BG_FADE: Duration = Duration::from_millis(1200);
/// 演示剧本每拍间隔。
const DEMO_BEAT: Duration = Duration::from_millis(2200);
/// 真实模式下的状态回落检查间隔。
const TICK: Duration = Duration::from_secs(1);

/// 三个桌位的中心 x（占场景宽的比例）。
const SLOT_CENTERS: [f32; 3] = [0.325, 0.52, 0.72];
const SLOT_WIDTH: f32 = 0.2;
const SLOT_HEIGHT: f32 = 0.4;
/// 槽位底边离场景底部的距离（底边贴 93.5%）。
const SLOT_BOTTOM: f32 = 0.065;

/// 画里的深色底与住户色，是像素画的一部分，故意不用主题令牌。
const INK: u32 = 0x0F1423;
const LINE: u32 = 0x26304A;
const TEXT: u32 = 0xE8ECF5;
const TEXT_DIM: u32 = 0x9AA6C0;
const AMBER: u32 = 0xF5B556;
const OK: u32 = 0x8BD16A;
const ALERT: u32 = 0xFF6B6B;
const RESIDENT_COLORS: [u32; 3] = [0xE8824F, 0xE8ECF5, 0x8E7CFF];

/// `Compact` 由左栏 / 菜单栏（C 块）使用，演示窗口只开 `Hero`。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HabitatSize {
    /// 仪表盘首行：带「名字 · 状态 · 体力」标签与时钟。
    Hero,
    /// 左栏 / 菜单栏：只有场景。
    Compact,
}

/// 演示剧本的一拍。
struct Beat {
    tod: TimeOfDay,
    residents: [(ResidentState, u8); 3],
    toast: Option<&'static str>,
}

/// 与 `docs/design/Habitat.dc.html` 里的 SCRIPT 一致。
const DEMO_SCRIPT: [Beat; 10] = {
    use ResidentState::*;
    use TimeOfDay::*;
    [
        Beat {
            tod: Night,
            residents: [(Working, 58), (Thinking, 82), (Sleeping, 0)],
            toast: None,
        },
        Beat {
            tod: Night,
            residents: [(Thinking, 57), (Working, 81), (Sleeping, 0)],
            toast: Some("Codex 开始执行工具 cargo test"),
        },
        Beat {
            tod: Night,
            residents: [(Working, 55), (Working, 80), (Sleeping, 0)],
            toast: None,
        },
        Beat {
            tod: Night,
            residents: [(Happy, 54), (Working, 79), (Sleeping, 0)],
            toast: Some("Claude 完成了一个会话 · $3.21"),
        },
        Beat {
            tod: Evening,
            residents: [(Working, 40), (Tired, 18), (Sleeping, 0)],
            toast: Some("Codex 5h 窗口只剩 18%"),
        },
        Beat {
            tod: Evening,
            residents: [(Working, 38), (Sleeping, 0), (Sleeping, 0)],
            toast: Some("Codex 额度用尽，03:41 后重置"),
        },
        Beat {
            tod: Day,
            residents: [(Tired, 15), (Sleeping, 0), (Away, 0)],
            toast: None,
        },
        Beat {
            tod: Day,
            residents: [(Sleeping, 0), (Happy, 100), (Away, 0)],
            toast: Some("Codex 窗口已重置，体力全满"),
        },
        Beat {
            tod: Day,
            residents: [(Happy, 100), (Working, 97), (Away, 0)],
            toast: Some("Claude 5h 窗口已重置"),
        },
        Beat {
            tod: Night,
            residents: [(Working, 92), (Thinking, 90), (Sleeping, 0)],
            toast: None,
        },
    ]
};

pub struct HabitatView {
    size: HabitatSize,
    state: HabitatState,
    /// `Some(下一拍下标)` 表示在跑演示剧本。
    demo_step: Option<usize>,
    /// 上一个时段与切换时刻：淡入期间垫在当前背景下面。
    fading_from: Option<(TimeOfDay, Instant)>,
    /// 每条 toast 换一个号，动画才会从头播。
    toast_serial: u64,
    clock: SharedString,
    ticker: Task<()>,
}

impl HabitatView {
    pub fn new(size: HabitatSize, cx: &mut Context<Self>) -> Self {
        let now = Instant::now();
        let local = chrono::Local::now();
        let mut view = Self {
            size,
            state: HabitatState::new(TimeOfDay::from_hour(local.hour()), now),
            demo_step: None,
            fading_from: None,
            toast_serial: 0,
            clock: clock_text(&local),
            ticker: Task::ready(()),
        };
        view.ticker = view.spawn_ticker(cx);
        view
    }

    /// 数据层事件入口。
    pub fn apply(&mut self, event: HabitatEvent, cx: &mut Context<Self>) {
        if matches!(event, HabitatEvent::Toast(_)) {
            self.toast_serial += 1;
        }
        self.state.reduce(event, Instant::now());
        cx.notify();
    }

    /// 开 / 关演示剧本。开时立刻播第一拍。
    pub fn set_demo(&mut self, demo: bool, cx: &mut Context<Self>) {
        if demo == self.demo_step.is_some() {
            return;
        }
        self.demo_step = demo.then_some(0);
        if demo {
            self.play_beat(cx);
        }
        self.ticker = self.spawn_ticker(cx);
        cx.notify();
    }

    /// 给侧栏住户行用；C / D 块接线前在 bin 里暂无调用方。
    #[allow(dead_code)]
    pub fn residents(&self) -> &[Resident; 3] {
        &self.state.residents
    }

    fn spawn_ticker(&self, cx: &mut Context<Self>) -> Task<()> {
        let interval = if self.demo_step.is_some() {
            DEMO_BEAT
        } else {
            TICK
        };
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    break;
                }
            }
        })
    }

    /// 定时推进：演示模式播下一拍，否则按本机时钟定时段；再做状态回落。
    fn tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let local = chrono::Local::now();
        if self.demo_step.is_some() {
            self.play_beat(cx);
        } else {
            self.set_time_of_day(TimeOfDay::from_hour(local.hour()), now);
        }
        self.state.reduce(HabitatEvent::Tick, now);
        if self
            .fading_from
            .is_some_and(|(_, at)| now.saturating_duration_since(at) >= BG_FADE)
        {
            self.fading_from = None;
        }
        self.clock = clock_text(&local);
        cx.notify();
    }

    fn set_time_of_day(&mut self, tod: TimeOfDay, now: Instant) {
        if tod != self.state.tod {
            self.fading_from = Some((self.state.tod, now));
            self.state.tod = tod;
        }
    }

    /// 把剧本里的一拍翻译成数据层事件送进 reducer。
    fn play_beat(&mut self, cx: &mut Context<Self>) {
        let Some(step) = self.demo_step else {
            return;
        };
        let beat = &DEMO_SCRIPT[step % DEMO_SCRIPT.len()];
        self.demo_step = Some((step + 1) % DEMO_SCRIPT.len());
        let now = Instant::now();
        self.set_time_of_day(beat.tod, now);
        for (who, (state, hp)) in ResidentId::ALL.into_iter().zip(beat.residents) {
            let detected = state != ResidentState::Away;
            self.state
                .reduce(HabitatEvent::Detected { who, detected }, now);
            self.state.reduce(HabitatEvent::Quota { who, hp }, now);
            let kind = match state {
                ResidentState::Working => Some(ActivityKind::AssistantText),
                ResidentState::Thinking => Some(ActivityKind::ToolCall),
                ResidentState::Happy => Some(ActivityKind::SessionEnd),
                ResidentState::Tired | ResidentState::Sleeping | ResidentState::Away => None,
            };
            if let Some(kind) = kind {
                self.state.reduce(HabitatEvent::Activity { who, kind }, now);
            }
        }
        if let Some(text) = beat.toast {
            self.apply(HabitatEvent::Toast(text.to_string()), cx);
        }
    }

    /// 字号基准：hero 10px、compact 6px，与设计稿的 em 换算一致。
    fn em(&self) -> f32 {
        match self.size {
            HabitatSize::Hero => 10.0,
            HabitatSize::Compact => 6.0,
        }
    }

    fn render_backgrounds(&self) -> Vec<AnyElement> {
        let tod = self.state.tod;
        let mut layers = Vec::with_capacity(2);
        if let Some((from, _)) = self.fading_from {
            layers.push(background(from).into_any_element());
            layers.push(
                background(tod)
                    .with_animation(
                        ("habitat-bg", tod.index()),
                        Animation::new(BG_FADE).with_easing(ease_in_out),
                        |layer, t| layer.opacity(t),
                    )
                    .into_any_element(),
            );
        } else {
            layers.push(background(tod).into_any_element());
        }
        layers
    }

    /// 壁炉与台灯的光，傍晚 / 夜里亮。没有径向渐变与混合模式，用半透明圆代替。
    fn render_lights(&self) -> Vec<AnyElement> {
        if self.state.tod == TimeOfDay::Day {
            return Vec::new();
        }
        let fire = div()
            .absolute()
            .left(relative(0.79))
            .top(relative(0.62))
            .w(relative(0.12))
            .h(relative(0.18))
            .rounded_full()
            .bg(rgba(0xFF963C40))
            .with_animation(
                "habitat-fire",
                // 三帧步进闪烁：0.75 → 1 → 0.85。
                Animation::new(Duration::from_millis(900))
                    .repeat()
                    .with_easing(|t| [0.75, 1.0, 0.85][((t * 3.0) as usize).min(2)]),
                |glow, v| glow.opacity(v),
            );
        let lamp = div()
            .absolute()
            .left(relative(0.265))
            .top(relative(0.30))
            .w(relative(0.09))
            .h(relative(0.14))
            .rounded_full()
            .bg(rgba(0xFFC86E40))
            .with_animation(
                "habitat-lamp",
                Animation::new(Duration::from_millis(3200))
                    .repeat()
                    .with_easing(bounce(ease_in_out)),
                |glow, v| glow.opacity(0.7 + 0.3 * v),
            );
        vec![fire.into_any_element(), lamp.into_any_element()]
    }

    /// 夜里窗外四颗错相闪烁的星。
    fn render_stars(&self) -> Vec<AnyElement> {
        if self.state.tod != TimeOfDay::Night {
            return Vec::new();
        }
        const STARS: [(f32, f32, f32); 4] = [
            (0.43, 0.30, 0.0),
            (0.47, 0.35, 0.7),
            (0.52, 0.28, 1.3),
            (0.56, 0.33, 0.4),
        ];
        STARS
            .iter()
            .enumerate()
            .map(|(index, &(x, y, delay))| {
                let phase = delay / 2.4;
                div()
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .w(relative(0.004))
                    .h(relative(0.008))
                    .bg(rgb(0xFFFFFF))
                    .with_animation(
                        ("habitat-star", index),
                        Animation::new(Duration::from_millis(2400))
                            .repeat()
                            .with_easing(move |t| twinkle((t + phase) % 1.0)),
                        |star, v| star.opacity(v),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_slot(&self, who: ResidentId) -> Vec<AnyElement> {
        let resident = self.state.resident(who);
        let center = SLOT_CENTERS[who.index()];
        let left = center - SLOT_WIDTH / 2.0;
        let em = self.em();
        let mut parts = Vec::with_capacity(4);

        if resident.state == ResidentState::Away {
            // 不画精灵，桌上立「外出」牌。
            parts.push(
                slot_box(left, SLOT_BOTTOM + SLOT_HEIGHT * 0.34, SLOT_HEIGHT)
                    .items_start()
                    .child(chip(em * 1.9).text_color(rgb(TEXT_DIM)).child("外出"))
                    .into_any_element(),
            );
        } else {
            parts.push(self.render_sprite(who, resident));
        }

        match resident.state {
            ResidentState::Sleeping => parts.push(
                div()
                    .absolute()
                    .left(relative(left + SLOT_WIDTH * 0.62))
                    .font_family(FONT_PIXEL_MONO)
                    .text_size(px(em * 1.8))
                    .text_color(rgb(0xC9D3F0))
                    .child("z")
                    .with_animation(
                        ("habitat-zz", who.index()),
                        Animation::new(Duration::from_millis(2200)).repeat(),
                        move |z, t| {
                            let alpha = if t < 0.2 {
                                t / 0.2
                            } else {
                                1.0 - (t - 0.2) / 0.8
                            };
                            z.opacity(alpha)
                                .bottom(relative(SLOT_BOTTOM + SLOT_HEIGHT * 0.88 + 0.12 * t))
                        },
                    )
                    .into_any_element(),
            ),
            ResidentState::Happy => {
                for (index, (x, y, delay)) in [(0.08, 0.04, 0.0), (0.86, 0.14, 0.4)]
                    .into_iter()
                    .enumerate()
                {
                    let phase = delay / 1.1;
                    parts.push(
                        div()
                            .absolute()
                            .left(relative(left + SLOT_WIDTH * x))
                            .bottom(relative(SLOT_BOTTOM + SLOT_HEIGHT * (1.0 - y)))
                            .text_size(px(em * 1.4))
                            .text_color(rgb(0xFFD86B))
                            .child("✦")
                            .with_animation(
                                ("habitat-spark", who.index() * 2 + index),
                                Animation::new(Duration::from_millis(1100))
                                    .repeat()
                                    .with_easing(move |t| twinkle((t + phase) % 1.0)),
                                |spark, v| spark.opacity(v),
                            )
                            .into_any_element(),
                    );
                }
            }
            _ => {}
        }

        if self.size == HabitatSize::Hero {
            parts.push(self.render_tag(who, resident));
        }
        parts
    }

    /// 按状态选精灵图并套对应动画。gpui 没有旋转 / 单轴缩放 / 滤镜，
    /// thinking 的摇头用左右平移代替，sleeping 的呼吸整体缩放，tired 的去饱和用降透明度。
    fn render_sprite(&self, who: ResidentId, resident: &Resident) -> AnyElement {
        let left = SLOT_CENTERS[who.index()] - SLOT_WIDTH / 2.0;
        let sprite = img(format!(
            "habitat/{}-{}.png",
            who.slug(),
            resident.state.slug()
        ))
        .w_full()
        .h_full()
        .object_fit(ObjectFit::Contain);
        let slot = slot_box(left, SLOT_BOTTOM, SLOT_HEIGHT).child(sprite);
        // 住户 + 状态共同决定动画身份：换状态即从头播。
        let id = (
            ElementId::from(("habitat-sprite", who.index())),
            resident.state.slug(),
        );
        match resident.state {
            ResidentState::Working => slot
                .with_animation(
                    id,
                    // 两帧步进：抬 2.5% 精灵高。
                    Animation::new(Duration::from_millis(420))
                        .repeat()
                        .with_easing(|t| if t < 0.5 { 0.0 } else { 1.0 }),
                    move |slot, v| slot.bottom(relative(SLOT_BOTTOM + SLOT_HEIGHT * 0.025 * v)),
                )
                .into_any_element(),
            ResidentState::Thinking => slot
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(2200))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    move |slot, v| {
                        slot.left(relative(left + (v - 0.5) * 0.01))
                            .bottom(relative(SLOT_BOTTOM + SLOT_HEIGHT * 0.01 * v))
                    },
                )
                .into_any_element(),
            ResidentState::Sleeping | ResidentState::Away => slot
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(3400))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    |slot, v| slot.h(relative(SLOT_HEIGHT * (1.0 + 0.035 * v))),
                )
                .into_any_element(),
            ResidentState::Tired => slot
                .opacity(0.85)
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(2800))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    move |slot, v| {
                        slot.left(relative(left - 0.003 * v))
                            .bottom(relative(SLOT_BOTTOM - SLOT_HEIGHT * 0.02 * v))
                    },
                )
                .into_any_element(),
            ResidentState::Happy => slot
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(550))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    move |slot, v| slot.bottom(relative(SLOT_BOTTOM + SLOT_HEIGHT * 0.07 * v)),
                )
                .into_any_element(),
        }
    }

    /// 槽位下方的「名字 · 状态 · 五格体力 · 百分比」标签。
    fn render_tag(&self, who: ResidentId, resident: &Resident) -> AnyElement {
        let em = self.em();
        let left = SLOT_CENTERS[who.index()] - SLOT_WIDTH / 2.0;
        let hp = resident.hp.min(100);
        let lit = usize::from(hp.div_ceil(20));
        let bar_color = if hp < 20 {
            ALERT
        } else if hp < 50 {
            AMBER
        } else {
            OK
        };
        div()
            .absolute()
            .left(relative(left - SLOT_WIDTH * 0.5))
            .w(relative(SLOT_WIDTH * 2.0))
            .bottom(relative(0.012))
            .flex()
            .justify_center()
            .child(
                chip(em * 1.25)
                    .border_1()
                    .border_color(rgb(LINE))
                    .text_color(rgb(TEXT))
                    .gap(px(em * 0.35))
                    .child(
                        div()
                            .size(px(em * 0.5 * 1.25))
                            .bg(rgb(RESIDENT_COLORS[who.index()])),
                    )
                    .child(format!("{} · {}", who.name(), resident.state.label()))
                    .child(div().flex().gap(px(1.0)).children((0..5).map(|index| {
                        div().size(px(em * 0.45 * 1.25)).bg(rgb(if index < lit {
                            bar_color
                        } else {
                            LINE
                        }))
                    }))),
            )
            .into_any_element()
    }

    fn render_toast(&self) -> Option<AnyElement> {
        let (text, _) = self.state.toast.as_ref()?;
        let em = self.em();
        Some(
            div()
                .absolute()
                .left(px(0.0))
                .right(px(0.0))
                .top(relative(0.12))
                .flex()
                .justify_center()
                .child(
                    chip(em * 1.9)
                        .border_1()
                        .border_color(rgb(LINE))
                        .text_color(rgb(AMBER))
                        .child(text.clone())
                        .with_animation(
                            ("habitat-toast", self.toast_serial),
                            Animation::new(Duration::from_millis(2400)),
                            |toast, t| {
                                let alpha = if t < 0.15 {
                                    t / 0.15
                                } else if t < 0.75 {
                                    1.0
                                } else {
                                    1.0 - (t - 0.75) / 0.25
                                };
                                toast.opacity(alpha).mt(px(10.0 - 24.0 * t))
                            },
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_clock(&self) -> AnyElement {
        let em = self.em();
        div()
            .absolute()
            .right(relative(0.012))
            .top(relative(0.03))
            .child(
                chip(em * 1.6)
                    .text_color(rgb(TEXT_DIM))
                    .child(self.clock.clone()),
            )
            .into_any_element()
    }
}

impl Render for HabitatView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded(px(4.0))
            .bg(rgb(INK))
            .children(self.render_backgrounds())
            .children(self.render_stars())
            .children(self.render_lights())
            .children(
                ResidentId::ALL
                    .into_iter()
                    .flat_map(|who| self.render_slot(who)),
            )
            .children(self.render_toast())
            .when(self.size == HabitatSize::Hero, |scene| {
                scene.child(self.render_clock())
            })
    }
}

fn background(tod: TimeOfDay) -> gpui_kit::Img {
    img(format!("habitat/bg-{}.png", tod.slug()))
        .absolute()
        .inset_0()
        .size_full()
        .object_fit(ObjectFit::Cover)
}

/// 一个槽位盒：绝对定位，精灵贴底居中。
fn slot_box(left: f32, bottom: f32, height: f32) -> gpui_kit::Div {
    div()
        .absolute()
        .left(relative(left))
        .w(relative(SLOT_WIDTH))
        .bottom(relative(bottom))
        .h(relative(height))
        .flex()
        .items_end()
        .justify_center()
}

/// 像素字小牌子：深色半透明底、VT323。
fn chip(font_px: f32) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .whitespace_nowrap()
        .font_family(FONT_PIXEL_MONO)
        .text_size(px(font_px))
        .line_height(px(font_px))
        .bg(rgba((INK << 8) | 0xD1))
        .px(px(font_px * 0.3))
        .py(px(font_px * 0.1))
        .rounded(px(2.0))
}

/// 0.25 → 1 → 0.25 的闪烁曲线。
fn twinkle(t: f32) -> f32 {
    0.25 + 0.75 * bounce(ease_in_out)(t)
}

fn clock_text(local: &chrono::DateTime<chrono::Local>) -> SharedString {
    format!("{:02}:{:02}", local.hour(), local.minute()).into()
}
