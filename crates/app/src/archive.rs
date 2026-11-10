//! 归档页两栏：会话列表 + 会话详情；左栏里的「视图 / 项目」两组由外壳通过
//! `render_sidebar_sections` 取用，点击仍回到本视图处理。
//!
//! 所有 `DataSource` 调用都在后台线程执行，结果带 revision 回到前台，过期结果直接丢弃。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use agenthome_core::model::{
    CostSource, ProviderHealth, SessionKind, SessionQuery, SessionSummary,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    message_scroller::{MessageScroller, MessageScrollerState},
    notification::Notification,
    scroll::Scrollbar,
    spinner::Spinner,
    tag::Tag,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window, div, px, uniform_list,
};

use crate::assets::{
    FONT_PIXEL_MONO, FONT_PIXEL_SANS, PixelMono as _, body_size_for_pixel, has_cjk,
};
use crate::data::{DataSource, ProjectRow};
use crate::format::{DayGroup, now_ms, relative_time, short_cost, short_datetime, short_tokens};
use crate::message_row::{
    BlockKey, Transcript, provider_avatar, provider_display_name, render_row, secondary,
};

/// 一次会话列表查询最多取多少行。
const SESSION_PAGE_LIMIT: usize = 5_000;
/// 左栏默认展示的项目数，其余折叠在「还有 N 个项目…」后。
const PROJECTS_SHOWN: usize = 7;
/// 中栏每行高度（分组头与会话行共用，uniform_list 要求等高）。
const SESSION_ROW_HEIGHT: f32 = 54.;
/// 轮次导航条每项高度。
const RAIL_ROW_HEIGHT: f32 = 26.;
/// 更新时间在此窗口内的会话标「进行中」。
const ACTIVE_WITHIN_MS: i64 = 10 * 60_000;
/// 子代理归属启发：父会话最后更新之后这段时间内开始的子代理仍算它的。
const SUBAGENT_SLACK_MS: i64 = 10 * 60_000;
/// 记住最近打开过的会话数（⌘K 空查询时列出）。
const RECENT_KEPT: usize = 8;
/// 从搜索跳过来后，命中消息保持高亮的时长。
const FLASH_FOR: std::time::Duration = std::time::Duration::from_millis(2_400);

/// 左栏「视图」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewFilter {
    All,
    Starred,
    ArchiveOnly,
    Expiring,
}

impl ViewFilter {
    const ALL: [ViewFilter; 4] = [
        ViewFilter::All,
        ViewFilter::Starred,
        ViewFilter::ArchiveOnly,
        ViewFilter::Expiring,
    ];

    fn label(self) -> SharedString {
        match self {
            ViewFilter::All => t!("archive.view_all"),
            ViewFilter::Starred => t!("archive.view_starred"),
            ViewFilter::ArchiveOnly => t!("archive.view_archive_only"),
            ViewFilter::Expiring => t!("archive.view_expiring"),
        }
        .into()
    }
}

/// 中栏的一行：分组头或会话（下标指向 `sessions`）。
#[derive(Clone, Copy, Debug)]
enum ListRow {
    Group(DayGroup),
    Session(usize),
}

/// 后台整理好的列表：主会话 + 找不到父会话的子代理；能归属的子代理折叠成父会话上的计数。
struct Arranged {
    sessions: Vec<SessionSummary>,
    subagents: HashMap<String, u32>,
    rows: Vec<ListRow>,
}

/// 把查询结果整理成分组列表（输入已按 updated_at 倒序，输出保持该顺序）。
///
/// 子代理归属：
/// 1. 有 `parent_session_id` 的按 id 精确归属；父会话不在本次结果里（被筛掉或超出上限）就不再猜。
/// 2. 没有 parent id 的才用启发式：同来源、同目录、开始时间落在父会话区间（+10 分钟）内。
/// 3. 仍找不到父会话的子代理作为独立行留在列表里，不静默丢弃（10.21 / 3.10）。
fn arrange(all: Vec<SessionSummary>, now_ms: i64) -> Arranged {
    let is_sub = |s: &SessionSummary| s.kind == SessionKind::Subagent;
    let main_ids: HashSet<&str> = all
        .iter()
        .filter(|s| !is_sub(s))
        .map(|s| s.id.as_str())
        .collect();
    let parents: Vec<Option<String>> = all
        .iter()
        .map(|sub| {
            if !is_sub(sub) {
                return None;
            }
            match sub.parent_session_id.as_deref() {
                Some(pid) => main_ids.contains(pid).then(|| pid.to_string()),
                None => all
                    .iter()
                    .find(|m| {
                        !is_sub(m)
                            && m.provider == sub.provider
                            && m.cwd == sub.cwd
                            && (m.started_at_ms..=m.updated_at_ms + SUBAGENT_SLACK_MS)
                                .contains(&sub.started_at_ms)
                    })
                    .map(|m| m.id.clone()),
            }
        })
        .collect();
    let mut subagents: HashMap<String, u32> = HashMap::new();
    let mut sessions = Vec::with_capacity(all.len());
    for (session, parent) in all.into_iter().zip(parents) {
        match parent {
            Some(parent) => *subagents.entry(parent).or_default() += 1,
            None => sessions.push(session),
        }
    }
    let mut rows = Vec::with_capacity(sessions.len() + 4);
    let mut current: Option<DayGroup> = None;
    for (ix, session) in sessions.iter().enumerate() {
        let group = DayGroup::of(session.updated_at_ms, now_ms);
        if current != Some(group) {
            rows.push(ListRow::Group(group));
            current = Some(group);
        }
        rows.push(ListRow::Session(ix));
    }
    Arranged {
        sessions,
        subagents,
        rows,
    }
}

/// 会话成本：用 core 给出的会话级成本（agent 真值优先，否则消息估算之和）；
/// 无法计价时退回 token 总量。
fn session_cost_label(session: &SessionSummary) -> String {
    session
        .cost_usd
        .map(short_cost)
        .unwrap_or_else(|| short_tokens(session.usage.total_tokens()))
}

/// 会话 id 的 native 部分（`{provider}:{machine}:{native}`）。
fn native_id(session_id: &str) -> &str {
    session_id.splitn(3, ':').nth(2).unwrap_or(session_id)
}

/// 「用 X 继续」要复制的命令；没有对应 CLI 的来源返回 None。
fn resume_command(session: &SessionSummary) -> Option<String> {
    use agenthome_core::model::ProviderId;
    let native = native_id(&session.id);
    match session.provider {
        ProviderId::ClaudeCode => Some(format!("claude --resume {native}")),
        ProviderId::Codex => Some(format!("codex resume {native}")),
        _ => None,
    }
}

/// 把会话导出成 Markdown 正文。
fn export_markdown(session: &SessionSummary, transcript: &Transcript) -> String {
    use agenthome_core::model::{ContentBlock, Role};
    use std::fmt::Write as _;
    let mut out = format!("# {}\n\n", session.title);
    let no_cwd = t!("archive.export_no_cwd");
    let _ = writeln!(
        out,
        "{}",
        t!(
            "archive.export_meta",
            provider = provider_display_name(&session.provider),
            cwd = session.cwd.as_deref().unwrap_or(&no_cwd),
            start = short_datetime(session.started_at_ms),
            end = short_datetime(session.updated_at_ms),
            count = session.message_count
        )
    );
    for message in transcript.messages.iter() {
        let role = match message.role {
            Role::User => t!("archive.role_user"),
            Role::Assistant => t!("archive.role_assistant"),
            Role::System => t!("archive.role_system"),
            Role::Tool => t!("archive.role_tool"),
        };
        let _ = writeln!(
            out,
            "## {role} · {}\n",
            crate::format::absolute_time(message.timestamp_ms)
        );
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => {
                    let _ = writeln!(out, "{text}\n");
                }
                ContentBlock::Thinking { text } => {
                    let _ = writeln!(
                        out,
                        "> {}{}\n",
                        t!("archive.export_thinking"),
                        text.replace('\n', "\n> ")
                    );
                }
                ContentBlock::ToolCall { name, input, .. } => {
                    let call = t!("archive.export_call");
                    let _ = writeln!(out, "```json\n// {call} {name}\n{input}\n```\n");
                }
                ContentBlock::ToolResult {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        t!("archive.export_result_error")
                    } else {
                        t!("archive.export_result")
                    };
                    let _ = writeln!(out, "```text\n// {label}\n{output}\n```\n");
                }
                ContentBlock::Image { media_type } => {
                    let image = t!("archive.export_image");
                    let _ = writeln!(out, "[{image} {media_type}]\n");
                }
                ContentBlock::Compaction { summary } => {
                    let label = t!("archive.export_compaction");
                    let _ = writeln!(out, "> {label}{summary}\n");
                }
                ContentBlock::Unknown { native_type, raw } => {
                    let _ = writeln!(out, "```json\n// {native_type}\n{raw}\n```\n");
                }
            }
        }
    }
    out
}

/// 导出文件名：标题里不能进文件名的字符换成下划线。
fn export_path(session: &SessionSummary) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let safe: String = session
        .title
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(80)
        .collect();
    Some(
        std::path::PathBuf::from(home)
            .join("Downloads")
            .join(format!("{}-{}.md", safe.trim(), native_id(&session.id))),
    )
}

pub struct ArchiveView {
    source: Arc<dyn DataSource>,
    /// 相对时间的参照点，每次重载列表时刷新。
    now_ms: i64,
    projects: Arc<Vec<ProjectRow>>,
    projects_expanded: bool,
    health: Vec<ProviderHealth>,
    view: ViewFilter,
    selected_project: Option<ProjectRow>,
    starred: HashSet<String>,
    /// 最近打开的会话，最新的在前（只在内存里）。
    recent: Vec<SessionSummary>,
    /// 短暂高亮的消息 id（从搜索跳转过来时）。
    flash_message: Option<String>,
    sessions: Arc<Vec<SessionSummary>>,
    subagents: Arc<HashMap<String, u32>>,
    rows: Arc<Vec<ListRow>>,
    sessions_revision: usize,
    loading_sessions: bool,
    sessions_error: Option<String>,
    selected_session: Option<SessionSummary>,
    transcript: Option<Arc<Transcript>>,
    messages_revision: usize,
    loading_messages: bool,
    messages_error: Option<String>,
    expanded_blocks: Rc<HashSet<BlockKey>>,
    scroller: Entity<MessageScrollerState>,
    session_scroll: UniformListScrollHandle,
    rail_scroll: UniformListScrollHandle,
    _sessions_task: Task<()>,
    _messages_task: Task<()>,
    _flash_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ArchiveView {
    pub fn new(source: Arc<dyn DataSource>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let subscriptions = vec![cx.observe(&scroller, |_, _, cx| cx.notify())];
        let mut this = Self {
            source,
            now_ms: now_ms(),
            projects: Arc::default(),
            projects_expanded: false,
            health: Vec::new(),
            view: ViewFilter::All,
            selected_project: None,
            starred: HashSet::new(),
            recent: Vec::new(),
            flash_message: None,
            sessions: Arc::default(),
            subagents: Arc::default(),
            rows: Arc::default(),
            sessions_revision: 0,
            loading_sessions: false,
            sessions_error: None,
            selected_session: None,
            transcript: None,
            messages_revision: 0,
            loading_messages: false,
            messages_error: None,
            expanded_blocks: Rc::default(),
            scroller,
            session_scroll: UniformListScrollHandle::new(),
            rail_scroll: UniformListScrollHandle::new(),
            _sessions_task: Task::ready(()),
            _messages_task: Task::ready(()),
            _flash_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_sidebar(cx);
        this.reload_sessions(cx);
        this
    }

    /// ⌘K 面板空查询时列出的会话：最近打开的；还没打开过就退回最近更新的。
    pub fn recent_for_palette(&self) -> (Vec<SessionSummary>, SharedString) {
        if self.recent.is_empty() {
            let latest = self.sessions.iter().take(RECENT_KEPT).cloned().collect();
            (latest, t!("archive.recently_updated").into())
        } else {
            (self.recent.clone(), t!("archive.recently_opened").into())
        }
    }

    /// 从 ⌘K 面板打开会话：选中它，`message_id` 有值时滚到那条消息并短暂高亮。
    /// 会话不在当前列表里（被项目筛掉、或是折叠进父会话的子代理）时，到全量列表里找摘要。
    pub fn open_session_at(
        &mut self,
        session_id: String,
        message_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(summary) = self.sessions.iter().find(|s| s.id == session_id).cloned() {
            self.open_summary(summary, message_id, cx);
            return;
        }
        let source = self.source.clone();
        cx.spawn_in(window, async move |this, cx| {
            let wanted = session_id.clone();
            let found = cx
                .background_spawn(async move {
                    source
                        .sessions(&SessionQuery {
                            limit: SESSION_PAGE_LIMIT,
                            ..SessionQuery::default()
                        })
                        .map(|rows| rows.into_iter().find(|s| s.id == wanted))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match found {
                Ok(Some(summary)) => this.open_summary(summary, message_id, cx),
                Ok(None) => window.push_notification(
                    Notification::warning(t!("archive.session_not_found", id = session_id)),
                    cx,
                ),
                Err(error) => window.push_notification(
                    Notification::error(t!(
                        "archive.load_session_failed",
                        error = format!("{error:#}")
                    )),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// 项目列表与归档健康一起拉，都只在启动时读一次。
    fn load_sidebar(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { (source.projects(), source.archive_health()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result.0 {
                    Ok(rows) => this.projects = Arc::new(rows),
                    Err(error) => {
                        this.sessions_error = Some(
                            t!("archive.load_projects_failed", error = format!("{error:#}"))
                                .into_owned(),
                        )
                    }
                }
                match result.1 {
                    Ok(health) => this.health = health,
                    Err(error) => {
                        this.sessions_error = Some(
                            t!("archive.load_health_failed", error = format!("{error:#}"))
                                .into_owned(),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 按当前搜索词、项目与视图重新查询会话列表。
    /// 同步引擎有新数据入库后调用：静默重拉左栏项目、会话列表与当前会话的消息，
    /// 不显示加载态、不把列表滚回顶部；当前会话有新消息时追加到详情尾部。
    pub fn refresh_data(&mut self, cx: &mut Context<Self>) {
        self.load_sidebar(cx);
        self.reload_sessions_with(true, cx);
    }

    fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        self.reload_sessions_with(false, cx);
    }

    fn reload_sessions_with(&mut self, quiet: bool, cx: &mut Context<Self>) {
        self.sessions_revision += 1;
        let revision = self.sessions_revision;
        self.loading_sessions = !quiet;
        self.sessions_error = None;
        self.now_ms = now_ms();
        let now = self.now_ms;
        let query = SessionQuery {
            cwd: self.selected_project.as_ref().and_then(|p| p.cwd.clone()),
            limit: SESSION_PAGE_LIMIT,
            ..SessionQuery::default()
        };
        // 星标只在内存里；TODO: 持久化到 ~/AgentHome。
        let starred = (self.view == ViewFilter::Starred).then(|| self.starred.clone());
        let source = self.source.clone();
        self._sessions_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    source.sessions(&query).map(|mut rows| {
                        if let Some(starred) = &starred {
                            rows.retain(|s| starred.contains(&s.id));
                        }
                        arrange(rows, now)
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.sessions_revision != revision {
                    return;
                }
                this.loading_sessions = false;
                match result {
                    Ok(arranged) => {
                        // 当前会话在后台有更新（正在进行的会话）：换上新摘要并追加新消息。
                        let updated = this.selected_session.as_ref().and_then(|selected| {
                            arranged
                                .sessions
                                .iter()
                                .find(|s| s.id == selected.id)
                                .filter(|s| s.updated_at_ms != selected.updated_at_ms)
                                .cloned()
                        });
                        this.sessions = Arc::new(arranged.sessions);
                        this.subagents = Arc::new(arranged.subagents);
                        this.rows = Arc::new(arranged.rows);
                        if !quiet {
                            this.session_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        if let Some(summary) = updated {
                            this.refresh_selected_messages(summary, cx);
                        }
                    }
                    Err(error) => {
                        this.sessions_error = Some(
                            t!("archive.load_sessions_failed", error = format!("{error:#}"))
                                .into_owned(),
                        )
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn select_view(&mut self, view: ViewFilter, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.reload_sessions(cx);
    }

    fn select_project(&mut self, project: Option<ProjectRow>, cx: &mut Context<Self>) {
        if self.selected_project == project {
            return;
        }
        self.selected_project = project;
        self.reload_sessions(cx);
    }

    fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == session_id)
        {
            return;
        }
        let Some(summary) = self.sessions.iter().find(|s| s.id == session_id).cloned() else {
            return;
        };
        self.open_summary(summary, None, cx);
    }

    /// 选中并载入一个会话。`target` 为某条消息 id 时载入后滚到它并短暂高亮，否则停在最新一条。
    fn open_summary(
        &mut self,
        summary: SessionSummary,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.recent.retain(|s| s.id != summary.id);
        self.recent.insert(0, summary.clone());
        self.recent.truncate(RECENT_KEPT);
        let same = self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == summary.id);
        if same && self.transcript.is_some() && !self.loading_messages {
            // 已经打开着：不重读，只定位。
            if let Some(target) = target {
                self.scroll_to_message(&target, cx);
            }
            cx.notify();
            return;
        }
        self.messages_revision += 1;
        let revision = self.messages_revision;
        self.loading_messages = true;
        self.messages_error = None;
        self.transcript = None;
        self.expanded_blocks = Rc::default();
        self.flash_message = None;
        self.scroller.update(cx, |state, cx| state.reset(0, cx));
        let id = summary.id.clone();
        self.selected_session = Some(summary);
        let source = self.source.clone();
        self._messages_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.messages(&id).map(Transcript::build) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.messages_revision != revision {
                    return;
                }
                this.loading_messages = false;
                match result {
                    Ok(transcript) => {
                        let count = transcript.messages.len();
                        this.transcript = Some(Arc::new(transcript));
                        this.scroller.update(cx, |state, cx| {
                            state.reset(count, cx);
                            // 打开会话直接停在最新一条：多数时候是回来看刚发生的事。
                            state.scroll_to_end(cx);
                        });
                        if let Some(target) = &target {
                            this.scroll_to_message(target, cx);
                        }
                    }
                    Err(error) => {
                        this.messages_error = Some(
                            t!("archive.load_messages_failed", error = format!("{error:#}"))
                                .into_owned(),
                        )
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 滚到某条消息并高亮约 2 秒；消息不在当前会话里时什么也不做（返回 false）。
    fn scroll_to_message(&mut self, message_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(transcript) = &self.transcript else {
            return false;
        };
        let Some(ix) = transcript.messages.iter().position(|m| m.id == message_id) else {
            return false;
        };
        let anchor = transcript.anchor_of(ix);
        self.flash_message = transcript.messages.get(anchor).map(|m| m.id.clone());
        self.scroller.update(cx, |state, cx| {
            _ = state.scroll_to_item(anchor, cx);
        });
        self._flash_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.flash_message = None;
                cx.notify();
            });
        });
        cx.notify();
        true
    }

    /// 静默重拉当前会话的消息：前缀不变时只把新增的行追加到虚拟列表，保住阅读位置。
    fn refresh_selected_messages(&mut self, summary: SessionSummary, cx: &mut Context<Self>) {
        self.messages_revision += 1;
        let revision = self.messages_revision;
        let id = summary.id.clone();
        self.selected_session = Some(summary);
        let source = self.source.clone();
        self._messages_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.messages(&id).map(Transcript::build) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.messages_revision != revision {
                    return;
                }
                // 静默刷新失败不覆盖已显示的内容，只在详情顶部提示。
                let transcript = match result {
                    Ok(transcript) => transcript,
                    Err(error) => {
                        this.messages_error = Some(
                            t!(
                                "archive.refresh_messages_failed",
                                error = format!("{error:#}")
                            )
                            .into_owned(),
                        );
                        cx.notify();
                        return;
                    }
                };
                let old_count = this.transcript.as_ref().map_or(0, |t| t.messages.len());
                let new_count = transcript.messages.len();
                this.transcript = Some(Arc::new(transcript));
                this.messages_error = None;
                this.loading_messages = false;
                this.scroller.update(cx, |state, cx| {
                    if new_count >= old_count && state.item_count() == old_count {
                        state.append(new_count - old_count, cx);
                    } else {
                        state.reset(new_count, cx);
                    }
                });
                cx.notify();
            });
        });
    }

    /// 展开 / 折叠一个内容块；行高变了，必须让虚拟列表重新测量该行。
    pub(crate) fn toggle_block(
        &mut self,
        message_ix: usize,
        key: BlockKey,
        cx: &mut Context<Self>,
    ) {
        let expanded = Rc::make_mut(&mut self.expanded_blocks);
        if !expanded.remove(&key) {
            expanded.insert(key);
        }
        self.scroller.update(cx, |state, cx| {
            _ = state.remeasure_items(message_ix..message_ix + 1, cx);
        });
        cx.notify();
    }

    fn scroll_to_turn(&mut self, turn_ix: usize, cx: &mut Context<Self>) {
        let Some(start) = self
            .transcript
            .as_ref()
            .and_then(|t| t.turn_starts.get(turn_ix).copied())
        else {
            return;
        };
        self.scroller.update(cx, |state, cx| {
            _ = state.scroll_to_item(start, cx);
        });
    }

    fn toggle_star(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_session.as_ref().map(|s| s.id.clone()) else {
            return;
        };
        if !self.starred.remove(&id) {
            self.starred.insert(id);
        }
        if self.view == ViewFilter::Starred {
            self.reload_sessions(cx);
        }
        cx.notify();
    }

    fn copy_resume_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.selected_session else {
            return;
        };
        match resume_command(session) {
            Some(command) => {
                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                window.push_notification(
                    Notification::success(t!("archive.copied", command = command)),
                    cx,
                );
            }
            None => {
                window.push_notification(Notification::warning(t!("archive.no_resume_command")), cx)
            }
        }
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(transcript)) = (&self.selected_session, &self.transcript) else {
            window.push_notification(Notification::warning(t!("archive.export_not_ready")), cx);
            return;
        };
        let Some(path) = export_path(session) else {
            window.push_notification(Notification::error(t!("archive.no_home")), cx);
            return;
        };
        let session = session.clone();
        let transcript = transcript.clone();
        cx.spawn_in(window, async move |_, cx| {
            let shown = crate::format::tilde(&path);
            let result = cx
                .background_spawn(async move {
                    std::fs::write(&path, export_markdown(&session, &transcript))
                })
                .await;
            let note = match result {
                Ok(()) => Notification::success(t!("archive.exported", path = shown)),
                Err(error) => Notification::error(t!("archive.export_failed", error = error)),
            };
            let _ = cx.update(|window, cx| window.push_notification(note, cx));
        })
        .detach();
    }

    fn current_turn(&self, cx: &App) -> Option<usize> {
        let transcript = self.transcript.as_ref()?;
        let top = self.scroller.read(cx).logical_scroll_top().item_ix;
        let turn = transcript.turn_of(top);
        (turn > 0).then(|| turn as usize - 1)
    }

    // ---------- 左栏页面专属区（外壳调用） ----------

    /// 左栏的「视图 / 项目」两组；外壳在归档页把它接在公共导航下面。
    pub fn render_sidebar_sections(&self, cx: &mut Context<Self>) -> AnyElement {
        let total: u32 = self.projects.iter().map(|p| p.session_count).sum();
        let archive_only: u64 = self.health.iter().map(|h| h.archive_only).sum();
        let expiring: u64 = self.health.iter().map(|h| h.expiring_within_7d).sum();
        let view_count = |view: ViewFilter| -> (String, bool) {
            match view {
                ViewFilter::All => (crate::format::thousands(u64::from(total)), false),
                ViewFilter::Starred => (self.starred.len().to_string(), false),
                ViewFilter::ArchiveOnly => (crate::format::thousands(archive_only), false),
                ViewFilter::Expiring => (crate::format::thousands(expiring), expiring > 0),
            }
        };
        let shown = if self.projects_expanded {
            self.projects.len()
        } else {
            PROJECTS_SHOWN
        };
        let hidden = self.projects.len().saturating_sub(shown);
        v_flex()
            .w_full()
            .child(side_head(t!("archive.side_views").into(), cx))
            .children(ViewFilter::ALL.map(|view| {
                let (count, alert) = view_count(view);
                side_item(
                    ("view", view as usize),
                    None,
                    view.label(),
                    Some((count, alert)),
                    self.view == view,
                    cx.listener(move |this, _, _, cx| this.select_view(view, cx)),
                    cx,
                )
            }))
            .child(side_head(t!("archive.side_projects").into(), cx))
            .children(self.projects.iter().take(shown).map(|project| {
                let row = project.clone();
                side_item(
                    SharedString::from(format!("project:{}", project.name)),
                    None,
                    project.name.clone(),
                    Some((
                        crate::format::thousands(u64::from(project.session_count)),
                        false,
                    )),
                    self.selected_project.as_ref() == Some(project),
                    cx.listener(move |this, _, _, cx| {
                        let next =
                            (this.selected_project.as_ref() != Some(&row)).then(|| row.clone());
                        this.select_project(next, cx);
                    }),
                    cx,
                )
            }))
            .when(hidden > 0, |col| {
                col.child(
                    div()
                        .id("more-projects")
                        .mx(px(6.))
                        .px(px(10.))
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .rounded(cx.theme().radius)
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .hover(|style| style.bg(cx.theme().secondary))
                        .cursor_pointer()
                        .child(if hidden == 1 {
                            t!("archive.more_projects_one")
                        } else {
                            t!("archive.more_projects_other", count = hidden)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.projects_expanded = true;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    // ---------- 中栏 ----------

    fn render_session_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows.clone();
        let sessions = self.sessions.clone();
        let subagents = self.subagents.clone();
        let selected_id = self.selected_session.as_ref().map(|s| s.id.clone());
        let owner = cx.entity().downgrade();
        let now = self.now_ms;
        let header = h_flex()
            .flex_none()
            .h(px(40.))
            .px(px(14.))
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().secondary)
            .child(
                div()
                    .font_family(FONT_PIXEL_SANS)
                    .text_sm()
                    .font_semibold()
                    .child(self.view.label()),
            )
            .child({
                let count_text = if self.loading_sessions {
                    "…".to_string()
                } else {
                    // 列表把子代理折叠进父会话，左栏「全部会话」数的是全部会话（含子代理），
                    // 两个数不同时把差别写出来，免得看着像统计错了。
                    let total: u32 = self.projects.iter().map(|p| p.session_count).sum();
                    let shown = sessions.len();
                    if self.view == ViewFilter::All && total as usize > shown {
                        t!(
                            "archive.count_with_subagents",
                            shown = crate::format::thousands(shown as u64),
                            total = crate::format::thousands(u64::from(total))
                        )
                        .into_owned()
                    } else {
                        crate::format::thousands(shown as u64)
                    }
                };
                // 「含子代理共」是中文：VT323 没有中文字形，含中文时退回正文字体。
                div()
                    .pixel_mono(&count_text, 16., cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(count_text)
            });
        let body = if let Some(error) = &self.sessions_error {
            div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else if matches!(self.view, ViewFilter::ArchiveOnly | ViewFilter::Expiring) {
            // TODO: `SessionSummary` 暂无原件状态，按归档状态筛选待 core 支持。
            empty_hint(t!("archive.archive_filter_pending").into(), cx)
        } else if self.loading_sessions && sessions.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element()
        } else if sessions.is_empty() {
            empty_hint(t!("archive.no_sessions").into(), cx)
        } else {
            div()
                .relative()
                .size_full()
                .child(
                    uniform_list("sessions", rows.len(), move |range, _, cx| {
                        range
                            .filter_map(|ix| rows.get(ix).copied())
                            .map(|row| match row {
                                ListRow::Group(group) => div()
                                    .h(px(SESSION_ROW_HEIGHT))
                                    .px_4()
                                    .flex()
                                    .items_end()
                                    .pb_1()
                                    .font_family(FONT_PIXEL_SANS)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(group.label())
                                    .into_any_element(),
                                ListRow::Session(ix) => match sessions.get(ix) {
                                    Some(session) => render_session_row(
                                        session,
                                        subagents.get(&session.id).copied().unwrap_or(0),
                                        selected_id.as_deref() == Some(session.id.as_str()),
                                        now,
                                        &owner,
                                        cx,
                                    ),
                                    None => div().into_any_element(),
                                },
                            })
                            .collect()
                    })
                    .size_full()
                    .track_scroll(&self.session_scroll),
                )
                .child(Scrollbar::vertical(&self.session_scroll))
                .into_any_element()
        };
        v_flex()
            .w(px(360.))
            .flex_none()
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().secondary)
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    // ---------- 右栏 ----------

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(session) = &self.selected_session else {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(t!("archive.select_session"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("archive.select_session_hint")),
                )
                .into_any_element();
        };
        let session = session.clone();
        let transcript = self.transcript.clone();
        let starred = self.starred.contains(&session.id);
        // 原件可能已被 agent 清理（Claude 默认 30 天）；不在磁盘上时按钮置灰并说明。
        let source_path = session
            .source_path
            .clone()
            .filter(|path| std::path::Path::new(path).exists());
        let reveal_tip = if source_path.is_some() {
            t!("archive.reveal_tip")
        } else if session.source_path.is_some() {
            t!("archive.reveal_missing")
        } else {
            t!("archive.reveal_no_path")
        };
        let subagents = self.subagents.get(&session.id).copied().unwrap_or(0);
        let usage = session.usage;
        let meta = format!(
            "{} · {} · {} → {}",
            session
                .cwd
                .as_deref()
                .map(|cwd| crate::format::tilde(std::path::Path::new(cwd)))
                .unwrap_or_else(|| t!("archive.no_cwd").into_owned()),
            provider_display_name(&session.provider),
            short_datetime(session.started_at_ms),
            short_datetime(session.updated_at_ms)
        );
        // TODO: 原件是否仍在磁盘上需要 core 在 `SessionSummary` 里给出。
        let model_label = match session.models.as_slice() {
            [] => t!("archive.unknown_model").into_owned(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        };
        // 与列表同一口径：会话级成本（有 agent 自报真值时用真值）优先，详情的逐条估算兜底。
        let cost_label = session
            .cost_usd
            .or_else(|| transcript.as_ref().and_then(|t| t.estimated_cost_usd))
            .map(short_cost)
            .unwrap_or_else(|| session_cost_label(&session));
        let (tool_calls, tool_failures) = transcript
            .as_ref()
            .map(|t| (t.tool_calls, t.tool_failures))
            .unwrap_or((0, 0));
        let tools_value: AnyElement = if tool_failures > 0 {
            h_flex()
                .items_baseline()
                .gap_1()
                .child(tool_calls.to_string())
                .child({
                    let text = t!("archive.tool_failures", count = tool_failures).into_owned();
                    div()
                        .pixel_mono(&text, 16., cx)
                        .text_color(cx.theme().muted_foreground)
                        .child(text)
                })
                .into_any_element()
        } else {
            div().child(tool_calls.to_string()).into_any_element()
        };

        let actions = h_flex()
            .flex_none()
            .gap(px(6.))
            .child(
                Button::new("star")
                    .outline()
                    .small()
                    .icon(if starred {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .tooltip(if starred {
                        t!("archive.unstar")
                    } else {
                        t!("archive.star")
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_star(cx))),
            )
            .child(
                Button::new("reveal")
                    .outline()
                    .small()
                    .label(t!("archive.reveal"))
                    .disabled(source_path.is_none())
                    .tooltip(reveal_tip)
                    .on_click(move |_, _, _| {
                        if let Some(path) = &source_path {
                            // `open -R` 只在 Finder 里选中文件，不打开、不修改。
                            if let Err(error) = std::process::Command::new("open")
                                .arg("-R")
                                .arg(path)
                                .spawn()
                            {
                                eprintln!("在 Finder 显示失败：{error}");
                            }
                        }
                    }),
            )
            .child(
                Button::new("export")
                    .outline()
                    .small()
                    .label(t!("archive.export"))
                    .tooltip(t!("archive.export_tip"))
                    .disabled(transcript.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
            )
            .child(
                Button::new("resume")
                    .primary()
                    .small()
                    .label(t!("archive.resume"))
                    .tooltip(t!(
                        "archive.resume_tip",
                        provider = provider_display_name(&session.provider)
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_resume_command(window, cx)),
                    ),
            );

        let header = v_flex()
            .flex_none()
            .px_6()
            .pt_4()
            .pb_3()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().secondary)
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().border)
                            .child(provider_avatar(&session.provider, 40.)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .truncate()
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(secondary(cx))
                                    .child(meta),
                            ),
                    )
                    .child(actions),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_end()
                    .gap(px(22.))
                    .child(stat(
                        t!("archive.stat_model").into(),
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .pt_1()
                            .child(model_label),
                        cx,
                    ))
                    .child(stat_text(
                        t!("archive.stat_messages").into(),
                        crate::format::thousands(u64::from(session.message_count)),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        t!("archive.stat_input").into(),
                        short_tokens(usage.input_tokens),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        t!("archive.stat_output").into(),
                        short_tokens(usage.output_tokens),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        t!("archive.stat_cache").into(),
                        format!(
                            "{} / {}",
                            short_tokens(usage.cache_read_tokens),
                            short_tokens(usage.cache_write_tokens)
                        ),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        if session.cost_source == CostSource::Recorded {
                            t!("archive.stat_cost").into()
                        } else {
                            t!("archive.stat_estimated_cost").into()
                        },
                        cost_label,
                        Some(cx.theme().primary),
                        cx,
                    ))
                    .child(stat(t!("archive.stat_tool_calls").into(), tools_value, cx))
                    .child(stat_text(
                        t!("archive.stat_subagents").into(),
                        subagents.to_string(),
                        None,
                        cx,
                    )),
            );

        let body = if let Some(error) = &self.messages_error {
            div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else if self.loading_messages {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new())
                .child(t!("archive.loading_messages"))
                .into_any_element()
        } else if let Some(transcript) = transcript {
            let expanded = self.expanded_blocks.clone();
            let owner = cx.entity().downgrade();
            let provider = session.provider.clone();
            let rows = transcript.clone();
            let flash = self.flash_message.clone();
            let scroller =
                MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                    let highlighted =
                        flash.is_some() && rows.messages.get(ix).map(|m| &m.id) == flash.as_ref();
                    render_row(ix, &rows, &provider, &expanded, highlighted, &owner, cx)
                })
                .with_jump_button_label(t!("archive.jump_to_latest"));
            h_flex()
                .items_stretch()
                .size_full()
                .min_h_0()
                .child(div().flex_1().min_w_0().min_h_0().child(scroller))
                .child(self.render_rail(&transcript, cx))
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .child(header)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }

    /// 右侧 56px 轮次导航条，虚拟化。
    fn render_rail(&self, transcript: &Arc<Transcript>, cx: &mut Context<Self>) -> AnyElement {
        let turns = transcript.turn_starts.len();
        let current = self.current_turn(cx);
        let owner = cx.entity().downgrade();
        div()
            .flex_none()
            .w(px(56.))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().secondary)
            .bg(cx.theme().group_box)
            .py_3()
            .child(
                uniform_list("turn-rail", turns, move |range, _, cx| {
                    range
                        .map(|turn_ix| {
                            let owner = owner.clone();
                            let on = current == Some(turn_ix);
                            div().h(px(RAIL_ROW_HEIGHT)).flex().justify_center().child(
                                div()
                                    .id(("turn", turn_ix))
                                    .w(px(36.))
                                    .h(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(3.))
                                    .font_family(FONT_PIXEL_MONO)
                                    .text_size(px(16.))
                                    .text_color(if on {
                                        cx.theme().primary
                                    } else {
                                        secondary(cx)
                                    })
                                    .when(on, |tick| {
                                        tick.bg(cx.theme().secondary)
                                            .border_1()
                                            .border_color(cx.theme().border)
                                    })
                                    .hover(|style| style.bg(cx.theme().secondary))
                                    .cursor_pointer()
                                    .child(format!("{:02}", turn_ix + 1))
                                    .on_click(move |_, _, cx| {
                                        let _ = owner.update(cx, |this, cx| {
                                            this.scroll_to_turn(turn_ix, cx)
                                        });
                                    }),
                            )
                        })
                        .collect()
                })
                .size_full()
                .track_scroll(&self.rail_scroll),
            )
            .into_any_element()
    }
}

impl Render for ArchiveView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_stretch()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_session_list(cx))
            .child(self.render_detail(cx))
    }
}

/// 中栏一行会话：头像 + 标题，次行项目 · 时间 · 条数 · 成本 + 标签。
fn render_session_row(
    session: &SessionSummary,
    subagents: u32,
    selected: bool,
    now: i64,
    owner: &gpui_kit::WeakEntity<ArchiveView>,
    cx: &mut App,
) -> AnyElement {
    let owner = owner.clone();
    let id = session.id.clone();
    let active = now - session.updated_at_ms <= ACTIVE_WITHIN_MS;
    // TODO: 「仅归档」「N 天后清理」标签需要 `SessionSummary` 带原件状态。
    let messages = if session.message_count == 1 {
        t!("archive.row_messages_one")
    } else {
        t!(
            "archive.row_messages_other",
            count = crate::format::thousands(u64::from(session.message_count))
        )
    };
    // 项目名放前面、空间不够时只截它；时间、消息数和成本不截（英文更长，成本曾被挤成「$0...」）。
    let meta_tail = format!(
        "· {} · {messages} · {}",
        relative_time(session.updated_at_ms, now),
        session_cost_label(session)
    );
    v_flex()
        .id(SharedString::from(session.id.clone()))
        // 撑满列宽：否则选中 / 悬停底色只铺到内容宽度为止。
        .w_full()
        .h(px(SESSION_ROW_HEIGHT))
        .justify_center()
        .pl_3()
        .pr(px(14.))
        .border_l_2()
        .border_color(if selected {
            cx.theme().primary
        } else {
            cx.theme().transparent
        })
        .when(selected, |row| row.bg(cx.theme().secondary))
        .hover(|style| style.bg(cx.theme().group_box))
        .cursor_pointer()
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .font_medium()
                .child(
                    div()
                        .flex_none()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(provider_avatar(&session.provider, 18.)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(session.title.clone()),
                ),
        )
        .child(
            h_flex()
                .mt_1()
                .pl(px(26.))
                .gap_2()
                .text_xs()
                .text_color(secondary(cx))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(session.project_name.clone()),
                        )
                        .child(div().flex_none().child(meta_tail)),
                )
                .when(subagents > 0, |row| {
                    let text = if subagents == 1 {
                        t!("archive.subagents_tag_one")
                    } else {
                        t!("archive.subagents_tag_other", count = subagents)
                    };
                    row.child(pixel_tag(Tag::secondary(), &text))
                })
                .when(active, |row| {
                    row.child(pixel_tag(Tag::success(), &t!("archive.active")))
                }),
        )
        .on_click(move |_, _, cx| {
            let _ = owner.update(cx, |this, cx| this.select_session(&id, cx));
        })
        .into_any_element()
}

/// 左栏区块标题（Pixelify Sans 小字）。
fn side_head(text: SharedString, cx: &App) -> AnyElement {
    div()
        .px_4()
        .pt(px(14.))
        .pb(px(6.))
        .font_family(FONT_PIXEL_SANS)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// 左栏可点行：可选图标 + 文字 + 可选右侧计数，选中态左缘 2px 琥珀条。外壳导航也用它。
pub(crate) fn side_item(
    id: impl Into<gpui_kit::ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    // 右侧计数与是否用警示色。
    count: Option<(String, bool)>,
    selected: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    h_flex()
        .id(id)
        .mx(px(6.))
        .px(px(10.))
        .h(px(30.))
        .gap_2()
        .rounded(cx.theme().radius)
        .text_sm()
        .cursor_pointer()
        .when(selected, |row| {
            row.bg(cx.theme().secondary)
                .text_color(cx.theme().primary)
                .border_l_2()
                .border_color(cx.theme().primary)
        })
        .hover(|style| style.bg(cx.theme().secondary))
        .when_some(icon, |row, icon| row.child(Icon::new(icon).small()))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .when_some(count, |row, (count, alert)| {
            row.child(
                div()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(if alert {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(count),
            )
        })
        .on_click(on_click)
        .into_any_element()
}

fn empty_hint(text: SharedString, cx: &App) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .px_4()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// 列表行的小标签：纯拉丁文字用 VT323；含中文（「+1 子代理」「进行中」）时保持 Tag 自带的正文字体，
/// 否则中文会退回到按 VT323 字号画的系统字体，比旁边的字大一圈。
fn pixel_tag(tag: Tag, text: &str) -> Tag {
    let tag = tag.small();
    if has_cjk(text) {
        tag.child(text.to_string())
    } else {
        tag.font_family(FONT_PIXEL_MONO).child(text.to_string())
    }
}

/// 指标块：小标题 + VT323 大数字。
fn stat(label: SharedString, value: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .min_w(px(88.))
        .gap(px(2.))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(
            div()
                .font_family(FONT_PIXEL_MONO)
                .text_size(px(22.))
                .line_height(px(22.))
                .child(value),
        )
        .into_any_element()
}

fn stat_text(
    label: SharedString,
    value: String,
    color: Option<gpui_kit::Hsla>,
    cx: &App,
) -> AnyElement {
    // 值可能是中文（如模型「未知」）：含中文时退回正文字体。
    stat(
        label,
        div()
            .when(has_cjk(&value), |v| {
                v.font_family(cx.theme().font_family.clone())
                    .text_size(px(body_size_for_pixel(22.)))
            })
            .when_some(color, |v, c| v.text_color(c))
            .child(value),
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agenthome_core::model::{CostSource, ProviderId, Usage};

    fn summary(id: &str, kind: SessionKind, started: i64, updated: i64) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            provider: ProviderId::ClaudeCode,
            kind,
            title: id.to_string(),
            cwd: Some("/p".into()),
            project_name: "p".into(),
            started_at_ms: started,
            updated_at_ms: updated,
            message_count: 1,
            usage: Usage::default(),
            models: vec![],
            parent_session_id: None,
            source_path: None,
            cost_usd: None,
            cost_source: CostSource::Unknown,
        }
    }

    fn sub_of(id: &str, parent: Option<&str>, started: i64, updated: i64) -> SessionSummary {
        SessionSummary {
            parent_session_id: parent.map(str::to_string),
            ..summary(id, SessionKind::Subagent, started, updated)
        }
    }

    fn listed_ids(arranged: &Arranged) -> Vec<&str> {
        arranged.sessions.iter().map(|s| s.id.as_str()).collect()
    }

    /// 子代理的父会话不在结果里（被筛掉 / 超出上限 / 原件已删）时，它不能从列表里凭空消失，
    /// 否则用户搜到的内容、花掉的 token 都看不见。
    #[test]
    fn orphan_subagent_stays_in_list() {
        let now = 1_800_000_000_000;
        let arranged = arrange(
            vec![
                sub_of(
                    "claude-code:m:gone/agent-1",
                    Some("claude-code:m:gone"),
                    now - 120_000,
                    now - 60_000,
                ),
                summary(
                    "claude-code:m:a",
                    SessionKind::Main,
                    now - 3_600_000,
                    now - 90_000,
                ),
                // 无 parent id、启发式也匹配不到（开始时间远早于任何主会话）
                sub_of(
                    "claude-code:m:x/agent-2",
                    None,
                    now - 9_000_000,
                    now - 100_000,
                ),
            ],
            now,
        );
        assert_eq!(
            listed_ids(&arranged),
            vec![
                "claude-code:m:gone/agent-1",
                "claude-code:m:a",
                "claude-code:m:x/agent-2"
            ],
            "孤儿子代理作为独立行保留，且保持 updated_at 倒序"
        );
        assert!(arranged.subagents.is_empty());
        assert_eq!(arranged.rows.len(), 4, "一个「今天」分组头 + 三行");
    }

    /// 有 parent id 时必须按 id 归属：时间启发式在并行会话、同目录多开时会挂错父会话。
    #[test]
    fn parent_id_wins_over_time_heuristic() {
        let now = 1_800_000_000_000;
        let arranged = arrange(
            vec![
                // a 的时间区间覆盖子代理开始时间，启发式会选 a
                summary(
                    "claude-code:m:a",
                    SessionKind::Main,
                    now - 3_600_000,
                    now - 60_000,
                ),
                // b 早已结束，按时间不可能是父会话，但 parent id 指向它
                summary(
                    "claude-code:m:b",
                    SessionKind::Main,
                    now - 9_000_000,
                    now - 8_000_000,
                ),
                sub_of(
                    "claude-code:m:b/agent-1",
                    Some("claude-code:m:b"),
                    now - 1_800_000,
                    now - 120_000,
                ),
            ],
            now,
        );
        assert_eq!(arranged.subagents.get("claude-code:m:b"), Some(&1));
        assert_eq!(arranged.subagents.get("claude-code:m:a"), None);
        assert_eq!(
            listed_ids(&arranged),
            vec!["claude-code:m:a", "claude-code:m:b"]
        );
    }

    /// 列表成本直接用 core 的会话级成本（真值或消息估算之和），不再用「首个模型 × 会话 usage」近似；
    /// 无法计价时退回 token 总量，而不是显示 $0。
    #[test]
    fn cost_label_uses_session_cost_or_falls_back_to_tokens() {
        let mut s = summary("codex:m:c", SessionKind::Main, 0, 0);
        s.models = vec!["gpt-5-codex".into()];
        s.usage = Usage {
            input_tokens: 1_000,
            cache_read_tokens: 1_000_000,
            ..Usage::default()
        };
        s.cost_usd = Some(3.5);
        s.cost_source = CostSource::Estimated;
        assert_eq!(session_cost_label(&s), short_cost(3.5));
        s.cost_usd = None;
        s.cost_source = CostSource::Unknown;
        assert_eq!(session_cost_label(&s), short_tokens(s.usage.total_tokens()));
    }

    #[test]
    fn subagents_fold_into_parent_and_rows_are_grouped() {
        let now = 1_800_000_000_000;
        let day = 86_400_000;
        let arranged = arrange(
            vec![
                summary(
                    "claude-code:m:a",
                    SessionKind::Main,
                    now - 3_600_000,
                    now - 60_000,
                ),
                summary(
                    "claude-code:m:a-sub",
                    SessionKind::Subagent,
                    now - 1_800_000,
                    now - 120_000,
                ),
                summary(
                    "claude-code:m:b",
                    SessionKind::Main,
                    now - 40 * day,
                    now - 40 * day,
                ),
            ],
            now,
        );
        assert_eq!(arranged.sessions.len(), 2, "能归属的子代理不单独出现在列表");
        assert_eq!(arranged.subagents.get("claude-code:m:a"), Some(&1));
        assert!(matches!(arranged.rows[0], ListRow::Group(DayGroup::Today)));
        assert!(matches!(arranged.rows[1], ListRow::Session(0)));
        assert!(matches!(
            arranged.rows[2],
            ListRow::Group(DayGroup::Earlier)
        ));
    }

    /// 导出的 Markdown 跟随界面语言；测试不切 locale（全局、并行），默认是英文，不能漏出中文标签。
    #[test]
    fn export_markdown_follows_locale() {
        let mut session = summary("claude-code:mac:abc", SessionKind::Main, 0, 0);
        session.cwd = None;
        let out = export_markdown(&session, &Transcript::build(Vec::new()));
        assert!(out.contains("- Source: "), "{out}");
        assert!(out.contains("- Directory: (none)"), "{out}");
        assert!(
            t!("archive.export_meta", locale = "zh-CN").contains("- 来源："),
            "中文模板保持原文"
        );
    }

    #[test]
    fn resume_command_uses_native_id() {
        let main = summary("claude-code:mac:abc-123", SessionKind::Main, 0, 0);
        assert_eq!(
            resume_command(&main).as_deref(),
            Some("claude --resume abc-123")
        );
    }
}
