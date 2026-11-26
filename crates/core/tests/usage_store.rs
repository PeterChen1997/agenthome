//! 采集端本地用量库（CARM-292，方案单元 6）。
//!
//! 为什么要这些用例：Runner 上的用量库是 Mac 汇总的唯一上游。
//! - 采集进程随时会被停（重启、OOM、systemd 停服务）：续采时多算一条就是重复计费，少算一条就是漏账；
//! - agent 自己会轮转 / 清理原日志（Claude 默认 30 天）：原件没了，已发生的花费不能跟着消失；
//! - 解析更正要能传到 Mac：同键升修订、整行替换，按任意顺序重放多少遍结果都一样；
//! - 这个库不能像索引库那样删了重建，版本不认识或机器不对时必须报错而不是清表。

use agenthome_core::GeminiProvider;
use agenthome_core::device::DeviceIdentity;
use agenthome_core::providers::SourceProvider;
use agenthome_core::usage_projection::{dedup, records};
use agenthome_core::usage_store::{
    Change, SourceProgress, UsageStore, collect_file, collect_source,
};
use agenthome_core::usage_v1::{Op, UsageRecord};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const A_ID: &str = "aaaaaaaa-1111-4222-8333-000000000001";
const B_ID: &str = "bbbbbbbb-1111-4222-8333-000000000002";

fn identity(dir: &Path) -> DeviceIdentity {
    DeviceIdentity::load_or_create(&dir.join("device.json"), "box").unwrap()
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn header(id: &str) -> String {
    line(json!({"sessionId": id, "startTime": "2026-11-01T08:00:00.000Z", "kind": "main"}))
}

/// 一次 gemini 回复。新鲜输入 = input − cached，输出 = output + thoughts。
fn reply(id: &str, ts: &str, input: u64, output: u64, cached: u64, thoughts: u64) -> String {
    line(json!({
        "id": id, "timestamp": ts, "type": "gemini", "content": "SECRET-REPLY",
        "model": "gemini-3-flash",
        "tokens": {"input": input, "output": output, "cached": cached, "thoughts": thoughts,
                   "tool": 0, "total": input + output + thoughts}
    }))
}

fn user(id: &str, ts: &str) -> String {
    line(json!({"id": id, "timestamp": ts, "type": "user", "content": "SECRET-PROMPT"}))
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    a: PathBuf,
    b: PathBuf,
    id: DeviceIdentity,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gemini");
        let a = root.join("tmp/proj-a/chats/session-2026-11-01T08-00-aaaaaaaa.jsonl");
        let b = root.join("tmp/proj-b/chats/session-2026-11-01T09-00-bbbbbbbb.jsonl");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(
            &a,
            header(A_ID)
                + &user("u1", "2026-11-01T08:00:01.000Z")
                + &reply("a1", "2026-11-01T08:00:02.000Z", 1000, 20, 600, 5)
                + &user("u2", "2026-11-01T08:01:00.000Z")
                + &reply("a2", "2026-11-01T08:01:02.000Z", 2000, 30, 1500, 0),
        )
        .unwrap();
        std::fs::write(
            &b,
            header(B_ID)
                + &user("u1", "2026-11-01T09:00:01.000Z")
                + &reply("b1", "2026-11-01T09:00:02.000Z", 300, 40, 0, 10),
        )
        .unwrap();
        let id = identity(dir.path());
        let db = dir.path().join("usage/usage.db");
        Self {
            root,
            db,
            a,
            b,
            id,
            _dir: dir,
        }
    }

    fn provider(&self) -> GeminiProvider {
        GeminiProvider::new(self.root.clone())
    }

    fn open(&self) -> UsageStore {
        UsageStore::open(&self.db, &self.id).unwrap()
    }

    fn append(&self, path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// 真值：直接对当前原件做投影（单元 2/3 已对齐本地统计）。
    fn expected(&self) -> Totals {
        let p = self.provider();
        let mut all = Vec::new();
        for f in p.discover().files {
            let file = SourceProvider::usage_events(&p, &f).unwrap().unwrap();
            all.extend(records(&self.id, &p.id(), &file).0);
        }
        Totals::of(dedup(all).iter())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    events: usize,
    requests: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
}

impl Totals {
    fn of<'a>(recs: impl Iterator<Item = &'a UsageRecord>) -> Self {
        let mut t = Totals::default();
        for r in recs.filter(|r| r.op == Op::Upsert) {
            t.events += 1;
            t.requests += u64::from(r.requests);
            t.input += r.input_tokens;
            t.output += r.output_tokens;
            t.cache_read += r.cache_read_tokens;
            t.reasoning += r.reasoning_tokens;
        }
        t
    }
}

/// 导入端的最小模型：同键只收更大的修订（单元 7 的规则），与到达顺序无关。
fn replay<'a>(
    acc: &mut HashMap<(String, String), UsageRecord>,
    changes: impl IntoIterator<Item = &'a Change>,
) {
    for c in changes {
        let key = (c.record.source.clone(), c.record.event_key.clone());
        match acc.get(&key) {
            Some(old) if old.revision >= c.record.revision => {}
            _ => {
                acc.insert(key, c.record.clone());
            }
        }
    }
}

fn all_changes(store: &UsageStore) -> Vec<Change> {
    store.changes_since(0, usize::MAX).unwrap()
}

fn store_totals(store: &UsageStore) -> Totals {
    let mut acc = HashMap::new();
    replay(&mut acc, &all_changes(store));
    Totals::of(acc.values())
}

/// 停进程再续采：停在两个文件之间、停在提交中途（事务回滚）、停的期间原件又追加，
/// 重开后一轮采完，与原件真值逐项相等；`seq` 只为真正的新事件分配，再跑一轮不再增长。
#[test]
fn resume_after_stop_neither_duplicates_nor_drops() {
    let fx = Fixture::new();
    let provider = fx.provider();

    // 第一次运行：只采完 A 就停了。
    {
        let mut store = fx.open();
        let out = collect_file(&mut store, &fx.id, &provider, &fx.a).unwrap();
        assert_eq!(out.stats.inserted, 2);
        assert_eq!(store.high_water().unwrap(), 2);
    }

    // 第二次运行：B 已解析，提交到一半出错（模拟进程在事务里被杀）——整笔回滚，进度不前移。
    {
        let mut store = fx.open();
        let file = SourceProvider::usage_events(&provider, &fx.b)
            .unwrap()
            .unwrap();
        let (mut recs, _) = records(&fx.id, &provider.id(), &file);
        let mut broken = recs[0].clone();
        broken.event_key = "not-a-key".into();
        recs.push(broken);
        let progress = SourceProgress::of(&fx.b).unwrap();
        assert!(
            store
                .commit_file(&provider.id(), &fx.b, progress, &recs)
                .is_err()
        );
        assert_eq!(store.high_water().unwrap(), 2, "回滚后不留半笔");
        assert_eq!(store.progress(&provider.id(), &fx.b).unwrap(), None);
        assert_eq!(all_changes(&store).len(), 2);
    }

    // 停机期间 A 又多了一次请求。
    fx.append(
        &fx.a,
        &(user("u3", "2026-11-01T08:02:00.000Z")
            + &reply("a3", "2026-11-01T08:02:02.000Z", 500, 7, 0, 0)),
    );

    let mut store = fx.open();
    let report = collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(report.failed, 0);
    assert_eq!(report.stats.unchanged, 2, "A 里已采过的两条重读不算新的");
    assert_eq!(report.stats.inserted, 2, "A 新追加的一条 + B 的一条");
    assert_eq!(store_totals(&store), fx.expected());
    assert_eq!(fx.expected().events, 4);
    assert_eq!(store.high_water().unwrap(), 4, "seq 只给新事件");

    let seqs: Vec<u64> = all_changes(&store).iter().map(|c| c.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq 严格递增");

    let again = collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(again.skipped, 2, "原件没变就不重读");
    assert_eq!(store.high_water().unwrap(), 4);
}

/// 原日志被 agent 清理：来源标成不可见，用量原样保留，也不产生撤回（导出端看不到任何变更）。
#[test]
fn cleaned_source_keeps_history() {
    let fx = Fixture::new();
    let provider = fx.provider();
    let mut store = fx.open();
    collect_source(&mut store, &fx.id, &provider).unwrap();
    let before = fx.expected();
    let hw = store.high_water().unwrap();

    let saved = std::fs::read(&fx.a).unwrap();
    std::fs::remove_file(&fx.a).unwrap();
    let report = collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(report.hidden, 1);
    assert_eq!(store.invisible_sources().unwrap(), 1);
    assert_eq!(store_totals(&store), before, "已发生的花费不随原件消失");
    assert!(store.changes_since(hw, 100).unwrap().is_empty());

    // 库在进程重启后同样保留。
    drop(store);
    let mut store = fx.open();
    assert_eq!(store_totals(&store), before);

    // 原件回来（例如从备份恢复）：恢复可见，重读不重复计。
    std::fs::write(&fx.a, saved).unwrap();
    collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(store.invisible_sources().unwrap(), 0);
    assert_eq!(store.high_water().unwrap(), hw);
    assert_eq!(store_totals(&store), before);
}

/// 解析更正 → 升修订；显式撤回 → 再升修订、用量清零且不被重读复活。
/// 导入端按增量、整段重放、乱序重放三种方式拿到的结果都等于真值。
#[test]
fn revisions_replay_to_correct_totals() {
    let fx = Fixture::new();
    let provider = fx.provider();
    let mut store = fx.open();
    collect_source(&mut store, &fx.id, &provider).unwrap();
    let mut incremental = HashMap::new();
    replay(&mut incremental, &all_changes(&store));
    let hw1 = store.high_water().unwrap();

    // 同一请求 a2 的 token 被更正（jsonl 同 id 重写，后写的为准）。
    fx.append(
        &fx.a,
        &reply("a2", "2026-11-01T08:01:02.000Z", 2400, 30, 1500, 0),
    );
    let report = collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(report.stats.revised, 1);
    let delta = store.changes_since(hw1, 100).unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].record.revision, 2);
    assert_eq!(delta[0].record.input_tokens, 2400 - 1500);
    replay(&mut incremental, &delta);
    assert_eq!(Totals::of(incremental.values()), fx.expected());
    assert_eq!(store_totals(&store), fx.expected());

    // 乱序、重复投递（旧页晚到）：结果不变。
    let mut shuffled = HashMap::new();
    let all = all_changes(&store);
    replay(&mut shuffled, all.iter().rev());
    replay(&mut shuffled, &all);
    replay(&mut shuffled, &delta);
    assert_eq!(Totals::of(shuffled.values()), fx.expected());

    // 显式撤回 a2。
    let key = delta[0].record.event_key.clone();
    let hw2 = store.high_water().unwrap();
    assert!(store.retract(&provider.id(), &key).unwrap());
    assert!(
        !store.retract(&provider.id(), &key).unwrap(),
        "重复撤回无效"
    );
    assert!(!store.retract(&provider.id(), &"0".repeat(64)).unwrap());
    let delta = store.changes_since(hw2, 100).unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].record.op, Op::Retract);
    assert_eq!(delta[0].record.revision, 3);
    delta[0].record.validate().unwrap();
    replay(&mut incremental, &delta);

    // 原件再变（追加一条用户消息），重读到 a2 也保持撤回。
    fx.append(&fx.a, &user("u9", "2026-11-01T08:09:00.000Z"));
    let report = collect_source(&mut store, &fx.id, &provider).unwrap();
    assert_eq!(report.stats.kept_retracted, 1);
    let mut want = fx.expected();
    want.events -= 1;
    want.requests -= 1;
    want.input -= 2400 - 1500;
    want.output -= 30;
    want.cache_read -= 1500;
    assert_eq!(store_totals(&store), want);
    assert_eq!(Totals::of(incremental.values()), want);
}

/// fork / resume 把同一响应复制到另一个文件、时间更晚：保留最早那条，不升修订、不重复计。
#[test]
fn later_copy_of_same_request_is_ignored() {
    let fx = Fixture::new();
    let provider = fx.provider();
    let mut store = fx.open();
    collect_source(&mut store, &fx.id, &provider).unwrap();
    let hw = store.high_water().unwrap();
    let mut copy = all_changes(&store)[0].record.clone();
    copy.ts_ms += 60_000;
    let fork = fx.root.join("tmp/proj-c/chats/fork.jsonl");
    std::fs::create_dir_all(fork.parent().unwrap()).unwrap();
    std::fs::write(&fork, "x").unwrap();
    let progress = SourceProgress::of(&fork).unwrap();
    let stats = store
        .commit_file(&provider.id(), &fork, progress, &[copy])
        .unwrap();
    assert_eq!(stats.kept_earlier, 1);
    assert_eq!(store.high_water().unwrap(), hw);
}

/// 用量库不能推倒重建：别的机器身份、不认识的版本都报错，数据原样留着。
#[test]
fn refuses_foreign_identity_and_unknown_schema() {
    let fx = Fixture::new();
    let provider = fx.provider();
    {
        let mut store = fx.open();
        collect_source(&mut store, &fx.id, &provider).unwrap();
    }
    let other_dir = tempfile::tempdir().unwrap();
    let other = identity(other_dir.path());
    assert!(UsageStore::open(&fx.db, &other).is_err());

    {
        let conn = rusqlite::Connection::open(&fx.db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    assert!(UsageStore::open(&fx.db, &fx.id).is_err());
    {
        let conn = rusqlite::Connection::open(&fx.db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3, "版本不认识时没有删表");
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let reopened = fx.open();
    assert_eq!(store_totals(&reopened), fx.expected());
    let id_before = reopened.store_id().to_string();
    drop(reopened);

    // 库丢了重建：seq 从头来，store_id 变了，导入端据此知道旧游标作废。
    std::fs::remove_dir_all(fx.db.parent().unwrap()).unwrap();
    let rebuilt = fx.open();
    assert_eq!(rebuilt.high_water().unwrap(), 0);
    assert_ne!(rebuilt.store_id(), id_before);
}

/// 运行中备份：备份库用同一身份打开，变更日志与高水位完全相同，可以接着采。
#[test]
fn live_backup_restores_same_log() {
    let fx = Fixture::new();
    let provider = fx.provider();
    let mut store = fx.open();
    collect_source(&mut store, &fx.id, &provider).unwrap();
    let dest = fx.db.with_file_name("backup.db");
    store.backup_to(&dest).unwrap();
    assert!(store.backup_to(&dest).is_err(), "不覆盖已有备份");

    let mut restored = UsageStore::open(&dest, &fx.id).unwrap();
    assert_eq!(
        restored.store_id(),
        store.store_id(),
        "恢复的是同一个库，游标继续有效"
    );
    assert_eq!(all_changes(&restored), all_changes(&store));
    assert_eq!(restored.high_water().unwrap(), store.high_water().unwrap());
    let again = collect_source(&mut restored, &fx.id, &provider).unwrap();
    assert_eq!(again.skipped, 2, "读取进度也在备份里");
}
