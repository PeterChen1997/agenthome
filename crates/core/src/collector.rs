//! 采集服务的一轮调度（CARM-284 方案单元 10）。
//!
//! 采集端常驻在别人的机器上（Runner 同机还有数据库等服务），所以每一轮都受三条上限约束：
//! - **并发 1**：同一状态目录只允许一个采集进程（锁文件），文件逐个读，不开线程。
//! - **源读取限速**：按读到的文件大小扣令牌桶（默认 2 MiB/s，突发 1 秒）。整文件解析的来源
//!   没法在解析器里逐块限速，所以是「读之前先等够额度」：任意时长 T 内读取量 ≤ 速率 × T + 突发。
//! - **状态目录预算**：用量库所在目录（库、WAL、临时文件）合计超过「预算 − 提交预留」就暂停
//!   读新文件，没读的文件进度不前移、已有用量和待导出的变更一条不删，只报告积压；腾出空间后下一轮继续。
//!
//! 首扫 / 历史回填按「分片」走：一轮最多读 `shard_bytes`，剩下的记为积压、下一轮接着读。
//! 每个文件的用量和进度同事务提交（`usage_store`），所以进程任何时刻被停，重启后从没提交的文件续上，
//! 不重不漏。同一进程里下一轮从上一轮最后读的文件之后开始，活跃的大文件不会饿死后面的文件。

use crate::device::DeviceIdentity;
use crate::diagnostics::{ErrorDiagnostic, ResultStep};
use crate::model::ProviderId;
use crate::providers::SourceProvider;
use crate::usage_store::{CommitStats, FailureKind, SourceProgress, UsageStore, collect_file};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;

/// 资源上限。默认值见方案「Runner 资源预算」。
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 常态两轮之间的间隔（从上一轮开始算）；有积压且没暂停时下一轮立即开始，由限速控制节奏。
    pub interval: Duration,
    /// 源读取限速，字节 / 秒。
    pub read_bytes_per_sec: u64,
    /// 一轮最多读这么多字节（至少读一个文件，单个文件更大也读完它）。
    pub shard_bytes: u64,
    /// 状态目录合计预算。
    pub state_budget_bytes: u64,
    /// 给一次提交（含 WAL 增长）预留的空间：已用 + 预留 > 预算 就暂停。
    pub commit_reserve_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            read_bytes_per_sec: 2 * MIB,
            shard_bytes: 64 * MIB,
            state_budget_bytes: 512 * MIB,
            commit_reserve_bytes: 32 * MIB,
        }
    }
}

/// 令牌桶：速率 `rate` 字节 / 秒，桶容量 1 秒的量。
#[derive(Debug)]
pub struct Throttle {
    rate: f64,
    available: f64,
    last: Instant,
}

impl Throttle {
    pub fn new(bytes_per_sec: u64, now: Instant) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        Self {
            rate,
            available: rate,
            last: now,
        }
    }

    /// 预定读 `bytes` 字节，返回读之前要等多久。额度可以透支：等够返回的时长后正好还清。
    pub fn reserve(&mut self, bytes: u64, now: Instant) -> Duration {
        let refill = now.saturating_duration_since(self.last).as_secs_f64() * self.rate;
        self.available = (self.available + refill).min(self.rate) - bytes as f64;
        self.last = now;
        if self.available >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.available / self.rate)
        }
    }
}

/// 一轮的结果。写入日志和状态文件的是计数与运行指标，不含路径、文件名或报错原文。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RoundReport {
    /// 本轮结束时间（UTC 毫秒）。
    pub finished_at_ms: i64,
    pub elapsed_ms: u64,
    /// 发现的来源文件数。
    pub files: usize,
    /// 进度没变、没读的文件数。
    pub unchanged: usize,
    /// 本轮读了的文件数与字节数（按读前大小计）。
    pub read_files: usize,
    pub read_bytes: u64,
    /// 因限速等待的总时长。
    pub throttled_ms: u64,
    pub stats: CommitStats,
    pub warnings: usize,
    pub failures: BTreeMap<FailureKind, usize>,
    /// One privacy-safe sample per failure kind for app telemetry.
    #[serde(skip)]
    pub failure_diagnostics: BTreeMap<FailureKind, (ProviderId, ErrorDiagnostic)>,
    /// 有变化但本轮没读的文件（分片用完或预算暂停），进度没前移，之后的轮次接着读。
    pub pending_files: usize,
    pub pending_bytes: u64,
    /// 状态目录接近预算，本轮暂停了读新文件。
    pub paused_for_budget: bool,
    /// 本轮结束时状态目录占用与预算。
    pub state_bytes: u64,
    pub state_budget_bytes: u64,
    /// 用量库高水位（已分配的最大 `seq`）。
    pub high_water: u64,
    pub hidden: usize,
    pub revived: usize,
}

impl RoundReport {
    pub fn failed(&self) -> usize {
        self.failures.values().sum()
    }

    fn record_failure(&mut self, provider: &dyn SourceProvider, error: &anyhow::Error) {
        let kind = FailureKind::of(error);
        *self.failures.entry(kind).or_default() += 1;
        self.failure_diagnostics
            .entry(kind)
            .or_insert_with(|| (provider.id(), ErrorDiagnostic::from_error(error)));
    }
}

pub struct Collector {
    store: UsageStore,
    identity: DeviceIdentity,
    providers: Vec<Box<dyn SourceProvider + Send + Sync>>,
    state_dir: PathBuf,
    limits: Limits,
    throttle: Throttle,
    /// 上一轮最后读的文件（来源序号, 路径），下一轮从它之后开始。
    cursor: Option<(usize, PathBuf)>,
    _lock: File,
}

impl Collector {
    /// 打开采集器：取得状态目录（用量库所在目录）的独占锁，加载或创建本机身份，打开用量库。
    /// 已有采集进程在跑时报错退出，保证并发 1。
    pub fn open(
        identity_path: &Path,
        usage_db: &Path,
        providers: Vec<Box<dyn SourceProvider + Send + Sync>>,
        limits: Limits,
    ) -> Result<Self> {
        let state_dir = usage_db
            .parent()
            .context("用量库路径没有上级目录")?
            .to_path_buf();
        std::fs::create_dir_all(&state_dir)
            .context("创建状态目录")
            .step("collect.create_state_dir")?;
        let lock = File::create(state_dir.join("collector.lock"))
            .context("创建锁文件")
            .step("collect.create_lock_file")?;
        if lock.try_lock().is_err() {
            bail!("已有采集进程在使用这个状态目录");
        }
        let identity =
            DeviceIdentity::load_or_create(identity_path, &crate::device::current_hostname())
                .step("collect.load_identity")?;
        let store = UsageStore::open(usage_db, &identity).step("collect.open_usage_store")?;
        Ok(Self {
            store,
            identity,
            providers,
            state_dir,
            limits,
            throttle: Throttle::new(limits.read_bytes_per_sec, Instant::now()),
            cursor: None,
            _lock: lock,
        })
    }

    pub fn store(&self) -> &UsageStore {
        &self.store
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// 状态目录里各文件大小之和（库、WAL、SHM、锁、状态文件……）。
    pub fn state_bytes(&self) -> Result<u64> {
        let mut total = 0;
        for entry in std::fs::read_dir(&self.state_dir).context("读状态目录")? {
            let meta = entry?.metadata()?;
            if meta.is_file() {
                total += meta.len();
            }
        }
        Ok(total)
    }

    fn has_room(&self, checkpointed: &mut bool) -> Result<bool> {
        let fits = |used: u64| {
            used.saturating_add(self.limits.commit_reserve_bytes) <= self.limits.state_budget_bytes
        };
        if fits(self.state_bytes().step("collect.state_bytes")?) {
            return Ok(true);
        }
        if !*checkpointed {
            // WAL 可能占了大头：合并截断后再量一次。
            *checkpointed = true;
            self.store.checkpoint().step("usage.local.checkpoint")?;
            return Ok(fits(self.state_bytes().step("collect.state_bytes")?));
        }
        Ok(false)
    }

    /// 跑一轮：发现全部来源文件、更新可见性，按分片、限速和预算读有变化的文件。
    pub fn run_round(&mut self) -> Result<RoundReport> {
        let started = Instant::now();
        let mut report = RoundReport {
            state_budget_bytes: self.limits.state_budget_bytes,
            ..RoundReport::default()
        };

        let mut queue: Vec<(usize, PathBuf)> = Vec::new();
        for (i, provider) in self.providers.iter().enumerate() {
            let files = if provider.detect() {
                provider.discover().files
            } else {
                Vec::new()
            };
            let (hidden, revived) = self
                .store
                .refresh_visibility(&provider.id(), &files)
                .step("usage.local.refresh_visibility")?;
            report.hidden += hidden;
            report.revived += revived;
            queue.extend(files.into_iter().map(|f| (i, f)));
        }
        queue.sort();
        report.files = queue.len();
        if let Some(cursor) = &self.cursor {
            let start = queue.iter().position(|k| k > cursor).unwrap_or(0);
            queue.rotate_left(start);
        }

        let mut shard_used = 0u64;
        let mut paused = false;
        let mut checkpointed = false;
        for (i, path) in queue {
            let provider = &*self.providers[i];
            let progress = match SourceProgress::of(&path).step("collect.source_metadata") {
                Ok(p) => p,
                Err(e) => {
                    report.record_failure(provider, &e);
                    continue;
                }
            };
            if self
                .store
                .progress(&provider.id(), &path)
                .step("usage.local.scan_progress")?
                == Some(progress)
            {
                report.unchanged += 1;
                continue;
            }
            let shard_full = shard_used > 0 && shard_used >= self.limits.shard_bytes;
            if !paused && !shard_full {
                paused = !self.has_room(&mut checkpointed)?;
            }
            if paused || shard_full {
                report.pending_files += 1;
                report.pending_bytes += progress.size;
                continue;
            }

            let wait = self.throttle.reserve(progress.size, Instant::now());
            if !wait.is_zero() {
                std::thread::sleep(wait);
                report.throttled_ms += wait.as_millis() as u64;
            }
            shard_used += progress.size.max(1);
            self.cursor = Some((i, path.clone()));
            match collect_file(&mut self.store, &self.identity, provider, &path) {
                Ok(outcome) => {
                    report.read_files += usize::from(!outcome.skipped);
                    report.read_bytes += progress.size;
                    report.stats.add(outcome.stats);
                    report.warnings += outcome.warnings.len();
                }
                Err(e) => report.record_failure(provider, &e),
            }
        }

        report.paused_for_budget = paused;
        report.state_bytes = self.state_bytes().step("collect.state_bytes")?;
        report.high_water = self.store.high_water().step("usage.local.high_water")?;
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        report.finished_at_ms = chrono::Utc::now().timestamp_millis();
        Ok(report)
    }

    /// 下一轮前该等多久：有积压且没被预算卡住就立即继续（节奏交给限速），否则等到间隔满。
    pub fn next_delay(&self, report: &RoundReport) -> Duration {
        if report.pending_files > 0 && !report.paused_for_budget {
            Duration::ZERO
        } else {
            self.limits
                .interval
                .saturating_sub(Duration::from_millis(report.elapsed_ms))
        }
    }

    /// 把本轮结果写成状态目录里的 `collector-status.json`（原子替换），供以后导出给 Mac 显示积压。
    pub fn write_status(&self, report: &RoundReport) -> Result<()> {
        let json = serde_json::to_vec_pretty(report)?;
        crate::statusline_setup::atomic_write(
            &self.state_dir.join("collector-status.json"),
            &json,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 限速的意义：任意窗口内读取量 ≤ 速率 × 时长 + 1 秒突发，Runner 上的磁盘不被采集打满。
    #[test]
    fn throttle_bounds_bytes_over_time() {
        let t0 = Instant::now();
        let mut t = Throttle::new(2 * MIB, t0);
        // 同一时刻连读 10 个 1 MiB：头 2 MiB 用突发额度，之后每 MiB 等 0.5 秒，累计等 4 秒。
        let mut now = t0;
        let mut waited = Duration::ZERO;
        for _ in 0..10 {
            let w = t.reserve(MIB, now);
            now += w;
            waited += w;
        }
        assert!((waited.as_secs_f64() - 4.0).abs() < 1e-6, "{waited:?}");

        // 空闲很久，额度也只攒到 1 秒的量，不会攒出一大波突发。
        let later = now + Duration::from_secs(3600);
        assert_eq!(t.reserve(2 * MIB, later), Duration::ZERO);
        let w = t.reserve(MIB, later);
        assert!((w.as_secs_f64() - 0.5).abs() < 1e-6, "{w:?}");
    }

    /// 单个文件比 1 秒额度大：照读，但读前等够，均速仍不超限。
    #[test]
    fn throttle_large_single_read_waits_proportionally() {
        let t0 = Instant::now();
        let mut t = Throttle::new(2 * MIB, t0);
        let w = t.reserve(20 * MIB, t0);
        assert!((w.as_secs_f64() - 9.0).abs() < 1e-6, "{w:?}");
    }
}
