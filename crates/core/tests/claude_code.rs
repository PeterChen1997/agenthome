//! Claude Code provider + Store + ingest 的集成测试。fixture 全部为合成数据。

use agenthome_core::model::{ContentBlock, Role, SessionKind, SessionQuery};
use agenthome_core::{ClaudeCodeProvider, Store, ingest_claude};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
}

fn main_file() -> PathBuf {
    fixture_root().join("-Users-tester-code-demo/11111111-aaaa-4bbb-8ccc-000000000001.jsonl")
}

fn provider() -> ClaudeCodeProvider {
    ClaudeCodeProvider::new(fixture_root())
}

#[test]
fn discover_finds_main_and_subagent_and_counts_superseded() {
    let d = provider().discover();
    let names: Vec<String> = d
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "agent-deadbeef01.jsonl",
            "11111111-aaaa-4bbb-8ccc-000000000001.jsonl",
            "22222222-aaaa-4bbb-8ccc-000000000002.jsonl",
        ]
    );
    assert_eq!(d.skipped_superseded, 1);
}

#[test]
fn merges_assistant_rows_and_dedups_usage() {
    let p = provider();
    let parsed = p.parse_file(&main_file()).unwrap();
    let s = &parsed.session;
    assert_eq!(s.kind, SessionKind::Main);
    assert_eq!(s.native_id, "11111111-aaaa-4bbb-8ccc-000000000001");
    assert_eq!(
        s.id,
        format!("claude-code:{}:{}", p.machine_id(), s.native_id)
    );
    assert_eq!(s.cwd.as_deref(), Some("/Users/tester/code/demo"));
    assert_eq!(s.git_branch.as_deref(), Some("main"));
    assert_eq!(s.agent_version.as_deref(), Some("2.1.300"));
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert_eq!(s.models, vec!["claude-test-model", "claude-test-model-2"]);

    // 消息：meta user、user、合并后的 assistant、tool_result、assistant、compact 系统行 = 6
    let roles: Vec<Role> = parsed.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::System
        ]
    );
    assert!(parsed.messages[0].is_meta);

    let merged = &parsed.messages[2];
    assert_eq!(merged.native_id.as_deref(), Some("msg_01"));
    assert_eq!(merged.blocks.len(), 3);
    assert!(matches!(merged.blocks[0], ContentBlock::Thinking { .. }));
    assert!(matches!(merged.blocks[1], ContentBlock::Text { .. }));
    assert!(matches!(&merged.blocks[2], ContentBlock::ToolCall { name, .. } if name == "Bash"));
    assert_eq!(merged.stop_reason.as_deref(), Some("tool_use"));
    // usage 只取带 stop_reason 的最后一条，不是三条相加
    let u = merged.usage.unwrap();
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 20);
    assert_eq!(u.cache_write_tokens, 100);
    assert_eq!(u.cache_read_tokens, 200);
    assert_eq!(u.cache_write_5m_tokens, 60);
    assert_eq!(u.cache_write_1h_tokens, 40);
    assert_eq!(u.reasoning_tokens, 7);

    // 会话 usage = 两条 assistant 之和
    assert_eq!(s.usage.input_tokens, 40);
    assert_eq!(s.usage.output_tokens, 28);
    assert_eq!(s.usage.cache_read_tokens, 500);
    assert_eq!(s.usage.requests, 2);
    assert!(!s.usage.input_includes_cache);
    assert_eq!(s.message_count, 6);

    // tool_result 正文被展开
    assert!(matches!(&parsed.messages[3].blocks[0],
        ContentBlock::ToolResult { output, is_error: false, .. } if output.contains("3 passed")));
    assert!(matches!(
        parsed.messages[5].blocks[0],
        ContentBlock::Compaction { .. }
    ));

    // 时间戳 ISO -> ms
    assert_eq!(parsed.messages[1].timestamp_ms, 1_767_323_045_000);
    assert_eq!(s.started_at_ms, 1_767_323_044_000);
    assert_eq!(s.updated_at_ms, 1_767_323_051_000);
}

#[test]
fn broken_line_goes_to_warnings_and_non_conversation_records_are_counted() {
    let parsed = provider().parse_file(&main_file()).unwrap();
    assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
    assert!(parsed.warnings[0].contains("第 10 行"));
    assert_eq!(parsed.ignored_records.get("queue-operation"), Some(&1));
    assert_eq!(parsed.ignored_records.get("attachment"), Some(&1));
    // cost-state 不再是「忽略」：它的 totalCostUSD 进 recorded_cost_usd
    assert_eq!(parsed.ignored_records.get("cost-state"), None);
    assert_eq!(parsed.session.recorded_cost_usd, Some(0.01));
    // fixture 用的是假模型名，定价表里没有：估算必须是 None，不能静默按 0
    assert_eq!(parsed.session.estimated_cost_usd, None);
    // resume 重写进来的同 uuid 行被去重，不会多出一条消息
    assert_eq!(parsed.ignored_records.get("duplicate-uuid"), Some(&1));
}

#[test]
fn title_priority_custom_then_summary_then_first_user() {
    let p = provider();
    let a = p.parse_file(&main_file()).unwrap();
    // 有 summary 行、无 custom-title：取 summary；meta 用户行不参与
    assert_eq!(a.session.title.as_deref(), Some("演示会话摘要标题"));

    let b = p
        .parse_file(
            &fixture_root()
                .join("-Users-tester-code-demo/22222222-aaaa-4bbb-8ccc-000000000002.jsonl"),
        )
        .unwrap();
    assert_eq!(b.session.title.as_deref(), Some("用户自定义标题"));
    assert_eq!(b.session.entrypoint.as_deref(), Some("claude-desktop"));

    let c = p
        .parse_file(&fixture_root().join(
            "-Users-tester-code-demo/11111111-aaaa-4bbb-8ccc-000000000001/subagents/agent-deadbeef01.jsonl",
        ))
        .unwrap();
    assert_eq!(
        c.session.title.as_deref(),
        Some("子代理任务：统计仓库里的文件数量")
    );
}

#[test]
fn subagent_is_linked_to_parent() {
    let p = provider();
    let c = p
        .parse_file(&fixture_root().join(
            "-Users-tester-code-demo/11111111-aaaa-4bbb-8ccc-000000000001/subagents/agent-deadbeef01.jsonl",
        ))
        .unwrap();
    assert_eq!(c.session.kind, SessionKind::Subagent);
    assert_eq!(
        c.session.native_id,
        "11111111-aaaa-4bbb-8ccc-000000000001/deadbeef01"
    );
    assert_eq!(
        c.session.parent_session_id.as_deref(),
        Some(
            format!(
                "claude-code:{}:11111111-aaaa-4bbb-8ccc-000000000001",
                p.machine_id()
            )
            .as_str()
        )
    );
    assert_eq!(c.messages.len(), 2);
}

#[test]
fn store_roundtrip_and_chinese_fts() {
    let p = provider();
    let parsed = p.parse_file(&main_file()).unwrap();
    let mut store = Store::in_memory().unwrap();
    store.upsert_session(&parsed.session).unwrap();
    store
        .replace_messages(&parsed.session.id, &parsed.messages)
        .unwrap();

    let back = store.get_session(&parsed.session.id).unwrap().unwrap();
    assert_eq!(back, parsed.session);
    let msgs = store.messages(&parsed.session.id).unwrap();
    assert_eq!(msgs, parsed.messages);
    assert_eq!(store.counts().unwrap(), (1, 6));

    // 前缀查找
    let found = store.find_session("11111111").unwrap().unwrap();
    assert_eq!(found.id, parsed.session.id);
    assert!(store.find_session("nope").unwrap().is_none());
    // 子代理：同一主会话 uuid 前缀优先命中主会话；agent id 前缀命中子代理
    let sub = p
        .parse_file(&fixture_root().join(
            "-Users-tester-code-demo/11111111-aaaa-4bbb-8ccc-000000000001/subagents/agent-deadbeef01.jsonl",
        ))
        .unwrap();
    store.store_parsed(&sub.session, &sub.messages).unwrap();
    assert_eq!(
        store.find_session("11111111").unwrap().unwrap().kind,
        SessionKind::Main
    );
    assert_eq!(
        store.find_session("deadbe").unwrap().unwrap().kind,
        SessionKind::Subagent
    );
    store.replace_messages(&sub.session.id, &[]).unwrap();

    // 中文子串搜索（命中正文）
    let hit = store
        .list_sessions(&SessionQuery {
            text: Some("全部通过".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].title, "演示会话摘要标题");
    assert_eq!(hit[0].project_name, "demo");
    // 命中标题
    let hit = store
        .list_sessions(&SessionQuery {
            text: Some("摘要标题".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hit.len(), 1);
    // 两字符退回 LIKE
    let hit = store
        .list_sessions(&SessionQuery {
            text: Some("测试".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hit.len(), 1);
    // 不存在
    let miss = store
        .list_sessions(&SessionQuery {
            text: Some("不存在的词语".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(miss.is_empty());

    // 重复 replace 不会累积
    store
        .replace_messages(&parsed.session.id, &parsed.messages)
        .unwrap();
    assert_eq!(store.counts().unwrap(), (2, 6));

    assert_eq!(
        store.projects().unwrap(),
        vec![("demo".to_string(), "/Users/tester/code/demo".to_string(), 2)]
    );
}

#[test]
fn ingest_is_incremental() {
    let p = provider();
    let mut store = Store::in_memory().unwrap();
    let mut n = 0;
    let r1 = ingest_claude(&mut store, &p, &mut |_| n += 1);
    assert_eq!(r1.scanned, 3);
    assert_eq!(r1.parsed, 3);
    assert_eq!(r1.skipped_unchanged, 0);
    assert_eq!(r1.skipped_superseded, 1);
    assert!(r1.failed.is_empty(), "{:?}", r1.failed);
    assert!(r1.id_collisions.is_empty());
    assert_eq!(r1.warnings, 1);
    assert_eq!(n, 3);
    assert_eq!(store.counts().unwrap(), (3, 10));

    let r2 = ingest_claude(&mut store, &p, &mut |_| {});
    assert_eq!(r2.skipped_unchanged, 3);
    assert_eq!(r2.parsed, 0);
    assert_eq!(store.counts().unwrap(), (3, 10));

    // 列表按 updated_at 倒序：第二个会话（1月3日）在前
    let list = store
        .list_sessions(&SessionQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].title, "用户自定义标题");
    assert_eq!(list[2].kind, SessionKind::Main);

    // 按天统计：两天、两个模型
    let daily = store.stats_daily(10_000).unwrap();
    assert_eq!(daily.len(), 3);
    let total_out: u64 = daily.iter().map(|d| d.usage.output_tokens).sum();
    assert_eq!(total_out, 20 + 8 + 6 + 2);

    // 来源状态
    let st = store.source_state(&main_file()).unwrap().unwrap();
    assert_eq!(st.size_bytes, std::fs::metadata(main_file()).unwrap().len());
}

#[test]
fn same_uuid_in_two_sessions_does_not_conflict() {
    // fork / resume 会把历史行（含 uuid）复制到新文件，两个会话里出现同一 uuid 必须都能入库。
    let p = provider();
    let a = p.parse_file(&main_file()).unwrap();
    let mut b_session = a.session.clone();
    b_session.id = format!("{}-fork", a.session.id);
    b_session.native_id = format!("{}-fork", a.session.native_id);
    let b_messages: Vec<_> = a
        .messages
        .iter()
        .cloned()
        .map(|mut m| {
            m.session_id = b_session.id.clone();
            m
        })
        .collect();
    let mut store = Store::in_memory().unwrap();
    store.store_parsed(&a.session, &a.messages).unwrap();
    store.store_parsed(&b_session, &b_messages).unwrap();
    assert_eq!(store.counts().unwrap(), (2, 12));
    assert_eq!(store.messages(&b_session.id).unwrap().len(), 6);
}

/// 老用户的库早已入库完毕（index 认为原件「未变」），开启 raw 层后的第一次扫描仍要把原件全部归档，
/// 而且 `.superseded-*` 旧副本不进归档；再扫一次全部「未变」，不重复拷贝。
#[test]
fn raw_layer_backfills_already_indexed_sources() {
    let p = provider();
    let mut store = Store::in_memory().unwrap();
    let r0 = ingest_claude(&mut store, &p, &mut |_| {});
    assert_eq!(r0.parsed, 3);

    let tmp = tempfile::tempdir().unwrap();
    let raw = agenthome_core::RawArchive::open(tmp.path().join("raw")).unwrap();
    let list: [&dyn agenthome_core::SourceProvider; 1] = [&p];
    let r1 = agenthome_core::ingest_all(&mut store, &list, Some(&raw), &mut |_| {});
    assert_eq!(r1.skipped_unchanged, 3, "index 层不该重新解析");
    assert_eq!(r1.raw.created, 3);
    assert!(r1.raw.failed.is_empty(), "{:?}", r1.raw.failed);
    assert_eq!(raw.stats().sessions, 3);

    let r2 = agenthome_core::ingest_all(&mut store, &list, Some(&raw), &mut |_| {});
    assert_eq!(r2.raw.unchanged, 3);
    assert!(!r2.raw.wrote_anything());
}
