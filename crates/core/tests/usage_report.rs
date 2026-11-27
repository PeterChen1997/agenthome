//! 按机器统计（CARM-298，方案单元 11）。
//!
//! 为什么要这些用例：
//! - 用户拿这张表和 Runner 本机 CLI 对账。分组错一格（机器、来源、供应商、模型任何一维串了），
//!   两边金额就对不上，也没法判断是哪台机器在花钱。
//! - 长上下文按单次请求判断档位。按合并后的用量算会把几次普通请求算成一次高价请求，
//!   同一份用量放在不同机器上看价钱不同，跨机器比较就不公平（CARM-290 实测本机偏高约 2%）。
//! - 定价表缺价的模型不能按 0 元悄悄混进合计：token 照计，未知价请求数单列，整组显示「未定价」。
//! - 「全部机器」必须正好等于「本机 + 各远端」：少合并一台是漏账，本机算两遍是翻倍。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::model::ProviderId;
use agenthome_core::usage_import::{ImportStore, PageCursor};
use agenthome_core::usage_report::{MachineFilter, MachineKind, Report, Tally, Window, query};
use agenthome_core::usage_store::{SourceProgress, UsageStore};
use agenthome_core::usage_v1::{EventBasis, Op, UsagePage, UsageRecord, VERSION, event_key};
use rusqlite::Connection;
use std::path::Path;

const T0: i64 = 1_763_500_000_000;
const HOUR: i64 = 3_600_000;

#[test]
fn cost_readers_can_query_while_local_and_remote_writers_hold_the_wal_lock() {
    let dir = tempfile::tempdir().unwrap();
    let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), "test").unwrap();
    let local = dir.path().join("local.db");
    let remote = dir.path().join("remote.db");
    assert!(UsageStore::open_read_only(&local, &id).unwrap().is_none());
    assert!(ImportStore::open_read_only(&remote, &id).unwrap().is_none());
    assert!(!local.exists() && !remote.exists());
    let uninitialized = Connection::open(&local).unwrap();
    assert!(UsageStore::open_read_only(&local, &id).unwrap().is_none());
    drop(uninitialized);

    let _local_store = UsageStore::open(&local, &id).unwrap();
    let _remote_store = ImportStore::open(&remote, &id).unwrap();
    let local_writer = Connection::open(&local).unwrap();
    let remote_writer = Connection::open(&remote).unwrap();
    local_writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    remote_writer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let local = UsageStore::open_read_only(&local, &id).unwrap().unwrap();
    let remote = ImportStore::open_read_only(&remote, &id).unwrap().unwrap();
    let report = query(
        Some(&local),
        Some(&remote),
        &MachineFilter::All,
        Window::new(T0, T0 + HOUR).unwrap(),
    )
    .unwrap();
    assert!(report.rows.is_empty());
}

struct Machine {
    _dir: tempfile::TempDir,
    id: DeviceIdentity,
    store: UsageStore,
    tick: i64,
}

/// 一条请求的要素：来源、模型、供应商、时间、新鲜输入、输出。
struct Req<'a> {
    key: &'a str,
    source: ProviderId,
    model: Option<&'a str>,
    vendor: Option<&'a str>,
    ts_ms: i64,
    input: u64,
    output: u64,
}

fn req<'a>(
    key: &'a str,
    source: ProviderId,
    model: &'a str,
    vendor: &'a str,
    ts_ms: i64,
) -> Req<'a> {
    Req {
        key,
        source,
        model: Some(model),
        vendor: Some(vendor),
        ts_ms,
        input: 1_000,
        output: 100,
    }
}

impl Machine {
    fn new(host: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&dir.path().join("device.json"), host).unwrap();
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

    fn record(&self, r: &Req<'_>) -> UsageRecord {
        UsageRecord {
            v: VERSION,
            event_key: event_key(&self.id, &r.source, EventBasis::Request(r.key)),
            revision: 1,
            op: Op::Upsert,
            machine_id: self.machine_id(),
            source: r.source.as_str().to_string(),
            vendor: r.vendor.map(str::to_string),
            model: r.model.map(str::to_string),
            ts_ms: r.ts_ms,
            requests: 1,
            input_tokens: r.input,
            output_tokens: r.output,
            cache_read_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    /// 按来源分文件提交，和采集器一样。
    fn collect(&mut self, reqs: &[Req<'_>]) {
        for source in [
            ProviderId::ClaudeCode,
            ProviderId::Codex,
            ProviderId::GeminiCli,
            ProviderId::AntigravityCli,
            ProviderId::OpenCode,
        ] {
            let records: Vec<_> = reqs
                .iter()
                .filter(|r| r.source == source)
                .map(|r| self.record(r))
                .collect();
            if records.is_empty() {
                continue;
            }
            self.tick += 1;
            let progress = SourceProgress {
                size: self.tick as u64,
                mtime_ms: self.tick,
            };
            let path = format!("/src/{}.jsonl", source.as_str());
            self.store
                .commit_file(&source, Path::new(&path), progress, &records)
                .unwrap();
        }
    }

    /// 把整库作为一页导入到 Mac 的导入库（配对名 `label`）。
    fn ship_to(&self, mac: &mut ImportStore, label: &str) {
        mac.pair(label, &self.machine_id(), T0).unwrap();
        let changes = self.store.changes_since(0, usize::MAX).unwrap();
        let through = changes.last().map_or(0, |c| c.seq);
        let page = UsagePage {
            v: VERSION,
            machine_id: self.machine_id(),
            records: changes.into_iter().map(|c| c.record).collect(),
        };
        let cursor = PageCursor {
            store_id: self.store.store_id().to_string(),
            after: 0,
            through,
        };
        mac.import_page(label, &cursor, &page.encode().unwrap())
            .unwrap();
    }
}

fn window() -> Window {
    Window::new(T0, T0 + 24 * HOUR).unwrap()
}

fn row<'a>(r: &'a Report, machine: &str, source: &str, model: &str) -> &'a Tally {
    &r.rows
        .iter()
        .find(|x| x.machine == machine && x.source == source && x.model == model)
        .unwrap_or_else(|| panic!("缺少 {machine}/{source}/{model}：{:#?}", r.rows))
        .tally
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-12)
}

/// 本机 + 一台 Runner，覆盖四种来源、同模型跨来源、窗口边界和撤回。
fn fixture() -> (Machine, Machine, ImportStore) {
    let mut mac = Machine::new("mac");
    mac.collect(&[
        req(
            "m1",
            ProviderId::ClaudeCode,
            "claude-opus-4-5",
            "anthropic",
            T0,
        ),
        req(
            "m2",
            ProviderId::ClaudeCode,
            "claude-opus-4-5",
            "anthropic",
            T0 + HOUR,
        ),
        req(
            "m3",
            ProviderId::Codex,
            "gpt-5-codex",
            "openai",
            T0 + 2 * HOUR,
        ),
        // 窗口外：结束时间是开区间，恰好在结束时刻的不算；开始前一毫秒的也不算。
        req(
            "m4",
            ProviderId::Codex,
            "gpt-5-codex",
            "openai",
            T0 + 24 * HOUR,
        ),
        req("m5", ProviderId::Codex, "gpt-5-codex", "openai", T0 - 1),
    ]);
    let mut runner = Machine::new("runner");
    runner.collect(&[
        req(
            "r1",
            ProviderId::ClaudeCode,
            "claude-opus-4-5",
            "anthropic",
            T0 + HOUR,
        ),
        // agy 跑的 Gemini：来源是 antigravity-cli，供应商按模型归 Google。
        req(
            "r2",
            ProviderId::AntigravityCli,
            "gemini-2.5-pro",
            "google",
            T0 + HOUR,
        ),
        // OpenCode 跑的 Claude：和 Claude Code 同模型，但来源不同，要分两行。
        req(
            "r3",
            ProviderId::OpenCode,
            "claude-opus-4-5",
            "anthropic",
            T0 + HOUR,
        ),
        req(
            "r4",
            ProviderId::OpenCode,
            "claude-opus-4-5",
            "anthropic",
            T0 + 3 * HOUR,
        ),
        req(
            "gone",
            ProviderId::OpenCode,
            "claude-opus-4-5",
            "anthropic",
            T0 + 3 * HOUR,
        ),
    ]);
    let gone = runner
        .record(&req(
            "gone",
            ProviderId::OpenCode,
            "claude-opus-4-5",
            "anthropic",
            0,
        ))
        .event_key;
    assert!(runner.store.retract(&ProviderId::OpenCode, &gone).unwrap());
    let mut import = ImportStore::in_memory(&mac.id).unwrap();
    runner.ship_to(&mut import, "runner-a");
    (mac, runner, import)
}

#[test]
fn groups_by_machine_source_vendor_model() {
    let (mac, runner, import) = fixture();
    let r = query(
        Some(&mac.store),
        Some(&import),
        &MachineFilter::All,
        window(),
    )
    .unwrap();
    let keys: Vec<_> = r
        .rows
        .iter()
        .map(|x| {
            (
                x.kind,
                x.machine.as_str(),
                x.source.as_str(),
                x.vendor.as_str(),
                x.model.as_str(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            (
                MachineKind::Local,
                "local",
                "claude-code",
                "anthropic",
                "claude-opus-4-5"
            ),
            (
                MachineKind::Local,
                "local",
                "codex",
                "openai",
                "gpt-5-codex"
            ),
            (
                MachineKind::Remote,
                "runner-a",
                "antigravity-cli",
                "google",
                "gemini-2.5-pro"
            ),
            (
                MachineKind::Remote,
                "runner-a",
                "claude-code",
                "anthropic",
                "claude-opus-4-5"
            ),
            (
                MachineKind::Remote,
                "runner-a",
                "opencode",
                "anthropic",
                "claude-opus-4-5"
            ),
        ]
    );
    assert_eq!(r.rows[0].machine_id, mac.machine_id());
    assert_eq!(r.rows[2].machine_id, runner.machine_id());

    // 本机 Claude 两条；Codex 只有窗口内一条（结束时刻那条与开始前那条都不算）。
    assert_eq!(
        row(&r, "local", "claude-code", "claude-opus-4-5").requests,
        2
    );
    assert_eq!(row(&r, "local", "codex", "gpt-5-codex").requests, 1);
    // 撤回的那条不计。
    let oc = row(&r, "runner-a", "opencode", "claude-opus-4-5");
    assert_eq!(
        (oc.requests, oc.input_tokens, oc.output_tokens),
        (2, 2_000, 200)
    );

    // 金额 = 每条请求单独查表之和：opus-4-5 一次 1000 输入 + 100 输出 = 0.005 + 0.0025。
    let opus = agenthome_core::pricing::lookup("claude-opus-4-5").unwrap();
    let one = (1_000.0 * opus.input + 100.0 * opus.output) / 1e6;
    assert!(close(oc.cost_usd, 2.0 * one), "{:?}", oc.cost_usd);
    assert_eq!(r.totals.requests, 7);
    assert_eq!(r.totals.unpriced_requests, 0);
    let sum: f64 = r.rows.iter().filter_map(|x| x.tally.cost_usd).sum();
    assert!(close(r.totals.cost_usd, sum));
}

/// 两次各 6 万提示的请求，合起来 12 万越过 haiku-5-5 的 10 万阈值；逐请求计价仍按基础价。
/// 单次 15 万的请求才整次按高价。
#[test]
fn long_context_tier_is_judged_per_request() {
    let model = "claude-haiku-5-5";
    let price = agenthome_core::pricing::lookup(model).unwrap();
    let lc = price.long_context.expect("haiku-5-5 有长上下文档");
    assert_eq!(
        lc.above_tokens, 100_000,
        "定价表阈值变了，调整本用例的 token 数"
    );

    let mut mac = Machine::new("mac");
    let small = |key, ts| Req {
        input: 60_000,
        output: 0,
        ..req(key, ProviderId::ClaudeCode, model, "anthropic", ts)
    };
    let (a, b) = (small("a", T0), small("b", T0 + 1));
    let big = Req {
        input: 150_000,
        output: 1_000,
        ..req("c", ProviderId::Codex, model, "anthropic", T0)
    };
    mac.collect(&[a, b, big]);
    let r = query(Some(&mac.store), None, &MachineFilter::Local, window()).unwrap();

    let split = row(&r, "local", "claude-code", model);
    assert_eq!(split.input_tokens, 120_000);
    let base = 120_000.0 * price.input / 1e6;
    let merged = 120_000.0 * lc.input / 1e6;
    assert!(
        close(split.cost_usd, base),
        "{:?} vs 基础价 {base}",
        split.cost_usd
    );
    assert!((merged - base).abs() > 1e-6, "合并计价会更贵，用例才有意义");

    let one = row(&r, "local", "codex", model);
    let high = (150_000.0 * lc.input + 1_000.0 * lc.output) / 1e6;
    assert!(
        close(one.cost_usd, high),
        "{:?} vs 高价档 {high}",
        one.cost_usd
    );
}

/// 缺价模型：token 照计，未知价请求数单列；整组没价时金额为空而不是 0；合计只含已计价部分。
#[test]
fn unpriced_models_keep_tokens_and_count_requests() {
    let mut mac = Machine::new("mac");
    let unknown = |key, ts| Req {
        model: Some("acme-unreleased-9"),
        vendor: None,
        ..req(key, ProviderId::OpenCode, "", "", ts)
    };
    let no_model = Req {
        model: None,
        vendor: None,
        ..req("n", ProviderId::AntigravityCli, "", "", T0)
    };
    mac.collect(&[
        unknown("u1", T0),
        unknown("u2", T0 + 1),
        no_model,
        req(
            "p",
            ProviderId::ClaudeCode,
            "claude-opus-4-5",
            "anthropic",
            T0,
        ),
    ]);
    let r = query(Some(&mac.store), None, &MachineFilter::Local, window()).unwrap();

    let u = row(&r, "local", "opencode", "acme-unreleased-9");
    assert_eq!(u.cost_usd, None, "整组没价是「未定价」，不是 0 元");
    assert_eq!((u.unpriced_requests, u.unpriced_tokens), (2, 2_200));
    assert_eq!(u.total_tokens, 2_200);

    // 模型缺失、来源又说不清厂商（agy 多厂商）：模型与供应商都记 unknown，不猜。
    let n = r
        .rows
        .iter()
        .find(|x| x.source == "antigravity-cli")
        .unwrap();
    assert_eq!(
        (n.vendor.as_str(), n.model.as_str()),
        ("unknown", "unknown")
    );
    assert_eq!(n.tally.unpriced_requests, 1);

    let p = row(&r, "local", "claude-code", "claude-opus-4-5");
    assert_eq!(r.totals.unpriced_requests, 3);
    assert_eq!(r.totals.unpriced_tokens, 3_300);
    assert_eq!(r.totals.total_tokens, 4_400);
    assert!(close(r.totals.cost_usd, p.cost_usd.unwrap()));
}

/// 全部机器 = 本机 + 每台远端：逐项相加相等，行集合也正好是各部分的并集。
#[test]
fn all_machines_equals_local_plus_each_remote() {
    let (mac, _runner, mut import) = fixture();
    let mut other = Machine::new("runner");
    other.collect(&[
        req("o1", ProviderId::Codex, "gpt-5-codex", "openai", T0 + HOUR),
        req(
            "o2",
            ProviderId::GeminiCli,
            "gemini-2.5-pro",
            "google",
            T0 + HOUR,
        ),
    ]);
    other.ship_to(&mut import, "runner-b");

    let all = query(
        Some(&mac.store),
        Some(&import),
        &MachineFilter::All,
        window(),
    )
    .unwrap();
    let mut parts = vec![
        query(
            Some(&mac.store),
            Some(&import),
            &MachineFilter::Local,
            window(),
        )
        .unwrap(),
    ];
    for (machine_id, _) in import.peers().unwrap() {
        parts.push(
            query(
                Some(&mac.store),
                Some(&import),
                &MachineFilter::Remote(machine_id),
                window(),
            )
            .unwrap(),
        );
    }
    assert_eq!(parts.len(), 3);

    let mut rows: Vec<_> = parts.iter().flat_map(|p| p.rows.clone()).collect();
    rows.sort_by(|a, b| {
        (a.kind, &a.machine, &a.source, &a.vendor, &a.model)
            .cmp(&(b.kind, &b.machine, &b.source, &b.vendor, &b.model))
    });
    assert_eq!(rows, all.rows);

    let t = |f: fn(&Tally) -> u64| parts.iter().map(|p| f(&p.totals)).sum::<u64>();
    assert_eq!(all.totals.requests, t(|x| x.requests));
    assert_eq!(all.totals.total_tokens, t(|x| x.total_tokens));
    assert_eq!(all.totals.input_tokens, t(|x| x.input_tokens));
    assert_eq!(all.totals.output_tokens, t(|x| x.output_tokens));
    assert_eq!(all.totals.unpriced_requests, t(|x| x.unpriced_requests));
    let cost: f64 = parts.iter().filter_map(|p| p.totals.cost_usd).sum();
    assert!((all.totals.cost_usd.unwrap() - cost).abs() < 1e-9);
    assert!(
        all.totals.requests > parts[0].totals.requests,
        "确实合进了远端"
    );
}

/// 条件写错要报错，不能悄悄返回空表让人以为没花钱。
#[test]
fn bad_filters_fail_loudly() {
    let (mac, runner, import) = fixture();
    let stranger = Machine::new("x").machine_id();
    assert!(
        query(
            Some(&mac.store),
            Some(&import),
            &MachineFilter::Remote(stranger),
            window()
        )
        .is_err()
    );
    assert!(query(None, Some(&import), &MachineFilter::Local, window()).is_err());
    assert!(
        query(
            Some(&mac.store),
            None,
            &MachineFilter::Remote(runner.machine_id()),
            window()
        )
        .is_err()
    );
    assert!(Window::new(T0, T0).is_err());
    // 本机还没采集过时「全部」只算远端，不报错。
    let r = query(None, Some(&import), &MachineFilter::All, window()).unwrap();
    assert!(r.rows.iter().all(|x| x.kind == MachineKind::Remote));
}

/// 首页花费卡（CARM-301）把对账表的行换成花费汇总：每种机器条件下，首页的金额、token、未计价 token
/// 都必须和 CLI 对账表的合计一致，否则用户在首页看到的和 `agenthome report` 对不上。
/// 远端单看时，本机的用量一条都不能混进来。
#[test]
fn home_cost_summary_matches_report_for_every_machine_filter() {
    let (mut mac, _runner, import) = fixture();
    mac.collect(&[Req {
        model: Some("acme-unreleased-9"),
        vendor: None,
        ..req("u", ProviderId::OpenCode, "", "", T0 + HOUR)
    }]);
    let runner_id = import.peers().unwrap()[0].0.clone();
    for filter in [
        MachineFilter::All,
        MachineFilter::Local,
        MachineFilter::Remote(runner_id),
    ] {
        let r = query(Some(&mac.store), Some(&import), &filter, window()).unwrap();
        let rows: Vec<_> = r.rows.iter().map(|x| x.cost_breakdown()).collect();
        let s = agenthome_core::cost::CostSummary::from_breakdown(&rows);
        assert!(
            (s.total_usd - r.totals.cost_usd.unwrap_or(0.0)).abs() < 1e-9,
            "{filter:?}"
        );
        let tokens: u64 = s.vendors.iter().map(|v| v.tokens).sum();
        assert_eq!(tokens, r.totals.total_tokens, "{filter:?}");
        assert_eq!(s.unpriced_tokens, r.totals.unpriced_tokens, "{filter:?}");
        let unpriced_model = s.models.iter().any(|m| m.model == "acme-unreleased-9");
        assert_eq!(
            unpriced_model,
            !matches!(filter, MachineFilter::Remote(_)),
            "未计价模型只属于本机：{filter:?}"
        );
    }
}
