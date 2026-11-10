//! 栖息地的数据与状态机：纯函数 reducer，不碰 gpui。
//!
//! 规则来自 `docs/04-栖息地状态系统.md` 第 2 节与 `docs/05` 的推导顺序：
//! away > sleeping（额度 0）> tired > happy > working > thinking > sleeping（空闲）。

use std::time::{Duration, Instant};

/// 30 秒内有新记录算工作中。
pub const WORKING_WINDOW: Duration = Duration::from_secs(30);
/// 10 分钟无活动算空闲（休息中）。
pub const IDLE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// 完成事件后保持 happy 的时长。
pub const HAPPY_HOLD: Duration = Duration::from_secs(3);
/// toast 显示时长（与 2.4s 上浮动画一致）。
pub const TOAST_HOLD: Duration = Duration::from_millis(2400);
/// 体力低于此值算快没电。
pub const TIRED_BELOW: u8 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResidentId {
    Claude,
    Codex,
    Gemini,
}

impl ResidentId {
    pub const ALL: [ResidentId; 3] = [ResidentId::Claude, ResidentId::Codex, ResidentId::Gemini];

    /// 槽位下标，也是精灵文件名前缀的顺序。
    pub fn index(self) -> usize {
        match self {
            ResidentId::Claude => 0,
            ResidentId::Codex => 1,
            ResidentId::Gemini => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ResidentId::Claude => "Claude",
            ResidentId::Codex => "Codex",
            ResidentId::Gemini => "Gemini",
        }
    }

    /// 素材文件名里的小写前缀。
    pub fn slug(self) -> &'static str {
        match self {
            ResidentId::Claude => "claude",
            ResidentId::Codex => "codex",
            ResidentId::Gemini => "gemini",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentState {
    Working,
    Thinking,
    Happy,
    Tired,
    Sleeping,
    Away,
}

impl ResidentState {
    pub fn label(self) -> String {
        match self {
            ResidentState::Working => t!("habitat.state_working").into(),
            ResidentState::Thinking => t!("habitat.state_thinking").into(),
            ResidentState::Happy => t!("habitat.state_happy").into(),
            ResidentState::Tired => t!("habitat.state_tired").into(),
            ResidentState::Sleeping => t!("habitat.state_sleeping").into(),
            ResidentState::Away => t!("habitat.state_away").into(),
        }
    }

    /// 精灵文件名后缀；away 没有精灵，借用 sleeping 占位（视图不会画它）。
    pub fn slug(self) -> &'static str {
        match self {
            ResidentState::Working => "working",
            ResidentState::Thinking => "thinking",
            ResidentState::Happy => "happy",
            ResidentState::Tired => "tired",
            ResidentState::Sleeping | ResidentState::Away => "sleeping",
        }
    }
}

/// 数据层活动记录的种类，与 core 规划的 `ActivityKind` 一一对应。
/// 演示剧本只用到其中几种，其余由数据层接线（D 块）后构造。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    UserMessage,
    AssistantText,
    Thinking,
    ToolCall,
    ToolResult,
    SessionEnd,
}

#[derive(Clone, Debug)]
pub struct Resident {
    pub state: ResidentState,
    /// 体力 0–100，取各额度窗口剩余的最小值。
    pub hp: u8,
    /// 进入当前状态的时刻。
    pub since: Instant,
    // 以下是推导状态所需的输入，只由 reducer 维护。
    detected: bool,
    last_activity: Option<(Instant, ActivityKind)>,
    happy_until: Option<Instant>,
}

impl Resident {
    fn new(now: Instant) -> Self {
        Self {
            state: ResidentState::Away,
            hp: 0,
            since: now,
            detected: false,
            last_activity: None,
            happy_until: None,
        }
    }

    /// 按优先级从输入推出状态。
    fn derive(&self, now: Instant) -> ResidentState {
        if !self.detected {
            return ResidentState::Away;
        }
        if self.hp == 0 {
            return ResidentState::Sleeping;
        }
        if self.hp < TIRED_BELOW {
            return ResidentState::Tired;
        }
        if self.happy_until.is_some_and(|until| now < until) {
            return ResidentState::Happy;
        }
        let Some((at, kind)) = self.last_activity else {
            return ResidentState::Sleeping;
        };
        let idle = now.saturating_duration_since(at);
        match kind {
            // 会话已结束：没事干。
            ActivityKind::SessionEnd => ResidentState::Sleeping,
            // 最近一条是 thinking 块或已发出未返回的 tool_call：在等。
            ActivityKind::Thinking | ActivityKind::ToolCall if idle < IDLE_WINDOW => {
                ResidentState::Thinking
            }
            _ if idle < WORKING_WINDOW => ResidentState::Working,
            // 有会话但 30 秒没动静：思考中；超过 10 分钟：休息。
            _ if idle < IDLE_WINDOW => ResidentState::Thinking,
            _ => ResidentState::Sleeping,
        }
    }

    /// 重新推导状态，变了就刷新 `since`。
    fn refresh(&mut self, now: Instant) {
        let next = self.derive(now);
        if next != self.state {
            self.state = next;
            self.since = now;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeOfDay {
    Day,
    Evening,
    Night,
}

impl TimeOfDay {
    /// 本机小时 → 时段：07–17 白天，17–20 傍晚，其余夜晚。
    pub fn from_hour(hour: u32) -> Self {
        match hour {
            7..=16 => TimeOfDay::Day,
            17..=19 => TimeOfDay::Evening,
            _ => TimeOfDay::Night,
        }
    }

    pub fn index(self) -> usize {
        match self {
            TimeOfDay::Day => 0,
            TimeOfDay::Evening => 1,
            TimeOfDay::Night => 2,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            TimeOfDay::Day => "day",
            TimeOfDay::Evening => "evening",
            TimeOfDay::Night => "night",
        }
    }
}

/// 送进 reducer 的事件。`Toast` 是额外加的：气泡文案（含成本等）由数据层拼好再送来。
#[derive(Clone, Debug)]
pub enum HabitatEvent {
    Activity { who: ResidentId, kind: ActivityKind },
    Quota { who: ResidentId, hp: u8 },
    Detected { who: ResidentId, detected: bool },
    Toast(String),
    Tick,
}

#[derive(Clone, Debug)]
pub struct HabitatState {
    pub tod: TimeOfDay,
    pub residents: [Resident; 3],
    pub toast: Option<(String, Instant)>,
}

impl HabitatState {
    pub fn new(tod: TimeOfDay, now: Instant) -> Self {
        Self {
            tod,
            residents: [Resident::new(now), Resident::new(now), Resident::new(now)],
            toast: None,
        }
    }

    pub fn resident(&self, who: ResidentId) -> &Resident {
        &self.residents[who.index()]
    }

    fn resident_mut(&mut self, who: ResidentId) -> &mut Resident {
        &mut self.residents[who.index()]
    }

    /// 应用一个事件。`now` 由调用方传入，便于测试。
    pub fn reduce(&mut self, event: HabitatEvent, now: Instant) {
        match event {
            HabitatEvent::Activity { who, kind } => {
                let resident = self.resident_mut(who);
                resident.last_activity = Some((now, kind));
                // 完成类记录开心 3 秒；其它新记录说明已经接着干了，提前结束 happy。
                resident.happy_until =
                    matches!(kind, ActivityKind::SessionEnd | ActivityKind::ToolResult)
                        .then_some(now + HAPPY_HOLD);
                resident.refresh(now);
            }
            HabitatEvent::Quota { who, hp } => {
                let resident = self.resident_mut(who);
                // 额度从 0 重置回来：开心 3 秒再回 idle。
                if resident.hp == 0 && hp > 0 && resident.detected {
                    resident.happy_until = Some(now + HAPPY_HOLD);
                }
                resident.hp = hp.min(100);
                resident.refresh(now);
            }
            HabitatEvent::Detected { who, detected } => {
                let resident = self.resident_mut(who);
                resident.detected = detected;
                resident.refresh(now);
            }
            HabitatEvent::Toast(text) => self.toast = Some((text, now)),
            HabitatEvent::Tick => {
                for resident in &mut self.residents {
                    resident.refresh(now);
                }
                if self
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= TOAST_HOLD)
                {
                    self.toast = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected(now: Instant) -> HabitatState {
        let mut state = HabitatState::new(TimeOfDay::Day, now);
        for who in ResidentId::ALL {
            state.reduce(
                HabitatEvent::Detected {
                    who,
                    detected: true,
                },
                now,
            );
            state.reduce(HabitatEvent::Quota { who, hp: 80 }, now);
        }
        state
    }

    fn state_of(state: &HabitatState, who: ResidentId) -> ResidentState {
        state.resident(who).state
    }

    #[test]
    fn undetected_resident_is_away_regardless_of_activity() {
        let now = Instant::now();
        let mut state = HabitatState::new(TimeOfDay::Day, now);
        state.reduce(
            HabitatEvent::Activity {
                who: ResidentId::Gemini,
                kind: ActivityKind::AssistantText,
            },
            now,
        );
        assert_eq!(state_of(&state, ResidentId::Gemini), ResidentState::Away);
        state.reduce(
            HabitatEvent::Detected {
                who: ResidentId::Gemini,
                detected: true,
            },
            now,
        );
        // 接入后没额度信息（hp 0）→ 休息中，而不是直接工作。
        assert_eq!(
            state_of(&state, ResidentId::Gemini),
            ResidentState::Sleeping
        );
    }

    #[test]
    fn activity_drives_working_then_thinking_then_sleeping() {
        let now = Instant::now();
        let mut state = detected(now);
        let who = ResidentId::Claude;
        state.reduce(
            HabitatEvent::Activity {
                who,
                kind: ActivityKind::AssistantText,
            },
            now,
        );
        assert_eq!(state_of(&state, who), ResidentState::Working);
        assert_eq!(state.resident(who).since, now);

        state.reduce(HabitatEvent::Tick, now + WORKING_WINDOW);
        assert_eq!(state_of(&state, who), ResidentState::Thinking);
        assert_eq!(state.resident(who).since, now + WORKING_WINDOW);

        state.reduce(HabitatEvent::Tick, now + IDLE_WINDOW);
        assert_eq!(state_of(&state, who), ResidentState::Sleeping);
    }

    #[test]
    fn pending_tool_call_is_thinking_until_result_arrives() {
        let now = Instant::now();
        let mut state = detected(now);
        let who = ResidentId::Codex;
        state.reduce(
            HabitatEvent::Activity {
                who,
                kind: ActivityKind::ToolCall,
            },
            now,
        );
        assert_eq!(state_of(&state, who), ResidentState::Thinking);
        let later = now + Duration::from_secs(5);
        state.reduce(
            HabitatEvent::Activity {
                who,
                kind: ActivityKind::ToolResult,
            },
            later,
        );
        // 工具成功 → happy 3 秒 → 回到 working（还在 30 秒窗口内）。
        assert_eq!(state_of(&state, who), ResidentState::Happy);
        state.reduce(HabitatEvent::Tick, later + HAPPY_HOLD);
        assert_eq!(state_of(&state, who), ResidentState::Working);
    }

    #[test]
    fn session_end_is_happy_then_sleeping() {
        let now = Instant::now();
        let mut state = detected(now);
        let who = ResidentId::Claude;
        state.reduce(
            HabitatEvent::Activity {
                who,
                kind: ActivityKind::SessionEnd,
            },
            now,
        );
        assert_eq!(state_of(&state, who), ResidentState::Happy);
        state.reduce(HabitatEvent::Tick, now + HAPPY_HOLD);
        assert_eq!(state_of(&state, who), ResidentState::Sleeping);
    }

    #[test]
    fn quota_outranks_activity() {
        let now = Instant::now();
        let mut state = detected(now);
        let who = ResidentId::Codex;
        state.reduce(
            HabitatEvent::Activity {
                who,
                kind: ActivityKind::AssistantText,
            },
            now,
        );
        state.reduce(HabitatEvent::Quota { who, hp: 18 }, now);
        assert_eq!(state_of(&state, who), ResidentState::Tired);
        state.reduce(HabitatEvent::Quota { who, hp: 0 }, now);
        assert_eq!(state_of(&state, who), ResidentState::Sleeping);
        // 窗口重置：开心 3 秒，然后按活动回到 working。
        state.reduce(HabitatEvent::Quota { who, hp: 100 }, now);
        assert_eq!(state_of(&state, who), ResidentState::Happy);
        state.reduce(HabitatEvent::Tick, now + HAPPY_HOLD);
        assert_eq!(state_of(&state, who), ResidentState::Working);
        assert_eq!(state.resident(who).hp, 100);
    }

    #[test]
    fn toast_expires_on_tick() {
        let now = Instant::now();
        let mut state = detected(now);
        state.reduce(HabitatEvent::Toast("Codex 开始执行工具".into()), now);
        state.reduce(HabitatEvent::Tick, now + Duration::from_secs(1));
        assert!(state.toast.is_some());
        state.reduce(HabitatEvent::Tick, now + TOAST_HOLD);
        assert!(state.toast.is_none());
    }

    #[test]
    fn time_of_day_boundaries() {
        assert_eq!(TimeOfDay::from_hour(6), TimeOfDay::Night);
        assert_eq!(TimeOfDay::from_hour(7), TimeOfDay::Day);
        assert_eq!(TimeOfDay::from_hour(16), TimeOfDay::Day);
        assert_eq!(TimeOfDay::from_hour(17), TimeOfDay::Evening);
        assert_eq!(TimeOfDay::from_hour(19), TimeOfDay::Evening);
        assert_eq!(TimeOfDay::from_hour(20), TimeOfDay::Night);
    }
}
