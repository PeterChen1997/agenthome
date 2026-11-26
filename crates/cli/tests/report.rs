//! `agenthome report` 对账表（CARM-298）。
//!
//! 为什么要这个用例：用户拿这条命令在 Mac 和 Runner 上各跑一遍来对账。命令行这层只做三件事——
//! 解析起止时间、按连接名找到机器、读两个库——任何一步错了（时区、开闭区间、连接名对错机器、
//! 本机库被当成远端再读一遍），输出的分组和金额就和真实用量对不上。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::ProviderId;
use agenthome_core::usage_import::{ImportStore, PageCursor};
use agenthome_core::usage_store::{SourceProgress, UsageStore};
use agenthome_core::usage_v1::{EventBasis, Op, UsagePage, UsageRecord, VERSION, event_key};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

// 2026-11-19T00:00:00Z
const T0: i64 = 1_795_046_400_000;
const HOUR: i64 = 3_600_000;

fn record(
    id: &DeviceIdentity,
    key: &str,
    source: ProviderId,
    model: &str,
    ts_ms: i64,
) -> UsageRecord {
    UsageRecord {
        v: VERSION,
        event_key: event_key(id, &source, EventBasis::Request(key)),
        revision: 1,
        op: Op::Upsert,
        machine_id: id.machine_id().to_string(),
        source: source.as_str().to_string(),
        vendor: None,
        model: Some(model.to_string()),
        ts_ms,
        requests: 1,
        input_tokens: 1_000,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_write_5m_tokens: 0,
        cache_write_1h_tokens: 0,
        reasoning_tokens: 0,
    }
}

fn commit(store: &mut UsageStore, source: ProviderId, records: &[UsageRecord]) {
    let path = format!("/src/{}.jsonl", source.as_str());
    store
        .commit_file(
            &source,
            Path::new(&path),
            SourceProgress {
                size: 1,
                mtime_ms: 1,
            },
            records,
        )
        .unwrap();
}

/// Mac 数据目录：本机用量库 + 导入了一台 Runner 的导入库。
fn fixture() -> tempfile::TempDir {
    let data = tempfile::tempdir().unwrap();
    let mac = DeviceIdentity::load_or_create(&data.path().join("device.json"), "mac").unwrap();
    let mut local = UsageStore::open(&data.path().join("usage").join("usage.db"), &mac).unwrap();
    commit(
        &mut local,
        ProviderId::Codex,
        &[
            record(&mac, "a", ProviderId::Codex, "gpt-5-codex", T0),
            record(&mac, "b", ProviderId::Codex, "gpt-5-codex", T0 + HOUR),
            // 结束时刻，不在窗口内。
            record(&mac, "c", ProviderId::Codex, "gpt-5-codex", T0 + 24 * HOUR),
        ],
    );

    let runner_dir = tempfile::tempdir().unwrap();
    let runner =
        DeviceIdentity::load_or_create(&runner_dir.path().join("device.json"), "mac").unwrap();
    let mut rstore = UsageStore::in_memory(&runner).unwrap();
    commit(
        &mut rstore,
        ProviderId::ClaudeCode,
        &[record(
            &runner,
            "r1",
            ProviderId::ClaudeCode,
            "claude-opus-4-5",
            T0,
        )],
    );
    commit(
        &mut rstore,
        ProviderId::OpenCode,
        &[record(
            &runner,
            "r2",
            ProviderId::OpenCode,
            "acme-unreleased-9",
            T0,
        )],
    );
    let mut import = ImportStore::open(&data.path().join("remote").join("usage.db"), &mac).unwrap();
    import.pair("runner-a", runner.machine_id(), T0).unwrap();
    let changes = rstore.changes_since(0, usize::MAX).unwrap();
    let page = UsagePage {
        v: VERSION,
        machine_id: runner.machine_id().to_string(),
        records: changes.iter().map(|c| c.record.clone()).collect(),
    };
    let cursor = PageCursor {
        store_id: rstore.store_id().to_string(),
        after: 0,
        through: changes.last().unwrap().seq,
    };
    import
        .import_page("runner-a", &cursor, &page.encode().unwrap())
        .unwrap();
    data
}

fn run(data: &Path, machine: &str) -> Result<Value, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_agenthome"))
        .args([
            "report",
            "--from",
            "2026-11-19T00:00:00Z",
            // 同一时刻的另一种写法：时区换算错了窗口就会偏 8 小时。
            "--to",
            "2026-11-20T08:00:00+08:00",
            "--machine",
            machine,
            "--data-dir",
        ])
        .arg(data)
        .output()
        .unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap())
}

fn keys(v: &Value) -> Vec<String> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}|{}",
                r["machine"].as_str().unwrap(),
                r["source"].as_str().unwrap(),
                r["vendor"].as_str().unwrap(),
                r["model"].as_str().unwrap(),
                r["requests"]
            )
        })
        .collect()
}

#[test]
fn report_groups_by_machine_source_vendor_model() {
    let data = fixture();
    let all = run(data.path(), "all").unwrap();
    assert_eq!(
        keys(&all),
        [
            "local|codex|openai|gpt-5-codex|2",
            "runner-a|claude-code|anthropic|claude-opus-4-5|1",
            "runner-a|opencode|unknown|acme-unreleased-9|1",
        ]
    );
    assert_eq!(all["from"], "2026-11-19T00:00:00.000Z");
    assert_eq!(all["to"], "2026-11-20T00:00:00.000Z");
    assert_eq!(all["from_ms"], T0);
    assert!(
        all["pricing_version"]
            .as_str()
            .unwrap()
            .starts_with("litellm-")
    );

    // 缺价：金额为 null（未定价），token 和未知价请求数照列。
    let unknown = &all["rows"][2];
    assert_eq!(unknown["cost_usd"], Value::Null);
    assert_eq!(unknown["unpriced_requests"], 1);
    assert_eq!(unknown["unpriced_tokens"], 1_100);
    assert_eq!(all["totals"]["unpriced_requests"], 1);
    assert_eq!(all["totals"]["requests"], 4);

    // 按机器筛选：本机、按连接名指定的远端，合起来正好是全部。
    let local = run(data.path(), "local").unwrap();
    let runner = run(data.path(), "runner-a").unwrap();
    assert_eq!(keys(&local), keys(&all)[..1]);
    assert_eq!(keys(&runner), keys(&all)[1..]);
    let cost = |v: &Value| v["totals"]["cost_usd"].as_f64().unwrap();
    assert!((cost(&local) + cost(&runner) - cost(&all)).abs() < 1e-12);

    // 写错连接名要报错，不能输出一张空表。
    let err = run(data.path(), "runner-x").unwrap_err();
    assert!(err.contains("runner-x"), "{err}");
}
