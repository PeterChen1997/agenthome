//! Gemini 合成 fixture（不含任何真实对话）：
//! - 旧版 Gemini CLI 整文件 `.json`；
//! - 新版 `.jsonl`（同 id 覆盖、`$set`、`$rewindTo`、`$patch`、坏行）与子代理文件；
//! - Antigravity 的 `.pb` + `conversation_summaries.db`；
//! - 坏 JSON 文件进 Report.failed，不 panic。

use agenthome_core::model::{ContentBlock, Message, ProviderId, Role, SessionKind};
use agenthome_core::{GeminiProvider, SourceProvider, Store, ingest_all};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const LEGACY_ID: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
const NEW_ID: &str = "22222222-aaaa-4bbb-8ccc-000000000002";
const AGENT_ID: &str = "agent-33333333";
const AG_ID: &str = "44444444-aaaa-4bbb-8ccc-000000000004";
const AG_CHILD: &str = "55555555-aaaa-4bbb-8ccc-000000000005";
const AG_ORPHAN: &str = "66666666-aaaa-4bbb-8ccc-000000000006";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn tokens(input: u64, output: u64, cached: u64, thoughts: u64, tool: u64) -> Value {
    json!({"input": input, "output": output, "cached": cached, "thoughts": thoughts,
           "tool": tool, "total": input + output + thoughts + tool})
}

fn tool_call(id: &str, name: &str, status: &str, output: &str) -> Value {
    json!({"id": id, "name": name, "args": {"path": "a.txt"}, "status": status,
           "timestamp": "2026-01-24T11:00:05.000Z",
           "result": [{"functionResponse": {"id": id, "name": name, "response": {"output": output}}}],
           "displayName": name, "description": "", "resultDisplay": "shown", "renderOutputAsMarkdown": false})
}

/// 旧版：整文件 JSON，info / user / 带思考+工具调用的 gemini / 纯文本 gemini。
fn legacy_session() -> String {
    json!({
        "sessionId": LEGACY_ID, "projectHash": "proj-a",
        "startTime": "2026-01-24T11:00:00.000Z", "lastUpdated": "2026-01-24T11:01:00.000Z",
        "messages": [
            {"id": "m0", "timestamp": "2026-01-24T11:00:00.500Z", "type": "info", "content": "Empty GEMINI.md created."},
            {"id": "m1", "timestamp": "2026-01-24T11:00:01.000Z", "type": "user", "content": "读一下  a.txt\n然后总结"},
            {"id": "m2", "timestamp": "2026-01-24T11:00:02.000Z", "type": "gemini", "content": "我先读文件。",
             "model": "gemini-3-pro-preview",
             "thoughts": [{"subject": "Planning", "description": "read the file", "timestamp": "2026-01-24T11:00:01.500Z"}],
             "tokens": tokens(1000, 20, 600, 50, 7),
             "toolCalls": [tool_call("read_file-1", "read_file", "success", "hello"),
                           tool_call("run-2", "run_shell_command", "error", "exit 1")]},
            {"id": "m3", "timestamp": "2026-01-24T11:00:10.000Z", "type": "gemini", "content": "文件内容是 hello。",
             "model": "gemini-3-pro-preview", "thoughts": [], "tokens": tokens(1200, 10, 1000, 0, 0)}
        ]
    })
    .to_string()
}

/// 新版 jsonl：首行元数据 → 消息 → 同 id 覆盖补 tokens → `$set` 摘要 → 被回滚的一轮 → `$patch` 改工具结果 → 坏行。
fn jsonl_session() -> String {
    let lines = [
        json!({"sessionId": NEW_ID, "projectHash": "proj-b", "startTime": "2026-11-01T08:00:00.000Z",
               "lastUpdated": "2026-11-01T08:00:00.000Z", "kind": "main"}),
        json!({"id": "u1", "timestamp": "2026-11-01T08:00:01.000Z", "type": "user",
               "content": [{"text": "画个图"}, {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]}),
        json!({"id": "g1", "timestamp": "2026-11-01T08:00:02.000Z", "type": "gemini", "content": "",
               "model": "gemini-3-flash", "thoughts": [],
               "toolCalls": [tool_call("t1", "write_file", "success", "pending")]}),
        // 同 id 再写一遍：补上 tokens 与正文（官方 recordMessageTokens 的行为），位置不变。
        json!({"id": "g1", "timestamp": "2026-11-01T08:00:02.000Z", "type": "gemini", "content": "写好了。",
               "model": "gemini-3-flash", "thoughts": [],
               "tokens": tokens(500, 30, 100, 10, 0),
               "toolCalls": [tool_call("t1", "write_file", "success", "pending")]}),
        json!({"$set": {"lastUpdated": "2026-11-01T08:05:00.000Z", "summary": "画图并保存",
                        "directories": ["/tmp/proj-b"]}}),
        json!({"id": "u2", "timestamp": "2026-11-01T08:01:00.000Z", "type": "user", "content": "这一轮会被回滚"}),
        json!({"id": "g2", "timestamp": "2026-11-01T08:01:01.000Z", "type": "gemini", "content": "被回滚",
               "model": "gemini-3-flash", "tokens": tokens(9999, 9999, 0, 0, 0)}),
        json!({"$rewindTo": "u2"}),
        json!({"$patch": {"id": "g1", "toolCalls": [{"id": "t1", "result": [
            {"functionResponse": {"id": "t1", "name": "write_file", "response": {"output": "saved"}}}]}]}}),
    ];
    let mut out: String = lines.iter().map(|l| format!("{l}\n")).collect();
    out.push_str("{not json\n");
    out
}

fn subagent_session() -> String {
    [
        json!({"sessionId": AGENT_ID, "projectHash": "proj-b", "startTime": "2026-11-01T08:00:03.000Z",
               "lastUpdated": "2026-11-01T08:00:04.000Z", "kind": "subagent"}),
        json!({"id": "s1", "timestamp": "2026-11-01T08:00:03.000Z", "type": "user", "content": "子任务"}),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect()
}

/// protobuf 字段 1（字符串）+ 一个无关字段，模拟 raw_summary。
fn raw_summary(title: &str) -> Vec<u8> {
    let mut buf = vec![0x0A, title.len() as u8];
    buf.extend_from_slice(title.as_bytes());
    buf.extend_from_slice(&[0x10, 0x05]);
    buf
}

fn build_tree(root: &Path) {
    // Gemini CLI：旧版哈希目录（无 .project_root）、新版 slug 目录（有 .project_root）。
    write(
        &root.join("tmp/hash-a/chats/session-2026-01-24T11-00-11111111.json"),
        &legacy_session(),
    );
    write(&root.join("tmp/hash-a/logs.json"), "[]");
    write(&root.join("tmp/hash-a/chats/checkpoint-mytag.json"), "[]");
    write(&root.join("tmp/proj-b/.project_root"), "/tmp/proj-b-root\n");
    write(
        &root.join("tmp/proj-b/chats/session-2026-11-01T08-00-22222222.jsonl"),
        &jsonl_session(),
    );
    write(
        &root.join(format!("tmp/proj-b/chats/{NEW_ID}/{AGENT_ID}.jsonl")),
        &subagent_session(),
    );

    // Antigravity：.pb 内容是不透明字节；摘要库给标题、时间、工作区、父会话。
    let ag = root.join("antigravity");
    for id in [AG_ID, AG_CHILD, AG_ORPHAN] {
        write(
            &ag.join(format!("conversations/{id}.pb")),
            "\u{1}\u{2}opaque",
        );
    }
    write(&ag.join(format!("conversations/{AG_ID}.x.tmp")), "partial");
    let conn = rusqlite::Connection::open(ag.join("conversation_summaries.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",
         `preview` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,
         `last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`status` text NOT NULL DEFAULT \"\",
         `source` text NOT NULL DEFAULT \"\",`project_id` text NOT NULL DEFAULT \"\",`agent_name` text NOT NULL DEFAULT \"\",
         `parent_conversation_id` text NOT NULL DEFAULT \"\",`nesting_depth` integer NOT NULL DEFAULT 0,
         `battle_id` text NOT NULL DEFAULT \"\",`winning_conversation_id` text NOT NULL DEFAULT \"\",
         `not_fully_idle` numeric NOT NULL DEFAULT false,`killed` numeric NOT NULL DEFAULT false,
         `last_user_input_time` datetime NOT NULL,`last_user_input_step_index` integer NOT NULL DEFAULT -1,
         `app_data_dir` text NOT NULL DEFAULT \"\",`raw_summary` blob,`group_id` text NOT NULL DEFAULT \"\",
         PRIMARY KEY (`conversation_id`));",
    )
    .unwrap();
    let insert = "INSERT INTO conversation_summaries (conversation_id, preview, last_modified_time,
                  workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
    conn.execute(
        insert,
        rusqlite::params![
            AG_ID,
            "preview text",
            "2026-02-01 01:30:54.026222+00:00",
            r#"["file:///Users/me/My%20Proj"]"#,
            "",
            "2026-02-01 01:14:01.157088+00:00",
            raw_summary("Fix Login Bug"),
        ],
    )
    .unwrap();
    conn.execute(
        insert,
        rusqlite::params![
            AG_CHILD,
            "子会话预览",
            "2026-02-02 00:00:00+00:00",
            "[]",
            AG_ID,
            "2026-02-01 23:00:00+00:00",
            Option::<Vec<u8>>::None,
        ],
    )
    .unwrap();
}

fn fixture() -> (tempfile::TempDir, GeminiProvider) {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path());
    let p = GeminiProvider::new(dir.path().to_path_buf());
    (dir, p)
}

fn find(p: &GeminiProvider, suffix: &str) -> PathBuf {
    p.discover()
        .files
        .into_iter()
        .find(|f| f.to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("未发现 {suffix}"))
}

fn tool_calls(m: &Message) -> Vec<(&str, &str)> {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                tool_call_id, name, ..
            } => Some((tool_call_id.as_str(), name.as_str())),
            _ => None,
        })
        .collect()
}

fn tool_results(m: &Message) -> Vec<(&str, &str, bool)> {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => Some((tool_call_id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn discover_picks_sessions_subagents_and_pb_only() {
    let (_dir, p) = fixture();
    assert!(p.detect());
    let names: Vec<String> = p
        .discover()
        .files
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // 3 个 CLI 文件 + 3 个 .pb；logs.json、checkpoint、.tmp 不算。
    assert_eq!(names.len(), 6, "{names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("checkpoint") || n == "logs.json")
    );
    assert!(!names.iter().any(|n| n.ends_with(".tmp")));
}

/// 旧版 json：消息映射、思考块、工具调用与结果配对、错误状态、tokens 统一成「新鲜输入」口径。
#[test]
fn legacy_json_maps_messages_tools_and_usage() {
    let (_dir, p) = fixture();
    let parsed = p.parse_file(&find(&p, "11111111.json")).unwrap();
    let s = &parsed.session;
    assert_eq!(s.provider, ProviderId::GeminiCli);
    assert_eq!(s.native_id, LEGACY_ID);
    assert_eq!(s.kind, SessionKind::Main);
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert_eq!(s.title.as_deref(), Some("读一下 a.txt 然后总结"));
    assert_eq!(s.cwd, None, "哈希目录且无 projects.json 时 cwd 留空");
    assert_eq!(s.models, vec!["gemini-3-pro-preview".to_string()]);

    let roles: Vec<Role> = parsed.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::System,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant
        ]
    );
    assert!(parsed.messages[0].is_meta);

    let a = &parsed.messages[2];
    assert!(
        matches!(&a.blocks[0], ContentBlock::Thinking { text } if text == "**Planning** read the file")
    );
    assert_eq!(
        tool_calls(a),
        [("read_file-1", "read_file"), ("run-2", "run_shell_command")]
    );
    let t = &parsed.messages[3];
    assert_eq!(
        tool_results(t),
        [("read_file-1", "hello", false), ("run-2", "exit 1", true)]
    );

    // input 含缓存：1000 − 600 + tool 7 = 407；output 含思考：20 + 50。
    let u = a.usage.as_ref().unwrap();
    assert_eq!(
        (
            u.input_tokens,
            u.cache_read_tokens,
            u.output_tokens,
            u.reasoning_tokens
        ),
        (407, 600, 70, 50)
    );
    assert!(!u.input_includes_cache);
    assert_eq!(s.usage.input_tokens, 407 + 200);
    assert_eq!(s.usage.cache_read_tokens, 1600);
    assert_eq!(s.usage.requests, 2);
    assert!(!s.usage.input_includes_cache);
    assert_eq!(s.message_count, 5);
}

/// 新版 jsonl：同 id 覆盖、$set、$rewindTo、$patch 都按官方规则回放；坏行进 warnings。
#[test]
fn jsonl_replays_updates_rewind_and_patch() {
    let (_dir, p) = fixture();
    let parsed = p.parse_file(&find(&p, "22222222.jsonl")).unwrap();
    let s = &parsed.session;
    assert_eq!(s.native_id, NEW_ID);
    assert_eq!(
        s.title.as_deref(),
        Some("画图并保存"),
        "$set 的 summary 优先"
    );
    assert_eq!(
        s.cwd.as_deref(),
        Some("/tmp/proj-b"),
        "directories 优先于 .project_root"
    );
    assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);

    let ids: Vec<&str> = parsed.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["u1", "g1", "g1:tool-results"], "u2/g2 已回滚");
    assert!(
        matches!(&parsed.messages[0].blocks[1], ContentBlock::Image { media_type } if media_type == "image/png")
    );
    assert_eq!(parsed.messages[1].text(), "写好了。");
    assert_eq!(
        tool_results(&parsed.messages[2]),
        [("t1", "saved", false)],
        "$patch 改写了工具结果"
    );
    // 只剩 g1 的 tokens：500 − 100 = 400 新鲜输入，被回滚的 9999 不计。
    assert_eq!(s.usage.input_tokens, 400);
    assert_eq!(s.usage.output_tokens, 40);
    assert_eq!(
        s.updated_at_ms,
        chrono::DateTime::parse_from_rfc3339("2026-11-01T08:05:00Z")
            .unwrap()
            .timestamp_millis()
    );
}

#[test]
fn subagent_file_links_to_parent_and_uses_project_root_marker() {
    let (_dir, p) = fixture();
    let parsed = p
        .parse_file(&find(&p, &format!("{AGENT_ID}.jsonl")))
        .unwrap();
    let s = &parsed.session;
    assert_eq!(s.kind, SessionKind::Subagent);
    assert_eq!(s.native_id, format!("{NEW_ID}/{AGENT_ID}"));
    let parent = p.parse_file(&find(&p, "22222222.jsonl")).unwrap().session;
    assert_eq!(s.parent_session_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(s.cwd.as_deref(), Some("/tmp/proj-b-root"));
}

/// Antigravity：内容解不开，只做存在性 + 标题 + 时间；没有摘要行的会话也要入库并给 warning。
#[test]
fn antigravity_uses_summary_db() {
    let (_dir, p) = fixture();
    let parsed = p.parse_file(&find(&p, &format!("{AG_ID}.pb"))).unwrap();
    let s = &parsed.session;
    assert_eq!(s.provider, ProviderId::GeminiCli);
    assert_eq!(s.entrypoint.as_deref(), Some("antigravity"));
    assert_eq!(s.title.as_deref(), Some("Fix Login Bug"));
    assert_eq!(s.cwd.as_deref(), Some("/Users/me/My Proj"));
    assert!(parsed.messages.is_empty());
    assert_eq!(
        parsed.ignored_records.get("antigravity-pb-undecoded"),
        Some(&1)
    );
    let modified = chrono::DateTime::parse_from_rfc3339("2026-02-01T01:30:54.026222Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(s.updated_at_ms, modified);
    assert!(s.started_at_ms <= s.updated_at_ms);

    let child = p.parse_file(&find(&p, &format!("{AG_CHILD}.pb"))).unwrap();
    assert_eq!(child.session.kind, SessionKind::Subagent);
    assert_eq!(
        child.session.parent_session_id.as_deref(),
        Some(s.id.as_str())
    );
    assert_eq!(
        child.session.title.as_deref(),
        Some("子会话预览"),
        "无 raw_summary 时退回 preview"
    );

    let orphan = p.parse_file(&find(&p, &format!("{AG_ORPHAN}.pb"))).unwrap();
    assert_eq!(orphan.session.title, None);
    assert_eq!(orphan.warnings.len(), 1);
}

/// 坏 JSON 文件计入 Report.failed，其余文件照常入库。
#[test]
fn broken_json_is_reported_not_panicking() {
    let (dir, p) = fixture();
    write(
        &dir.path()
            .join("tmp/hash-c/chats/session-2026-01-01T00-00-deadbeef.json"),
        "{\"sessionId\": ",
    );
    let db = tempfile::tempdir().unwrap();
    let mut store = Store::open(&db.path().join("t.db")).unwrap();
    let list: [&dyn SourceProvider; 1] = [&p];
    let r = ingest_all(&mut store, &list, None, &mut |_| {});
    assert_eq!(r.scanned, 7);
    assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
    assert!(
        r.failed[0]
            .0
            .ends_with("session-2026-01-01T00-00-deadbeef.json")
    );
    assert_eq!(r.parsed, 6);
    assert!(r.warnings >= 2, "jsonl 坏行 + 孤儿 .pb");
}
