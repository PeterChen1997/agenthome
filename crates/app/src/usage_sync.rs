//! 用量同步线程（CARM-299，方案单元 12）：本机用量采集 + 远端拉取，同一个后台调度。
//!
//! - **本机采集**：和 Runner 上的 `agenthome collect` 是同一个 [`Collector`]，按单次请求把用量写进
//!   `~/AgentHome/usage/usage.db`（首页统计读它，CARM-298）。采集器持有状态目录的独占锁，命令行的
//!   采集进程在跑时 App 不另起一份，下一轮再试。
//! - **远端拉取**：[`remote_pull::pull_ssh`]，打开即拉、常态 5 分钟、有积压很快再拉、失败退避、
//!   休眠醒来立即补拉（[`PullSchedule`]，按墙钟判断：Mac 休眠时单调时钟停走）。
//!
//! 同一远端同时只有一个拉取：这里一台台按顺序拉，另有跨进程文件锁。每页用量和游标在导入库同一个
//! 事务里提交，采集也是每个文件的用量和进度同事务提交：App 退出、休眠、断网时线程停在哪都不会留下
//! 半提交，下次从已确认的位置续上。
//!
//! 失败上报 Sentry 带阶段和类别，本机采集再附安全的错误诊断，每种每次运行只报一次。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agenthome_core::collector::{Collector, Limits, RoundReport};
use agenthome_core::device::DeviceIdentity;
use agenthome_core::diagnostics::{ErrorDiagnostic, local_error_message};
use agenthome_core::model::ProviderId;
use agenthome_core::providers::opencode::OpenCodeProvider;
use agenthome_core::providers::registry;
use agenthome_core::remote_pull::{self, PullFailure, PullOutcome, PullSchedule};
use agenthome_core::remote_ssh;
use agenthome_core::usage_import::ImportStore;
use agenthome_core::usage_store::FailureKind;
use agenthome_core::{SourceProvider, paths};

use crate::format::now_ms;
use crate::sync::{RemotePull, SyncState, update};
use crate::telemetry;

const MIB: u64 = 1024 * 1024;

/// Mac 上的采集上限。Runner 的默认值（2 MiB/s）是为了不挤同机的数据库；Mac 是自己的机器，
/// 读快一些让首扫几分钟内追上，但一轮最多读一个分片，好让远端拉取不被首扫挡太久。
fn mac_limits() -> Limits {
    Limits {
        interval: Duration::from_secs(60),
        read_bytes_per_sec: 256 * MIB,
        shard_bytes: 1024 * MIB,
        state_budget_bytes: 2048 * MIB,
        ..Limits::default()
    }
}

/// 每种失败每次运行只报一次：`(远端名或空, 阶段, 类别)`。
type Reported = HashSet<(String, &'static str, &'static str)>;

fn report_once(
    reported: &mut Reported,
    who: &str,
    stage: &'static str,
    kind: &'static str,
    diagnostic: Option<(&ErrorDiagnostic, Option<&ProviderId>)>,
) {
    if reported.insert((who.to_string(), stage, kind)) {
        telemetry::report_usage_failure(stage, kind, diagnostic);
    }
}

fn report_collect_error(reported: &mut Reported, error: &anyhow::Error) {
    report_once(
        reported,
        "",
        "collect",
        FailureKind::of(error).as_str(),
        Some((&ErrorDiagnostic::from_error(error), None)),
    );
}

pub fn run(data_dir: PathBuf, shared: Arc<Mutex<SyncState>>) {
    let mut local = Local::default();
    let mut remotes = Remotes::default();
    let mut reported = Reported::new();
    loop {
        // 本机身份由同步线程在启动时建好（含已有会话标签的迁移）；还没有就等下一轮，不抢着建。
        let identity = DeviceIdentity::load(&data_dir.join("device.json"))
            .ok()
            .flatten();
        if let Some(identity) = &identity {
            local.tick(&data_dir, &shared, &mut reported);
            remotes.tick(&data_dir, identity, &shared, &mut reported);
        }
        let now = now_ms();
        let next = [local.next_due_ms, remotes.next_due_ms()]
            .into_iter()
            .min()
            .unwrap_or(i64::MAX);
        let wait = (next - now).clamp(200, remote_pull::TICK_MS);
        std::thread::sleep(Duration::from_millis(wait as u64));
    }
}

#[derive(Default)]
struct Local {
    collector: Option<Collector>,
    next_due_ms: i64,
}

impl Local {
    fn tick(&mut self, data_dir: &Path, shared: &Mutex<SyncState>, reported: &mut Reported) {
        let now = now_ms();
        if now < self.next_due_ms {
            return;
        }
        if self.collector.is_none() {
            let mut sources: Vec<Box<dyn SourceProvider + Send + Sync>> =
                registry::instantiate_all();
            // OpenCode 只接了用量，不在来源注册表里（CARM-293）。
            sources.push(Box::new(OpenCodeProvider::new(paths::opencode_dir())));
            match Collector::open(
                &data_dir.join("device.json"),
                &data_dir.join("usage").join("usage.db"),
                sources,
                mac_limits(),
            ) {
                Ok(collector) => self.collector = Some(collector),
                Err(error) => {
                    // 最常见的是命令行采集进程正占着锁；那边在采，这里一分钟后再试。
                    let message = local_error_message(&error);
                    if !message.contains("已有采集进程") {
                        report_collect_error(reported, &error);
                    }
                    update(shared, |s| s.local_usage.error = Some(message));
                    self.next_due_ms = now + mac_limits().interval.as_millis() as i64;
                    return;
                }
            }
        }
        let Some(collector) = self.collector.as_mut() else {
            return;
        };
        match collector.run_round() {
            Ok(report) => {
                for (kind, (provider, diagnostic)) in &report.failure_diagnostics {
                    report_once(
                        reported,
                        "",
                        "collect",
                        kind.as_str(),
                        Some((diagnostic, Some(provider))),
                    );
                }
                self.next_due_ms = now_ms() + collector.next_delay(&report).as_millis() as i64;
                apply_round(shared, &report);
            }
            Err(error) => {
                report_collect_error(reported, &error);
                let message = local_error_message(&error);
                update(shared, |s| s.local_usage.error = Some(message));
                self.next_due_ms = now_ms() + collector.limits().interval.as_millis() as i64;
            }
        }
    }
}

fn apply_round(shared: &Mutex<SyncState>, report: &RoundReport) {
    update(shared, |s| {
        s.local_usage.last_ok_ms = Some(report.finished_at_ms);
        s.local_usage.pending_files = report.pending_files;
        s.local_usage.failed = report.failed();
        s.local_usage.error = None;
        if report.stats.inserted + report.stats.revised > 0 {
            s.data_revision += 1;
        }
    });
}

#[derive(Default)]
struct Remotes {
    schedules: HashMap<String, PullSchedule>,
    store: Option<ImportStore>,
}

impl Remotes {
    fn next_due_ms(&self) -> i64 {
        self.schedules
            .values()
            .map(PullSchedule::next_due_ms)
            .min()
            .unwrap_or(i64::MAX)
    }

    fn tick(
        &mut self,
        data_dir: &Path,
        identity: &DeviceIdentity,
        shared: &Mutex<SyncState>,
        reported: &mut Reported,
    ) {
        let names = remote_pull::list_remotes(data_dir);
        self.schedules.retain(|name, _| names.contains(name));
        sync_names(data_dir, &names, shared);
        if names.is_empty() {
            return;
        }
        if self.store.is_none() {
            match ImportStore::open(&data_dir.join("remote").join("usage.db"), identity) {
                Ok(store) => self.store = Some(store),
                Err(error) => {
                    let failure = PullFailure::of_import(&error);
                    report_once(reported, "", failure.stage(), failure.kind(), None);
                    let label = format!("{}/{}", failure.stage(), failure.kind());
                    let detail = format!("{error:#}");
                    update(shared, |s| {
                        for r in &mut s.remotes {
                            r.failure = Some(label.clone());
                            r.last_error = Some(detail.clone());
                        }
                    });
                    return;
                }
            }
        }
        let Some(store) = self.store.as_mut() else {
            return;
        };
        for name in &names {
            let schedule = self.schedules.entry(name.clone()).or_default();
            if !schedule.due(now_ms()) {
                continue;
            }
            set_remote(shared, name, |r| r.running = true);
            let result =
                remote_pull::pull_ssh(data_dir, store, identity, name, &|| false, now_ms());
            let done = now_ms();
            match &result {
                Ok(outcome) => {
                    schedule.on_success(done, outcome);
                    if let Ok(dir) = remote_ssh::remote_dir(data_dir, name) {
                        let _ = remote_pull::save_last_ok(&dir, done);
                    }
                }
                Err(error) => {
                    schedule.on_failure(done);
                    report_once(
                        reported,
                        name,
                        error.failure.stage(),
                        error.failure.kind(),
                        None,
                    );
                }
            }
            apply_pull(shared, name, done, &result);
        }
    }
}

/// 状态里的远端列表跟目录保持一致；新出现的远端读回上次成功时间。
fn sync_names(data_dir: &Path, names: &[String], shared: &Mutex<SyncState>) {
    update(shared, |s| {
        s.remotes.retain(|r| names.contains(&r.name));
        for name in names {
            if !s.remotes.iter().any(|r| &r.name == name) {
                let last_ok_ms = remote_ssh::remote_dir(data_dir, name)
                    .ok()
                    .and_then(|dir| remote_pull::load_last_ok(&dir));
                s.remotes.push(RemotePull {
                    name: name.clone(),
                    last_ok_ms,
                    ..RemotePull::default()
                });
            }
        }
        s.remotes.sort_by(|a, b| a.name.cmp(&b.name));
    });
}

fn set_remote(shared: &Mutex<SyncState>, name: &str, f: impl FnOnce(&mut RemotePull)) {
    update(shared, |s| {
        if let Some(r) = s.remotes.iter_mut().find(|r| r.name == name) {
            f(r);
        }
    });
}

fn apply_pull(
    shared: &Mutex<SyncState>,
    name: &str,
    done: i64,
    result: &Result<PullOutcome, remote_pull::PullError>,
) {
    update(shared, |s| {
        if result.as_ref().is_ok_and(|o| o.inserted + o.replaced > 0) {
            s.data_revision += 1;
        }
        let Some(r) = s.remotes.iter_mut().find(|r| r.name == name) else {
            return;
        };
        r.running = false;
        match result {
            Ok(outcome) => {
                r.last_ok_ms = Some(done);
                r.backlog = outcome.backlog;
                r.in_snapshot = outcome.in_snapshot;
                r.failure = None;
                r.last_error = None;
            }
            Err(error) => {
                r.failure = Some(format!(
                    "{}/{}",
                    error.failure.stage(),
                    error.failure.kind()
                ));
                r.last_error = Some(error.detail.clone());
            }
        }
    });
}
