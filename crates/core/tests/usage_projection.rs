//! 远程用量投影（CARM-290，方案单元 2）：Claude、Codex 解析结果 → 协议 v1 记录。
//!
//! 为什么要这些用例：远端用量是按事件键去重入账的，键一旦不稳或撞键，Mac 上就会重复计费或漏计。
//! - fork 复制的同一次响应只计一次（Claude 真实库曾因此多算 $446，docs/12 §7）；
//! - 同一 response 重复写入只计一次，同一毫秒的两个独立请求都要计入；
//! - 会话归档改名 / 压缩后重读，键不变，否则重扫一遍就翻倍；
//! - 投影后的 token 合计与成本必须等于现有本地解析入库后的统计，否则同一台机器本地看和远程看对不上。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::Period;
use agenthome_core::providers::SourceProvider;
use agenthome_core::usage_projection::{dedup, records};
use agenthome_core::usage_v1::{UsagePage, UsageRecord, VERSION};
use agenthome_core::{ClaudeCodeProvider, CodexProvider, Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const MAIN: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const FORK: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const CODEX_ID: &str = "01a1aa6a-39e1-7b82-b249-204e4e4110aa";
const MODEL: &str = "claude-opus-4-5-20251101";

fn identity(dir: &Path) -> DeviceIdentity {
    DeviceIdentity::load_or_create(&dir.join("device.json"), "box").unwrap()
}

// ---------- Claude ----------

fn claude_user(session: &str, uuid: &str, ts: &str) -> String {
    json!({"type": "user", "uuid": uuid, "parentUuid": null, "timestamp": ts, "sessionId": session,
           "cwd": "/work/demo", "message": {"role": "user", "content": "跑一下测试"}})
    .to_string()
}

fn claude_assistant(
    session: &str,
    uuid: &str,
    ts: &str,
    msg_id: Option<&str>,
    usage: Value,
) -> String {
    let mut message = json!({"type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "text", "text": "好的"}], "stop_reason": "end_turn", "usage": usage});
    if let Some(id) = msg_id {
        message["id"] = id.into();
    }
    json!({"type": "assistant", "uuid": uuid, "parentUuid": null, "timestamp": ts,
           "sessionId": session, "message": message})
    .to_string()
}

fn claude_usage(input: u64, output: u64, read: u64, w5: u64, w1h: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output, "cache_read_input_tokens": read,
           "cache_creation_input_tokens": w5 + w1h,
           "cache_creation": {"ephemeral_5m_input_tokens": w5, "ephemeral_1h_input_tokens": w1h}})
}

/// 主会话：msg_a（流式两行，同 message.id 合并）、msg_b 与 msg_c 同一毫秒、一条没有 message.id 的旧格式回复。
/// fork 会话：把主会话的 msg_a 原样复制过来（同 uuid、同时间），再加自己的 msg_d。
fn write_claude(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join("-work-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let msg_a_partial = claude_assistant(
        MAIN,
        "a1",
        "2026-11-20T01:00:01.000Z",
        Some("msg_a"),
        claude_usage(10, 1, 0, 0, 0),
    )
    // 流式的前几行还没有 stop_reason。
    .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":null"#);
    let msg_a = claude_assistant(
        MAIN,
        "a1b",
        "2026-11-20T01:00:01.500Z",
        Some("msg_a"),
        claude_usage(10, 300, 20_000, 1_000, 2_000),
    );
    let main = [
        claude_user(MAIN, "u1", "2026-11-20T01:00:00.000Z"),
        msg_a_partial.clone(),
        msg_a.clone(),
        claude_assistant(
            MAIN,
            "a2",
            "2026-11-20T01:00:05.000Z",
            Some("msg_b"),
            claude_usage(5, 50, 30_000, 0, 0),
        ),
        claude_assistant(
            MAIN,
            "a3",
            "2026-11-20T01:00:05.000Z",
            Some("msg_c"),
            claude_usage(5, 50, 30_000, 0, 0),
        ),
        claude_assistant(
            MAIN,
            "a4",
            "2026-11-20T01:00:09.000Z",
            None,
            json!({"input_tokens": 7, "output_tokens": 70, "cache_read_input_tokens": 0,
                   "cache_creation_input_tokens": 400}),
        ),
    ];
    let fork = [
        claude_user(FORK, "u1", "2026-11-20T01:00:00.000Z"),
        msg_a_partial,
        msg_a,
        claude_assistant(
            FORK,
            "f1",
            "2026-11-20T02:00:00.000Z",
            Some("msg_d"),
            claude_usage(3, 30, 10_000, 500, 0),
        ),
    ];
    let p_main = dir.join(format!("{MAIN}.jsonl"));
    let p_fork = dir.join(format!("{FORK}.jsonl"));
    std::fs::write(&p_main, main.join("\n")).unwrap();
    std::fs::write(&p_fork, fork.join("\n")).unwrap();
    (p_main, p_fork)
}

// ---------- Codex ----------

fn codex_line(ts: &str, ty: &str, payload: Value) -> String {
    json!({"timestamp": ts, "type": ty, "payload": payload}).to_string()
}

fn codex_usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": reasoning})
}

fn codex_reply(ts: &str, text: &str) -> String {
    codex_line(
        ts,
        "response_item",
        json!({"type": "message", "role": "assistant",
               "content": [{"type": "output_text", "text": text}]}),
    )
}

fn codex_record(ts: &str, rid: &str, usage: Value) -> String {
    codex_line(
        ts,
        "token_usage_record",
        json!({"thread_id": CODEX_ID, "response_id": rid, "usage": usage}),
    )
}

fn codex_head(id: &str) -> Vec<String> {
    vec![
        codex_line(
            "2026-11-20T03:00:00.000Z",
            "session_meta",
            json!({"id": id, "timestamp": "2026-11-20T03:00:00.000Z", "cwd": "/work/demo"}),
        ),
        codex_line(
            "2026-11-20T03:00:00.100Z",
            "turn_context",
            json!({"cwd": "/work/demo", "model": "gpt-5-codex"}),
        ),
        codex_line(
            "2026-11-20T03:00:00.200Z",
            "response_item",
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "看一下"}]}),
        ),
    ]
}

/// 新格式：resp_1 被重复写了一次；resp_2、resp_3 两个独立请求在同一毫秒。
fn codex_new() -> String {
    let mut lines = codex_head(CODEX_ID);
    lines.extend([
        codex_reply("2026-11-20T03:00:01.000Z", "一"),
        codex_record(
            "2026-11-20T03:00:01.100Z",
            "resp_1",
            codex_usage(1000, 600, 50, 20),
        ),
        codex_record(
            "2026-11-20T03:00:01.101Z",
            "resp_1",
            codex_usage(1000, 600, 50, 20),
        ),
        codex_reply("2026-11-20T03:00:02.000Z", "二"),
        codex_record(
            "2026-11-20T03:00:02.000Z",
            "resp_2",
            codex_usage(1200, 1000, 30, 0),
        ),
        codex_line(
            "2026-11-20T03:00:02.000Z",
            "response_item",
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "再来"}]}),
        ),
        codex_reply("2026-11-20T03:00:02.000Z", "三"),
        codex_record(
            "2026-11-20T03:00:02.000Z",
            "resp_3",
            codex_usage(1200, 1000, 30, 0),
        ),
    ]);
    lines.join("\n")
}

/// 旧格式：只有 token_count 累计值，没有 response_id，按序号生成键。
fn codex_old(id: &str) -> String {
    let total = |i, c, o| json!({"total_token_usage": codex_usage(i, c, o, 0), "last_token_usage": codex_usage(i, c, o, 0)});
    let mut lines = codex_head(id);
    lines.extend([
        codex_reply("2026-11-20T03:00:01.000Z", "一"),
        codex_line(
            "2026-11-20T03:00:01.100Z",
            "event_msg",
            json!({"type": "token_count", "info": total(500, 0, 10)}),
        ),
        codex_reply("2026-11-20T03:00:02.000Z", "二"),
        codex_line(
            "2026-11-20T03:00:02.100Z",
            "event_msg",
            json!({"type": "token_count", "info": total(1100, 400, 25)}),
        ),
    ]);
    lines.join("\n")
}

fn codex_path(root: &Path, id: &str) -> PathBuf {
    let d = root.join("2026").join("11").join("20");
    std::fs::create_dir_all(&d).unwrap();
    d.join(format!("rollout-2026-11-20T03-00-00-{id}.jsonl"))
}

// ---------- 工具 ----------

fn project(
    provider: &dyn SourceProvider,
    id: &DeviceIdentity,
    paths: &[&Path],
) -> Vec<UsageRecord> {
    let mut all = Vec::new();
    for p in paths {
        let file = provider.usage_events(p).unwrap().expect("来源已接入投影");
        let (recs, warnings) = records(id, &provider.id(), &file);
        assert!(warnings.is_empty(), "{warnings:?}");
        all.extend(recs);
    }
    dedup(all)
}

/// 投影记录与本地入库统计逐项对比：token 各分量、等价成本。
/// `cache_write_extra`：原件里缓存写总数与 5m / 1h 分档不一致时，投影按分档（计价同口径）比本地总数多出的量。
/// token 逐项断言相等；返回 (投影逐请求计价, 本地入库计价)，由调用方决定怎么比。
fn token_totals_match_local(
    provider: &dyn SourceProvider,
    recs: &[UsageRecord],
    cache_write_extra: u64,
) -> (f64, f64) {
    let mut store = Store::in_memory().unwrap();
    let list: [&dyn SourceProvider; 1] = [provider];
    let report = agenthome_core::ingest_all(&mut store, &list, None, &mut |_| {});
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let local = store
        .stats_totals(Period::All, Some(&provider.id()))
        .unwrap();
    let sum = |f: fn(&UsageRecord) -> u64| recs.iter().map(f).sum::<u64>();
    let u = local.usage;
    assert_eq!(sum(|r| r.input_tokens), u.input_tokens, "新鲜输入");
    assert_eq!(sum(|r| r.output_tokens), u.output_tokens, "输出");
    assert_eq!(sum(|r| r.cache_read_tokens), u.cache_read_tokens, "缓存读");
    assert_eq!(
        sum(|r| r.cache_write_5m_tokens + r.cache_write_1h_tokens),
        u.cache_write_tokens + cache_write_extra,
        "缓存写"
    );
    assert_eq!(sum(|r| r.reasoning_tokens), u.reasoning_tokens, "推理");
    let cost: f64 = recs
        .iter()
        .filter_map(|r| {
            let usage = agenthome_core::model::Usage {
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cache_read_tokens: r.cache_read_tokens,
                cache_write_tokens: r.cache_write_5m_tokens + r.cache_write_1h_tokens,
                cache_write_5m_tokens: r.cache_write_5m_tokens,
                cache_write_1h_tokens: r.cache_write_1h_tokens,
                reasoning_tokens: r.reasoning_tokens,
                requests: 1,
                input_includes_cache: false,
            };
            agenthome_core::pricing::estimate_usd(r.model.as_deref()?, &usage)
        })
        .sum();
    (cost, local.cost_usd)
}

/// fixture 里每条消息只对应一次请求，成本也必须与本地完全相同。
fn assert_matches_local(provider: &dyn SourceProvider, recs: &[UsageRecord]) {
    let (cost, local) = token_totals_match_local(provider, recs, 0);
    assert!(local > 0.0, "fixture 模型要在定价表里，否则测不出成本");
    assert!((cost - local).abs() < 1e-9, "成本 {cost} vs 本地 {local}");
}

// ---------- 用例 ----------

#[test]
fn claude_fork_counts_once_and_same_ms_requests_both_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let (p_main, p_fork) = write_claude(&root);
    let id = identity(dir.path());
    let provider = ClaudeCodeProvider::new(root);

    let main_only = project(&provider, &id, &[&p_main]);
    // msg_a（两行合并为一）、msg_b、msg_c（同一毫秒，各计一次）、无 id 的旧格式一条。
    assert_eq!(main_only.len(), 4);
    let both = project(&provider, &id, &[&p_main, &p_fork]);
    // fork 复制过来的 msg_a 不再计，只多出 fork 自己的 msg_d。
    assert_eq!(both.len(), 5);
    let mut fork_first = project(&provider, &id, &[&p_fork, &p_main]);
    let mut sorted = both.clone();
    sorted.sort_by(|a, b| a.event_key.cmp(&b.event_key));
    fork_first.sort_by(|a, b| a.event_key.cmp(&b.event_key));
    assert_eq!(sorted, fork_first, "扫描顺序不影响结果");

    // 合并后 usage 取带 stop_reason 的终值，缓存写分档原样保留。
    let msg_a = both
        .iter()
        .find(|r| r.output_tokens == 300)
        .expect("msg_a 终值");
    assert_eq!(
        (msg_a.cache_write_5m_tokens, msg_a.cache_write_1h_tokens),
        (1_000, 2_000)
    );
    // 旧格式没拆分档：整体按 5m（与计价同口径）。
    let legacy = both.iter().find(|r| r.output_tokens == 70).unwrap();
    assert_eq!(
        (legacy.cache_write_5m_tokens, legacy.cache_write_1h_tokens),
        (400, 0)
    );
    for r in &both {
        assert_eq!(r.source, "claude-code");
        assert_eq!(r.vendor.as_deref(), Some("anthropic"));
        assert_eq!(r.model.as_deref(), Some(MODEL));
        assert_eq!((r.v, r.revision, r.requests), (VERSION, 1, 1));
    }

    // 能直接装页发出去（白名单、校验都过）。
    let page = UsagePage {
        v: VERSION,
        machine_id: id.machine_id().to_string(),
        records: both.clone(),
    };
    assert_eq!(UsagePage::decode(&page.encode().unwrap()).unwrap(), page);

    assert_matches_local(&provider, &both);
}

/// 载荷里不能出现正文、路径、会话 id、请求 id 原文。
#[test]
fn records_carry_no_content_or_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let (p_main, p_fork) = write_claude(&root);
    let id = identity(dir.path());
    let recs = project(&ClaudeCodeProvider::new(root), &id, &[&p_main, &p_fork]);
    let text = serde_json::to_string(&recs).unwrap();
    for needle in [
        "跑一下测试",
        "好的",
        "/work/demo",
        "-work-demo",
        MAIN,
        FORK,
        "msg_a",
        "box",
    ] {
        assert!(!text.contains(needle), "载荷里出现了 {needle:?}");
    }
}

/// 会话目录改名（Claude 按 cwd 编码的目录名会随项目搬家变）后重读，键不变。
#[test]
fn claude_keys_survive_directory_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let (p_main, _) = write_claude(&root);
    let id = identity(dir.path());
    let provider = ClaudeCodeProvider::new(root.clone());
    let before = project(&provider, &id, &[&p_main]);
    let moved_dir = root.join("-work-demo-renamed");
    std::fs::rename(root.join("-work-demo"), &moved_dir).unwrap();
    let after = project(&provider, &id, &[&moved_dir.join(format!("{MAIN}.jsonl"))]);
    assert_eq!(before, after);
}

#[test]
fn codex_duplicate_response_once_same_ms_requests_both() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let p = codex_path(&root, CODEX_ID);
    std::fs::write(&p, codex_new()).unwrap();
    let id = identity(dir.path());
    let provider = CodexProvider::with_archive(root, None);
    let recs = project(&provider, &id, &[&p]);
    // resp_1 重复写入只计一次；resp_2、resp_3 同一毫秒，各计一次。
    assert_eq!(recs.len(), 3);
    let first = &recs[0];
    // Codex 原始 input 含缓存读，投影成新鲜输入：1000 - 600。
    assert_eq!(
        (
            first.input_tokens,
            first.cache_read_tokens,
            first.output_tokens
        ),
        (400, 600, 50)
    );
    assert_eq!(first.reasoning_tokens, 20);
    assert_eq!(recs[1].ts_ms, recs[2].ts_ms, "同一毫秒");
    assert_ne!(recs[1].event_key, recs[2].event_key);
    for r in &recs {
        assert_eq!(r.source, "codex");
        assert_eq!(r.vendor.as_deref(), Some("openai"));
        assert_eq!(r.model.as_deref(), Some("gpt-5-codex"));
    }
    assert_matches_local(&provider, &recs);
}

/// Codex 归档：rollout 从 `sessions/YYYY/MM/DD/` 挪到平铺的 `archived_sessions/` 并压成 `.zst`。
/// 新格式（按 response_id）和旧格式（按序号）的键都不能变。
#[test]
fn codex_keys_survive_archive_move_and_compression() {
    const OLD: &str = "0199d624-3fc4-7752-a28c-fc1e6c3f00bb";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let archive = dir.path().join("archived_sessions");
    std::fs::create_dir_all(&archive).unwrap();
    let id = identity(dir.path());
    let provider = CodexProvider::with_archive(root.clone(), Some(archive.clone()));

    for (sid, body) in [(CODEX_ID, codex_new()), (OLD, codex_old(OLD))] {
        let live = codex_path(&root, sid);
        std::fs::write(&live, &body).unwrap();
        let before = project(&provider, &id, &[&live]);
        assert!(!before.is_empty());

        let archived = archive.join(format!(
            "{}.zst",
            live.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&archived, zstd::encode_all(body.as_bytes(), 3).unwrap()).unwrap();
        std::fs::remove_file(&live).unwrap();
        let after = project(&provider, &id, &[&archived]);
        assert_eq!(before, after, "{sid} 归档后键或用量变了");
    }
}

/// 旧格式按 token_count 差值：两条请求，第二条只计差值；与本地入库一致。
#[test]
fn codex_legacy_token_count_matches_local() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let p = codex_path(&root, CODEX_ID);
    std::fs::write(&p, codex_old(CODEX_ID)).unwrap();
    let id = identity(dir.path());
    let provider = CodexProvider::with_archive(root, None);
    let recs = project(&provider, &id, &[&p]);
    assert_eq!(recs.len(), 2);
    assert_eq!(
        (
            recs[1].input_tokens,
            recs[1].cache_read_tokens,
            recs[1].output_tokens
        ),
        (200, 400, 15)
    );
    assert_matches_local(&provider, &recs);
}

/// 文件里先有 token_count、后出现 record：本地只认 record，投影也要把序号事件撤掉，不能两边都计。
#[test]
fn codex_switch_to_records_drops_count_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let p = codex_path(&root, CODEX_ID);
    let mut body = codex_old(CODEX_ID);
    body.push('\n');
    body.push_str(&codex_reply("2026-11-20T03:00:03.000Z", "三"));
    body.push('\n');
    body.push_str(&codex_record(
        "2026-11-20T03:00:03.100Z",
        "resp_9",
        codex_usage(800, 100, 40, 0),
    ));
    std::fs::write(&p, body).unwrap();
    let id = identity(dir.path());
    let provider = CodexProvider::with_archive(root, None);
    let recs = project(&provider, &id, &[&p]);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].input_tokens, 700);
    assert_matches_local(&provider, &recs);
}

/// 真实数据对账：`REAL_CLAUDE`（`~/.claude/projects` 的快照）、`REAL_CODEX`（`~/.codex/sessions` 快照）、
/// 可选 `REAL_CODEX_ARCHIVE`。投影去重后的 token 与成本必须等于同一批文件入库后的本地统计。
/// 用快照而不是活目录：agent 正在写的文件两次读之间会变。
#[test]
#[ignore]
fn real_data_matches_local_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let mut ran = false;
    let check = |provider: &dyn SourceProvider| {
        let files = provider.discover().files;
        let started = std::time::Instant::now();
        let mut all = Vec::new();
        let mut extra = std::collections::HashMap::new();
        let mut warned = 0;
        for p in &files {
            let file = provider.usage_events(p).unwrap().unwrap();
            let (recs, warnings) = records(&id, &provider.id(), &file);
            warned += warnings.len();
            assert_eq!(recs.len(), file.events.len(), "{warnings:?}");
            for (r, e) in recs.iter().zip(&file.events) {
                let split = e.usage.cache_write_5m_tokens + e.usage.cache_write_1h_tokens;
                if split > 0 && split != e.usage.cache_write_tokens {
                    extra.insert(
                        r.event_key.clone(),
                        split as i64 - e.usage.cache_write_tokens as i64,
                    );
                }
            }
            all.extend(recs);
        }
        let raw = all.len();
        let recs = dedup(all);
        let extra: i64 = recs.iter().filter_map(|r| extra.get(&r.event_key)).sum();
        eprintln!("  缓存写总数与分档不一致的请求合计差 {extra}（投影按分档，与计价同口径）");
        eprintln!(
            "{}: {} 个文件，{raw} 条用量 → 去重后 {} 条，投影警告 {warned} 条，用时 {:?}",
            provider.id().as_str(),
            files.len(),
            recs.len(),
            started.elapsed()
        );
        let (cost, local) =
            token_totals_match_local(provider, &recs, u64::try_from(extra).unwrap());
        eprintln!("  token 各分量与本地入库逐项一致；成本 逐请求 ${cost:.2} / 本地 ${local:.2}");
        // Codex 本地把同一条 assistant 消息上的多次请求合起来计价，合计提示跨过长上下文阈值时整条按高价档，
        // 偏高；投影按单次请求判断档位（方案「长上下文跨档在单请求上判断」），所以只会更低或相等。
        // Claude 一条消息就是一次请求，必须相等。
        if provider.id() == agenthome_core::model::ProviderId::Codex {
            assert!(cost <= local + 1e-6, "逐请求计价不应高于按消息合并计价");
        } else {
            assert!((cost - local).abs() < 1e-6, "成本 {cost} vs 本地 {local}");
        }
    };
    if let Ok(root) = std::env::var("REAL_CLAUDE") {
        check(&ClaudeCodeProvider::new(root.into()));
        ran = true;
    }
    if let Ok(root) = std::env::var("REAL_CODEX") {
        let archive = std::env::var("REAL_CODEX_ARCHIVE").ok().map(PathBuf::from);
        check(&CodexProvider::with_archive(root.into(), archive));
        ran = true;
    }
    if !ran {
        eprintln!("未设置 REAL_CLAUDE / REAL_CODEX，跳过");
    }
}
