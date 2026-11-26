//! Mac 后台拉取（CARM-299，方案单元 12）。
//!
//! 为什么要这些用例：Mac 会在任意时刻合盖休眠、断网、换网或被退出，拉取可能停在任意两页之间，
//! 也可能刚拉完一页就丢了连接。不管停在哪，下次接着拉之后，远端用量都必须**逐项等于采集端真值**：
//! 少一页是漏账，多算一页是重复计费。采集端库重建 / 从备份恢复时要自动走完整快照，
//! 快照拉到一半中断，首页的旧统计也不能变。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::ProviderId;
use agenthome_core::remote_pull::{PullFailure, PullOutcome, PullSchedule, lock_remote, pull};
use agenthome_core::remote_ssh::{ExportPage, RemoteError, RemoteErrorKind, export_page};
use agenthome_core::usage_import::{ImportStore, Totals};
use agenthome_core::usage_store::{SourceProgress, UsageStore};
use agenthome_core::usage_v1::{self, EventBasis, Op, UsageRecord, event_key};
use std::cell::Cell;
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

/// 采集端真值：`(0..n)` 每条 `output` 个输出 token，另有 `revised` 里的条目被改成 999。
fn truth(n: u64, output: u64, revised: &[u64]) -> Totals {
    let outputs: u64 = (0..n)
        .map(|i| if revised.contains(&i) { 999 } else { output })
        .sum();
    Totals {
        events: n,
        requests: n,
        input_tokens: 12 * n,
        output_tokens: outputs,
        cache_read_tokens: 50_000 * n,
        cache_write_5m_tokens: 1_200 * n,
        cache_write_1h_tokens: 800 * n,
        reasoning_tokens: 0,
    }
}

/// 模拟 SSH：从采集端库导出一页、编码、在 Mac 端解码。`fail_at` 里的调用序号（从 0 计）返回连不上。
fn ssh<'a>(
    store: &'a UsageStore,
    calls: &'a Cell<usize>,
    fail_at: &'a [usize],
) -> impl FnMut(u64) -> anyhow::Result<ExportPage> + 'a {
    move |after| {
        let n = calls.get();
        calls.set(n + 1);
        if fail_at.contains(&n) {
            return Err(RemoteError {
                kind: RemoteErrorKind::Unreachable,
                detail: "ssh: connect to host 10.0.0.2 port 22: Network is unreachable".into(),
            }
            .into());
        }
        ExportPage::decode(&export_page(store, after)?.encode()?)
    }
}

struct Pair {
    runner: DeviceIdentity,
    mac: DeviceIdentity,
    _dir: tempfile::TempDir,
}

fn ids() -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let runner = DeviceIdentity::load_or_create(&dir.path().join("runner.json"), "runner").unwrap();
    let mac = DeviceIdentity::load_or_create(&dir.path().join("mac.json"), "mac").unwrap();
    Pair {
        runner,
        mac,
        _dir: dir,
    }
}

const NEVER: &dyn Fn() -> bool = &|| false;

/// 首次拉取把连接名绑到对方的 machine_id；历史回填分多轮（每轮页数有上限）也只计一次，
/// 每轮报告的积压单调减少到 0。
#[test]
fn first_pull_binds_peer_and_backfills_in_rounds() {
    let p = ids();
    let mut runner = UsageStore::in_memory(&p.runner).unwrap();
    let all: Vec<_> = (0..3000).map(|n| record(&p.runner, n, 340)).collect();
    commit(&mut runner, "/src/a.jsonl", &all, 1);
    let mut mac = ImportStore::in_memory(&p.mac).unwrap();

    let calls = Cell::new(0);
    let mut fetch = ssh(&runner, &calls, &[]);
    let first = pull(&mut mac, "runner", &mut fetch, 1, NEVER, 1).unwrap();
    assert_eq!(
        mac.peer("runner").unwrap().as_deref(),
        Some(p.runner.machine_id())
    );
    assert!(first.more && first.backlog > 0, "{first:?}");
    let mut backlog = first.backlog;
    let mut rounds = 1;
    loop {
        let o = pull(&mut mac, "runner", &mut fetch, 1, NEVER, 1).unwrap();
        rounds += 1;
        assert!(
            o.backlog <= backlog,
            "积压只减不增：{backlog} → {}",
            o.backlog
        );
        backlog = o.backlog;
        if !o.more {
            break;
        }
    }
    assert!(rounds >= 3, "3000 条应分多页（{rounds} 轮）");
    assert_eq!(backlog, 0);
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap(),
        truth(3000, 340, &[])
    );
}

/// 休眠 / 断网 / 换网：拉到一半连接断了，已提交的页保留、游标停在最后提交的位置；
/// 调度退避后重试接着拉，期间采集端又有修订，最终逐项等于真值。
#[test]
fn interrupted_pull_resumes_and_counts_once() {
    let p = ids();
    let mut runner = UsageStore::in_memory(&p.runner).unwrap();
    let all: Vec<_> = (0..3000).map(|n| record(&p.runner, n, 340)).collect();
    commit(&mut runner, "/src/a.jsonl", &all, 1);
    let mut mac = ImportStore::in_memory(&p.mac).unwrap();
    let mut schedule = PullSchedule::new();

    // 第 2 次请求时断网（第 1 页已提交）。
    let calls = Cell::new(0);
    let fail = [1];
    let err = {
        let mut fetch = ssh(&runner, &calls, &fail);
        assert!(schedule.due(0));
        pull(&mut mac, "runner", &mut fetch, 32, NEVER, 0).unwrap_err()
    };
    assert_eq!(err.failure, PullFailure::Sync(RemoteErrorKind::Unreachable));
    assert_eq!(err.pages, 1);
    let partial = mac.totals(Some(p.runner.machine_id())).unwrap();
    assert!(partial.events > 0 && partial.events < 3000, "{partial:?}");
    schedule.on_failure(0);
    assert!(!schedule.due(10_000), "退避期内不重试");

    // 断网期间采集端继续采集：前 10 条被修订。
    let revised: Vec<u64> = (0..10).collect();
    let new: Vec<_> = revised.iter().map(|&n| record(&p.runner, n, 999)).collect();
    commit(&mut runner, "/src/a.jsonl", &new, 2);

    // 合盖 1 小时后醒来：立即到点，接着拉完。
    let woke = 3_600_000;
    assert!(schedule.due(woke));
    let calls = Cell::new(0);
    let mut fetch = ssh(&runner, &calls, &[]);
    let done = pull(&mut mac, "runner", &mut fetch, 32, NEVER, woke).unwrap();
    assert!(!done.more && done.backlog == 0);
    schedule.on_success(woke, &done);
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap(),
        truth(3000, 340, &revised)
    );

    // 再拉一次没有新东西：一页空页，合计不变。
    let again = pull(&mut mac, "runner", &mut fetch, 32, NEVER, woke).unwrap();
    assert_eq!((again.pages, again.inserted, again.replaced), (1, 0, 0));
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap(),
        truth(3000, 340, &revised)
    );
}

/// App 退出：在任意两页之间叫停，关库重开（模拟进程重启）再拉，结果与一次拉完相同。
#[test]
fn quit_between_pages_then_reopen_counts_once() {
    let p = ids();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("remote").join("usage.db");
    let mut runner = UsageStore::in_memory(&p.runner).unwrap();
    let all: Vec<_> = (0..3000).map(|n| record(&p.runner, n, 340)).collect();
    commit(&mut runner, "/src/a.jsonl", &all, 1);

    for stop_after in 1..=3 {
        let _ = std::fs::remove_dir_all(dir.path().join("remote"));
        {
            let mut mac = ImportStore::open(&db, &p.mac).unwrap();
            let calls = Cell::new(0);
            let mut fetch = ssh(&runner, &calls, &[]);
            let stop = || calls.get() >= stop_after;
            let o = pull(&mut mac, "runner", &mut fetch, 32, &stop, 0).unwrap();
            assert!(o.more, "第 {stop_after} 页后叫停应报告还有");
        }
        let mut mac = ImportStore::open(&db, &p.mac).unwrap();
        let calls = Cell::new(0);
        let mut fetch = ssh(&runner, &calls, &[]);
        let o = pull(&mut mac, "runner", &mut fetch, 32, NEVER, 0).unwrap();
        assert!(!o.more);
        assert_eq!(
            mac.totals(Some(p.runner.machine_id())).unwrap(),
            truth(3000, 340, &[]),
            "第 {stop_after} 页后退出"
        );
    }
}

/// 采集端库重建：拉取自动转完整快照；快照拉到一半断网，旧统计照常可查；
/// 醒来续拉完成替换，之后的增量照常。
#[test]
fn store_reset_switches_to_snapshot_and_survives_interruption() {
    let p = ids();
    let mut runner = UsageStore::in_memory(&p.runner).unwrap();
    let all: Vec<_> = (0..3000).map(|n| record(&p.runner, n, 340)).collect();
    commit(&mut runner, "/src/a.jsonl", &all, 1);
    let mut mac = ImportStore::in_memory(&p.mac).unwrap();
    let calls = Cell::new(0);
    pull(
        &mut mac,
        "runner",
        &mut ssh(&runner, &calls, &[]),
        32,
        NEVER,
        0,
    )
    .unwrap();
    let before = mac.totals(Some(p.runner.machine_id())).unwrap();
    assert_eq!(before, truth(3000, 340, &[]));

    // 重建：同一身份、新库（新 store_id），重新采集时前 10 条解析成 999。
    let mut runner = UsageStore::in_memory(&p.runner).unwrap();
    let rebuilt: Vec<_> = (0..3000)
        .map(|n| record(&p.runner, n, if n < 10 { 999 } else { 340 }))
        .collect();
    commit(&mut runner, "/src/a.jsonl", &rebuilt, 1);

    // 第 1 次请求发现游标失效、开始快照；第 2 次请求拉到第一页快照；第 3 次断网。
    let calls = Cell::new(0);
    let err = pull(
        &mut mac,
        "runner",
        &mut ssh(&runner, &calls, &[2]),
        32,
        NEVER,
        0,
    )
    .unwrap_err();
    assert_eq!(err.failure.kind(), "unreachable");
    let mid = mac.snapshot(p.runner.machine_id()).unwrap().unwrap();
    assert!(!mid.complete() && mid.staged_through > 0, "{mid:?}");
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap(),
        before,
        "快照没拉完，旧统计不变"
    );

    let calls = Cell::new(0);
    let done = pull(
        &mut mac,
        "runner",
        &mut ssh(&runner, &calls, &[]),
        32,
        NEVER,
        0,
    )
    .unwrap();
    assert_eq!(done.snapshots, 1);
    assert!(!done.in_snapshot && !done.more);
    let revised: Vec<u64> = (0..10).collect();
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap(),
        truth(3000, 340, &revised)
    );

    // 之后照常增量。
    commit(
        &mut runner,
        "/src/b.jsonl",
        &[record(&p.runner, 5000, 7)],
        1,
    );
    let calls = Cell::new(0);
    let o: PullOutcome = pull(
        &mut mac,
        "runner",
        &mut ssh(&runner, &calls, &[]),
        32,
        NEVER,
        0,
    )
    .unwrap();
    assert_eq!(o.inserted, 1);
    assert_eq!(
        mac.totals(Some(p.runner.machine_id())).unwrap().events,
        3001
    );
}

/// 把本机当远端：导入阶段拒绝，什么都不写。
#[test]
fn self_as_remote_is_rejected() {
    let p = ids();
    let mut own = UsageStore::in_memory(&p.mac).unwrap();
    commit(&mut own, "/src/a.jsonl", &[record(&p.mac, 1, 10)], 1);
    let mut mac = ImportStore::in_memory(&p.mac).unwrap();
    let calls = Cell::new(0);
    let err = pull(&mut mac, "me", &mut ssh(&own, &calls, &[]), 32, NEVER, 0).unwrap_err();
    assert_eq!(err.failure.stage(), "import");
    assert!(mac.peer("me").unwrap().is_none());
    assert_eq!(mac.totals(None).unwrap().events, 0);
}

/// 同一远端同时只能有一个拉取（跨进程用文件锁）；前一个结束后可以再拉。
#[test]
fn one_pull_per_remote_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let held = lock_remote(dir.path()).unwrap();
    let busy = lock_remote(dir.path()).unwrap_err();
    assert_eq!(busy.failure, PullFailure::Busy);
    drop(held);
    assert!(lock_remote(dir.path()).is_ok());
}
