//! 归档页两栏：会话列表 + 会话详情；左栏里的「视图 / 项目」两组由外壳通过
//! `render_sidebar_sections` 取用，点击仍回到本视图处理。
//!
//! 所有 `DataSource` 调用都在后台线程执行，结果带 revision 回到前台，过期结果直接丢弃。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use agenthome_core::model::{ProviderHealth, SessionKind, SessionQuery, SessionSummary};
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

use crate::assets::{FONT_PIXEL_MONO, FONT_PIXEL_SANS};
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

    fn label(self) -> &'static str {
        match self {
            ViewFilter::All => "全部会话",
            ViewFilter::Starred => "星标",
            ViewFilter::ArchiveOnly => "仅归档存在",
            ViewFilter::Expiring => "7 天内将清理",
        }
    }
}

/// 中栏的一行：分组头或会话（下标指向 `sessions`）。
#[derive(Clone, Copy, Debug)]
enum ListRow {
    Group(DayGroup),
    Session(usize),
}

/// 后台整理好的列表：只含主会话，子代理折叠成计数。
struct Arranged {
    sessions: Vec<SessionSummary>,
    subagents: HashMap<String, u32>,
    rows: Vec<ListRow>,
}

/// 把查询结果整理成分组列表。子代理按「同来源、同目录、时间落在父会话区间内」归属，
/// TODO: `SessionSummary` 带上 `parent_session_id` 后改为精确归属。
fn arrange(all: Vec<SessionSummary>, now_ms: i64) -> Arranged {
    let (subs, mains): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|s| s.kind == SessionKind::Subagent);
    let mut subagents: HashMap<String, u32> = HashMap::new();
    for sub in &subs {
        let parent = mains.iter().find(|m| {
            m.provider == sub.provider
                && m.cwd == sub.cwd
                && (m.started_at_ms..=m.updated_at_ms + SUBAGENT_SLACK_MS)
                    .contains(&sub.started_at_ms)
        });
        if let Some(parent) = parent {
            *subagents.entry(parent.id.clone()).or_default() += 1;
        }
    }
    let mut rows = Vec::with_capacity(mains.len() + 4);
    let mut current: Option<DayGroup> = None;
    for (ix, session) in mains.iter().enumerate() {
        let group = DayGroup::of(session.updated_at_ms, now_ms);
        if current != Some(group) {
            rows.push(ListRow::Group(group));
            current = Some(group);
        }
        rows.push(ListRow::Session(ix));
    }
    Arranged {
        sessions: mains,
        subagents,
        rows,
    }
}

/// 会话成本：优先按定价表估算，模型未知时退回 token 总量。
/// TODO: `SessionSummary` 带上 `estimated_cost_usd` 后直接用真值。
fn session_cost_label(session: &SessionSummary) -> String {
    session
        .models
        .first()
        .and_then(|model| agenthome_core::pricing::estimate_usd(model, &session.usage))
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
    let _ = writeln!(
        out,
        "- 来源：{}\n- 目录：{}\n- 时间：{} → {}\n- 消息：{}\n",
        provider_display_name(&session.provider),
        session.cwd.as_deref().unwrap_or("（无）"),
        short_datetime(session.started_at_ms),
        short_datetime(session.updated_at_ms),
        session.message_count
    );
    for message in transcript.messages.iter() {
        let role = match message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::System => "系统",
            Role::Tool => "工具",
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
                    let _ = writeln!(out, "> 思考：{}\n", text.replace('\n', "\n> "));
                }
                ContentBlock::ToolCall { name, input, .. } => {
                    let _ = writeln!(out, "```json\n// 调用 {name}\n{input}\n```\n");
                }
                ContentBlock::ToolResult {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        "结果（错误）"
                    } else {
                        "结果"
                    };
                    let _ = writeln!(out, "```text\n// {label}\n{output}\n```\n");
                }
                ContentBlock::Image { media_type } => {
                    let _ = writeln!(out, "[图片 {media_type}]\n");
                }
                ContentBlock::Compaction { summary } => {
                    let _ = writeln!(out, "> 压缩摘要：{summary}\n");
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
    search_text: String,
    starred: HashSet<String>,
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
            search_text: String::new(),
            starred: HashSet::new(),
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
            _subscriptions: subscriptions,
        };
        this.load_sidebar(cx);
        this.reload_sessions(cx);
        this
    }

    /// 标题栏搜索框的文字变化入口。
    pub fn set_search(&mut self, text: String, cx: &mut Context<Self>) {
        if self.search_text == text {
            return;
        }
        self.search_text = text;
        self.reload_sessions(cx);
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
                        this.sessions_error = Some(format!("读取项目列表失败：{error:#}"))
                    }
                }
                match result.1 {
                    Ok(health) => this.health = health,
                    Err(error) => {
                        this.sessions_error = Some(format!("读取归档健康失败：{error:#}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 按当前搜索词、项目与视图重新查询会话列表。
    fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        self.sessions_revision += 1;
        let revision = self.sessions_revision;
        self.loading_sessions = true;
        self.sessions_error = None;
        self.now_ms = now_ms();
        let now = self.now_ms;
        let text = self.search_text.trim().to_string();
        let query = SessionQuery {
            text: (!text.is_empty()).then_some(text),
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
                        this.sessions = Arc::new(arranged.sessions);
                        this.subagents = Arc::new(arranged.subagents);
                        this.rows = Arc::new(arranged.rows);
                        this.session_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                    Err(error) => {
                        this.sessions_error = Some(format!("读取会话列表失败：{error:#}"))
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
        self.messages_revision += 1;
        let revision = self.messages_revision;
        self.loading_messages = true;
        self.messages_error = None;
        self.transcript = None;
        self.expanded_blocks = Rc::default();
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
                            // 回放从第一条读起，而不是跟随尾部。
                            _ = state.scroll_to_item(0, cx);
                        });
                    }
                    Err(error) => this.messages_error = Some(format!("读取消息失败：{error:#}")),
                }
                cx.notify();
            });
        });
        cx.notify();
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
                    Notification::success(format!("已复制到剪贴板：{command}")),
                    cx,
                );
            }
            None => window
                .push_notification(Notification::warning("这个来源没有可恢复会话的命令行"), cx),
        }
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(transcript)) = (&self.selected_session, &self.transcript) else {
            window.push_notification(Notification::warning("消息还没读完，稍后再导出"), cx);
            return;
        };
        let Some(path) = export_path(session) else {
            window.push_notification(Notification::error("找不到 HOME 目录"), cx);
            return;
        };
        let session = session.clone();
        let transcript = transcript.clone();
        cx.spawn_in(window, async move |_, cx| {
            let shown = path.display().to_string();
            let result = cx
                .background_spawn(async move {
                    std::fs::write(&path, export_markdown(&session, &transcript))
                })
                .await;
            let note = match result {
                Ok(()) => Notification::success(format!("已导出到 {shown}")),
                Err(error) => Notification::error(format!("导出失败：{error}")),
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
                ViewFilter::All => (total.to_string(), false),
                ViewFilter::Starred => (self.starred.len().to_string(), false),
                ViewFilter::ArchiveOnly => (archive_only.to_string(), false),
                ViewFilter::Expiring => (expiring.to_string(), expiring > 0),
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
            .child(side_head("视图", cx))
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
            .child(side_head("项目", cx))
            .children(self.projects.iter().take(shown).map(|project| {
                let row = project.clone();
                side_item(
                    SharedString::from(format!("project:{}", project.name)),
                    None,
                    project.name.clone(),
                    Some((project.session_count.to_string(), false)),
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
                        .child(format!("还有 {hidden} 个项目…"))
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
            .child(
                div()
                    .font_family(FONT_PIXEL_MONO)
                    .text_size(px(16.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.loading_sessions {
                        "…".to_string()
                    } else {
                        sessions.len().to_string()
                    }),
            );
        let body = if let Some(error) = &self.sessions_error {
            div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else if matches!(self.view, ViewFilter::ArchiveOnly | ViewFilter::Expiring) {
            // TODO: `SessionSummary` 暂无原件状态，按归档状态筛选待 core 支持。
            empty_hint("按归档状态筛选待 core 提供每会话原件状态", cx)
        } else if self.loading_sessions && sessions.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element()
        } else if sessions.is_empty() {
            empty_hint("没有匹配的会话", cx)
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
                .child("从中间列表选择一个会话")
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("右侧会显示标题、指标与按轮次组织的消息流。"),
                )
                .into_any_element();
        };
        let session = session.clone();
        let transcript = self.transcript.clone();
        let starred = self.starred.contains(&session.id);
        let subagents = self.subagents.get(&session.id).copied().unwrap_or(0);
        let usage = session.usage;
        let meta = format!(
            "{} · {} · {} → {}",
            session.cwd.as_deref().unwrap_or("（无工作目录）"),
            provider_display_name(&session.provider),
            short_datetime(session.started_at_ms),
            short_datetime(session.updated_at_ms)
        );
        // TODO: 原件是否仍在磁盘上需要 core 在 `SessionSummary` 里给出。
        let model_label = match session.models.as_slice() {
            [] => "未知".to_string(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        };
        let cost_label = transcript
            .as_ref()
            .and_then(|t| t.estimated_cost_usd)
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
                .child(
                    div()
                        .text_size(px(16.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("· {tool_failures} 失败")),
                )
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
                    .tooltip(if starred { "取消星标" } else { "星标" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_star(cx))),
            )
            .child(
                // TODO: `DataSource` 暂未暴露原件路径，接上后用 `open -R <path>`。
                Button::new("reveal")
                    .outline()
                    .small()
                    .label("在 Finder 显示")
                    .disabled(true)
                    .tooltip("待 core 提供原件路径"),
            )
            .child(
                Button::new("export")
                    .outline()
                    .small()
                    .label("导出 Markdown")
                    .disabled(transcript.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
            )
            .child(
                Button::new("resume")
                    .primary()
                    .small()
                    .label(format!(
                        "用 {} 继续",
                        provider_display_name(&session.provider)
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
                        "模型",
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .pt_1()
                            .child(model_label),
                        cx,
                    ))
                    .child(stat_text(
                        "消息",
                        session.message_count.to_string(),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        "输入",
                        short_tokens(usage.input_tokens),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        "输出",
                        short_tokens(usage.output_tokens),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        "缓存读 / 写",
                        format!(
                            "{} / {}",
                            short_tokens(usage.cache_read_tokens),
                            short_tokens(usage.cache_write_tokens)
                        ),
                        None,
                        cx,
                    ))
                    .child(stat_text(
                        "估算成本",
                        cost_label,
                        Some(cx.theme().primary),
                        cx,
                    ))
                    .child(stat("工具调用", tools_value, cx))
                    .child(stat_text("子代理", subagents.to_string(), None, cx)),
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
                .child("读取消息…")
                .into_any_element()
        } else if let Some(transcript) = transcript {
            let expanded = self.expanded_blocks.clone();
            let owner = cx.entity().downgrade();
            let provider = session.provider.clone();
            let rows = transcript.clone();
            let scroller =
                MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                    render_row(ix, &rows, &provider, &expanded, &owner, cx)
                })
                .with_jump_button_label("跳到最新");
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
    let meta = format!(
        "{} · {} · {} 条 · {}",
        session.project_name,
        relative_time(session.updated_at_ms, now),
        session.message_count,
        session_cost_label(session)
    );
    v_flex()
        .id(SharedString::from(session.id.clone()))
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
                .child(div().flex_1().min_w_0().truncate().child(meta))
                .when(subagents > 0, |row| {
                    row.child(
                        Tag::secondary()
                            .small()
                            .font_family(FONT_PIXEL_MONO)
                            .child(format!("+{subagents} 子代理")),
                    )
                })
                .when(active, |row| {
                    row.child(
                        Tag::success()
                            .small()
                            .font_family(FONT_PIXEL_MONO)
                            .child("进行中"),
                    )
                }),
        )
        .on_click(move |_, _, cx| {
            let _ = owner.update(cx, |this, cx| this.select_session(&id, cx));
        })
        .into_any_element()
}

/// 左栏区块标题（Pixelify Sans 小字）。
fn side_head(text: &'static str, cx: &App) -> AnyElement {
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

fn empty_hint(text: &'static str, cx: &App) -> AnyElement {
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

/// 指标块：小标题 + VT323 大数字。
fn stat(label: &'static str, value: impl IntoElement, cx: &App) -> AnyElement {
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
    label: &'static str,
    value: String,
    color: Option<gpui_kit::Hsla>,
    cx: &App,
) -> AnyElement {
    stat(
        label,
        div().when_some(color, |v, c| v.text_color(c)).child(value),
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agenthome_core::model::{ProviderId, Usage};

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
        }
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
        assert_eq!(arranged.sessions.len(), 2, "子代理不单独出现在列表");
        assert_eq!(arranged.subagents.get("claude-code:m:a"), Some(&1));
        assert!(matches!(arranged.rows[0], ListRow::Group(DayGroup::Today)));
        assert!(matches!(arranged.rows[1], ListRow::Session(0)));
        assert!(matches!(
            arranged.rows[2],
            ListRow::Group(DayGroup::Earlier)
        ));
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
