//! UI 侧的数据访问抽象。所有实现都在 `cx.background_spawn` 里被调用，
//! 因此必须 `Send + Sync`；UI 线程只拿结果写 Entity。

use agenthome_core::model::{
    ContentBlock, CostBreakdown, CostSource, DailyUsage, Freshness, Message, Period,
    ProviderActivity, ProviderHealth, ProviderId, QuotaSnapshot, QuotaWindow, Role,
    SEARCH_MARK_END, SEARCH_MARK_START, SearchHit, SessionKind, SessionQuery, SessionSummary,
    Usage, UsageTotals,
};

/// 左栏项目行：按 `SessionSummary.project_name` 聚合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    /// 用于 `SessionQuery.cwd` 过滤；None 时无法按项目过滤。
    pub cwd: Option<String>,
    pub session_count: u32,
}

pub trait DataSource: Send + Sync + 'static {
    fn projects(&self) -> anyhow::Result<Vec<ProjectRow>>;
    fn sessions(&self, q: &SessionQuery) -> anyhow::Result<Vec<SessionSummary>>;
    fn messages(&self, session_id: &str) -> anyhow::Result<Vec<Message>>;
    fn daily(&self, days: u32) -> anyhow::Result<Vec<DailyUsage>>;
    /// 周期汇总（provider 为 None 表示全部来源）。
    fn totals(&self, period: Period, provider: Option<ProviderId>) -> anyhow::Result<UsageTotals>;
    fn by_model(&self, period: Period) -> anyhow::Result<Vec<CostBreakdown>>;
    /// 同一项目按来源多行；limit 为项目数上限。
    fn by_project(&self, period: Period, limit: usize) -> anyhow::Result<Vec<CostBreakdown>>;
    fn archive_health(&self) -> anyhow::Result<Vec<ProviderHealth>>;
    /// 各来源最近活动（栖息地状态机的输入之一）。
    fn activity(&self) -> anyhow::Result<Vec<ProviderActivity>>;
    /// 额度快照（Claude statusline 文件、Codex rollout 快照），没有的来源不出现。
    fn quota(&self) -> anyhow::Result<Vec<QuotaSnapshot>>;
    /// 全文搜索会话标题与消息正文，按相关度至多 `limit` 条；片段里的命中词用
    /// `SEARCH_MARK_START` / `SEARCH_MARK_END` 包住。`message_id` 为 None 表示命中标题。
    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>>;
}

/// mock 搜索每个会话最多扫多少条消息（第一个会话有 1 万条，全扫没必要）。
const MOCK_SEARCH_MESSAGES: usize = 300;
/// mock 搜索每个会话最多给几条消息命中。
const MOCK_HITS_PER_SESSION: usize = 4;
/// mock 片段在命中词前后各保留的字数。
const MOCK_SNIPPET_CONTEXT: usize = 24;

/// 在 `text` 里找 `needle`（不分大小写），截出前后各一小段并用标记包住命中词；换行压成空格。
/// 与 core 的片段格式一致，供 mock 数据源使用。
fn mark_snippet(text: &str, needle: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let needle: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    // 小写化可能改变字数（极少见）；对不齐时放弃，宁可不命中也不切错位置。
    if needle.is_empty() || lower.len() != chars.len() {
        return None;
    }
    let start = lower
        .windows(needle.len())
        .position(|w| w == needle.as_slice())?;
    let end = start + needle.len();
    let from = start.saturating_sub(MOCK_SNIPPET_CONTEXT);
    let to = (end + MOCK_SNIPPET_CONTEXT).min(chars.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    let piece = |range: std::ops::Range<usize>| -> String {
        chars
            .get(range)
            .unwrap_or_default()
            .iter()
            .map(|c| if c.is_whitespace() { ' ' } else { *c })
            .collect()
    };
    out.push_str(&piece(from..start));
    out.push(SEARCH_MARK_START);
    out.push_str(&piece(start..end));
    out.push(SEARCH_MARK_END);
    out.push_str(&piece(end..to));
    if to < chars.len() {
        out.push('…');
    }
    Some(out)
}

/// 确定性伪随机（LCG），让每次启动的 mock 数据一致，便于复现界面问题。
struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

const PROJECTS: [&str; 8] = [
    "agenthome",
    "pixel-notes",
    "todo-desk",
    "metronome",
    "gpui-kit",
    "notes-sync",
    "infra-scripts",
    "blog",
];

/// 会话标题的翻译 key（`locales/mock.yml`）。mock 会话的 `title` 存 key，取数时按当前语言翻译，
/// 这样切换界面语言后仪表盘 / 归档重新取数就是新语言。
const TITLES: [&str; 24] = [
    "mock.title.0",
    "mock.title.1",
    "mock.title.2",
    "mock.title.3",
    "mock.title.4",
    "mock.title.5",
    "mock.title.6",
    "mock.title.7",
    "mock.title.8",
    "mock.title.9",
    "mock.title.10",
    "mock.title.11",
    "mock.title.12",
    "mock.title.13",
    "mock.title.14",
    "mock.title.15",
    "mock.title.16",
    "mock.title.17",
    "mock.title.18",
    "mock.title.19",
    "mock.title.20",
    "mock.title.21",
    "mock.title.22",
    "mock.title.23",
];

// 都要在内置价表里，否则演示数据的成本显示「未知」。
const MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "gpt-5.5",
    "gemini-3-pro-preview",
];

const USER_TEXTS: [&str; 5] = [
    "mock.user.0",
    "mock.user.1",
    "mock.user.2",
    "mock.user.3",
    "mock.user.4",
];

const ASSISTANT_MARKDOWN: [&str; 3] = ["mock.assistant.0", "mock.assistant.1", "mock.assistant.2"];

const SUBAGENT_TITLES: [&str; 3] = ["mock.subagent.0", "mock.subagent.1", "mock.subagent.2"];

/// 按指定语言翻译 mock 文案；显式传 locale，测试不用改全局语言。
fn tr(key: &str, locale: &str) -> String {
    t!(key, locale = locale).into_owned()
}

/// mock 会话的原料；`mock_summary` 据此补齐原件路径与会话级成本。
struct MockSession {
    id: String,
    provider: ProviderId,
    kind: SessionKind,
    title: String,
    cwd: String,
    project_name: &'static str,
    started_at_ms: i64,
    updated_at_ms: i64,
    message_count: u32,
    usage: Usage,
    models: Vec<String>,
    parent_session_id: Option<String>,
    /// Some(f)：有 agent 自记真值，取估算 × f；None：按定价表估算。
    recorded_factor: Option<f64>,
}

/// 像真实库那样补齐 `source_path` / `cost_usd` / `cost_source`。
fn mock_summary(m: MockSession) -> SessionSummary {
    let native = m.id.splitn(3, ':').nth(2).unwrap_or(&m.id).to_string();
    let source_path = match m.provider {
        ProviderId::ClaudeCode => {
            let dir = m.cwd.replace('/', "-");
            match native.split_once('/') {
                Some((parent, agent)) => {
                    format!("/Users/peter/.claude/projects/{dir}/{parent}/subagents/{agent}.jsonl")
                }
                None => format!("/Users/peter/.claude/projects/{dir}/{native}.jsonl"),
            }
        }
        ProviderId::Codex => {
            format!(
                "/Users/peter/.codex/sessions/2026/11/05/rollout-2026-11-05T09-00-00-{native}.jsonl"
            )
        }
        _ => format!("/Users/peter/.gemini/tmp/{native}/chats/session.json"),
    };
    let estimated = m
        .models
        .first()
        .and_then(|model| agenthome_core::pricing::estimate_usd(model, &m.usage));
    let (cost_usd, cost_source) = match (m.recorded_factor, estimated) {
        (Some(f), Some(c)) => (Some(c * f), CostSource::Recorded),
        (_, Some(c)) => (Some(c), CostSource::Estimated),
        (_, None) => (None, CostSource::Unknown),
    };
    SessionSummary {
        id: m.id,
        provider: m.provider,
        kind: m.kind,
        title: m.title,
        cwd: Some(m.cwd),
        project_name: m.project_name.to_string(),
        started_at_ms: m.started_at_ms,
        updated_at_ms: m.updated_at_ms,
        message_count: m.message_count,
        usage: m.usage,
        models: m.models,
        parent_session_id: m.parent_session_id,
        source_path: Some(source_path),
        cost_usd,
        cost_source,
    }
}

/// 内存 mock：8 个项目、约 60 个主会话，其中第一个会话有 10,000 条消息；
/// 部分 Claude 会话带子代理（有 parent id），另有两个找不到父会话的孤儿子代理。
pub struct MockDataSource {
    projects: Vec<ProjectRow>,
    sessions: Vec<SessionSummary>,
}

impl MockDataSource {
    pub fn new() -> Self {
        let now_ms = crate::format::now_ms();
        let mut rng = Lcg(20_261_105);
        let mut sessions = Vec::with_capacity(60);
        for i in 0..60usize {
            let project_ix = usize::try_from(rng.below(PROJECTS.len() as u64)).unwrap_or(0);
            let project_name = PROJECTS.get(project_ix).copied().unwrap_or("agenthome");
            let title_ix = i % TITLES.len();
            let title = TITLES.get(title_ix).copied().unwrap_or("mock.untitled");
            let age_ms = i64::try_from(rng.below(40 * 86_400_000)).unwrap_or(0) + 60_000;
            let updated_at_ms = now_ms - age_ms;
            let duration_ms = i64::try_from(rng.below(5 * 3_600_000)).unwrap_or(0) + 120_000;
            let message_count = if i == 0 {
                10_000
            } else {
                u32::try_from(rng.below(112)).unwrap_or(0) + 8
            };
            let provider = match rng.below(10) {
                0 => ProviderId::Codex,
                1 => ProviderId::GeminiCli,
                _ => ProviderId::ClaudeCode,
            };
            // 模型跟着来源走：Claude 会话里不该出现 gpt 模型。
            let models = match provider {
                ProviderId::Codex => vec![MODELS[2].to_string()],
                ProviderId::GeminiCli => vec![MODELS[3].to_string()],
                _ => {
                    let first = usize::try_from(rng.below(2)).unwrap_or(0);
                    let count = usize::try_from(rng.below(2)).unwrap_or(0) + 1;
                    (0..count)
                        .filter_map(|k| MODELS.get((first + k) % 2))
                        .map(|m| (*m).to_string())
                        .collect()
                }
            };
            let per_message = rng.below(4_000) + 500;
            let usage = Usage {
                input_tokens: per_message * u64::from(message_count) / 2,
                output_tokens: per_message * u64::from(message_count) / 8,
                cache_read_tokens: per_message * u64::from(message_count) / 3,
                cache_write_tokens: per_message * u64::from(message_count) / 20,
                requests: u64::from(message_count) / 2,
                ..Usage::default()
            };
            let id = format!("{}:mock-machine:{i:04}", provider.as_str());
            let cwd = format!("/Users/peter/code_hub/{project_name}");
            // 每三个 Claude 会话里有一个带 cost-state 真值（比估算略高），其余按定价表估算。
            let recorded = (provider == ProviderId::ClaudeCode && i % 3 == 0).then_some(1.08);
            let started_at_ms = updated_at_ms - duration_ms;
            sessions.push(mock_summary(MockSession {
                id: id.clone(),
                provider: provider.clone(),
                kind: SessionKind::Main,
                title: if i == 0 {
                    "mock.stress_title".to_string()
                } else {
                    title.to_string()
                },
                cwd: cwd.clone(),
                project_name,
                started_at_ms,
                updated_at_ms,
                message_count,
                usage,
                models,
                parent_session_id: None,
                recorded_factor: recorded,
            }));
            // 约四分之一的 Claude 会话派生 1~3 个子代理，带 parent id，时间落在父会话区间内。
            if provider == ProviderId::ClaudeCode && i % 4 == 1 {
                let count = rng.below(3) + 1;
                for k in 0..count {
                    let offset =
                        i64::try_from(rng.below(duration_ms.unsigned_abs() / 2)).unwrap_or(0);
                    let sub_start = started_at_ms + offset;
                    let sub_messages = u32::try_from(rng.below(24)).unwrap_or(0) + 4;
                    sessions.push(mock_summary(MockSession {
                        id: format!("{id}/agent-{k}"),
                        provider: provider.clone(),
                        kind: SessionKind::Subagent,
                        title: SUBAGENT_TITLES
                            .get(usize::try_from(k).unwrap_or(0) % SUBAGENT_TITLES.len())
                            .copied()
                            .unwrap_or("mock.subagent_fallback")
                            .to_string(),
                        cwd: cwd.clone(),
                        project_name,
                        started_at_ms: sub_start,
                        updated_at_ms: (sub_start + 300_000).min(updated_at_ms),
                        message_count: sub_messages,
                        usage: Usage {
                            input_tokens: per_message * u64::from(sub_messages) / 2,
                            output_tokens: per_message * u64::from(sub_messages) / 8,
                            cache_read_tokens: per_message * u64::from(sub_messages) / 3,
                            requests: u64::from(sub_messages) / 2,
                            ..Usage::default()
                        },
                        models: vec!["claude-sonnet-5-5".to_string()],
                        parent_session_id: Some(id.clone()),
                        recorded_factor: None,
                    }));
                }
            }
        }
        // 两个孤儿子代理：一个的父会话原件已被清理（parent id 指向不存在的会话），
        // 一个是旧版本没写 parent id、启发式也匹配不到。列表里必须仍能看到它们。
        let orphan_usage = Usage {
            input_tokens: 42_000,
            output_tokens: 6_100,
            cache_read_tokens: 180_000,
            requests: 9,
            ..Usage::default()
        };
        sessions.push(mock_summary(MockSession {
            id: "claude-code:mock-machine:0999/agent-0".to_string(),
            provider: ProviderId::ClaudeCode,
            kind: SessionKind::Subagent,
            title: "mock.subagent_codex_cost".to_string(),
            cwd: "/Users/peter/code_hub/agenthome".to_string(),
            project_name: "agenthome",
            started_at_ms: now_ms - 2 * 3_600_000,
            updated_at_ms: now_ms - 2 * 3_600_000 + 240_000,
            message_count: 18,
            usage: orphan_usage,
            models: vec!["claude-sonnet-5-5".to_string()],
            parent_session_id: Some("claude-code:mock-machine:0999".to_string()),
            recorded_factor: None,
        }));
        sessions.push(mock_summary(MockSession {
            id: "claude-code:mock-machine:legacy/agent-7".to_string(),
            provider: ProviderId::ClaudeCode,
            kind: SessionKind::Subagent,
            title: "mock.subagent_legacy_scripts".to_string(),
            cwd: "/Users/peter/code_hub/infra-scripts/legacy".to_string(),
            project_name: "legacy",
            started_at_ms: now_ms - 12 * 86_400_000,
            updated_at_ms: now_ms - 12 * 86_400_000 + 600_000,
            message_count: 11,
            usage: orphan_usage,
            models: vec!["claude-opus-5-5".to_string()],
            parent_session_id: None,
            recorded_factor: None,
        }));
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));

        let projects = PROJECTS
            .iter()
            .map(|name| ProjectRow {
                name: (*name).to_string(),
                cwd: Some(format!("/Users/peter/code_hub/{name}")),
                session_count: u32::try_from(
                    sessions.iter().filter(|s| s.project_name == *name).count(),
                )
                .unwrap_or(u32::MAX),
            })
            .collect();

        Self { projects, sessions }
    }

    // 下面三个按显式 locale 取数，`DataSource` 实现传当前界面语言；每次调用都重新翻译，
    // 切换语言后再取一次即为新语言。
    fn search_in(&self, query: &str, limit: usize, locale: &str) -> anyhow::Result<Vec<SearchHit>> {
        let needle = query.trim();
        let mut hits = Vec::new();
        if needle.is_empty() {
            return Ok(hits);
        }
        let mut sessions: Vec<SessionSummary> = self
            .sessions
            .iter()
            .map(|s| Self::localized(s, locale))
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));
        for session in &sessions {
            if hits.len() >= limit {
                break;
            }
            let hit = |message_id: Option<String>, role, timestamp_ms, snippet| SearchHit {
                session_id: session.id.clone(),
                message_id,
                session_title: session.title.clone(),
                provider: session.provider.clone(),
                project_name: session.project_name.to_string(),
                role,
                timestamp_ms,
                snippet,
            };
            if let Some(snippet) = mark_snippet(&session.title, needle) {
                hits.push(hit(None, None, session.updated_at_ms, snippet));
            }
            let mut found = 0;
            for message in Self::build_messages(session, locale)
                .into_iter()
                .take(MOCK_SEARCH_MESSAGES)
            {
                if found >= MOCK_HITS_PER_SESSION || hits.len() >= limit {
                    break;
                }
                if let Some(snippet) = mark_snippet(&message.text(), needle) {
                    found += 1;
                    hits.push(hit(
                        Some(message.id.clone()),
                        Some(message.role),
                        message.timestamp_ms,
                        snippet,
                    ));
                }
            }
        }
        hits.truncate(limit);
        Ok(hits)
    }

    fn sessions_in(&self, q: &SessionQuery, locale: &str) -> anyhow::Result<Vec<SessionSummary>> {
        let needle = q
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase);
        let rows = self
            .sessions
            .iter()
            .map(|s| Self::localized(s, locale))
            .filter(|s| q.cwd.as_ref().is_none_or(|cwd| s.cwd.as_ref() == Some(cwd)))
            .filter(|s| q.provider.as_ref().is_none_or(|p| &s.provider == p))
            .filter(|s| {
                needle.as_ref().is_none_or(|n| {
                    s.title.to_lowercase().contains(n) || s.project_name.to_lowercase().contains(n)
                })
            })
            .skip(q.offset)
            // limit 为 0 时视为不限制。
            .take(if q.limit == 0 { usize::MAX } else { q.limit })
            .collect();
        Ok(rows)
    }

    fn messages_in(&self, session_id: &str, locale: &str) -> anyhow::Result<Vec<Message>> {
        let summary = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{}",
                    t!("mock.session_not_found", locale = locale, id = session_id)
                )
            })?;
        Ok(Self::build_messages(summary, locale))
    }

    /// 把存成 key 的标题翻成 `locale` 下的文案。
    fn localized(summary: &SessionSummary, locale: &str) -> SessionSummary {
        SessionSummary {
            title: tr(&summary.title, locale),
            ..summary.clone()
        }
    }

    fn build_messages(summary: &SessionSummary, locale: &str) -> Vec<Message> {
        let mut rng = Lcg(summary.updated_at_ms.unsigned_abs());
        let count = usize::try_from(summary.message_count).unwrap_or(0);
        let step_ms = if count > 1 {
            (summary.updated_at_ms - summary.started_at_ms) / count as i64
        } else {
            0
        };
        let model = summary
            .models
            .first()
            .cloned()
            .unwrap_or_else(|| "claude-sonnet-5-5".to_string());
        let mut messages = Vec::with_capacity(count);
        for k in 0..count {
            let id = format!("{}:m{k:05}", summary.id);
            let timestamp_ms = summary.started_at_ms + step_ms * k as i64;
            let pick = |items: &[&str], rng: &mut Lcg| -> String {
                let ix = usize::try_from(rng.below(items.len() as u64)).unwrap_or(0);
                items.get(ix).map(|key| tr(key, locale)).unwrap_or_default()
            };
            let (role, blocks, usage) = match k % 6 {
                0 => (
                    Role::User,
                    vec![ContentBlock::Text {
                        text: t!(
                            "mock.turn",
                            locale = locale,
                            n = k / 6 + 1,
                            text = pick(&USER_TEXTS, &mut rng)
                        )
                        .into_owned(),
                    }],
                    None,
                ),
                1 => (
                    Role::Assistant,
                    vec![ContentBlock::Text {
                        text: pick(&ASSISTANT_MARKDOWN, &mut rng),
                    }],
                    Some(Usage {
                        input_tokens: rng.below(3_000) + 200,
                        output_tokens: rng.below(800) + 50,
                        cache_read_tokens: rng.below(2_000),
                        requests: 1,
                        ..Usage::default()
                    }),
                ),
                2 => {
                    let tool = pick(&["Read", "Bash", "Edit", "Grep"], &mut rng);
                    let input = match tool.as_str() {
                        "Read" => {
                            serde_json::json!({ "file_path": "/Users/peter/code_hub/agenthome/crates/app/src/main.rs" })
                        }
                        "Bash" => {
                            serde_json::json!({ "command": "cargo clippy -p agenthome-app", "description": tr("mock.bash_description", locale) })
                        }
                        "Edit" => {
                            serde_json::json!({ "file_path": "crates/app/src/data.rs", "old_string": "items[0]", "new_string": "items.first()" })
                        }
                        _ => {
                            serde_json::json!({ "pattern": "fn render", "path": "crates/app/src" })
                        }
                    };
                    (
                        Role::Assistant,
                        vec![
                            ContentBlock::Text {
                                text: tr("mock.look_first", locale),
                            },
                            ContentBlock::ToolCall {
                                tool_call_id: format!("call_{k:05}"),
                                name: tool,
                                input,
                            },
                        ],
                        Some(Usage {
                            input_tokens: rng.below(2_000) + 100,
                            output_tokens: rng.below(200) + 20,
                            requests: 1,
                            ..Usage::default()
                        }),
                    )
                }
                3 => (
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_call_id: format!("call_{:05}", k - 1),
                        output: format!(
                            "warning: unused variable: `ix`\n  --> crates/app/src/data.rs:{}:9\n   |\n   = note: `#[warn(unused_variables)]` on by default\n\n{}",
                            rng.below(300) + 1,
                            tr("mock.tool_output_tail", locale)
                        ),
                        is_error: rng.below(7) == 0,
                    }],
                    None,
                ),
                4 => (
                    Role::Assistant,
                    vec![
                        ContentBlock::Thinking {
                            text: tr("mock.thinking", locale),
                        },
                        ContentBlock::Text {
                            text: tr("mock.keep_signature", locale),
                        },
                    ],
                    Some(Usage {
                        input_tokens: rng.below(2_500) + 100,
                        output_tokens: rng.below(600) + 30,
                        reasoning_tokens: rng.below(900),
                        requests: 1,
                        ..Usage::default()
                    }),
                ),
                _ => (
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: tr("mock.follow_up", locale),
                        },
                        ContentBlock::Unknown {
                            native_type: "server_tool_use".to_string(),
                            raw: serde_json::json!({ "name": "web_search", "query": "gpui list virtualization" }),
                        },
                    ],
                    Some(Usage {
                        input_tokens: rng.below(1_500) + 100,
                        output_tokens: rng.below(300) + 20,
                        requests: 1,
                        ..Usage::default()
                    }),
                ),
            };
            messages.push(Message {
                id,
                session_id: summary.id.clone(),
                native_id: None,
                parent_id: None,
                role,
                is_meta: false,
                timestamp_ms,
                model: matches!(role, Role::Assistant).then(|| model.clone()),
                stop_reason: None,
                blocks,
                usage,
            });
        }
        messages
    }
}

impl Default for MockDataSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSource for MockDataSource {
    fn projects(&self) -> anyhow::Result<Vec<ProjectRow>> {
        Ok(self.projects.clone())
    }

    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        self.search_in(query, limit, &rust_i18n::locale())
    }

    fn sessions(&self, q: &SessionQuery) -> anyhow::Result<Vec<SessionSummary>> {
        self.sessions_in(q, &rust_i18n::locale())
    }

    fn messages(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        self.messages_in(session_id, &rust_i18n::locale())
    }

    fn daily(&self, days: u32) -> anyhow::Result<Vec<DailyUsage>> {
        let now_ms = crate::format::now_ms();
        let mut rng = Lcg(7);
        let rows = (0..days)
            .flat_map(|d| {
                let day = crate::format::absolute_date(now_ms - i64::from(d) * 86_400_000);
                let input_tokens = rng.below(200_000);
                let output_tokens = rng.below(40_000);
                let message_count = rng.below(400);
                [
                    DailyUsage {
                        day: day.clone(),
                        provider: ProviderId::ClaudeCode,
                        model: "claude-sonnet-5-5".to_string(),
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                            requests: message_count / 2,
                            ..Usage::default()
                        },
                        message_count,
                        cost_usd: 0.0,
                    },
                    DailyUsage {
                        day,
                        provider: ProviderId::Codex,
                        model: "gpt-5.5".to_string(),
                        usage: Usage {
                            input_tokens: input_tokens / 3,
                            output_tokens: output_tokens / 3,
                            requests: message_count / 6,
                            input_includes_cache: true,
                            ..Usage::default()
                        },
                        message_count: message_count / 3,
                        cost_usd: 0.0,
                    },
                ]
            })
            .collect();
        Ok(rows)
    }
    fn totals(&self, period: Period, provider: Option<ProviderId>) -> anyhow::Result<UsageTotals> {
        let scale = match period {
            Period::Today => 1.0,
            Period::Week => 5.0,
            Period::Month => 22.0,
            Period::Days(n) => n as f64,
            Period::All => 180.0,
        };
        let share = match provider {
            Some(ProviderId::Codex) => 0.12,
            Some(ProviderId::GeminiCli) => 0.0,
            Some(_) => 0.88,
            None => 1.0,
        };
        let k = scale * share;
        Ok(UsageTotals {
            period,
            provider,
            usage: Usage {
                input_tokens: (84_000.0 * k) as u64,
                output_tokens: (182_000.0 * k) as u64,
                cache_read_tokens: (14_200_000.0 * k) as u64,
                cache_write_tokens: (860_000.0 * k) as u64,
                cache_write_5m_tokens: (736_000.0 * k) as u64,
                cache_write_1h_tokens: (124_000.0 * k) as u64,
                reasoning_tokens: (21_000.0 * k) as u64,
                requests: (420.0 * k) as u64,
                input_includes_cache: false,
            },
            cost_usd: 67.4 * k,
            cost_source: CostSource::Mixed,
            sessions: (19.0 * k) as u64,
            messages: (2_190.0 * k) as u64,
        })
    }

    fn by_model(&self, period: Period) -> anyhow::Result<Vec<CostBreakdown>> {
        let k = self.totals(period, None)?.cost_usd / 67.4;
        let rows = [
            ("claude-opus-5-5", ProviderId::ClaudeCode, 0.58),
            ("claude-fable-5-1", ProviderId::ClaudeCode, 0.24),
            ("claude-sonnet-5-5", ProviderId::ClaudeCode, 0.07),
            ("gpt-5.5", ProviderId::Codex, 0.09),
            ("gpt-5-mini", ProviderId::Codex, 0.02),
        ];
        Ok(rows
            .iter()
            .map(|(m, p, share)| CostBreakdown {
                key: (*m).to_string(),
                provider: p.clone(),
                usage: Usage {
                    output_tokens: (182_000.0 * k * share) as u64,
                    ..Usage::default()
                },
                cost_usd: 67.4 * k * share,
            })
            .collect())
    }

    fn by_project(&self, period: Period, limit: usize) -> anyhow::Result<Vec<CostBreakdown>> {
        let k = self.totals(period, None)?.cost_usd / 67.4;
        let mut out = Vec::new();
        for (i, p) in PROJECTS.iter().take(limit).enumerate() {
            let share = 0.42 / (i as f64 + 1.0);
            out.push(CostBreakdown {
                key: (*p).to_string(),
                provider: ProviderId::ClaudeCode,
                usage: Usage::default(),
                cost_usd: 67.4 * k * share * 0.88,
            });
            out.push(CostBreakdown {
                key: (*p).to_string(),
                provider: ProviderId::Codex,
                usage: Usage::default(),
                cost_usd: 67.4 * k * share * 0.12,
            });
        }
        Ok(out)
    }

    fn archive_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        Ok(vec![
            ProviderHealth {
                provider: ProviderId::ClaudeCode,
                sessions: 71,
                archive_only: 6,
                expiring_within_7d: 3,
                compressed: 0,
            },
            ProviderHealth {
                provider: ProviderId::Codex,
                sessions: 12,
                archive_only: 0,
                expiring_within_7d: 0,
                compressed: 4,
            },
        ])
    }

    fn activity(&self) -> anyhow::Result<Vec<ProviderActivity>> {
        let now = crate::format::now_ms();
        Ok(vec![
            ProviderActivity {
                provider: ProviderId::ClaudeCode,
                detected: true,
                last_activity_ms: Some(now - 12_000),
                last_kind: Some(agenthome_core::model::ActivityKind::ToolCall),
                active_sessions: 2,
            },
            ProviderActivity {
                provider: ProviderId::Codex,
                detected: true,
                last_activity_ms: Some(now - 4 * 60_000),
                last_kind: Some(agenthome_core::model::ActivityKind::AssistantText),
                active_sessions: 1,
            },
            ProviderActivity {
                provider: ProviderId::GeminiCli,
                detected: false,
                last_activity_ms: None,
                last_kind: None,
                active_sessions: 0,
            },
        ])
    }

    fn quota(&self) -> anyhow::Result<Vec<QuotaSnapshot>> {
        let now = crate::format::now_ms();
        let locale = rust_i18n::locale();
        Ok(vec![
            QuotaSnapshot {
                provider: ProviderId::ClaudeCode,
                plan: Some("Max".to_string()),
                windows: vec![
                    QuotaWindow {
                        id: "five_hour".to_string(),
                        label: tr("mock.quota_5h", &locale),
                        used_percent: 42.0,
                        resets_at_ms: Some(now + 2 * 3_600_000 + 10 * 60_000),
                        window_minutes: Some(300),
                    },
                    QuotaWindow {
                        id: "seven_day".to_string(),
                        label: tr("mock.quota_7d", &locale),
                        used_percent: 78.0,
                        resets_at_ms: Some(now + 2 * 86_400_000 + 17 * 3_600_000),
                        window_minutes: Some(10_080),
                    },
                ],
                credits: None,
                fetched_at_ms: now - 3 * 60_000,
                source: "claude /usage".to_string(),
                freshness: Freshness::Live,
            },
            QuotaSnapshot {
                provider: ProviderId::Codex,
                plan: Some("Plus".to_string()),
                windows: vec![
                    QuotaWindow {
                        id: "primary".to_string(),
                        label: tr("mock.quota_5h", &locale),
                        used_percent: 18.0,
                        resets_at_ms: Some(now + 3 * 3_600_000 + 41 * 60_000),
                        window_minutes: Some(300),
                    },
                    QuotaWindow {
                        id: "secondary".to_string(),
                        label: tr("mock.quota_weekly", &locale),
                        used_percent: 37.0,
                        resets_at_ms: Some(now + 4 * 86_400_000 + 2 * 3_600_000),
                        window_minutes: Some(10_080),
                    },
                ],
                credits: Some(agenthome_core::model::Credits {
                    balance: Some("3".to_string()),
                    unlimited: false,
                    has_credits: true,
                    reset_credits: Some(3),
                    expires_at_ms: Some(now + 6 * 86_400_000 + 14 * 3_600_000),
                }),
                fetched_at_ms: now - 11 * 60_000,
                source: "rollout".to_string(),
                freshness: Freshness::Cached,
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_snippet_marks_the_match_like_core() {
        // mock 的片段格式要和 core 一致（命中词被私用区标记包住），面板的高亮才能在演示数据上验证。
        let snippet = mark_snippet("修复登录页在 iOS 27 上的启动崩溃", "ios").unwrap();
        assert_eq!(
            snippet,
            format!("修复登录页在 {SEARCH_MARK_START}iOS{SEARCH_MARK_END} 27 上的启动崩溃")
        );
        assert_eq!(mark_snippet("没有关键字", "panic"), None);
    }

    #[test]
    fn mock_search_finds_titles_and_messages() {
        let source = MockDataSource::new();
        let hits = source.search_in("panic", 20, "en").unwrap();
        assert!(!hits.is_empty() && hits.len() <= 20);
        assert!(hits.iter().all(|h| h.message_id.is_some()));
        // 消息命中多，标题命中的会话可能排在 50 条之后；放宽上限只为确认标题命中会被返回。
        let titles = source.search_in("SQLite", 500, "en").unwrap();
        assert!(titles.iter().any(|h| h.message_id.is_none()));
        assert!(source.search_in("  ", 20, "en").unwrap().is_empty());
    }

    fn has_cjk(text: &str) -> bool {
        text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    /// 演示数据要跟着界面语言走：README / 落地页的英文截图里不能冒出中文，
    /// 中文界面也不能露出没翻译的 key。显式传 locale，不碰全局语言。
    #[test]
    fn mock_text_follows_the_requested_locale() {
        let source = MockDataSource::new();
        let all = SessionQuery {
            limit: 0,
            ..SessionQuery::default()
        };
        for locale in ["en", "zh-CN"] {
            let sessions = source.sessions_in(&all, locale).unwrap();
            assert!(sessions.len() > 60);
            for session in &sessions {
                assert!(
                    !session.title.starts_with("mock."),
                    "{locale}: {}",
                    session.title
                );
                assert_eq!(
                    has_cjk(&session.title),
                    locale == "zh-CN",
                    "{}",
                    session.title
                );
            }
            let first = sessions.iter().find(|s| s.message_count < 100).unwrap();
            for message in source.messages_in(&first.id, locale).unwrap() {
                // 序列化整条消息的块，正文、思考、工具输入与输出一并检查。
                let text = serde_json::to_string(&message.blocks).unwrap();
                assert!(!text.contains("mock."), "{locale}: {text}");
                if locale == "en" {
                    assert!(!has_cjk(&text), "{text}");
                }
            }
        }
        // 切换语言后再取一次就是新语言（仪表盘 / 归档靠重新调用拿新文案）。
        let id = &source.sessions.first().unwrap().id;
        let pick = |locale| {
            source
                .sessions_in(&all, locale)
                .unwrap()
                .into_iter()
                .find(|s| &s.id == id)
                .unwrap()
                .title
        };
        assert_ne!(pick("en"), pick("zh-CN"));
    }
}
