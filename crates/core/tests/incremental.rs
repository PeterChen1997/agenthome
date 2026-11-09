//! 增量入库：按字节偏移续读 append-only 的 jsonl，只写受影响的消息。
//!
//! 合成数据全部在临时目录里生成。`bench_append_5_lines` 同时是基准：
//! `cargo test -p agenthome-core --release --test incremental -- --nocapture bench` 打印改动行数与耗时。

use agenthome_core::model::{Message, Session};
use agenthome_core::{
    ClaudeCodeProvider, CodexProvider, IngestCache, SourceProvider, Store, ingest_all_cached,
    ingest_paths,
};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SESSION: &str = "33333333-aaaa-4bbb-8ccc-000000000003";

fn ts(i: usize) -> String {
    let secs = i as i64;
    chrono::DateTime::from_timestamp(1_790_000_000 + secs, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn user(i: usize, text: &str) -> String {
    json!({"type": "user", "uuid": format!("u-{i}"), "parentUuid": null, "timestamp": ts(i),
           "sessionId": SESSION, "cwd": "/Users/tester/code/bench", "version": "2.1.300",
           "gitBranch": "main", "entrypoint": "cli",
           "message": {"role": "user", "content": text}})
    .to_string()
}

fn tool_result(i: usize, call: &str) -> String {
    json!({"type": "user", "uuid": format!("r-{i}"), "parentUuid": format!("a-{i}"), "timestamp": ts(i),
           "sessionId": SESSION,
           "message": {"role": "user", "content": [
               {"type": "tool_result", "tool_use_id": call, "content": "x".repeat(400)}]}})
    .to_string()
}

/// assistant 的一行；同一 `msg_id` 的多行在入库时合并成一条消息。
fn assistant(i: usize, uuid: &str, msg_id: &str, block: Value, stop: Option<&str>) -> String {
    json!({"type": "assistant", "uuid": uuid, "parentUuid": null, "timestamp": ts(i),
           "sessionId": SESSION,
           "message": {"id": msg_id, "role": "assistant", "model": "claude-sonnet-4-6",
               "content": [block], "stop_reason": stop,
               "usage": {"input_tokens": 3, "output_tokens": if stop.is_some() { 40 } else { 1 },
                         "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 20}}})
    .to_string()
}

/// 一轮 5 行：用户提问、assistant 调工具、工具结果、assistant 回复（思考 + 正文两行，同一 message.id）。
fn turn(t: usize) -> Vec<String> {
    let base = t * 10;
    vec![
        user(base, &format!("第 {t} 轮问题：请检查模块 {t} 的写放大")),
        assistant(
            base + 1,
            &format!("a-{base}-1"),
            &format!("msg-{t}-tool"),
            json!({"type": "tool_use", "id": format!("call-{t}"), "name": "Read",
                   "input": {"file_path": format!("/tmp/{t}.rs")}}),
            Some("tool_use"),
        ),
        tool_result(base + 2, &format!("call-{t}")),
        assistant(
            base + 3,
            &format!("a-{base}-3"),
            &format!("msg-{t}-reply"),
            json!({"type": "thinking", "thinking": format!("思考第 {t} 轮")}),
            None,
        ),
        assistant(
            base + 4,
            &format!("a-{base}-4"),
            &format!("msg-{t}-reply"),
            json!({"type": "text", "text": format!("第 {t} 轮回答：{}", "正文".repeat(60))}),
            Some("end_turn"),
        ),
    ]
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    file: PathBuf,
}

fn fixture(lines: &[String]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-tester-code-bench");
    std::fs::create_dir_all(&proj).unwrap();
    let file = proj.join(format!("{SESSION}.jsonl"));
    write_lines(&file, lines, false);
    Fixture {
        _dir: dir,
        root,
        file,
    }
}

fn write_lines(path: &Path, lines: &[String], append: bool) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(append)
        .write(true)
        .truncate(!append)
        .open(path)
        .unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
    f.sync_all().unwrap();
    // size + mtime 判断变化：mtime 精度到毫秒，同一毫秒内的两次写入要靠 size 区分，这里额外保证 mtime 前进。
    let t = std::time::SystemTime::now() + std::time::Duration::from_millis(5);
    f.set_modified(t).unwrap();
}

fn ingest_with(
    store: &mut Store,
    provider: &dyn SourceProvider,
    cache: &mut IngestCache,
) -> agenthome_core::Report {
    let list: [&dyn SourceProvider; 1] = [provider];
    let r = ingest_all_cached(store, &list, None, cache, &mut |_| {});
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    r
}

fn session_and_messages(store: &Store, id: &str) -> (Session, Vec<Message>) {
    (
        store.get_session(id).unwrap().expect("会话应已入库"),
        store.messages(id).unwrap(),
    )
}

/// 增量入库的结果必须与「同一份文件整文件解析入库」完全一致（成本只差浮点末位）。
fn assert_same_as_full(store: &Store, provider: &dyn SourceProvider, file: &Path) {
    let mut fresh = Store::in_memory().unwrap();
    let parsed = provider.parse_file(file).unwrap();
    fresh
        .store_parsed(&parsed.session, &parsed.messages)
        .unwrap();
    let id = parsed.session.id.clone();
    let (mut a, am) = session_and_messages(store, &id);
    let (mut b, bm) = session_and_messages(&fresh, &id);
    let (ca, cb) = (a.estimated_cost_usd.take(), b.estimated_cost_usd.take());
    match (ca, cb) {
        (Some(x), Some(y)) => assert!((x - y).abs() < 1e-9, "成本 {x} vs {y}"),
        (x, y) => assert_eq!(x, y, "成本"),
    }
    assert_eq!(a, b, "会话行不一致");
    assert_eq!(am.len(), bm.len(), "消息数不一致");
    for (x, y) in am.iter().zip(&bm) {
        assert_eq!(x, y, "消息不一致");
    }
}

#[test]
fn bench_append_5_lines() {
    // 400 轮 × 5 行 = 2,000 行。
    let lines: Vec<String> = (0..400).flat_map(turn).collect();
    assert_eq!(lines.len(), 2_000);
    // 追加 5 行：上一轮回复的续行（同一 message.id）+ 新一轮的前 4 行。
    let mut extra = vec![assistant(
        3_995,
        "a-tail-continuation",
        "msg-399-reply",
        json!({"type": "text", "text": "补充的一段正文"}),
        None,
    )];
    extra.extend(turn(400).into_iter().take(4));

    // 两种情形：app（同步线程持有续读缓存）与 CLI（每次新缓存，先在内存里重放偏移之前的部分）。
    for cached in [true, false] {
        let fx = fixture(&lines);
        let provider = ClaudeCodeProvider::new(fx.root.clone());
        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("bench.db")).unwrap();
        let mut cache = IngestCache::new();

        let t0 = Instant::now();
        let r1 = ingest_with(&mut store, &provider, &mut cache);
        let first = t0.elapsed();
        assert_eq!(r1.parsed, 1);
        let after_first = store.total_changes();
        let (_, before) = store.counts().unwrap();

        write_lines(&fx.file, &extra, true);
        if !cached {
            cache = IngestCache::new();
        }
        let t1 = Instant::now();
        let r2 = ingest_with(&mut store, &provider, &mut cache);
        let second = t1.elapsed();
        assert_eq!((r2.parsed, r2.incremental), (1, 1), "{:?}", r2.fallbacks);
        let changes = store.total_changes() - after_first;
        let (_, after) = store.counts().unwrap();
        println!(
            "bench（{}）：首次入库 {first:?}（{before} 条消息）；追加 5 行后再入库 {second:?}，\
             改动行数（含 FTS 与映射表）{changes}，其中消息 upsert {}、FTS 增删 {}，消息 {before} → {after}",
            if cached {
                "有续读缓存"
            } else {
                "无缓存，重放前缀"
            },
            r2.writes.messages,
            r2.writes.fts,
        );
        // 1 条被续行改动的 assistant + 4 条新消息。
        assert_eq!(r2.writes.messages, 5);
        assert!(changes < 40, "增量入库改动了 {changes} 行");
        assert_same_as_full(&store, &provider, &fx.file);
    }
}

/// Claude 的各种记录：重命名、摘要、成本真值、system 正文、resume 重写的重复 uuid、无 message.id 的回复、
/// `<synthetic>` 占位回复。
fn claude_mixed() -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for t in 0..6 {
        lines.extend(turn(t));
        if t == 1 {
            lines.push(
                json!({"type": "summary", "summary": "摘要标题", "leafUuid": "x"}).to_string(),
            );
        }
        if t == 2 {
            // resume 把前面一段历史原样重写进同一文件。
            lines.extend(turn(0));
            lines.push(
                json!({"type": "system", "subtype": "compact_boundary", "uuid": "sys-1",
                       "timestamp": ts(25), "content": ""})
                .to_string(),
            );
            lines.push(
                json!({"type": "system", "subtype": "informational", "uuid": "sys-2",
                       "timestamp": ts(26), "content": "系统提示正文"})
                .to_string(),
            );
        }
        if t == 3 {
            lines.push(
                json!({"type": "assistant", "uuid": "noid-1", "timestamp": ts(36),
                       "message": {"role": "assistant", "model": "claude-sonnet-4-6",
                                   "content": [{"type": "text", "text": "无 id 的回复"}]}})
                .to_string(),
            );
            lines.push(
                json!({"type": "assistant", "uuid": "syn-1", "timestamp": ts(37),
                       "message": {"id": "msg-syn", "role": "assistant", "model": "<synthetic>",
                                   "content": [{"type": "text", "text": "占位"}],
                                   "usage": {"input_tokens": 0, "output_tokens": 0}}})
                .to_string(),
            );
            lines.push(json!({"type": "cost-state", "totalCostUSD": 1.25}).to_string());
        }
        if t == 4 {
            lines.push(json!({"type": "custom-title", "customTitle": "改过的名字"}).to_string());
            lines.push(json!({"type": "attachment", "foo": 1}).to_string());
        }
    }
    lines
}

/// 任意切分点：先入库前 k 行，再追加其余（分两批），结果与整文件解析一致。有缓存、无缓存两条路径都走一遍。
#[test]
fn claude_incremental_matches_full_at_every_split() {
    let lines = claude_mixed();
    for cached in [true, false] {
        for k in 1..lines.len() {
            let fx = fixture(&lines[..k]);
            let provider = ClaudeCodeProvider::new(fx.root.clone());
            let mut store = Store::in_memory().unwrap();
            let mut cache = IngestCache::new();
            ingest_with(&mut store, &provider, &mut cache);
            let mid = k + (lines.len() - k) / 2;
            for chunk in [&lines[k..mid], &lines[mid..]] {
                if chunk.is_empty() {
                    continue;
                }
                write_lines(&fx.file, chunk, true);
                if !cached {
                    cache = IngestCache::new();
                }
                let r = ingest_with(&mut store, &provider, &mut cache);
                assert_eq!(r.incremental, 1, "k={k}: {:?}", r.fallbacks);
            }
            assert_same_as_full(&store, &provider, &fx.file);
        }
    }
}

/// 只追加一条已有 assistant 的续行：只改那一条消息，会话 usage 与整文件解析一致。
#[test]
fn continuation_line_upserts_only_that_message() {
    let lines: Vec<String> = (0..20).flat_map(turn).collect();
    // 最后一轮回复只写到思考那一行，正文行稍后才到。
    let (head, tail) = lines.split_at(lines.len() - 1);
    let fx = fixture(head);
    let provider = ClaudeCodeProvider::new(fx.root.clone());
    let mut store = Store::in_memory().unwrap();
    let mut cache = IngestCache::new();
    ingest_with(&mut store, &provider, &mut cache);
    write_lines(&fx.file, tail, true);
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!(r.incremental, 1);
    assert_eq!(r.writes.messages, 1, "只应改那条 assistant");
    assert_same_as_full(&store, &provider, &fx.file);
    let msgs = store
        .messages(&store_session_id(&provider, &fx.file))
        .unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last.blocks.len(), 2, "思考 + 正文两行合并成一条");
    assert_eq!(last.stop_reason.as_deref(), Some("end_turn"));
}

fn store_session_id(provider: &dyn SourceProvider, file: &Path) -> String {
    provider.parse_file(file).unwrap().session.id
}

/// 原件被截断（变短）或前段被改写（同 inode、变长）：不能续读，整文件重解析，旧消息不残留。
#[test]
fn truncated_or_rewritten_source_is_reparsed() {
    let lines: Vec<String> = (0..10).flat_map(turn).collect();
    let fx = fixture(&lines);
    let provider = ClaudeCodeProvider::new(fx.root.clone());
    let mut store = Store::in_memory().unwrap();
    let mut cache = IngestCache::new();
    ingest_with(&mut store, &provider, &mut cache);

    write_lines(&fx.file, &lines[..12], false);
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!(r.incremental, 0);
    assert!(
        r.fallbacks.iter().any(|(_, why)| why.contains("变短")),
        "{:?}",
        r.fallbacks
    );
    assert_same_as_full(&store, &provider, &fx.file);

    // 同一个文件被改写成别的内容并且更长（inode 不变）：首尾指纹不符。
    let other: Vec<String> = (50..60).flat_map(turn).collect();
    write_lines(&fx.file, &other, false);
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!(r.incremental, 0);
    assert!(
        r.fallbacks.iter().any(|(_, why)| why.contains("首尾")),
        "{:?}",
        r.fallbacks
    );
    assert_same_as_full(&store, &provider, &fx.file);
}

/// 写了一半的末行（不是完整 JSON）不消费、不报坏行；写完后接着读。
#[test]
fn half_written_last_line_waits_for_the_rest() {
    let lines: Vec<String> = (0..4).flat_map(turn).collect();
    let fx = fixture(&lines);
    let provider = ClaudeCodeProvider::new(fx.root.clone());
    let mut store = Store::in_memory().unwrap();
    let mut cache = IngestCache::new();
    ingest_with(&mut store, &provider, &mut cache);

    let next = user(1_000, "后来的问题");
    let (a, b) = next.split_at(next.len() / 2);
    append_raw(&fx.file, a.as_bytes());
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!(r.warnings, 0, "半行不该当坏行");
    append_raw(&fx.file, format!("{b}\n").as_bytes());
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!((r.incremental, r.warnings), (1, 0));
    assert_eq!(r.writes.messages, 1);
    assert_same_as_full(&store, &provider, &fx.file);
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
    let t = std::time::SystemTime::now() + std::time::Duration::from_millis(5);
    f.set_modified(t).unwrap();
}

// ---------------------------------------------------------------- Codex

const CODEX_ID: &str = "01a1aa6a-39e1-7b82-b249-204e4e4100aa";

fn cx(sec: usize, ty: &str, payload: Value) -> String {
    json!({"timestamp": ts(sec), "type": ty, "payload": payload}).to_string()
}

fn cx_usage(input: u64, cached: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "output_tokens": output,
           "reasoning_output_tokens": 1, "total_tokens": input + output})
}

/// 6 轮对话。`records`：每轮带 token_usage_record；第 0 轮先只有 token_count（迫使口径中途切换）。
fn codex_lines(records: bool) -> Vec<String> {
    let mut v = vec![
        cx(
            0,
            "session_meta",
            json!({"id": CODEX_ID, "timestamp": ts(0), "cwd": "/tmp/cx",
            "originator": "codex_cli_rs", "cli_version": "0.99.0", "git": {"branch": "main"}}),
        ),
        cx(
            1,
            "response_item",
            json!({"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "dev"}]}),
        ),
        cx(
            2,
            "turn_context",
            json!({"cwd": "/tmp/cx", "model": "gpt-5-codex"}),
        ),
    ];
    let mut total = (0u64, 0u64, 0u64);
    for t in 0..6 {
        let b = 10 + t * 10;
        v.push(cx(
            b,
            "response_item",
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": format!("第 {t} 个问题")}]}),
        ));
        v.push(cx(
            b + 1,
            "response_item",
            json!({"type": "reasoning",
            "summary": [{"type": "summary_text", "text": format!("想 {t}")}]}),
        ));
        v.push(cx(
            b + 2,
            "response_item",
            json!({"type": "function_call", "name": "shell",
            "call_id": format!("c{t}"), "arguments": "{\"cmd\":[\"ls\"]}"}),
        ));
        total = (total.0 + 100, total.1 + 60, total.2 + 10);
        v.push(cx(
            b + 3,
            "event_msg",
            json!({"type": "token_count", "info": {
            "total_token_usage": cx_usage(total.0, total.1, total.2),
            "last_token_usage": cx_usage(100, 60, 10)}}),
        ));
        if records && t > 0 {
            v.push(cx(
                b + 3,
                "token_usage_record",
                json!({"response_id": format!("r{t}a"),
                "usage": cx_usage(100, 60, 10)}),
            ));
        }
        v.push(cx(
            b + 4,
            "response_item",
            json!({"type": "function_call_output",
            "call_id": format!("c{t}"), "output": "ok"}),
        ));
        v.push(cx(
            b + 5,
            "response_item",
            json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": format!("答 {t}")}]}),
        ));
        v.push(cx(
            b + 6,
            "response_item",
            json!({"type": "reasoning", "summary": []}),
        ));
        total = (total.0 + 50, total.1 + 20, total.2 + 5);
        v.push(cx(
            b + 7,
            "event_msg",
            json!({"type": "token_count", "info": {
            "total_token_usage": cx_usage(total.0, total.1, total.2),
            "last_token_usage": cx_usage(50, 20, 5)}, "rate_limits": null}),
        ));
        if records && t > 0 {
            v.push(cx(
                b + 7,
                "token_usage_record",
                json!({"response_id": format!("r{t}b"),
                "usage": cx_usage(50, 20, 5)}),
            ));
        }
    }
    v
}

fn codex_fixture(lines: &[String]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let day = root.join("2026").join("09").join("21");
    std::fs::create_dir_all(&day).unwrap();
    let file = day.join(format!("rollout-2026-09-21T10-00-00-{CODEX_ID}.jsonl"));
    write_lines(&file, lines, false);
    (dir, root, file)
}

#[test]
fn codex_incremental_matches_full_at_every_split() {
    for records in [false, true] {
        let lines = codex_lines(records);
        for cached in [true, false] {
            for k in 1..lines.len() {
                let (_dir, root, file) = codex_fixture(&lines[..k]);
                let provider = CodexProvider::new(root);
                let mut store = Store::in_memory().unwrap();
                let mut cache = IngestCache::new();
                ingest_with(&mut store, &provider, &mut cache);
                let mid = k + (lines.len() - k) / 2;
                for chunk in [&lines[k..mid], &lines[mid..]] {
                    if chunk.is_empty() {
                        continue;
                    }
                    write_lines(&file, chunk, true);
                    if !cached {
                        cache = IngestCache::new();
                    }
                    let r = ingest_with(&mut store, &provider, &mut cache);
                    assert_eq!(
                        r.incremental, 1,
                        "records={records} k={k}: {:?}",
                        r.fallbacks
                    );
                }
                assert_same_as_full(&store, &provider, &file);
            }
        }
    }
}

/// 末行是完整 JSON 但还没写换行：先消费；之后补上的换行不占行号（Codex 的消息 id 是行号）。
#[test]
fn codex_unterminated_complete_line_keeps_line_numbers() {
    let lines = codex_lines(true);
    let (_dir, root, file) = codex_fixture(&lines[..5]);
    append_raw(&file, lines[5].as_bytes());
    let provider = CodexProvider::new(root);
    let mut store = Store::in_memory().unwrap();
    let mut cache = IngestCache::new();
    ingest_with(&mut store, &provider, &mut cache);
    append_raw(&file, b"\n");
    write_lines(&file, &lines[6..], true);
    let r = ingest_with(&mut store, &provider, &mut cache);
    assert_eq!(r.incremental, 1, "{:?}", r.fallbacks);
    assert_same_as_full(&store, &provider, &file);
}

/// Codex 把冷文件压成 `.jsonl.zst`（改名）、续写时再解压回 `.jsonl`（新 inode）：
/// 两次都整文件重解析，会话不重复、内容不变。
#[test]
fn codex_zst_rename_round_trip() {
    let lines = codex_lines(true);
    let (_dir, root, file) = codex_fixture(&lines[..20]);
    let provider = CodexProvider::new(root.clone());
    let mut store = Store::in_memory().unwrap();
    let mut cache = IngestCache::new();
    let list: [&dyn SourceProvider; 1] = [&provider];
    let r = ingest_paths(
        &mut store,
        &list,
        std::slice::from_ref(&file),
        None,
        &mut cache,
        &mut |_| {},
    );
    assert_eq!(r.parsed, 1);
    let counts = store.counts().unwrap();

    let zst = file.with_extension("jsonl.zst");
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(&zst, zstd::encode_all(&bytes[..], 3).unwrap()).unwrap();
    std::fs::remove_file(&file).unwrap();
    let r = ingest_paths(
        &mut store,
        &list,
        &[file.clone(), zst.clone()],
        None,
        &mut cache,
        &mut |_| {},
    );
    assert_eq!((r.scanned, r.parsed, r.incremental), (1, 1, 0));
    assert_eq!(
        store.counts().unwrap(),
        counts,
        "压缩改名不该多出会话或消息"
    );
    assert_same_as_full(&store, &provider, &zst);

    // 解压回 .jsonl 并续写：新文件（新 inode），整文件解析，然后能接着增量。
    std::fs::write(&file, &bytes).unwrap();
    std::fs::remove_file(&zst).unwrap();
    write_lines(&file, &lines[20..30], true);
    let r = ingest_paths(
        &mut store,
        &list,
        &[file.clone(), zst],
        None,
        &mut cache,
        &mut |_| {},
    );
    assert_eq!((r.parsed, r.incremental), (1, 0), "{:?}", r.fallbacks);
    assert_same_as_full(&store, &provider, &file);
    write_lines(&file, &lines[30..], true);
    let r = ingest_paths(
        &mut store,
        &list,
        std::slice::from_ref(&file),
        None,
        &mut cache,
        &mut |_| {},
    );
    assert_eq!(r.incremental, 1, "{:?}", r.fallbacks);
    assert_same_as_full(&store, &provider, &file);
}

/// 监听给的路径里混着锁文件、旧副本、目录外的文件：都不入库，也不触发目录遍历。
#[test]
fn ingest_paths_ignores_noise() {
    let lines: Vec<String> = (0..3).flat_map(turn).collect();
    let fx = fixture(&lines);
    let provider = ClaudeCodeProvider::new(fx.root.clone());
    let proj = fx.file.parent().unwrap().to_path_buf();
    let superseded = proj.join(format!("{SESSION}.jsonl.superseded-1700000000"));
    std::fs::copy(&fx.file, &superseded).unwrap();
    let lock = proj.join("x.jsonl.lock");
    std::fs::write(&lock, b"").unwrap();
    let mut store = Store::in_memory().unwrap();
    let list: [&dyn SourceProvider; 1] = [&provider];
    let r = ingest_paths(
        &mut store,
        &list,
        &[
            superseded,
            lock,
            PathBuf::from("/elsewhere/a.jsonl"),
            fx.file.clone(),
        ],
        None,
        &mut IngestCache::new(),
        &mut |_| {},
    );
    assert_eq!((r.scanned, r.parsed), (1, 1));
    assert_eq!(store.counts().unwrap().0, 1);
}

/// 真实库对比（默认忽略）：`REAL_DB` 指向真实库的**副本**，`REAL_SRC` 指向一个真实的 Claude 会话原件。
/// 原件拷进临时来源目录、副本里的 sources 路径改指过去，先入库一次到稳态，再追加 1 行计时。
#[test]
#[ignore]
fn real_db_append_one_line() {
    let (Ok(db), Ok(src)) = (std::env::var("REAL_DB"), std::env::var("REAL_SRC")) else {
        eprintln!("未设置 REAL_DB / REAL_SRC，跳过");
        return;
    };
    let src = PathBuf::from(src);
    let projects = dirs_projects();
    let rel = src
        .strip_prefix(&projects)
        .expect("REAL_SRC 要在 ~/.claude/projects 下");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let file = root.join(rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::copy(&src, &file).unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE sources SET path = ?1 WHERE path = ?2",
            [file.to_string_lossy(), src.to_string_lossy()],
        )
        .unwrap();
    }
    let provider = ClaudeCodeProvider::new(root.clone());
    let t = Instant::now();
    let mut store = Store::open(Path::new(&db)).unwrap();
    println!("real: 打开库（含一次性迁移）{:?}", t.elapsed());
    let only: Vec<PathBuf> = vec![file.clone()];
    let mut cache = IngestCache::new();
    // 第 0 次：老库没有续读位置，整文件解析一次；之后每次只读新增的一行。第 3 次模拟 app 重启（缓存清空）。
    for round in 0..5 {
        if round == 3 {
            cache = IngestCache::new();
        }
        let c0 = store.total_changes();
        let line =
            json!({"type": "user", "uuid": format!("bench-extra-{round}"), "parentUuid": null,
            "timestamp": "2026-11-09T12:00:00.000Z", "sessionId": "x",
            "message": {"role": "user", "content": format!("追加的一行 {round}")}})
            .to_string();
        write_lines(&file, &[line], true);
        let t = Instant::now();
        let r = ingest_paths_or_all(&mut store, &provider, &only, &mut cache);
        println!(
            "real: 第 {round} 次追加 1 行后入库 {:?}，改动行数（含 FTS 与映射表）{}，增量 {}，消息 upsert {}",
            t.elapsed(),
            store.total_changes() - c0,
            r.incremental,
            r.writes.messages,
        );
    }
}

fn dirs_projects() -> PathBuf {
    agenthome_core::paths::claude_projects_dir()
}

fn ingest_paths_or_all(
    store: &mut Store,
    provider: &ClaudeCodeProvider,
    paths: &[PathBuf],
    cache: &mut IngestCache,
) -> agenthome_core::Report {
    let list: [&dyn SourceProvider; 1] = [provider];
    let r = ingest_paths(store, &list, paths, None, cache, &mut |_| {});
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    r
}

/// raw 层对比（默认忽略）：`REAL_SRC` 拷进临时目录归档一次，再逐次追加 1 行计时。
/// 第 1 次追加要重读前缀重算 sha256（进程内还没有中间状态），之后只读新增字节。
#[test]
#[ignore]
fn real_raw_append_one_line() {
    let Ok(src) = std::env::var("REAL_SRC") else {
        eprintln!("未设置 REAL_SRC，跳过");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    let file = root.join("-Users-x-proj").join("s.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::copy(&src, &file).unwrap();
    let raw = agenthome_core::RawArchive::open(tmp.path().join("raw")).unwrap();
    let provider = agenthome_core::model::ProviderId::ClaudeCode;
    let t = Instant::now();
    raw.archive(&provider, &root, &file).unwrap();
    println!("raw: 首次归档 {:?}", t.elapsed());
    for round in 0..4 {
        write_lines(&file, &[user(round, "追加")], true);
        let t = Instant::now();
        let outcome = raw.archive(&provider, &root, &file).unwrap();
        println!(
            "raw: 第 {round} 次追加后归档 {:?}（{outcome:?}）",
            t.elapsed()
        );
    }
}

/// Codex 归档会话会被挪到 `~/.codex/archived_sessions/`（平铺、`.zst`）。不扫它就漏掉这些会话的用量
/// （真实库 42 个、约 $47.5）。同一会话两处都有时以 `sessions/` 为准，不能重复计。
#[test]
fn codex_archived_sessions_are_ingested_once() {
    let lines = codex_lines(true);
    let (dir, root, live) = codex_fixture(&lines);
    let archive = dir.path().join("archived_sessions");
    std::fs::create_dir_all(&archive).unwrap();
    // 一个只在归档目录里的会话。
    let other_id = "01a1aa6a-39e1-7b82-b249-204e4e4100bb";
    let body: String = lines
        .iter()
        .map(|l| l.replace(CODEX_ID, other_id) + "\n")
        .collect();
    let archived = archive.join(format!("rollout-2026-09-22T10-00-00-{other_id}.jsonl.zst"));
    std::fs::write(&archived, zstd::encode_all(body.as_bytes(), 3).unwrap()).unwrap();
    // 一个与 sessions/ 里重复的会话（归档过程中两处并存）。
    let dup = archive.join(live.file_name().unwrap());
    std::fs::copy(&live, &dup).unwrap();

    let provider = CodexProvider::with_archive(root, Some(archive.clone()));
    let d = provider.discover();
    assert_eq!(d.files.len(), 2, "{:?}", d.files);
    assert!(d.files.contains(&archived));
    assert!(!d.files.contains(&dup));
    assert_eq!(provider.accepts(&dup), Some(false));
    assert_eq!(provider.accepts(&archived), Some(true));

    let raw = agenthome_core::RawArchive::open(dir.path().join("raw")).unwrap();
    let mut store = Store::in_memory().unwrap();
    let list: [&dyn SourceProvider; 1] = [&provider];
    let r = agenthome_core::ingest_all(&mut store, &list, Some(&raw), &mut |_| {});
    assert!(r.failed.is_empty() && r.raw.failed.is_empty(), "{r:?}");
    assert_eq!((r.parsed, r.raw.created), (2, 2));
    assert_eq!(store.counts().unwrap().0, 2);
    assert_same_as_full(&store, &provider, &archived);
}
