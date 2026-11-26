//! Mac 端远程用量导入（CARM-294，方案单元 7）。
//!
//! 为什么要这些用例：Mac 经 SSH 拉 Runner 的用量页，网络和进程都不可靠。
//! - 同一页可能收到多次（重试、丢了回执后重拉）、旧页可能在新页之后才到、一页里记录顺序不保证：
//!   任何一种多算一次都是重复计费，少算一次是漏账；
//! - 采集端会升修订（解析更正）和撤回：Mac 必须只留最新修订，旧修订晚到也不能覆盖回去；
//! - 本机用量走索引库，若把本机再当远端汇入、或同一台 Runner 配对两次，全部机器视图就会翻倍；
//! - 索引库随时可删了重建，远端用量只存在导入库里，重建后必须原样还在、游标也不回退。

use agenthome_core::Store;
use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::ProviderId;
use agenthome_core::usage_import::{ImportStore, PageCursor, StoreReset, Totals};
use agenthome_core::usage_store::{SourceProgress, UsageStore};
use agenthome_core::usage_v1::{EventBasis, Op, UsagePage, UsageRecord, VERSION, event_key};
use std::path::Path;

/// 一台采集端：身份 + 本地用量库（单元 6 的真实实现）。
struct Collector {
    _dir: tempfile::TempDir,
    id: DeviceIdentity,
    store: UsageStore,
    /// 每次提交用不同的进度，让 `commit_file` 认为文件变了。
    tick: i64,
}

impl Collector {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "box").unwrap();
        let store = UsageStore::in_memory(&id).unwrap();
        Self {
            _dir: dir,
            id,
            store,
            tick: 0,
        }
    }

    fn machine_id(&self) -> String {
        self.id.machine_id().to_string()
    }

    fn record(&self, request: &str, output: u64) -> UsageRecord {
        UsageRecord {
            v: VERSION,
            event_key: event_key(
                &self.id,
                &ProviderId::ClaudeCode,
                EventBasis::Request(request),
            ),
            revision: 1,
            op: Op::Upsert,
            machine_id: self.machine_id(),
            source: "claude-code".into(),
            vendor: Some("anthropic".into()),
            model: Some("claude-opus-4-5".into()),
            ts_ms: 1_763_500_000_000,
            requests: 1,
            input_tokens: 10,
            output_tokens: output,
            cache_read_tokens: 1_000,
            cache_write_5m_tokens: 100,
            cache_write_1h_tokens: 50,
            reasoning_tokens: 0,
        }
    }

    /// 采集一轮：`(请求 id, 输出 token)`；同 id 输出变了就是一次修订。
    fn collect(&mut self, items: &[(&str, u64)]) {
        self.tick += 1;
        let records: Vec<_> = items.iter().map(|(r, o)| self.record(r, *o)).collect();
        let progress = SourceProgress {
            size: self.tick as u64,
            mtime_ms: self.tick,
        };
        self.store
            .commit_file(
                &ProviderId::ClaudeCode,
                Path::new("/src/a.jsonl"),
                progress,
                &records,
            )
            .unwrap();
    }

    fn retract(&mut self, request: &str) {
        let key = self.record(request, 0).event_key;
        assert!(self.store.retract(&ProviderId::ClaudeCode, &key).unwrap());
    }

    /// 导出一页：`seq > after` 的最多 `limit` 条变更。
    fn export(&self, after: u64, limit: usize) -> (PageCursor, Vec<u8>) {
        let changes = self.store.changes_since(after, limit).unwrap();
        let through = changes.last().map_or(after, |c| c.seq);
        let page = UsagePage {
            v: VERSION,
            machine_id: self.machine_id(),
            records: changes.into_iter().map(|c| c.record).collect(),
        };
        let cursor = PageCursor {
            store_id: self.store.store_id().to_string(),
            after,
            through,
        };
        (cursor, page.encode().unwrap())
    }

    /// 采集端当前状态的真值：每个键的最新修订，撤回不计。
    fn truth(&self) -> Totals {
        let mut t = Totals::default();
        for c in self.store.changes_since(0, usize::MAX).unwrap() {
            let r = c.record;
            if r.op == Op::Retract {
                continue;
            }
            t.events += 1;
            t.requests += u64::from(r.requests);
            t.input_tokens += r.input_tokens;
            t.output_tokens += r.output_tokens;
            t.cache_read_tokens += r.cache_read_tokens;
            t.cache_write_5m_tokens += r.cache_write_5m_tokens;
            t.cache_write_1h_tokens += r.cache_write_1h_tokens;
            t.reasoning_tokens += r.reasoning_tokens;
        }
        t
    }
}

struct Mac {
    _dir: tempfile::TempDir,
    id: DeviceIdentity,
}

impl Mac {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "mac").unwrap();
        Self { _dir: dir, id }
    }

    fn store(&self) -> ImportStore {
        ImportStore::in_memory(&self.id).unwrap()
    }
}

fn paired(mac: &Mac, runner: &Collector) -> ImportStore {
    let mut store = mac.store();
    store.pair("runner", &runner.machine_id(), 1).unwrap();
    store
}

/// 同一页收到多次（重试、回执丢失后重拉），只计一次。
#[test]
fn duplicate_page_counts_once() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 100), ("m2", 200), ("m3", 300)]);
    let mut store = paired(&mac, &runner);
    let (cursor, page) = runner.export(0, 100);

    let first = store.import_page("runner", &cursor, &page).unwrap();
    assert_eq!((first.inserted, first.advanced), (3, true));
    for _ in 0..3 {
        let again = store.import_page("runner", &cursor, &page).unwrap();
        assert_eq!((again.inserted, again.stale, again.advanced), (0, 3, false));
    }
    assert_eq!(store.totals(None).unwrap(), runner.truth());
    assert_eq!(store.totals(None).unwrap().output_tokens, 600);
}

/// 提交后回执丢了：Mac 不知道成没成，从旧游标重拉一遍（与下一批合成一页），结果不变、不重复。
#[test]
fn lost_receipt_replay_counts_once() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 100), ("m2", 200)]);
    let mut store = paired(&mac, &runner);
    let (cursor, page) = runner.export(0, 100);
    store.import_page("runner", &cursor, &page).unwrap();

    runner.collect(&[("m1", 100), ("m2", 200), ("m3", 300)]);
    // 从 0 重拉：页里有已入账的 m1、m2 和新的 m3。
    let (cursor, page) = runner.export(0, 100);
    let stats = store.import_page("runner", &cursor, &page).unwrap();
    assert_eq!((stats.inserted, stats.stale), (1, 2));
    assert_eq!(store.totals(None).unwrap(), runner.truth());
    assert_eq!(store.totals(None).unwrap().events, 3);
    assert_eq!(store.cursor(&runner.machine_id()).unwrap().unwrap().seq, 3);
}

/// 旧页在新页之后才到：旧修订不能把新修订覆盖回去，游标也不回退。
#[test]
fn late_old_page_does_not_roll_back() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 100)]);
    let old = runner.export(0, 100);
    runner.collect(&[("m1", 999)]); // 解析更正：修订 2
    let new = runner.export(0, 100);
    let mut store = paired(&mac, &runner);

    store.import_page("runner", &new.0, &new.1).unwrap();
    let late = store.import_page("runner", &old.0, &old.1).unwrap();
    assert_eq!((late.stale, late.advanced), (1, false));
    assert_eq!(store.totals(None).unwrap().output_tokens, 999);
    assert_eq!(store.cursor(&runner.machine_id()).unwrap().unwrap().seq, 2);
}

/// 一页里记录顺序不保证：同键的旧修订排在后面也不生效。撤回同样按修订处理。
#[test]
fn out_of_order_records_and_retract_resolve_by_revision() {
    let mac = Mac::new();
    let runner = Collector::new();
    let mut store = paired(&mac, &runner);
    let v1 = runner.record("m1", 100);
    let mut v2 = runner.record("m1", 150);
    v2.revision = 2;
    let mut gone = runner.record("m2", 0);
    gone.revision = 3;
    gone.op = Op::Retract;
    gone.requests = 0;
    gone.input_tokens = 0;
    gone.cache_read_tokens = 0;
    gone.cache_write_5m_tokens = 0;
    gone.cache_write_1h_tokens = 0;
    let m2 = runner.record("m2", 200);
    let page = UsagePage {
        v: VERSION,
        machine_id: runner.machine_id(),
        // 倒序：新修订在前，旧修订在后；撤回在原记录之前。
        records: vec![v2, gone, v1, m2],
    };
    let cursor = PageCursor {
        store_id: runner.store.store_id().to_string(),
        after: 0,
        through: 4,
    };
    store
        .import_page("runner", &cursor, &page.encode().unwrap())
        .unwrap();
    let t = store.totals(None).unwrap();
    assert_eq!((t.events, t.output_tokens), (1, 150));
}

/// 多轮采集（新增、修订、撤回）后，把所有导出过的页打乱、重复投递，缺页的被拒后下一轮重试；
/// 最终结果逐项等于采集端真值。
#[test]
fn shuffled_duplicated_delivery_converges_to_truth() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    let mut store = paired(&mac, &runner);
    let mut pages = Vec::new();
    let mut exported = 0;
    let rounds: [&[(&str, u64)]; 4] = [
        &[("a", 1), ("b", 2), ("c", 3)],
        &[("a", 1), ("b", 20), ("d", 4), ("e", 5)],
        &[("a", 10), ("b", 20), ("d", 4), ("e", 5), ("f", 6)],
        &[
            ("a", 10),
            ("b", 20),
            ("d", 40),
            ("e", 5),
            ("f", 6),
            ("g", 7),
        ],
    ];
    for (i, round) in rounds.iter().enumerate() {
        runner.collect(round);
        if i == 2 {
            runner.retract("c");
        }
        // 每轮按 2 条一页导出新增的变更；另外再从头导一页（模拟重拉），制造重叠。
        loop {
            let (cursor, page) = runner.export(exported, 2);
            if cursor.through == exported {
                break;
            }
            exported = cursor.through;
            pages.push((cursor, page));
        }
        pages.push(runner.export(0, 3));
    }
    // 每页投递 3 次，确定性打乱。
    let mut deliveries: Vec<_> = (0..3).flat_map(|_| pages.iter()).collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in (1..deliveries.len()).rev() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        deliveries.swap(i, (seed >> 33) as usize % (i + 1));
    }
    let mut rejected_gaps = 0;
    let mut pending = deliveries;
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(
            |(cursor, page)| match store.import_page("runner", cursor, page) {
                Ok(_) => false,
                Err(e) => {
                    assert!(e.to_string().contains("页"), "只应因缺页被拒：{e}");
                    rejected_gaps += 1;
                    true
                }
            },
        );
        assert!(pending.len() < before, "没有进展");
    }
    assert!(rejected_gaps > 0, "打乱后应出现缺页拒收再重试的情况");
    let truth = runner.truth();
    assert_eq!(store.totals(None).unwrap(), truth);
    assert_eq!(
        (truth.events, truth.output_tokens),
        (6, 10 + 20 + 40 + 5 + 6 + 7)
    );
    assert_eq!(
        store.cursor(&runner.machine_id()).unwrap().unwrap().seq,
        runner.store.high_water().unwrap()
    );
}

/// 缺页（起点超过已确认游标）整页拒收、什么都不写；补上前一页后再收就成功。
#[test]
fn gap_is_rejected_without_writing() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 1), ("m2", 2), ("m3", 3), ("m4", 4)]);
    let mut store = paired(&mac, &runner);
    let first = runner.export(0, 2);
    let second = runner.export(2, 2);

    assert!(store.import_page("runner", &second.0, &second.1).is_err());
    assert_eq!(store.totals(None).unwrap(), Totals::default());
    assert_eq!(store.cursor(&runner.machine_id()).unwrap(), None);

    store.import_page("runner", &first.0, &first.1).unwrap();
    store.import_page("runner", &second.0, &second.1).unwrap();
    assert_eq!(store.totals(None).unwrap(), runner.truth());
}

/// 采集端库被重建（`store_id` 变了），旧游标作废：返回 `StoreReset` 交给完整快照，不混入新编号的页。
#[test]
fn store_reset_is_reported_and_writes_nothing() {
    let mac = Mac::new();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 1)]);
    let mut store = paired(&mac, &runner);
    let (cursor, page) = runner.export(0, 10);
    store.import_page("runner", &cursor, &page).unwrap();

    runner.store = UsageStore::in_memory(&runner.id).unwrap();
    runner.collect(&[("m1", 1), ("m2", 2)]);
    let (cursor, page) = runner.export(0, 10);
    let err = store.import_page("runner", &cursor, &page).unwrap_err();
    assert!(err.downcast_ref::<StoreReset>().is_some(), "{err}");
    assert_eq!(store.totals(None).unwrap().events, 1);
}

/// 本机不能当远端配对或汇入；同一台机器不能配对两次；连接名必须对应配对时的那台机器。
#[test]
fn pairing_rejects_local_duplicate_and_mismatched_machines() {
    let mac = Mac::new();
    let runner = Collector::new();
    let other = Collector::new();
    let mut store = mac.store();

    assert!(store.pair("self", mac.id.machine_id(), 1).is_err(), "本机");
    store.pair("runner", &runner.machine_id(), 1).unwrap();
    assert!(
        store.pair("runner-again", &runner.machine_id(), 2).is_err(),
        "同机重复配对"
    );
    assert!(
        store.pair("runner", &other.machine_id(), 2).is_err(),
        "连接名已被占用"
    );
    assert!(store.pair("", &other.machine_id(), 2).is_err(), "空连接名");

    let mut other = other;
    other.collect(&[("m1", 1)]);
    let (cursor, page) = other.export(0, 10);
    assert!(
        store.import_page("runner", &cursor, &page).is_err(),
        "连接送来别的机器的数据"
    );
    assert!(
        store.import_page("unknown", &cursor, &page).is_err(),
        "未配对的连接"
    );

    // 本机身份的数据（例如把本机 CLI 当 Runner 连上）即使走已配对的连接也拒收。
    let mut local = Collector::new();
    local.id = mac.id.clone();
    local.store = UsageStore::in_memory(&mac.id).unwrap();
    local.collect(&[("m1", 1)]);
    let (cursor, page) = local.export(0, 10);
    assert!(store.import_page("runner", &cursor, &page).is_err());
    assert_eq!(store.totals(None).unwrap(), Totals::default());
}

/// 唯一键含 `machine_id`：两台机器同来源同事件键各算各的。
#[test]
fn same_event_key_on_two_machines_counts_twice() {
    let mac = Mac::new();
    let a = Collector::new();
    let b = Collector::new();
    let mut store = mac.store();
    store.pair("a", &a.machine_id(), 1).unwrap();
    store.pair("b", &b.machine_id(), 1).unwrap();
    let key = a.record("m1", 0).event_key;
    for (label, c) in [("a", &a), ("b", &b)] {
        let mut r = c.record("m1", 100);
        r.event_key = key.clone();
        let page = UsagePage {
            v: VERSION,
            machine_id: c.machine_id(),
            records: vec![r],
        };
        let cursor = PageCursor {
            store_id: c.store.store_id().to_string(),
            after: 0,
            through: 1,
        };
        store
            .import_page(label, &cursor, &page.encode().unwrap())
            .unwrap();
    }
    assert_eq!(store.totals(None).unwrap().events, 2);
    assert_eq!(store.totals(Some(&a.machine_id())).unwrap().events, 1);
}

/// 删掉索引库重建：远端用量和游标都在导入库里，原样还在，不用从头重拉。
#[test]
fn remote_usage_survives_index_rebuild() {
    let data = tempfile::tempdir().unwrap();
    let index_db = data.path().join("index").join("agenthome.db");
    let remote_db = data.path().join("remote").join("usage.db");
    let mac_id = DeviceIdentity::load_or_create(&data.path().join("device.json"), "mac").unwrap();
    let mut runner = Collector::new();
    runner.collect(&[("m1", 100), ("m2", 200)]);

    drop(Store::open(&index_db).unwrap());
    {
        let mut store = ImportStore::open(&remote_db, &mac_id).unwrap();
        store.pair("runner", &runner.machine_id(), 1).unwrap();
        let (cursor, page) = runner.export(0, 100);
        store.import_page("runner", &cursor, &page).unwrap();
    }

    std::fs::remove_dir_all(index_db.parent().unwrap()).unwrap();
    drop(Store::open(&index_db).unwrap());

    let store = ImportStore::open(&remote_db, &mac_id).unwrap();
    assert_eq!(store.totals(None).unwrap(), runner.truth());
    assert_eq!(store.peer("runner").unwrap(), Some(runner.machine_id()));
    assert_eq!(store.cursor(&runner.machine_id()).unwrap().unwrap().seq, 2);
    // 默认位置也不在索引目录下。
    let default_index = agenthome_core::paths::default_index_db();
    let default_remote = agenthome_core::paths::default_remote_usage_db();
    assert!(!default_remote.starts_with(default_index.parent().unwrap()));
}

/// 版本不认识时报错，不删表（游标丢了要从头重拉，旧库内容也许还有用）。
#[test]
fn unknown_schema_version_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("usage.db");
    let mac = Mac::new();
    drop(ImportStore::open(&db, &mac.id).unwrap());
    let conn = rusqlite_set_version(&db, 99);
    drop(conn);
    assert!(ImportStore::open(&db, &mac.id).is_err());
}

fn rusqlite_set_version(db: &Path, v: i32) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.pragma_update(None, "user_version", v).unwrap();
    conn
}
