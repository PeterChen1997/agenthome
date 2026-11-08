//! Codex rollout 合成 fixture：新格式（token_usage_record）与旧格式（仅 token_count）各一份，
//! 其中旧格式用 zstd 压缩，验证解压路径与 Discovery 的去重规则。

use agenthome_core::model::{ContentBlock, ProviderId, Role};
use agenthome_core::{CodexProvider, SourceProvider};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const NEW_ID: &str = "01a1aa6a-39e1-7b82-b249-204e4e411ff7";
const OLD_ID: &str = "0199d624-3fc4-7752-a28c-fc1e6c3fcf54";

fn line(ts: &str, ty: &str, payload: Value) -> String {
    json!({"timestamp": ts, "ordinal": 0, "type": ty, "payload": payload}).to_string()
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": reasoning,
           "total_tokens": input + output})
}

/// 新格式：session_meta / turn_context / 用户消息 / 推理+工具调用 / usage 记录 / 工具输出 / 回复 / usage 记录。
fn new_format() -> Vec<String> {
    vec![
        line(
            "2026-11-05T14:34:10.000Z",
            "session_meta",
            json!({
                "id": NEW_ID, "timestamp": "2026-11-05T14:34:10.000Z", "cwd": "/tmp/proj-a",
                "originator": "codex_cli_rs", "cli_version": "0.99.0", "source": "cli",
                "git": {"branch": "main"}
            }),
        ),
        line(
            "2026-11-05T14:34:10.100Z",
            "response_item",
            json!({
                "type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev instructions"}]
            }),
        ),
        line(
            "2026-11-05T14:34:10.200Z",
            "response_item",
            json!({
                "type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>"}]
            }),
        ),
        line(
            "2026-11-05T14:34:11.000Z",
            "turn_context",
            json!({"turn_id": "t1", "cwd": "/tmp/proj-a", "model": "gpt-5-codex"}),
        ),
        line(
            "2026-11-05T14:34:11.500Z",
            "response_item",
            json!({
                "type": "message", "role": "user", "content": [{"type": "input_text", "text": "请把 fixture 第一句\n做成标题"}]
            }),
        ),
        line(
            "2026-11-05T14:34:12.000Z",
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ),
        line(
            "2026-11-05T14:34:13.000Z",
            "response_item",
            json!({
                "type": "reasoning", "summary": [{"type": "summary_text", "text": "先看文件"}], "encrypted_content": "xxx"
            }),
        ),
        line(
            "2026-11-05T14:34:13.100Z",
            "response_item",
            json!({
                "type": "function_call", "name": "exec", "call_id": "c1", "arguments": "{\"cmd\":\"ls\"}"
            }),
        ),
        line(
            "2026-11-05T14:34:13.200Z",
            "token_usage_record",
            json!({
                "thread_id": NEW_ID, "turn_id": "t1", "response_id": "resp_1", "usage": usage(1000, 600, 50, 20)
            }),
        ),
        // 同一 response_id 重复写入：必须去重
        line(
            "2026-11-05T14:34:13.201Z",
            "token_usage_record",
            json!({
                "thread_id": NEW_ID, "turn_id": "t1", "response_id": "resp_1", "usage": usage(1000, 600, 50, 20)
            }),
        ),
        line(
            "2026-11-05T14:34:13.300Z",
            "response_item",
            json!({
                "type": "function_call_output", "call_id": "c1", "output": "a.rs\nb.rs"
            }),
        ),
        line(
            "2026-11-05T14:34:13.400Z",
            "event_msg",
            json!({"type": "token_count",
            "info": {"total_token_usage": usage(1000, 600, 50, 20), "last_token_usage": usage(1000, 600, 50, 20)},
            "rate_limits": {"primary": {"used_percent": 2.0, "window_minutes": 10080, "resets_at": 1}}}),
        ),
        line(
            "2026-11-05T14:34:14.000Z",
            "response_item",
            json!({
                "type": "reasoning", "summary": [], "encrypted_content": "yyy"
            }),
        ),
        line(
            "2026-11-05T14:34:14.100Z",
            "response_item",
            json!({
                "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "两个文件。"}]
            }),
        ),
        line(
            "2026-11-05T14:34:14.200Z",
            "token_usage_record",
            json!({
                "thread_id": NEW_ID, "turn_id": "t1", "response_id": "resp_2", "usage": usage(1200, 1000, 30, 0)
            }),
        ),
        line(
            "2026-11-05T14:34:14.300Z",
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t1"}),
        ),
        line(
            "2026-11-05T14:34:14.400Z",
            "world_state",
            json!({"full": true}),
        ),
        "not json at all".to_string(),
    ]
}

/// 旧格式：没有 token_usage_record，只有累计的 token_count；且有一条 info 为 null 的限额事件。
fn old_format() -> Vec<String> {
    vec![
        line(
            "2025-10-12T09:58:43.000Z",
            "session_meta",
            json!({
                "id": OLD_ID, "timestamp": "2025-10-12T09:58:43.000Z", "cwd": "/tmp/proj-b",
                "originator": "Codex Desktop", "cli_version": "0.40.0", "source": "vscode"
            }),
        ),
        line(
            "2025-10-12T09:58:43.100Z",
            "response_item",
            json!({
                "type": "message", "role": "user", "content": [{"type": "input_text", "text": "旧格式会话"}]
            }),
        ),
        line(
            "2025-10-12T09:58:44.000Z",
            "turn_context",
            json!({"cwd": "/tmp/proj-b", "model": "gpt-5"}),
        ),
        line(
            "2025-10-12T09:58:45.000Z",
            "response_item",
            json!({
                "type": "function_call", "name": "shell", "call_id": "c1", "arguments": "{\"command\":[\"ls\"]}"
            }),
        ),
        line(
            "2025-10-12T09:58:45.100Z",
            "response_item",
            json!({
                "type": "function_call_output", "call_id": "c1", "output": "ok"
            }),
        ),
        line(
            "2025-10-12T09:58:45.200Z",
            "event_msg",
            json!({"type": "token_count",
            "info": {"total_token_usage": usage(500, 0, 10, 0), "last_token_usage": usage(500, 0, 10, 0)},
            "rate_limits": null}),
        ),
        line(
            "2025-10-12T09:58:46.000Z",
            "response_item",
            json!({
                "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]
            }),
        ),
        line(
            "2025-10-12T09:58:46.100Z",
            "event_msg",
            json!({"type": "token_count",
            "info": {"total_token_usage": usage(1100, 400, 25, 0), "last_token_usage": usage(600, 400, 15, 0)},
            "rate_limits": null}),
        ),
        line(
            "2025-10-12T09:58:46.200Z",
            "event_msg",
            json!({"type": "token_count", "info": null,
            "rate_limits": {"primary": {"used_percent": 1.0}}}),
        ),
    ]
}

fn write_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let d1 = root.join("2026").join("11").join("05");
    let d2 = root.join("2025").join("10").join("12");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    let p_new = d1.join(format!("rollout-2026-11-05T14-34-10-{NEW_ID}.jsonl"));
    std::fs::write(&p_new, new_format().join("\n")).unwrap();
    // 同一会话再放一份 .zst，Discovery 必须只取 .jsonl
    let dup = d1.join(format!("rollout-2026-11-05T14-34-10-{NEW_ID}.jsonl.zst"));
    std::fs::write(
        &dup,
        zstd::encode_all(new_format().join("\n").as_bytes(), 3).unwrap(),
    )
    .unwrap();
    let p_old = d2.join(format!("rollout-2025-10-12T09-58-43-{OLD_ID}.jsonl.zst"));
    std::fs::write(
        &p_old,
        zstd::encode_all(old_format().join("\n").as_bytes(), 3).unwrap(),
    )
    .unwrap();
    // 不该被发现的东西
    std::fs::write(d2.join("notes.txt"), "x").unwrap();
    (p_new, p_old)
}

#[test]
fn 发现与去重() {
    let dir = tempfile::tempdir().unwrap();
    let (p_new, p_old) = write_fixture(dir.path());
    let provider = CodexProvider::new(dir.path().to_path_buf());
    assert!(provider.detect());
    let d = provider.discover();
    assert_eq!(d.files, vec![p_old.clone(), p_new.clone()]);
    assert_eq!(d.skipped_superseded, 1, "并存的 .zst 应被跳过");
    assert!(!CodexProvider::new(dir.path().join("nope")).detect());
}

#[test]
fn 新格式_聚合与去重() {
    let dir = tempfile::tempdir().unwrap();
    let (p_new, _) = write_fixture(dir.path());
    let provider = CodexProvider::new(dir.path().to_path_buf());
    let parsed = provider.parse_file(&p_new).unwrap();
    let s = &parsed.session;
    assert_eq!(s.provider, ProviderId::Codex);
    assert_eq!(s.native_id, NEW_ID);
    assert!(s.id.ends_with(NEW_ID));
    assert_eq!(s.title.as_deref(), Some("请把 fixture 第一句 做成标题"));
    assert_eq!(s.cwd.as_deref(), Some("/tmp/proj-a"));
    assert_eq!(s.git_branch.as_deref(), Some("main"));
    assert_eq!(s.entrypoint.as_deref(), Some("codex_cli_rs"));
    assert_eq!(s.agent_version.as_deref(), Some("0.99.0"));
    assert_eq!(s.models, vec!["gpt-5-codex".to_string()]);
    assert!(!s.source.compressed);
    assert_eq!(s.recorded_cost_usd, None);

    // 角色序列：developer(系统) / env(用户 meta) / 用户 / 助手(推理+工具) / 工具 / 助手(推理+文本)
    let roles: Vec<(Role, bool)> = parsed
        .messages
        .iter()
        .map(|m| (m.role, m.is_meta))
        .collect();
    assert_eq!(
        roles,
        vec![
            (Role::System, true),
            (Role::User, true),
            (Role::User, false),
            (Role::Assistant, false),
            (Role::Tool, false),
            (Role::Assistant, false),
        ]
    );
    let a1 = &parsed.messages[3];
    assert!(matches!(a1.blocks[0], ContentBlock::Thinking { ref text } if text == "先看文件"));
    assert!(matches!(
        &a1.blocks[1],
        ContentBlock::ToolCall { tool_call_id, name, input }
            if tool_call_id == "c1" && name == "exec" && input["cmd"] == "ls"
    ));
    let u1 = a1.usage.unwrap();
    assert_eq!(
        (
            u1.input_tokens,
            u1.cache_read_tokens,
            u1.output_tokens,
            u1.reasoning_tokens
        ),
        (400, 600, 50, 20),
        "input 入库前已减去缓存读，统一成新鲜输入口径"
    );
    assert_eq!(u1.requests, 1, "重复的 response_id 不能重复计");
    assert!(!u1.input_includes_cache);
    let a2 = &parsed.messages[5];
    assert_eq!(a2.text(), "两个文件。");
    assert_eq!(a2.usage.unwrap().input_tokens, 200);

    assert_eq!(s.usage.input_tokens, 600);
    assert_eq!(s.usage.output_tokens, 80);
    assert_eq!(s.usage.requests, 2);
    assert!(
        !s.usage.input_includes_cache,
        "会话级 usage 由消息级累加，已是新鲜输入口径"
    );
    assert_eq!(s.message_count, 6);
    assert_eq!(s.started_at_ms, 1_793_889_250_000);
    // 成本：gpt-5-codex，(400*1.25 + 600*0.125 + 50*10) + (200*1.25 + 1000*0.125 + 30*10) 每百万
    let est = s.estimated_cost_usd.unwrap();
    let expect =
        (400.0 * 1.25 + 600.0 * 0.125 + 50.0 * 10.0 + 200.0 * 1.25 + 1000.0 * 0.125 + 30.0 * 10.0)
            / 1e6;
    assert!((est - expect).abs() < 1e-12, "{est} vs {expect}");

    assert_eq!(
        parsed.warnings.len(),
        1,
        "坏行要进 warnings: {:?}",
        parsed.warnings
    );
    assert_eq!(
        parsed.ignored_records.get("token_usage_record:duplicate"),
        Some(&1)
    );
    assert_eq!(parsed.ignored_records.get("world_state"), Some(&1));
    assert_eq!(
        parsed.ignored_records.get("event_msg:task_started"),
        Some(&1)
    );
}

#[test]
fn 旧格式_zst_与_token_count_差值() {
    let dir = tempfile::tempdir().unwrap();
    let (_, p_old) = write_fixture(dir.path());
    let provider = CodexProvider::new(dir.path().to_path_buf());
    let parsed = provider.parse_file(&p_old).unwrap();
    let s = &parsed.session;
    assert!(s.source.compressed);
    assert_eq!(s.native_id, OLD_ID);
    assert_eq!(s.title.as_deref(), Some("旧格式会话"));
    assert_eq!(s.entrypoint.as_deref(), Some("Codex Desktop"));
    assert_eq!(s.models, vec!["gpt-5".to_string()]);
    let assistants: Vec<_> = parsed
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(assistants.len(), 2);
    let u1 = assistants[0].usage.unwrap();
    assert_eq!((u1.input_tokens, u1.output_tokens), (500, 10));
    let u2 = assistants[1].usage.unwrap();
    // 累计差值：输入 (1100-500) 再减缓存差 (400-0) = 200，输出 25-10，缓存 400-0
    assert_eq!(
        (u2.input_tokens, u2.output_tokens, u2.cache_read_tokens),
        (200, 15, 400)
    );
    assert_eq!(s.usage.input_tokens, 700);
    assert_eq!(s.usage.output_tokens, 25);
    assert_eq!(s.usage.requests, 2);
    assert_eq!(
        parsed.ignored_records.get("event_msg:token_count:no_info"),
        Some(&1)
    );
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
}

#[test]
fn 续写文件不覆盖原会话() {
    let dir = tempfile::tempdir().unwrap();
    let (p_new, _) = write_fixture(dir.path());
    // 同一 thread 的续写文件：文件名带 `_<新 uuid>`，session_meta.id 仍是原 thread id
    let cont = p_new.with_file_name(format!(
        "rollout-2026-11-06T09-00-00-{NEW_ID}_00000000-0000-7000-8000-00000000beef.jsonl"
    ));
    std::fs::write(&cont, new_format().join("\n")).unwrap();
    let provider = CodexProvider::new(dir.path().to_path_buf());
    let a = provider.parse_file(&p_new).unwrap().session;
    let b = provider.parse_file(&cont).unwrap().session;
    assert_eq!(a.native_id, NEW_ID);
    assert_eq!(
        b.native_id,
        format!("{NEW_ID}_00000000-0000-7000-8000-00000000beef")
    );
    assert_ne!(a.id, b.id);
    assert_eq!(b.parent_session_id.as_deref(), Some(a.id.as_str()));
    assert_eq!(a.parent_session_id, None);
}

#[test]
fn 入库后可查() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());
    let provider = CodexProvider::new(dir.path().to_path_buf());
    let mut store = agenthome_core::Store::in_memory().unwrap();
    let list: [&dyn SourceProvider; 1] = [&provider];
    let report = agenthome_core::ingest_all(&mut store, &list, &mut |_| {});
    assert_eq!(report.scanned, 2);
    assert_eq!(report.parsed, 2);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.per_provider.len(), 1);
    assert_eq!(report.per_provider[0].0, ProviderId::Codex);
    let (sessions, messages) = store.counts().unwrap();
    assert_eq!(sessions, 2);
    assert_eq!(messages, 6 + 4);
    let health = store.archive_health().unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].compressed, 1);
    assert_eq!(health[0].archive_only, 0);
    // 列表摘要的会话级成本 = 该会话各消息估算之和（Codex 无真值，来源为 computed）
    let summaries = store
        .list_sessions(&agenthome_core::model::SessionQuery::default())
        .unwrap();
    assert_eq!(summaries.len(), 2);
    for sum in &summaries {
        let expect: f64 = store
            .messages(&sum.id)
            .unwrap()
            .iter()
            .filter_map(|m| {
                agenthome_core::pricing::estimate_usd(m.model.as_deref()?, m.usage.as_ref()?)
            })
            .sum();
        let got = sum.cost_usd.expect("fixture 模型都在定价表里");
        assert!(
            (got - expect).abs() < 1e-12,
            "{}: {got} vs {expect}",
            sum.id
        );
        assert_eq!(
            sum.cost_source,
            agenthome_core::model::CostSource::Estimated
        );
        assert!(
            sum.source_path
                .as_deref()
                .is_some_and(|p| p.contains("rollout-"))
        );
    }
    // 再扫一次：全部未变跳过
    let report = agenthome_core::ingest_all(&mut store, &list, &mut |_| {});
    assert_eq!(report.skipped_unchanged, 2);
    assert_eq!(report.parsed, 0);
}

/// 10.29：会话级 usage 与消息级同为"新鲜输入"口径。若会话级误标 `input_includes_cache = true`，
/// 定价会把缓存读从 input 里再扣一次，列表上的 Codex 成本就会系统性偏低。
/// 新旧两种格式（token_usage_record / token_count 差值）都要成立。
#[test]
fn 会话级估算成本等于消息之和_不二次扣缓存() {
    let dir = tempfile::tempdir().unwrap();
    let (p_new, p_old) = write_fixture(dir.path());
    let provider = CodexProvider::new(dir.path().to_path_buf());
    for path in [p_new, p_old] {
        let parsed = provider.parse_file(&path).unwrap();
        let s = &parsed.session;
        let model = s.models.first().expect("fixture 只有一个模型");
        assert!(
            s.usage.cache_read_tokens > 0,
            "fixture 必须带缓存读，否则测不出二次扣减"
        );
        let per_message: f64 = parsed
            .messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| agenthome_core::pricing::estimate_usd(model, u).unwrap())
            .sum();
        let session_level = agenthome_core::pricing::estimate_usd(model, &s.usage).unwrap();
        assert!(
            (session_level - per_message).abs() < 1e-12,
            "{}: 会话级 {session_level} 与消息之和 {per_message} 不一致",
            path.display()
        );
        assert!((s.estimated_cost_usd.unwrap() - per_message).abs() < 1e-12);
    }
}
