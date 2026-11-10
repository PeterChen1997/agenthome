//! ⌘K 全局搜索面板（参照 Notion）：居中浮层，顶部输入框，下面按会话分组的命中列表。
//!
//! 浮层用 gpui-kit 的 `Dialog`（`window.open_dialog`，自带遮罩、焦点归还与 Esc 关闭），
//! 里面放 `Command` 面板（↑↓ 移动、回车 / 点击确认、虚拟滚动、可变行高）。
//! 查询走 `DataSource::search`：输入防抖 150ms、后台线程执行、revision 丢弃过期结果。
//! 确认后发出 [`PaletteEvent::Open`]，由外壳切到归档页并定位到命中的消息。

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use agenthome_core::model::{
    ProviderId, Role, SEARCH_MARK_END, SEARCH_MARK_START, SearchHit, SessionSummary,
};
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Focusable as _, FontWeight,
    HighlightStyle, IntoElement, ParentElement as _, SharedString, Styled as _, StyledText, Task,
    WeakEntity, Window, div, px,
};

use crate::assets::FONT_PIXEL_MONO;
use crate::data::DataSource;
use crate::format::{now_ms, relative_time};
use crate::message_row::{provider_avatar, provider_short_name};

/// 一次查询最多取多少条命中。
const SEARCH_LIMIT: usize = 80;
/// 输入防抖：停顿这么久才真正查询。
const DEBOUNCE: Duration = Duration::from_millis(150);
/// 每个会话最多列出几条消息命中，其余折成「还有 N 处」。
const HITS_PER_SESSION: usize = 3;
/// 空查询时最多列出几个最近会话。
const RECENT_SHOWN: usize = 8;
/// 面板宽与列表高。
const PALETTE_WIDTH: f32 = 640.;
const LIST_HEIGHT: f32 = 440.;

/// 面板确认了一个目标：打开会话，并定位到 `message_id`（None 表示不定位到具体消息）。
#[derive(Clone, Debug)]
pub enum PaletteEvent {
    Open {
        session_id: String,
        message_id: Option<String>,
    },
}

/// 把 core 片段里的标记去掉，返回纯文本与命中区间（字节下标，落在字符边界上）。
/// 标记不成对时：多余的结束标记忽略，未闭合的开始标记高亮到结尾；空区间不输出。
pub(crate) fn split_marks(snippet: &str) -> (String, Vec<Range<usize>>) {
    let mut text = String::with_capacity(snippet.len());
    let mut ranges = Vec::new();
    let mut open: Option<usize> = None;
    for ch in snippet.chars() {
        match ch {
            SEARCH_MARK_START => {
                open.get_or_insert(text.len());
            }
            SEARCH_MARK_END => {
                if let Some(start) = open.take()
                    && start < text.len()
                {
                    ranges.push(start..text.len());
                }
            }
            _ => text.push(ch),
        }
    }
    if let Some(start) = open
        && start < text.len()
    {
        ranges.push(start..text.len());
    }
    (text, ranges)
}

/// 同一会话的命中：组头是会话，组内是消息命中（最多 `HITS_PER_SESSION` 条）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HitGroup {
    pub session_id: String,
    pub title: String,
    pub provider: ProviderId,
    pub project_name: String,
    /// 组内最新一条命中的时间。
    pub latest_ms: i64,
    /// 标题命中时的高亮片段。
    pub title_snippet: Option<String>,
    pub hits: Vec<SearchHit>,
    /// 超出上限、没列出来的消息命中数。
    pub more: usize,
}

impl HitGroup {
    /// 没有命中的会话（⌘K 空查询时的「最近」列表），复用同一种组头。
    fn of_session(session: &SessionSummary) -> Self {
        Self {
            session_id: session.id.clone(),
            title: session.title.clone(),
            provider: session.provider.clone(),
            project_name: session.project_name.clone(),
            latest_ms: session.updated_at_ms,
            title_snippet: None,
            hits: Vec::new(),
            more: 0,
        }
    }
}

/// 按会话分组，组的顺序 = 该会话第一条命中在结果里的位置（core 已按相关度排序）。
pub(crate) fn group_hits(hits: Vec<SearchHit>, per_session: usize) -> Vec<HitGroup> {
    let mut groups: Vec<HitGroup> = Vec::new();
    for hit in hits {
        let ix = match groups.iter().position(|g| g.session_id == hit.session_id) {
            Some(ix) => ix,
            None => {
                groups.push(HitGroup {
                    session_id: hit.session_id.clone(),
                    title: hit.session_title.clone(),
                    provider: hit.provider.clone(),
                    project_name: hit.project_name.clone(),
                    latest_ms: hit.timestamp_ms,
                    title_snippet: None,
                    hits: Vec::new(),
                    more: 0,
                });
                groups.len() - 1
            }
        };
        let Some(group) = groups.get_mut(ix) else {
            continue;
        };
        group.latest_ms = group.latest_ms.max(hit.timestamp_ms);
        if hit.message_id.is_none() {
            group.title_snippet.get_or_insert(hit.snippet);
        } else if group.hits.len() < per_session {
            group.hits.push(hit);
        } else {
            group.more += 1;
        }
    }
    groups
}

/// 列表里每一行确认后去哪：(会话 id, 消息 id)。按 `IndexPath` 的 section / row 存。
type Targets = Vec<Vec<(String, Option<String>)>>;

pub struct SearchPalette {
    source: Arc<dyn DataSource>,
    state: Entity<CommandState>,
    /// 已发出查询的关键字（去掉首尾空白）。
    query: String,
    groups: Arc<Vec<HitGroup>>,
    recent: Arc<Vec<SessionSummary>>,
    recent_label: SharedString,
    targets: Arc<Targets>,
    error: Option<SharedString>,
    now_ms: i64,
    revision: usize,
    _task: Task<()>,
}

impl EventEmitter<PaletteEvent> for SearchPalette {}

impl SearchPalette {
    pub fn new(source: Arc<dyn DataSource>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            source,
            state: cx.new(|cx| CommandState::new(window, cx)),
            query: String::new(),
            groups: Arc::default(),
            recent: Arc::default(),
            recent_label: SharedString::default(),
            targets: Arc::default(),
            error: None,
            now_ms: now_ms(),
            revision: 0,
            _task: Task::ready(()),
        }
    }

    /// 弹出面板：清空上次的查询，空查询时列出 `recent`（标题为 `recent_label`）。
    pub fn show(
        &mut self,
        recent: Vec<SessionSummary>,
        recent_label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.revision += 1;
        self._task = Task::ready(());
        self.query.clear();
        self.groups = Arc::default();
        self.error = None;
        self.now_ms = now_ms();
        self.recent = Arc::new(recent.into_iter().take(RECENT_SHOWN).collect());
        self.recent_label = recent_label.into();
        self.rebuild_targets();
        self.state.update(cx, |state, cx| {
            state.set_query("", window, cx);
            state.set_loading(false, window, cx);
        });

        let owner = cx.weak_entity();
        let state = self.state.clone();
        let focus_on_mount = std::rc::Rc::new(std::cell::Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            let close_owner = owner.clone();
            let state = state.clone();
            let focus_on_mount = focus_on_mount.clone();
            dialog
                .close_button(false)
                .w(px(PALETTE_WIDTH))
                .margin_top(px(96.))
                .p_0()
                .on_close(move |_, _, cx| {
                    // 关掉面板即放弃进行中的查询。
                    let _ = close_owner.update(cx, |this, _| {
                        this.revision += 1;
                        this._task = Task::ready(());
                    });
                })
                .content(move |content, window, cx| {
                    if focus_on_mount.replace(false) {
                        let state = state.clone();
                        window.defer(cx, move |window, cx| {
                            let handle = state.read(cx).focus_handle(cx);
                            handle.focus(window, cx);
                        });
                    }
                    match owner.upgrade() {
                        Some(palette) => {
                            let command = palette.read(cx).command(owner.clone(), cx);
                            content.child(command)
                        }
                        None => content,
                    }
                })
        });
        cx.notify();
    }

    /// 弹出面板并直接填入查询（启动参数 `--search`，截图与端到端核对用）。
    pub fn show_with_query(
        &mut self,
        recent: Vec<SessionSummary>,
        recent_label: impl Into<SharedString>,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show(recent, recent_label, window, cx);
        self.state
            .update(cx, |state, cx| state.set_query(query, window, cx));
        self.on_query(query, window, cx);
    }

    /// 输入变化（Command 已在本次更新结束后回调）。空查询立即回到「最近」，否则防抖后后台查。
    fn on_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let query = query.trim().to_string();
        if query == self.query {
            return;
        }
        self.query = query.clone();
        self.revision += 1;
        let revision = self.revision;
        let state = self.state.clone();
        if query.is_empty() {
            self._task = Task::ready(());
            self.groups = Arc::default();
            self.error = None;
            self.rebuild_targets();
            state.update(cx, |state, cx| state.set_loading(false, window, cx));
            cx.notify();
            window.refresh();
            return;
        }
        state.update(cx, |state, cx| state.set_loading(true, window, cx));
        let source = self.source.clone();
        self._task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let result = cx
                .background_spawn(async move { source.search(&query, SEARCH_LIMIT) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.revision != revision {
                    return;
                }
                state.update(cx, |state, cx| state.set_loading(false, window, cx));
                match result {
                    Ok(hits) => {
                        this.groups = Arc::new(group_hits(hits, HITS_PER_SESSION));
                        this.error = None;
                    }
                    Err(error) => {
                        this.groups = Arc::default();
                        this.error = Some(
                            t!("search.failed", error = format!("{error:#}"))
                                .to_string()
                                .into(),
                        );
                    }
                }
                this.now_ms = now_ms();
                this.rebuild_targets();
                cx.notify();
                // 面板内容由对话框（Root）在渲染时向本实体取；本实体 notify 不会让 Root 重绘，
                // 不刷新窗口的话结果要等下一次按键才出现。
                window.refresh();
            });
        });
    }

    fn on_confirm(&mut self, index: IndexPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session_id, message_id)) = self
            .targets
            .get(index.section)
            .and_then(|rows| rows.get(index.row))
            .cloned()
        else {
            return;
        };
        window.close_dialog(cx);
        cx.emit(PaletteEvent::Open {
            session_id,
            message_id,
        });
    }

    /// 与 `command` 里的分组一一对应：每组第 0 行是组头（会话），之后是消息命中。
    fn rebuild_targets(&mut self) {
        let targets: Targets = if self.query.is_empty() {
            vec![
                self.recent
                    .iter()
                    .map(|s| (s.id.clone(), None))
                    .collect::<Vec<_>>(),
            ]
        } else {
            self.groups
                .iter()
                .map(|group| {
                    // 组头直接跳到组内第一条命中；只有标题命中时按普通方式打开。
                    let first = group.hits.first().and_then(|h| h.message_id.clone());
                    std::iter::once((group.session_id.clone(), first))
                        .chain(
                            group
                                .hits
                                .iter()
                                .map(|h| (group.session_id.clone(), h.message_id.clone())),
                        )
                        .collect()
                })
                .collect()
        };
        self.targets = Arc::new(targets);
    }

    /// 按当前结果构建 Command（每次浮层重绘都会重建，条目都是轻量克隆）。
    fn command(&self, owner: WeakEntity<Self>, _cx: &App) -> Command {
        let query_owner = owner.clone();
        let confirm_owner = owner;
        let query: SharedString = self.query.clone().into();
        let has_recent = !self.recent.is_empty();
        let mut command = Command::new(&self.state)
            .bordered(false)
            .filterable(false)
            .placeholder(t!("search.placeholder").to_string())
            .min_h(px(LIST_HEIGHT))
            .max_h(px(LIST_HEIGHT))
            .on_query(move |query, window, cx| {
                let _ = query_owner.update(cx, |this, cx| this.on_query(query, window, cx));
            })
            .on_confirm(move |index, window, cx| {
                let _ = confirm_owner.update(cx, |this, cx| this.on_confirm(index, window, cx));
            })
            .empty(move |_, _, cx| {
                let text: SharedString = if query.is_empty() {
                    if has_recent {
                        "".into()
                    } else {
                        t!("search.empty_hint").to_string().into()
                    }
                } else {
                    t!("search.no_results", query = query).to_string().into()
                };
                v_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .py_6()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Search).size_6())
                    .child(text)
            })
            .footer({
                let summary = self.footer_summary();
                let error = self.error.clone();
                move |_, _, cx| {
                    h_flex()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("search.key_select").to_string())
                        .child(t!("search.key_open").to_string())
                        .child(t!("search.key_close").to_string())
                        .child(div().flex_1())
                        .map(|row| match &error {
                            Some(error) => row.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(cx.theme().danger)
                                    .child(error.clone()),
                            ),
                            None => row.child(summary.clone()),
                        })
                }
            });
        let now = self.now_ms;
        if self.query.is_empty() {
            if !self.recent.is_empty() {
                let items = self.recent.iter().map(|session| {
                    let session = HitGroup::of_session(session);
                    CommandItem::new()
                        .label(session.title.clone())
                        .child(move |_, cx| session_header(&session, now, cx))
                });
                command = command.group(
                    CommandGroup::new()
                        .label(self.recent_label.clone())
                        .items(items),
                );
            }
            return command;
        }
        for (ix, group) in self.groups.iter().enumerate() {
            if ix > 0 {
                command = command.separator();
            }
            let header = {
                let group = group.clone();
                CommandItem::new()
                    .label(group.title.clone())
                    .child(move |_, cx| session_header(&group, now, cx))
            };
            let hits = group.hits.iter().map(|hit| {
                let hit = hit.clone();
                CommandItem::new()
                    .label(hit.snippet.clone())
                    .child(move |_, cx| hit_row(&hit, cx))
            });
            command = command.group(CommandGroup::new().item(header).items(hits));
        }
        command
    }

    fn footer_summary(&self) -> SharedString {
        if self.query.is_empty() {
            return "".into();
        }
        let hits: usize = self
            .groups
            .iter()
            .map(|g| g.hits.len() + g.more + usize::from(g.title_snippet.is_some()))
            .sum();
        if hits == 0 {
            return "".into();
        }
        let capped = if hits >= SEARCH_LIMIT {
            t!("search.capped")
        } else {
            "".into()
        };
        let sessions = self.groups.len();
        let hits = if hits == 1 {
            t!("search.hits_one", count = hits)
        } else {
            t!("search.hits_other", count = hits)
        };
        let sessions = if sessions == 1 {
            t!("search.sessions_one", count = sessions)
        } else {
            t!("search.sessions_other", count = sessions)
        };
        t!(
            "search.summary",
            hits = hits,
            sessions = sessions,
            capped = capped
        )
        .to_string()
        .into()
    }
}

/// 命中词高亮：强调色字 + 半透明强调色底 + 加粗。
fn highlighted(snippet: &str, cx: &App) -> StyledText {
    let (text, ranges) = split_marks(snippet);
    let style = HighlightStyle {
        color: Some(cx.theme().primary),
        background_color: Some(cx.theme().primary.opacity(0.18)),
        font_weight: Some(FontWeight::BOLD),
        ..HighlightStyle::default()
    };
    StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, style)))
}

/// 组头：来源头像 + 会话标题（标题命中时高亮并标「标题」）+ 项目 · 相对时间。
fn session_header(group: &HitGroup, now: i64, cx: &App) -> AnyElement {
    let HitGroup {
        provider,
        title,
        title_snippet,
        project_name: project,
        latest_ms: at_ms,
        more,
        ..
    } = group;
    let (at_ms, more, title_snippet) = (*at_ms, *more, title_snippet.as_deref());
    let title_el: AnyElement = match title_snippet {
        Some(snippet) => highlighted(snippet, cx).into_any_element(),
        None => SharedString::from(title.to_string()).into_any_element(),
    };
    h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_none()
                .rounded(px(3.))
                .border_1()
                .border_color(cx.theme().border)
                .child(provider_avatar(provider, 20.)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_semibold()
                .child(title_el),
        )
        .when(title_snippet.is_some(), |row| {
            row.child(badge(&t!("search.title_badge"), cx.theme().primary, cx))
        })
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "{} · {}",
                    if project.is_empty() {
                        t!("search.no_project")
                    } else {
                        project.as_str().into()
                    },
                    relative_time(at_ms, now)
                )),
        )
        .when(more > 0, |row| {
            row.child(
                div()
                    .flex_none()
                    .font_family(FONT_PIXEL_MONO)
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("search.more_hits", count = more).to_string()),
            )
        })
        .into_any_element()
}

/// 组内一条消息命中：缩进到标题下，角色徽标 + 最多两行的高亮片段。
fn hit_row(hit: &SearchHit, cx: &App) -> AnyElement {
    let role = match hit.role {
        Some(Role::User) => t!("search.role_user"),
        Some(Role::Assistant) => provider_short_name(&hit.provider).to_string().into(),
        Some(Role::Tool) => t!("search.role_tool"),
        Some(Role::System) => t!("search.role_system"),
        None => t!("search.role_message"),
    };
    let color = match hit.role {
        Some(Role::User) => cx.theme().foreground,
        _ => cx.theme().muted_foreground,
    };
    h_flex()
        .w_full()
        .min_w_0()
        .pl(px(28.))
        .gap_2()
        .items_start()
        .child(div().flex_none().pt(px(1.)).child(badge(&role, color, cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .line_clamp(2)
                .child(highlighted(&hit.snippet, cx)),
        )
        .into_any_element()
}

fn badge(text: &str, color: gpui_kit::Hsla, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.))
        .rounded(px(2.))
        .border_1()
        .border_color(cx.theme().border)
        .text_xs()
        .text_color(color)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(parts: &[(&str, bool)]) -> String {
        parts
            .iter()
            .map(|(text, hit)| {
                if *hit {
                    format!("{SEARCH_MARK_START}{text}{SEARCH_MARK_END}")
                } else {
                    (*text).to_string()
                }
            })
            .collect()
    }

    #[test]
    fn marks_become_highlight_ranges_on_char_boundaries() {
        // 用户反馈「搜索结果没有高亮，不知道搜的是什么」：标记必须精确切成高亮区间，
        // 中文多字节字符下区间也要落在字符边界上，否则 StyledText 会切坏字。
        let snippet = marked(&[
            ("…为什么在", false),
            ("空列表", true),
            ("上会 ", false),
            ("panic", true),
            ("，顺便", false),
        ]);
        let (text, ranges) = split_marks(&snippet);
        assert_eq!(text, "…为什么在空列表上会 panic，顺便");
        let picked: Vec<&str> = ranges.iter().filter_map(|r| text.get(r.clone())).collect();
        assert_eq!(picked, ["空列表", "panic"]);
        assert!(
            ranges
                .iter()
                .all(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
        );
    }

    #[test]
    fn snippet_without_marks_is_not_highlighted() {
        let (text, ranges) = split_marks("没有任何标记的片段");
        assert_eq!(text, "没有任何标记的片段");
        assert!(ranges.is_empty());
    }

    #[test]
    fn unbalanced_marks_never_panic_or_highlight_nothing() {
        // 片段在命中词中间被截断时，开始标记可能没有结束标记：高亮到结尾。
        let (text, ranges) = split_marks(&format!("前缀{SEARCH_MARK_START}半截"));
        assert_eq!(text.get(ranges.first().unwrap().clone()), Some("半截"));
        // 多余的结束标记与空命中都不产出区间。
        let (_, ranges) = split_marks(&format!(
            "{SEARCH_MARK_END}a{SEARCH_MARK_START}{SEARCH_MARK_END}b"
        ));
        assert!(ranges.is_empty());
    }

    fn hit(session: &str, message: Option<&str>, at: i64) -> SearchHit {
        SearchHit {
            session_id: session.into(),
            message_id: message.map(Into::into),
            session_title: format!("{session} 标题"),
            provider: ProviderId::ClaudeCode,
            project_name: "agenthome".into(),
            role: message.map(|_| Role::User),
            timestamp_ms: at,
            snippet: format!("{session}-{}", message.unwrap_or("title")),
        }
    }

    #[test]
    fn hits_group_by_session_in_relevance_order() {
        // 一个会话的多处命中要收在同一组下，组顺序沿用 core 的相关度顺序，
        // 标题命中单独记在组头而不是混进消息行；超出上限的折成计数而不是丢掉。
        let groups = group_hits(
            vec![
                hit("b", Some("b1"), 10),
                hit("a", None, 50),
                hit("b", Some("b2"), 30),
                hit("a", Some("a1"), 20),
                hit("b", Some("b3"), 5),
            ],
            2,
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.session_id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        let b = groups.first().unwrap();
        assert_eq!(b.hits.len(), 2);
        assert_eq!(b.more, 1);
        assert_eq!(b.latest_ms, 30);
        assert!(b.title_snippet.is_none());
        let a = groups.get(1).unwrap();
        assert_eq!(a.title_snippet.as_deref(), Some("a-title"));
        assert_eq!(a.hits.len(), 1);
        assert_eq!(a.latest_ms, 50);
    }
}
