//! 远程用量投影（CARM-291，方案单元 3）：Gemini CLI、Antigravity CLI（agy）→ 协议 v1 记录。
//!
//! 为什么要这些用例：Mac 端按投影记录逐条计价、按来源 / 厂商分组。
//! - Gemini 的 `input` 含缓存读、`thoughts` 另算，口径一错，远程看到的缓存和思考 token 就和本机对不上；
//! - `.jsonl` 同 id 重写、`$rewindTo` 回滚的请求不能多算，也不能少算；
//! - 来源标签要分清 Gemini CLI 和 agy，厂商按实际模型归（agy 跑的 Gemini 归 Google），
//!   否则首页「按来源 / 厂商」两个维度会串；
//! - 投影的 token 与成本必须等于本机入库后的统计。
//!
//! agy 的合成 fixture 需要 protobuf 编码器（只在单元测试里有），那部分用例在 `antigravity_cli.rs`。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::{Period, Usage};
use agenthome_core::providers::SourceProvider;
use agenthome_core::usage_projection::{dedup, records};
use agenthome_core::usage_v1::{UsagePage, UsageRecord, VERSION};
use agenthome_core::{AntigravityCliProvider, GeminiProvider, Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const LEGACY_ID: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
const NEW_ID: &str = "22222222-aaaa-4bbb-8ccc-000000000002";
const MODEL: &str = "gemini-3-pro-preview";

fn identity(dir: &Path) -> DeviceIdentity {
    DeviceIdentity::load_or_create(&dir.join("device.json"), "box").unwrap()
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn tokens(input: u64, output: u64, cached: u64, thoughts: u64, tool: u64) -> Value {
    json!({"input": input, "output": output, "cached": cached, "thoughts": thoughts,
           "tool": tool, "total": input + output + thoughts + tool})
}

/// 旧版整文件 JSON：一条带 id、一条没有 id（旧记录）的 gemini 回复，同一毫秒。
fn legacy() -> String {
    json!({
        "sessionId": LEGACY_ID, "startTime": "2026-01-24T11:00:00.000Z",
        "messages": [
            {"id": "m1", "timestamp": "2026-01-24T11:00:01.000Z", "type": "user", "content": "SECRET-PROMPT-7f3a"},
            {"id": "msg-a2", "timestamp": "2026-01-24T11:00:02.000Z", "type": "gemini", "content": "SECRET-REPLY-7f3a",
             "model": MODEL, "tokens": tokens(1000, 20, 600, 50, 7)},
            {"timestamp": "2026-01-24T11:00:02.000Z", "type": "gemini", "content": "没有 id 的旧记录",
             "model": MODEL, "tokens": tokens(1200, 10, 1000, 0, 0)}
        ]
    })
    .to_string()
}

/// 新版 jsonl：g1 先写无 tokens、再同 id 补 tokens（只算一次）；g2 被 `$rewindTo` 回滚（不算）。
fn jsonl() -> String {
    [
        json!({"sessionId": NEW_ID, "startTime": "2026-11-01T08:00:00.000Z", "kind": "main"}),
        json!({"id": "u1", "timestamp": "2026-11-01T08:00:01.000Z", "type": "user", "content": "画个图"}),
        json!({"id": "g1", "timestamp": "2026-11-01T08:00:02.000Z", "type": "gemini", "content": "",
               "model": "gemini-3-flash"}),
        json!({"id": "g1", "timestamp": "2026-11-01T08:00:02.000Z", "type": "gemini", "content": "写好了。",
               "model": "gemini-3-flash", "tokens": tokens(500, 30, 100, 10, 0)}),
        json!({"id": "u2", "timestamp": "2026-11-01T08:01:00.000Z", "type": "user", "content": "这一轮会被回滚"}),
        json!({"id": "g2", "timestamp": "2026-11-01T08:01:01.000Z", "type": "gemini", "content": "被回滚",
               "model": "gemini-3-flash", "tokens": tokens(9999, 9999, 0, 0, 0)}),
        json!({"$rewindTo": "u2"}),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect()
}

fn tree(root: &Path) -> (PathBuf, PathBuf) {
    let legacy_path = root.join("tmp/hash-a/chats/session-2026-01-24T11-00-11111111.json");
    let jsonl_path = root.join("tmp/proj-b/chats/session-2026-11-01T08-00-22222222.jsonl");
    write(&legacy_path, &legacy());
    write(&jsonl_path, &jsonl());
    (legacy_path, jsonl_path)
}

fn project(provider: &dyn SourceProvider, id: &DeviceIdentity) -> Vec<UsageRecord> {
    let mut all = Vec::new();
    for p in provider.discover().files {
        let file = provider.usage_events(&p).unwrap().expect("来源已接入投影");
        let (recs, warnings) = records(id, &provider.id(), &file);
        assert!(warnings.is_empty(), "{warnings:?}");
        all.extend(recs);
    }
    dedup(all)
}

fn as_usage(r: &UsageRecord) -> Usage {
    Usage {
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cache_read_tokens: r.cache_read_tokens,
        cache_write_tokens: r.cache_write_5m_tokens + r.cache_write_1h_tokens,
        cache_write_5m_tokens: r.cache_write_5m_tokens,
        cache_write_1h_tokens: r.cache_write_1h_tokens,
        reasoning_tokens: r.reasoning_tokens,
        requests: 1,
        input_includes_cache: false,
    }
}

/// 投影记录与本机入库统计逐项对比：请求数、token 各分量（含缓存读、思考）、等价成本。
fn assert_matches_local(provider: &dyn SourceProvider, recs: &[UsageRecord]) {
    let mut store = Store::in_memory().unwrap();
    let list: [&dyn SourceProvider; 1] = [provider];
    let report = agenthome_core::ingest_all(&mut store, &list, None, &mut |_| {});
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let local = store
        .stats_totals(Period::All, Some(&provider.id()))
        .unwrap();
    let sum = |f: fn(&UsageRecord) -> u64| recs.iter().map(f).sum::<u64>();
    let u = local.usage;
    assert_eq!(sum(|r| u64::from(r.requests)), u.requests, "请求数");
    assert_eq!(sum(|r| r.input_tokens), u.input_tokens, "新鲜输入");
    assert_eq!(sum(|r| r.output_tokens), u.output_tokens, "输出（含思考）");
    assert_eq!(sum(|r| r.cache_read_tokens), u.cache_read_tokens, "缓存读");
    assert_eq!(
        sum(|r| r.cache_write_5m_tokens + r.cache_write_1h_tokens),
        u.cache_write_tokens,
        "缓存写"
    );
    assert_eq!(sum(|r| r.reasoning_tokens), u.reasoning_tokens, "思考");
    let cost: f64 = recs
        .iter()
        .filter_map(|r| agenthome_core::pricing::estimate_usd(r.model.as_deref()?, &as_usage(r)))
        .sum();
    assert!(
        (cost - local.cost_usd).abs() < 1e-9,
        "成本 {cost} vs 本地 {}",
        local.cost_usd
    );
}

#[test]
fn gemini_cli_cache_and_thoughts_match_local() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("gemini");
    tree(&root);
    let id = identity(dir.path());
    let provider = GeminiProvider::new(root);
    let recs = project(&provider, &id);

    // 3 次请求：m2、无 id 的旧记录（与 m2 同毫秒，各自计入）、g1（重写只算一次）；回滚的 g2 不算。
    assert_eq!(recs.len(), 3, "{recs:#?}");
    for r in &recs {
        r.validate().unwrap();
        assert_eq!(r.source, "gemini-cli", "来源标签");
        assert_eq!(r.vendor.as_deref(), Some("google"));
        assert_eq!(r.requests, 1);
    }
    let m2 = recs
        .iter()
        .find(|r| r.output_tokens == 20 + 50)
        .expect("m2");
    // input 1000 含缓存读 600，另有工具 7：新鲜输入 = 1000 − 600 + 7。
    assert_eq!(m2.input_tokens, 1000 - 600 + 7);
    assert_eq!(m2.cache_read_tokens, 600);
    assert_eq!(m2.reasoning_tokens, 50, "思考是输出的子项");
    assert_eq!(m2.model.as_deref(), Some(MODEL));
    let g1 = recs
        .iter()
        .find(|r| r.model.as_deref() == Some("gemini-3-flash"))
        .expect("g1");
    assert_eq!(
        (
            g1.input_tokens,
            g1.cache_read_tokens,
            g1.output_tokens,
            g1.reasoning_tokens
        ),
        (400, 100, 40, 10)
    );
    assert_eq!(
        recs.iter().filter(|r| r.ts_ms == m2.ts_ms).count(),
        2,
        "同一毫秒的两个独立请求都计入"
    );

    assert_matches_local(&provider, &recs);
}

/// 重读、挪目录：事件键不变（否则重扫一次就翻倍）；协议页里没有正文和路径。
#[test]
fn gemini_keys_stable_and_payload_has_no_content() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let a = dir.path().join("a");
    tree(&a);
    let first = project(&GeminiProvider::new(a.clone()), &id);
    let again = project(&GeminiProvider::new(a.clone()), &id);
    let b = dir.path().join("b");
    std::fs::rename(&a, &b).unwrap();
    std::fs::rename(b.join("tmp/hash-a"), b.join("tmp/slug-renamed")).unwrap();
    let moved = project(&GeminiProvider::new(b), &id);
    let keys = |v: &[UsageRecord]| {
        let mut k: Vec<String> = v.iter().map(|r| r.event_key.clone()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(&first), keys(&again));
    assert_eq!(keys(&first), keys(&moved));

    let page = UsagePage {
        v: VERSION,
        machine_id: id.machine_id().to_string(),
        records: first,
    };
    let text = String::from_utf8(page.encode().unwrap()).unwrap();
    for leak in ["SECRET", LEGACY_ID, NEW_ID, "hash-a", "chats", "msg-a2"] {
        assert!(!text.contains(leak), "载荷里出现了 {leak}");
    }
}

/// Antigravity 桌面版 `.pb` 解不开：投影为空但必须带警告，不能静默算 0。
#[test]
fn antigravity_desktop_pb_warns_instead_of_silent_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("gemini");
    let pb = root.join("antigravity/conversations/44444444-aaaa-4bbb-8ccc-000000000004.pb");
    write(&pb, "\u{1}\u{2}\u{3}");
    let provider = GeminiProvider::new(root);
    let file = SourceProvider::usage_events(&provider, &pb)
        .unwrap()
        .expect("来源已接入投影");
    assert!(file.events.is_empty());
    assert!(
        file.warnings.iter().any(|w| w.contains("无法解码")),
        "{:?}",
        file.warnings
    );
}

/// 真实数据核对（`cargo test -p agenthome-core --test usage_projection_gemini -- --ignored --nocapture`）：
/// `REAL_GEMINI=<~/.gemini 的快照>`、`REAL_AGY=<~/.gemini/antigravity-cli 的快照>`。
/// 用快照而不是活目录：agy 正在写的库两次读之间会变。
#[test]
#[ignore]
fn real_data_matches_local_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let check = |provider: &dyn SourceProvider| {
        let files = provider.discover().files;
        let mut all = Vec::new();
        let mut warned = 0;
        for p in &files {
            let file = provider.usage_events(p).unwrap().unwrap();
            let (recs, warnings) = records(&id, &provider.id(), &file);
            warned += warnings.len();
            all.extend(recs);
        }
        let raw = all.len();
        let recs = dedup(all);
        assert_eq!(raw, recs.len(), "同一来源里不该有重复的请求标识");
        let mut by_model: std::collections::BTreeMap<(String, String), (u64, u64, u64)> =
            Default::default();
        for r in &recs {
            let e = by_model
                .entry((
                    r.vendor.clone().unwrap_or_else(|| "未知".into()),
                    r.model.clone().unwrap_or_else(|| "未知".into()),
                ))
                .or_default();
            e.0 += 1;
            e.1 += r.cache_read_tokens;
            e.2 += r.reasoning_tokens;
        }
        eprintln!(
            "{}: {} 个文件，{} 条用量，投影警告 {warned} 条",
            provider.id().as_str(),
            files.len(),
            recs.len()
        );
        for ((vendor, model), (n, cache, thoughts)) in by_model {
            eprintln!("  {vendor} / {model}: 请求 {n}，缓存读 {cache}，思考 {thoughts}");
        }
        assert_matches_local(provider, &recs);
        eprintln!("  请求数、各类 token、成本与本机入库逐项一致");
    };
    let mut ran = false;
    if let Ok(root) = std::env::var("REAL_GEMINI") {
        check(&GeminiProvider::new(root.into()));
        ran = true;
    }
    if let Ok(root) = std::env::var("REAL_AGY") {
        check(&AntigravityCliProvider::new(root.into()));
        ran = true;
    }
    if !ran {
        eprintln!("未设置 REAL_GEMINI / REAL_AGY，跳过");
    }
}
