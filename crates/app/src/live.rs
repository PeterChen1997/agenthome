//! 实时信号：每 5 秒在后台拉 `activity()`（一条 SQL），额度 `quota()` 每 60 秒拉一次，
//! 外壳在额度来源有新数据时（同步引擎入库、后台 /usage 读完）可以要求下一轮提前拉。
//! 结果翻译成栖息地事件喂给所有 `HabitatView`，并把最新快照缓存在 `LiveState` 供住户行与状态条读取。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agenthome_core::model::{
    ActivityKind as CoreActivityKind, ProviderActivity, ProviderId, QuotaSnapshot, QuotaWindow,
};
use gpui_kit::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity};

use crate::data::DataSource;
use crate::format::now_ms;
use crate::habitat::{ActivityKind, HabitatEvent, HabitatView, ResidentId};

/// 活动信号轮询间隔。
const POLL: Duration = Duration::from_secs(5);
/// 额度轮询间隔：额度文件只在后台 /usage（15 分钟）或 rollout 写入时变化，不必每 5 秒读。
const QUOTA_POLL: Duration = Duration::from_secs(60);
/// 最近活动在此窗口内才算「正在干活」，与 `habitat::state::WORKING_WINDOW` 一致。
const RECENT_ACTIVITY_MS: i64 = 30_000;
/// 某窗口 used_percent 比上次下降至少这么多点，视为窗口已重置。
const RESET_DROP_PERCENT: f32 = 30.0;

/// 最近一次轮询的结果。
#[derive(Default)]
pub struct LiveState {
    pub activity: Vec<ProviderActivity>,
    pub quota: Vec<QuotaSnapshot>,
    /// 最近一次成功轮询的时刻（Unix 毫秒）。
    pub refreshed_at_ms: Option<i64>,
    /// 最近一次轮询的错误；UI 以文字展示，不弹窗。
    pub error: Option<String>,
}

impl LiveState {
    pub fn quota_for(&self, who: ResidentId) -> Option<&QuotaSnapshot> {
        self.quota
            .iter()
            .find(|q| resident_of(&q.provider) == Some(who))
    }

    /// 本机检测到的来源数。
    pub fn detected_count(&self) -> usize {
        self.activity.iter().filter(|a| a.detected).count()
    }
}

/// core 的来源 → 栖息地住户；没有住户形象的来源返回 None。
pub fn resident_of(provider: &ProviderId) -> Option<ResidentId> {
    match provider {
        ProviderId::ClaudeCode => Some(ResidentId::Claude),
        ProviderId::Codex => Some(ResidentId::Codex),
        ProviderId::GeminiCli => Some(ResidentId::Gemini),
        _ => None,
    }
}

/// 主窗口：Claude 取 five_hour，Codex 取 primary，其余取第一个。
pub fn primary_window(snapshot: &QuotaSnapshot) -> Option<&QuotaWindow> {
    let wanted = match snapshot.provider {
        ProviderId::ClaudeCode => "five_hour",
        ProviderId::Codex => "primary",
        _ => "",
    };
    snapshot
        .windows
        .iter()
        .find(|w| w.id == wanted)
        .or_else(|| snapshot.windows.first())
}

/// 体力 = 100 − 主窗口已用百分比。
fn hp_of(snapshot: &QuotaSnapshot) -> Option<u8> {
    primary_window(snapshot).map(|w| (100.0 - w.used_percent).round().clamp(0.0, 100.0) as u8)
}

fn habitat_kind(kind: CoreActivityKind) -> ActivityKind {
    match kind {
        CoreActivityKind::UserMessage => ActivityKind::UserMessage,
        CoreActivityKind::AssistantText => ActivityKind::AssistantText,
        CoreActivityKind::Thinking => ActivityKind::Thinking,
        CoreActivityKind::ToolCall => ActivityKind::ToolCall,
        CoreActivityKind::ToolResult => ActivityKind::ToolResult,
        CoreActivityKind::SessionEnd => ActivityKind::SessionEnd,
    }
}

/// 把一次轮询结果与上一次对比，产出要送进栖息地的事件。纯函数，便于测试。
fn diff_events(
    previous: Option<&(Vec<ProviderActivity>, Vec<QuotaSnapshot>)>,
    activity: &[ProviderActivity],
    quota: &[QuotaSnapshot],
    now_ms: i64,
) -> Vec<HabitatEvent> {
    let mut events = Vec::new();
    for current in activity {
        let Some(who) = resident_of(&current.provider) else {
            continue;
        };
        events.push(HabitatEvent::Detected {
            who,
            detected: current.detected,
        });
        let last =
            previous.and_then(|(prev, _)| prev.iter().find(|p| p.provider == current.provider));
        let is_new_record = last.is_none_or(|p| p.last_activity_ms != current.last_activity_ms);
        if let (Some(at), Some(kind)) = (current.last_activity_ms, current.last_kind)
            && is_new_record
            && now_ms - at <= RECENT_ACTIVITY_MS
        {
            events.push(HabitatEvent::Activity {
                who,
                kind: habitat_kind(kind),
            });
        }
        if last.is_some_and(|p| current.active_sessions > p.active_sessions) {
            events.push(HabitatEvent::Toast(format!("{} 开始了新会话", who.name())));
        }
        if current.detected && !quota.iter().any(|q| q.provider == current.provider) {
            // 没装 statusline 脚本时拿不到额度；按满体力喂，否则住户会一直「休息中」。
            events.push(HabitatEvent::Quota { who, hp: 100 });
        }
    }
    for snapshot in quota {
        let Some(who) = resident_of(&snapshot.provider) else {
            continue;
        };
        if let Some(hp) = hp_of(snapshot) {
            events.push(HabitatEvent::Quota { who, hp });
        }
        let Some((_, prev_quota)) = previous else {
            continue;
        };
        let Some(prev) = prev_quota.iter().find(|q| q.provider == snapshot.provider) else {
            continue;
        };
        for window in &snapshot.windows {
            let dropped = prev
                .windows
                .iter()
                .find(|w| w.id == window.id)
                .is_some_and(|w| w.used_percent - window.used_percent >= RESET_DROP_PERCENT);
            if dropped {
                events.push(HabitatEvent::Toast(format!(
                    "{} {} 窗口已重置",
                    who.name(),
                    window.label
                )));
            }
        }
    }
    events
}

/// 后台轮询任务的句柄；drop 即停止。
pub struct LiveFeed {
    /// 置位后下一轮（至多 5 秒后）连同额度一起拉。
    quota_requested: Arc<AtomicBool>,
    _task: Task<()>,
}

impl LiveFeed {
    /// 启动轮询：IO 在后台线程，事件在前台一次性应用到所有栖息地并更新 `state`。
    pub fn start(
        source: Arc<dyn DataSource>,
        habitats: Vec<Entity<HabitatView>>,
        state: Entity<LiveState>,
        cx: &mut App,
    ) -> Self {
        // 弱引用：外壳释放实体后循环自行退出。
        let habitats: Vec<WeakEntity<HabitatView>> =
            habitats.iter().map(Entity::downgrade).collect();
        let state = state.downgrade();
        let quota_requested = Arc::new(AtomicBool::new(false));
        let requested = quota_requested.clone();
        let task = cx.spawn(async move |cx: &mut AsyncApp| {
            let mut previous: Option<(Vec<ProviderActivity>, Vec<QuotaSnapshot>)> = None;
            let mut quota_at: Option<Instant> = None;
            // 额度读取的错误跨轮保留，直到下一次额度读取成功。
            let mut quota_error: Option<String> = None;
            loop {
                let fetch_quota = requested.swap(false, Ordering::Relaxed)
                    || quota_at.is_none_or(|at| at.elapsed() >= QUOTA_POLL);
                let io_source = source.clone();
                let (activity, fresh_quota) = cx
                    .background_spawn(async move {
                        (io_source.activity(), fetch_quota.then(|| io_source.quota()))
                    })
                    .await;
                let activity_error;
                let activity = match activity {
                    Ok(activity) => {
                        activity_error = None;
                        activity
                    }
                    Err(e) => {
                        activity_error = Some(format!("读取活动信号失败：{e:#}"));
                        Vec::new()
                    }
                };
                let quota = match fresh_quota {
                    Some(result) => {
                        quota_at = Some(Instant::now());
                        match result {
                            Ok(quota) => {
                                quota_error = None;
                                quota
                            }
                            // 只记错误不中断，住户行退回「未接入额度」。
                            Err(e) => {
                                quota_error = Some(format!("读取额度失败：{e:#}"));
                                Vec::new()
                            }
                        }
                    }
                    None => previous
                        .as_ref()
                        .map(|(_, q)| q.clone())
                        .unwrap_or_default(),
                };
                let error = activity_error.or_else(|| quota_error.clone());
                let now = now_ms();
                let events = diff_events(previous.as_ref(), &activity, &quota, now);
                for habitat in &habitats {
                    let _ = habitat.update(cx, |view, cx| {
                        for event in &events {
                            view.apply(event.clone(), cx);
                        }
                    });
                }
                let applied = state.update(cx, |live, cx| {
                    live.activity = activity.clone();
                    live.quota = quota.clone();
                    live.refreshed_at_ms = Some(now);
                    live.error = error;
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
                previous = Some((activity, quota));
                cx.background_executor().timer(POLL).await;
            }
        });
        Self {
            quota_requested,
            _task: task,
        }
    }

    /// 额度来源刚有新数据：下一轮（至多 5 秒后）就重读额度，不等 60 秒。
    pub fn request_quota(&self) {
        self.quota_requested.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agenthome_core::model::Freshness;

    fn activity(active_sessions: u32, last_ms: Option<i64>) -> ProviderActivity {
        ProviderActivity {
            provider: ProviderId::ClaudeCode,
            detected: true,
            last_activity_ms: last_ms,
            last_kind: Some(CoreActivityKind::ToolCall),
            active_sessions,
        }
    }

    fn quota(used: f32) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: ProviderId::ClaudeCode,
            plan: None,
            windows: vec![QuotaWindow {
                id: "five_hour".into(),
                label: "5 小时".into(),
                used_percent: used,
                resets_at_ms: None,
                window_minutes: Some(300),
            }],
            credits: None,
            fetched_at_ms: 0,
            source: String::new(),
            freshness: Freshness::Live,
        }
    }

    #[test]
    fn new_session_and_reset_raise_toasts() {
        let prev = (vec![activity(1, Some(900))], vec![quota(80.0)]);
        let now = 10_000;
        let events = diff_events(
            Some(&prev),
            &[activity(2, Some(now - 1_000))],
            &[quota(5.0)],
            now,
        );
        let toasts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                HabitatEvent::Toast(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(toasts, ["Claude 开始了新会话", "Claude 5 小时 窗口已重置"]);
        assert!(events.iter().any(|e| matches!(
            e,
            HabitatEvent::Activity {
                who: ResidentId::Claude,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HabitatEvent::Quota { hp: 95, .. }))
        );
    }

    #[test]
    fn stale_activity_and_missing_quota() {
        let now = 100_000;
        let events = diff_events(None, &[activity(1, Some(now - 60_000))], &[], now);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HabitatEvent::Activity { .. }))
        );
        // 没有额度文件时按满体力喂。
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HabitatEvent::Quota { hp: 100, .. }))
        );
    }
}
