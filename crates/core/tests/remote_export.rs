//! 受限 SSH 导出的分页（CARM-295，方案单元 9）。
//!
//! 为什么要这些用例：Mac 每次只经 SSH 取一页，靠 `next` 续读。
//! - 页超过 256 KiB 会被 Mac 整页拒收，那台机器的同步就永远卡在这一页；
//! - 游标断档或重叠会漏账 / 重复计费，所以按 `next` 一页页读完必须正好覆盖每个键的最新修订；
//! - 修订会让旧键换到更大的 `seq`，翻页途中发生修订也不能丢。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::ProviderId;
use agenthome_core::remote_ssh::{ExportPage, export_page};
use agenthome_core::usage_import::ImportStore;
use agenthome_core::usage_store::{SourceProgress, UsageStore};
use agenthome_core::usage_v1::{self, EventBasis, Op, UsageRecord, event_key};
use std::collections::HashMap;
use std::path::Path;

fn record(id: &DeviceIdentity, n: u64, output: u64) -> UsageRecord {
    UsageRecord {
        v: usage_v1::VERSION,
        event_key: event_key(
            id,
            &ProviderId::ClaudeCode,
            EventBasis::Request(&format!("msg_{n}")),
        ),
        revision: 1,
        op: Op::Upsert,
        machine_id: id.machine_id().to_string(),
        source: "claude-code".into(),
        vendor: Some("anthropic".into()),
        model: Some("claude-opus-4-5-20251101".into()),
        ts_ms: 1_763_500_000_000 + n as i64,
        requests: 1,
        input_tokens: 12,
        output_tokens: output,
        cache_read_tokens: 50_000,
        cache_write_5m_tokens: 1_200,
        cache_write_1h_tokens: 800,
        reasoning_tokens: 0,
    }
}

fn commit(store: &mut UsageStore, file: &str, records: &[UsageRecord], size: u64) {
    store
        .commit_file(
            &ProviderId::ClaudeCode,
            Path::new(file),
            SourceProgress { size, mtime_ms: 1 },
            records,
        )
        .unwrap();
}

/// 按 `next` 一页页读到 `more == false`，每页都过 Mac 端的解码；返回每个键最后见到的记录。
fn drain(store: &UsageStore, mut after: u64) -> (HashMap<String, UsageRecord>, usize, u64) {
    let mut seen = HashMap::new();
    let mut pages = 0;
    loop {
        let page = export_page(store, after).unwrap();
        let bytes = page.encode().unwrap();
        assert!(bytes.len() <= usage_v1::MAX_PAGE_BYTES);
        let page = ExportPage::decode(&bytes).unwrap();
        assert_eq!(page.after, after);
        pages += 1;
        for r in page.page.records {
            seen.insert(r.event_key.clone(), r);
        }
        after = page.next;
        if !page.more {
            return (seen, pages, after);
        }
    }
}

#[test]
fn pages_stay_under_limit_and_cover_every_key_once() {
    let dir = tempfile::tempdir().unwrap();
    let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "box").unwrap();
    let mut store = UsageStore::in_memory(&id).unwrap();

    let empty = export_page(&store, 0).unwrap();
    assert!(empty.page.records.is_empty() && !empty.more && empty.next == 0);

    let all: Vec<_> = (0..3000).map(|n| record(&id, n, 340)).collect();
    commit(&mut store, "/src/a.jsonl", &all, 1);
    let (seen, pages, cursor) = drain(&store, 0);
    assert!(pages >= 3, "3000 条必须切成多页（实际 {pages} 页）");
    assert_eq!(seen.len(), 3000, "每个键恰好出现");
    assert_eq!(cursor, store.high_water().unwrap());

    // 读完后有 10 条被修订（换到更大的 seq）：从上次游标续读只拿到这 10 条的新修订。
    let revised: Vec<_> = (0..10).map(|n| record(&id, n, 999)).collect();
    commit(&mut store, "/src/a.jsonl", &revised, 2);
    let (delta, pages, _) = drain(&store, cursor);
    assert_eq!(pages, 1);
    assert_eq!(delta.len(), 10);
    assert!(
        delta
            .values()
            .all(|r| r.output_tokens == 999 && r.revision == 2)
    );

    // 游标在高水位：空页、不再有更多。
    let tail = export_page(&store, store.high_water().unwrap()).unwrap();
    assert!(tail.page.records.is_empty() && !tail.more);
}

/// Mac 端解码时，外层多出字段（例如被塞进路径）或游标自相矛盾都整页拒收。
#[test]
fn decode_rejects_tampered_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "box").unwrap();
    let mut store = UsageStore::in_memory(&id).unwrap();
    commit(&mut store, "/src/a.jsonl", &[record(&id, 1, 10)], 1);
    let page = export_page(&store, 0).unwrap();

    let mut v = serde_json::to_value(&page).unwrap();
    v["path"] = "/home/someone/.claude".into();
    assert!(ExportPage::decode(&serde_json::to_vec(&v).unwrap()).is_err());

    let mut bad = page.clone();
    bad.next = 0; // 有记录但游标没前进
    assert!(ExportPage::decode(&serde_json::to_vec(&bad).unwrap()).is_err());

    let mut bad = page.clone();
    bad.store_id = "not-hex".into();
    assert!(ExportPage::decode(&serde_json::to_vec(&bad).unwrap()).is_err());

    let huge = vec![b' '; usage_v1::MAX_PAGE_BYTES + 1];
    assert!(ExportPage::decode(&huge).is_err());
}

/// 导出的页原样交给 Mac 端导入（单元 7）：信封的游标区间接得上，翻页、修订后续读都只计一次。
#[test]
fn exported_pages_import_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let runner = DeviceIdentity::load_or_create(&dir.path().join("runner.json"), "runner").unwrap();
    let mac = DeviceIdentity::load_or_create(&dir.path().join("mac.json"), "mac").unwrap();
    let mut store = UsageStore::in_memory(&runner).unwrap();
    let mut import = ImportStore::in_memory(&mac).unwrap();
    import.pair("runner", runner.machine_id(), 1).unwrap();

    let all: Vec<_> = (0..1500).map(|n| record(&runner, n, 340)).collect();
    commit(&mut store, "/src/a.jsonl", &all, 1);
    let pull = |store: &UsageStore, import: &mut ImportStore, mut after: u64| loop {
        let page =
            ExportPage::decode(&export_page(store, after).unwrap().encode().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&page.page).unwrap();
        import
            .import_page("runner", &page.cursor(), &bytes)
            .unwrap();
        after = page.next;
        if !page.more {
            return after;
        }
    };
    let cursor = pull(&store, &mut import, 0);
    commit(&mut store, "/src/a.jsonl", &[record(&runner, 0, 999)], 2);
    pull(&store, &mut import, cursor);

    let totals = import.totals(Some(runner.machine_id())).unwrap();
    assert_eq!(totals.events, 1500);
    assert_eq!(totals.output_tokens, 1499 * 340 + 999);
}
