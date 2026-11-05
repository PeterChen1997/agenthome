//! 归档浏览器主视图：项目 / 会话列表 / 会话详情三栏。
//!
//! 所有 `DataSource` 调用都在后台线程执行，结果带 revision 回到前台，
//! 过期结果直接丢弃，避免快速切换时旧数据覆盖新选择。

use std::{collections::HashSet, rc::Rc, sync::Arc};

use agenthome_core::model::{Message, SessionQuery, SessionSummary};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, TitleBar, h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    message_scroller::{MessageScroller, MessageScrollerState},
    resizable::{h_resizable, resizable_panel},
    scroll::Scrollbar,
    sidebar::{Sidebar, SidebarCollapsible, SidebarGroup, SidebarMenu, SidebarMenuItem},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    ScrollStrategy, SharedString, Styled as _, Subscription, Task, UniformListScrollHandle, Window,
    div, uniform_list,
};

use crate::data::{DataSource, ProjectRow};
use crate::format::{now_ms, relative_time, short_tokens};
use crate::message_row::render_message;

/// 一次会话列表查询最多取多少行。P0 没有分页，接 SQLite 时按需调整。
const SESSION_PAGE_LIMIT: usize = 1_000;

/// 消息流里一个可折叠块的标识：(消息 id, 块下标)。
pub type BlockKey = (String, usize);

pub struct Workspace {
    source: Arc<dyn DataSource>,
    /// 相对时间的参照点，启动时取一次。
    now_ms: i64,
    projects: Arc<Vec<ProjectRow>>,
    /// None 表示"全部"。
    selected_project: Option<ProjectRow>,
    sessions: Arc<Vec<SessionSummary>>,
    sessions_revision: usize,
    loading_sessions: bool,
    sessions_error: Option<String>,
    selected_session: Option<SessionSummary>,
    messages: Arc<Vec<Message>>,
    messages_revision: usize,
    loading_messages: bool,
    messages_error: Option<String>,
    expanded_blocks: Rc<HashSet<BlockKey>>,
    search: Entity<InputState>,
    scroller: Entity<MessageScrollerState>,
    session_scroll: UniformListScrollHandle,
    _sessions_task: Task<()>,
    _messages_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(source: Arc<dyn DataSource>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("AgentHome");
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索会话标题或项目…"));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.reload_sessions(cx);
                }
            }),
            cx.observe(&scroller, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            source,
            now_ms: now_ms(),
            projects: Arc::default(),
            selected_project: None,
            sessions: Arc::default(),
            sessions_revision: 0,
            loading_sessions: false,
            sessions_error: None,
            selected_session: None,
            messages: Arc::default(),
            messages_revision: 0,
            loading_messages: false,
            messages_error: None,
            expanded_blocks: Rc::default(),
            search,
            scroller,
            session_scroll: UniformListScrollHandle::new(),
            _sessions_task: Task::ready(()),
            _messages_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_projects(cx);
        this.reload_sessions(cx);
        this
    }

    fn load_projects(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { source.projects() }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(rows) => this.projects = Arc::new(rows),
                    Err(error) => {
                        this.sessions_error = Some(format!("读取项目列表失败：{error:#}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 按当前搜索词与项目重新查询会话列表。
    fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        self.sessions_revision += 1;
        let revision = self.sessions_revision;
        self.loading_sessions = true;
        self.sessions_error = None;
        let text = self.search.read(cx).value().to_string();
        let query = SessionQuery {
            text: (!text.trim().is_empty()).then_some(text),
            cwd: self.selected_project.as_ref().and_then(|p| p.cwd.clone()),
            limit: SESSION_PAGE_LIMIT,
            ..SessionQuery::default()
        };
        let source = self.source.clone();
        self._sessions_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.sessions(&query) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.sessions_revision != revision {
                    return;
                }
                this.loading_sessions = false;
                match result {
                    Ok(rows) => {
                        this.sessions = Arc::new(rows);
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
        self.messages = Arc::default();
        self.expanded_blocks = Rc::default();
        self.scroller.update(cx, |state, cx| state.reset(0, cx));
        let id = summary.id.clone();
        self.selected_session = Some(summary);
        let source = self.source.clone();
        self._messages_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.messages(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.messages_revision != revision {
                    return;
                }
                this.loading_messages = false;
                match result {
                    Ok(messages) => {
                        let count = messages.len();
                        this.messages = Arc::new(messages);
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

    fn render_projects(&self, cx: &mut Context<Self>) -> AnyElement {
        let total: u32 = self.projects.iter().map(|p| p.session_count).sum();
        let all_item = SidebarMenuItem::new("全部")
            .icon(IconName::Inbox)
            .active(self.selected_project.is_none())
            .suffix(move |_, cx| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(total.to_string())
            })
            .on_click(cx.listener(|this, _, _, cx| this.select_project(None, cx)));
        let items = self.projects.iter().map(|project| {
            let count = project.session_count;
            let row = project.clone();
            SidebarMenuItem::new(project.name.clone())
                .icon(IconName::Folder)
                .active(self.selected_project.as_ref() == Some(project))
                .suffix(move |_, cx| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(count.to_string())
                })
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_project(Some(row.clone()), cx)),
                )
        });
        Sidebar::new("projects")
            .collapsible(SidebarCollapsible::None)
            .w_full()
            .child(
                SidebarGroup::new("项目").child(SidebarMenu::new().child(all_item).children(items)),
            )
            .into_any_element()
    }

    fn render_sessions(&self, cx: &mut Context<Self>) -> AnyElement {
        let sessions = self.sessions.clone();
        let selected_id = self.selected_session.as_ref().map(|s| s.id.clone());
        let owner = cx.entity().downgrade();
        let now_ms = self.now_ms;
        let status = if self.loading_sessions {
            "加载中…".to_string()
        } else {
            format!("{} 个会话", sessions.len())
        };
        let body = if let Some(error) = &self.sessions_error {
            div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else if self.loading_sessions && sessions.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element()
        } else if sessions.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("没有匹配的会话")
                .into_any_element()
        } else {
            div()
                .relative()
                .size_full()
                .child(
                    uniform_list("sessions", sessions.len(), move |range, _, cx| {
                        range
                            .filter_map(|ix| sessions.get(ix))
                            .map(|session| {
                                let owner = owner.clone();
                                let id = session.id.clone();
                                let summary_line = format!(
                                    "{} · {} · {} 条 · {}",
                                    session.project_name,
                                    relative_time(session.updated_at_ms, now_ms),
                                    session.message_count,
                                    short_tokens(session.usage.total_tokens())
                                );
                                ListItem::new(SharedString::from(session.id.clone()))
                                    .selected(selected_id.as_deref() == Some(session.id.as_str()))
                                    .h_16()
                                    .px_3()
                                    .w_full()
                                    .accessibility_label(session.title.clone())
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .min_w_0()
                                            .flex_1()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .truncate()
                                                    .child(session.title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .truncate()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(summary_line),
                                            ),
                                    )
                                    .on_click(move |_, _, cx| {
                                        let _ = owner
                                            .update(cx, |this, cx| this.select_session(&id, cx));
                                    })
                                    .into_any_element()
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
            .size_full()
            .min_h_0()
            .child(
                v_flex()
                    .flex_none()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Input::new(&self.search)
                            .prefix(Icon::new(IconName::Search).small())
                            .cleanable(true),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(session) = &self.selected_session else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child("从中间列表选择一个会话")
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("右侧会显示标题、工作目录、模型、用量与完整消息流。"),
                )
                .into_any_element();
        };
        let usage = session.usage;
        let header = v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_base()
                    .font_medium()
                    .truncate()
                    .child(session.title.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .truncate()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        session
                            .cwd
                            .clone()
                            .unwrap_or_else(|| "（无工作目录）".to_string()),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("模型：{}", session.models.join(", ")))
                    .child(format!("消息：{}", session.message_count))
                    .child(format!(
                        "输入 {} · 输出 {} · 缓存读 {} · 缓存写 {} · 合计 {}",
                        short_tokens(usage.input_tokens),
                        short_tokens(usage.output_tokens),
                        short_tokens(usage.cache_read_tokens),
                        short_tokens(usage.cache_write_tokens),
                        short_tokens(usage.total_tokens())
                    )),
            );
        let body =
            if let Some(error) = &self.messages_error {
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
            } else {
                let messages = self.messages.clone();
                let expanded = self.expanded_blocks.clone();
                let owner = cx.entity().downgrade();
                MessageScroller::new("messages", self.scroller.clone(), move |ix, _, cx| {
                    match messages.get(ix) {
                        Some(message) => render_message(ix, message, &expanded, &owner, cx),
                        None => div().into_any_element(),
                    }
                })
                .with_jump_button_label("跳到最新")
                .into_any_element()
            };
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .size_full()
                        .px_1()
                        .pr_3()
                        .gap_2()
                        .child(div().text_sm().font_medium().child("AgentHome"))
                        .child(div().flex_1()),
                ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("agenthome-panes")
                        .child(
                            resizable_panel()
                                .size(rem * 14.)
                                .size_range(rem * 10. ..rem * 24.)
                                .child(self.render_projects(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size(rem * 24.)
                                .size_range(rem * 16. ..rem * 40.)
                                .child(self.render_sessions(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(rem * 30. ..rem * 200.)
                                .child(self.render_detail(cx)),
                        ),
                ),
            )
    }
}
