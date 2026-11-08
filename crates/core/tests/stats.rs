//! 仪表盘查询：周期边界、成本口径（真值 / 估算 / 混合）、按模型 / 项目、活动推断。

use agenthome_core::Store;
use agenthome_core::model::{
    ActivityKind, ContentBlock, CostSource, Message, Period, ProviderId, Role, Session,
    SessionKind, SourceInfo, Usage,
};
use agenthome_core::providers::{Discovery, Parsed, SourceProvider};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn session(id: &str, provider: ProviderId, cwd: &str, ts: i64, recorded: Option<f64>) -> Session {
    Session {
        id: id.to_string(),
        provider,
        native_id: id.to_string(),
        parent_session_id: None,
        kind: SessionKind::Main,
        title: Some(format!("t-{id}")),
        cwd: Some(cwd.to_string()),
        git_branch: None,
        machine_id: "m".to_string(),
        agent_version: None,
        entrypoint: None,
        started_at_ms: ts,
        ended_at_ms: Some(ts),
        updated_at_ms: ts,
        models: vec![],
        usage: Usage::default(),
        message_count: 0,
        recorded_cost_usd: recorded,
        estimated_cost_usd: None,
        source: SourceInfo {
            path: std::path::PathBuf::from(format!("/nonexistent/{id}.jsonl")),
            size_bytes: 1,
            mtime_ms: ts,
            compressed: false,
        },
    }
}

fn msg(
    sid: &str,
    id: &str,
    role: Role,
    ts: i64,
    model: Option<&str>,
    usage: Option<Usage>,
    blocks: Vec<ContentBlock>,
) -> Message {
    Message {
        id: id.to_string(),
        session_id: sid.to_string(),
        native_id: None,
        parent_id: None,
        role,
        is_meta: false,
        timestamp_ms: ts,
        model: model.map(str::to_string),
        stop_reason: None,
        blocks,
        usage,
    }
}

fn text(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: t.to_string(),
    }]
}

/// 1M input 的 opus-5-5 = $4；1M input 的 gpt-5 = $1.25。
fn one_m_input(includes_cache: bool) -> Usage {
    Usage {
        input_tokens: 1_000_000,
        requests: 1,
        input_includes_cache: includes_cache,
        ..Usage::default()
    }
}

fn seed(store: &mut Store) {
    let now = now_ms();
    let day = 86_400_000;
    // Claude 会话 A：有真值 $9，消息估算 $4（今天）
    let mut a = session(
        "claude-code:m:a",
        ProviderId::ClaudeCode,
        "/w/proj-x",
        now - 60_000,
        Some(9.0),
    );
    let ma = vec![
        msg(
            &a.id,
            "a1",
            Role::User,
            now - 120_000,
            None,
            None,
            text("hi"),
        ),
        msg(
            &a.id,
            "a2",
            Role::Assistant,
            now - 60_000,
            Some("claude-opus-5-5"),
            Some(one_m_input(false)),
            vec![
                ContentBlock::Text { text: "ok".into() },
                ContentBlock::ToolCall {
                    tool_call_id: "t".into(),
                    name: "Bash".into(),
                    input: serde_json::Value::Null,
                },
            ],
        ),
    ];
    a.estimated_cost_usd = Some(4.0);
    a.message_count = 2;
    store.store_parsed(&a, &ma).unwrap();
    // Claude 会话 B：无真值，消息估算 $4（40 天前）
    let mut b = session(
        "claude-code:m:b",
        ProviderId::ClaudeCode,
        "/w/proj-y",
        now - 40 * day,
        None,
    );
    let mb = vec![msg(
        &b.id,
        "b1",
        Role::Assistant,
        now - 40 * day,
        Some("claude-opus-5-5"),
        Some(one_m_input(false)),
        text("old"),
    )];
    b.estimated_cost_usd = Some(4.0);
    store.store_parsed(&b, &mb).unwrap();
    // Codex 会话 C：$1.25（3 天前），原件为 zst
    let mut c = session(
        "codex:m:c",
        ProviderId::Codex,
        "/w/proj-x",
        now - 3 * day,
        None,
    );
    c.source.compressed = true;
    c.usage.input_includes_cache = true;
    c.estimated_cost_usd = Some(1.25);
    let mc = vec![
        msg(
            &c.id,
            "c1",
            Role::Assistant,
            now - 3 * day,
            Some("gpt-5"),
            Some(one_m_input(true)),
            text("codex"),
        ),
        msg(
            &c.id,
            "c2",
            Role::Tool,
            now - 3 * day + 1,
            None,
            None,
            vec![ContentBlock::ToolResult {
                tool_call_id: "x".into(),
                output: "o".into(),
                is_error: false,
            }],
        ),
    ];
    store.store_parsed(&c, &mc).unwrap();
    // 未知模型：有 usage 但无法计价
    let d = session("codex:m:d", ProviderId::Codex, "", now - 2 * day, None);
    let md = vec![msg(
        &d.id,
        "d1",
        Role::Assistant,
        now - 2 * day,
        Some("mystery-9"),
        Some(one_m_input(true)),
        text("?"),
    )];
    store.store_parsed(&d, &md).unwrap();
}

#[test]
fn 周期起点按本地时区() {
    let now = chrono::Local::now();
    let today = Period::Today.since_ms().unwrap();
    let start = chrono::DateTime::from_timestamp_millis(today)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(start.date_naive(), now.date_naive());
    assert_eq!(start.format("%H:%M:%S").to_string(), "00:00:00");
    let week = Period::Week.since_ms().unwrap();
    let ws = chrono::DateTime::from_timestamp_millis(week)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(ws.format("%u").to_string(), "1", "周一");
    assert!(week <= today);
    let month = Period::Month.since_ms().unwrap();
    let ms = chrono::DateTime::from_timestamp_millis(month)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(ms.format("%d").to_string(), "01");
    assert!(Period::Days(7).since_ms().unwrap() < today);
    assert_eq!(Period::All.since_ms(), None);
    assert_eq!(Period::Days(3).as_str(), "3d");
}

#[test]
fn 汇总_成本口径() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store);

    // All：A 用真值 9，B 估算 4，C 估算 1.25，D 无 → 14.25，混合
    let all = store.stats_totals(Period::All, None).unwrap();
    assert_eq!(all.sessions, 4);
    assert_eq!(all.messages, 6);
    assert_eq!(all.usage.requests, 4);
    assert!(all.usage.input_includes_cache, "混了 Codex 要标出来");
    assert!((all.cost_usd - 14.25).abs() < 1e-9, "{}", all.cost_usd);
    assert_eq!(all.cost_source, CostSource::Mixed);

    // 只看 Claude：9 + 4 = 13，混合；只看 Codex：1.25，估算
    let cl = store
        .stats_totals(Period::All, Some(&ProviderId::ClaudeCode))
        .unwrap();
    assert!((cl.cost_usd - 13.0).abs() < 1e-9);
    assert_eq!(cl.cost_source, CostSource::Mixed);
    assert!(!cl.usage.input_includes_cache);
    let cx = store
        .stats_totals(Period::All, Some(&ProviderId::Codex))
        .unwrap();
    assert!((cx.cost_usd - 1.25).abs() < 1e-9);
    assert_eq!(cx.cost_source, CostSource::Estimated);

    // 7 天：A(4) + C(1.25)，按消息估算；B 在 40 天前不计
    let week = store.stats_totals(Period::Days(7), None).unwrap();
    assert_eq!(week.sessions, 3);
    assert!((week.cost_usd - 5.25).abs() < 1e-9, "{}", week.cost_usd);
    assert_eq!(week.cost_source, CostSource::Estimated);
    assert_eq!(week.usage.input_tokens, 3_000_000);

    // 今天：只有 A
    let today = store.stats_totals(Period::Today, None).unwrap();
    assert_eq!(today.sessions, 1);
    assert!((today.cost_usd - 4.0).abs() < 1e-9);

    // 空周期
    let none = store
        .stats_totals(Period::Days(0), Some(&ProviderId::Other("x".into())))
        .unwrap();
    assert_eq!(none.messages, 0);
    assert_eq!(none.cost_source, CostSource::Unknown);
}

#[test]
fn 按模型_按项目_每日() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store);
    let by_model = store.stats_by_model(Period::All).unwrap();
    let keys: Vec<(String, f64)> = by_model
        .iter()
        .map(|b| (b.key.clone(), b.cost_usd))
        .collect();
    assert_eq!(keys[0].0, "claude-opus-5-5");
    assert!((keys[0].1 - 8.0).abs() < 1e-9, "两条消息各 $4");
    assert_eq!(by_model[0].usage.requests, 2);
    assert!(
        keys.iter()
            .any(|(k, c)| k == "gpt-5" && (c - 1.25).abs() < 1e-9)
    );
    assert!(
        keys.iter().any(|(k, c)| k == "mystery-9" && *c == 0.0),
        "未知模型成本 0 但 token 仍列出"
    );

    let by_project = store.stats_by_project(Period::All, 2).unwrap();
    assert_eq!(by_project.len(), 2);
    assert_eq!(by_project[0].key, "proj-x");
    assert_eq!(by_project[0].provider, ProviderId::ClaudeCode);
    assert!((by_project[0].cost_usd - 4.0).abs() < 1e-9);
    // 同一项目不同来源各一行
    let all_projects = store.stats_by_project(Period::All, 0).unwrap();
    assert!(
        all_projects
            .iter()
            .any(|b| b.key == "proj-x" && b.provider == ProviderId::Codex)
    );

    let daily = store.stats_daily(7).unwrap();
    let today = daily
        .iter()
        .find(|d| d.provider == ProviderId::ClaudeCode)
        .unwrap();
    assert!((today.cost_usd - 4.0).abs() < 1e-9);
    assert_eq!(today.message_count, 1);
    let codex_day = daily.iter().find(|d| d.model == "gpt-5").unwrap();
    assert!((codex_day.cost_usd - 1.25).abs() < 1e-9);
}

struct FakeProvider(ProviderId, bool);
impl SourceProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.0.clone()
    }
    fn detect(&self) -> bool {
        self.1
    }
    fn discover(&self) -> Discovery {
        Discovery::default()
    }
    fn parse_file(&self, _: &std::path::Path) -> anyhow::Result<Parsed> {
        unreachable!()
    }
}

#[test]
fn 归档健康与活动() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store);
    let health = store.archive_health().unwrap();
    let claude = health
        .iter()
        .find(|h| h.provider == ProviderId::ClaudeCode)
        .unwrap();
    assert_eq!(claude.sessions, 2);
    assert_eq!(claude.archive_only, 2, "原件路径都不存在");
    assert_eq!(claude.expiring_within_7d, 0, "不存在的原件不算将到期");
    let codex = health
        .iter()
        .find(|h| h.provider == ProviderId::Codex)
        .unwrap();
    assert_eq!(codex.compressed, 1);

    let cl = FakeProvider(ProviderId::ClaudeCode, true);
    let cx = FakeProvider(ProviderId::Codex, false);
    let list: [&dyn SourceProvider; 2] = [&cl, &cx];
    let act = store.activity(&list).unwrap();
    let a = act
        .iter()
        .find(|a| a.provider == ProviderId::ClaudeCode)
        .unwrap();
    assert!(a.detected);
    assert_eq!(
        a.last_kind,
        Some(ActivityKind::ToolCall),
        "最后一个块是 tool_use"
    );
    assert_eq!(a.active_sessions, 1, "A 在 10 分钟内更新过");
    let c = act
        .iter()
        .find(|a| a.provider == ProviderId::Codex)
        .unwrap();
    assert!(!c.detected);
    assert_eq!(
        c.last_kind,
        Some(ActivityKind::AssistantText),
        "最新会话 D 的最后一条是文本回复"
    );
    assert_eq!(c.active_sessions, 0);
}

#[test]
fn 到期判断看原件_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::in_memory().unwrap();
    let now = now_ms();
    for (id, age_days) in [("old", 25), ("fresh", 1)] {
        let p = dir.path().join(format!("{id}.jsonl"));
        std::fs::write(&p, "{}").unwrap();
        let mut s = session(
            &format!("claude-code:m:{id}"),
            ProviderId::ClaudeCode,
            "/w",
            now,
            None,
        );
        s.source.path = p;
        s.source.mtime_ms = now - age_days * 86_400_000;
        store.store_parsed(&s, &[]).unwrap();
    }
    let h = store.archive_health().unwrap();
    assert_eq!(h[0].sessions, 2);
    assert_eq!(h[0].archive_only, 0);
    assert_eq!(h[0].expiring_within_7d, 1);
}

/// 10.15 / 10.21 / 10.35：列表摘要要带父会话、原件路径与会话级成本。
/// 成本口径与仪表盘 `All` 一致：真值优先（即使估算不同也不混用），无真值用估算，都没有就是 None
/// ——列表不能因为"没价"而显示 $0，也不能把 Claude 真值与估算混成一个数。
#[test]
fn 列表摘要_父会话_原件路径_会话级成本() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store);
    let now = now_ms();
    let mut sub = session(
        "claude-code:m:a/agent-1",
        ProviderId::ClaudeCode,
        "/w/proj-x",
        now - 90_000,
        None,
    );
    sub.kind = SessionKind::Subagent;
    sub.parent_session_id = Some("claude-code:m:a".to_string());
    store.store_parsed(&sub, &[]).unwrap();

    let list = store
        .list_sessions(&agenthome_core::model::SessionQuery::default())
        .unwrap();
    let get = |id: &str| list.iter().find(|s| s.id == id).unwrap().clone();

    let a = get("claude-code:m:a");
    assert_eq!(
        a.cost_usd,
        Some(9.0),
        "有 cost-state 真值时用真值，不用估算 $4"
    );
    assert_eq!(a.cost_source, CostSource::Recorded);
    assert_eq!(a.parent_session_id, None);
    assert_eq!(
        a.source_path.as_deref(),
        Some("/nonexistent/claude-code:m:a.jsonl")
    );

    let b = get("claude-code:m:b");
    assert_eq!(b.cost_usd, Some(4.0));
    assert_eq!(b.cost_source, CostSource::Estimated);

    let d = get("codex:m:d");
    assert_eq!(d.cost_usd, None, "未知模型不可计价，不能当成 $0");
    assert_eq!(d.cost_source, CostSource::Unknown);

    let s = get("claude-code:m:a/agent-1");
    assert_eq!(s.kind, SessionKind::Subagent);
    assert_eq!(s.parent_session_id.as_deref(), Some("claude-code:m:a"));
}
