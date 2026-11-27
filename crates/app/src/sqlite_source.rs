//! 基于 `agenthome_core::Store` 的数据源。`rusqlite::Connection` 只 `Send` 不 `Sync`，
//! 用 `Mutex` 包一层满足 `DataSource: Send + Sync`；查询都在后台线程发生，UI 线程不阻塞。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agenthome_core::device::DeviceIdentity;
use agenthome_core::diagnostics::ResultStep;
use agenthome_core::model::{
    CostBreakdown, DailyUsage, Message, Period, ProviderActivity, ProviderHealth, ProviderId,
    QuotaSnapshot, SearchHit, SessionQuery, SessionSummary, UsageTotals,
};
use agenthome_core::providers::registry;
use agenthome_core::usage_import::ImportStore;
use agenthome_core::usage_report::{self, MachineFilter, Window};
use agenthome_core::usage_store::UsageStore;
use agenthome_core::{SourceProvider, Store, claude_statusline, codex_rollout_quota, paths};
use anyhow::{Context as _, anyhow};

use crate::data::{DataSource, ProjectRow};

pub struct SqliteDataSource {
    store: Mutex<Store>,
    /// 来源注册表里的全部来源（活动探测用）。
    providers: Vec<Box<dyn SourceProvider + Send + Sync>>,
    /// 用量库所在的数据目录（`~/AgentHome`）。只在打开默认索引库时有：`--db` 指定的测试库
    /// 不碰真实用量库（和 `sync.rs` 里是否启动用量同步的判断一致），花费卡退回读索引。
    usage_data_dir: Option<PathBuf>,
}

impl SqliteDataSource {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let store = Store::open(path)
            .with_context(|| t!("sync.index_open_failed", path = path.display()).to_string())?;
        Ok(Self {
            store: Mutex::new(store),
            providers: registry::instantiate_all(),
            usage_data_dir: (path == paths::default_index_db()).then(paths::default_data_dir),
        })
    }

    fn with_store<T>(&self, f: impl FnOnce(&Store) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let guard = self
            .store
            .lock()
            .map_err(|_| anyhow!(t!("sync.index_lock_poisoned").to_string()))?;
        f(&guard)
    }

    fn load_usage_identity(data: &Path) -> anyhow::Result<Option<DeviceIdentity>> {
        DeviceIdentity::load(&data.join("device.json")).step("usage.load_identity")
    }

    fn open_remote_usage(
        data: &Path,
        identity: &DeviceIdentity,
    ) -> anyhow::Result<Option<ImportStore>> {
        ImportStore::open_read_only(&data.join("remote").join("usage.db"), identity)
            .step("usage.remote.open_read_only")
    }
}

impl DataSource for SqliteDataSource {
    fn projects(&self) -> anyhow::Result<Vec<ProjectRow>> {
        self.with_store(|store| {
            Ok(store
                .projects()?
                .into_iter()
                .map(|(name, cwd, count)| ProjectRow {
                    name,
                    cwd: Some(cwd),
                    session_count: u32::try_from(count).unwrap_or(u32::MAX),
                })
                .collect())
        })
    }

    fn sessions(&self, q: &SessionQuery) -> anyhow::Result<Vec<SessionSummary>> {
        self.with_store(|store| store.list_sessions(q))
    }

    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        self.with_store(|store| store.search_messages(query, limit))
    }

    fn messages(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        self.with_store(|store| store.messages(session_id))
    }

    fn daily(&self, days: u32) -> anyhow::Result<Vec<DailyUsage>> {
        self.with_store(|store| store.stats_daily(days))
    }

    fn totals(&self, period: Period, provider: Option<ProviderId>) -> anyhow::Result<UsageTotals> {
        self.with_store(|store| store.stats_totals(period, provider.as_ref()))
    }

    /// 读采集用量库（CARM-298 的按机器统计），和 CLI `agenthome report` 同一口径。
    /// 本机还没有用量库（首次采集未完成）时本机部分为空，不报错。
    fn cost_by_model(
        &self,
        period: Period,
        machine: &MachineFilter,
    ) -> anyhow::Result<Vec<CostBreakdown>> {
        let Some(data) = &self.usage_data_dir else {
            return self
                .with_store(|store| store.stats_by_model(period))
                .step("index.stats_by_model");
        };
        let Some(identity) = Self::load_usage_identity(data)? else {
            return Ok(Vec::new());
        };
        let local = UsageStore::open_read_only(&data.join("usage").join("usage.db"), &identity)
            .step("usage.local.open_read_only")?;
        if local.is_none() && *machine == MachineFilter::Local {
            return Ok(Vec::new());
        }
        let remote = if *machine == MachineFilter::Local {
            None
        } else {
            Self::open_remote_usage(data, &identity)?
        };
        let window =
            Window::new(period.since_ms().unwrap_or(0), i64::MAX).step("usage.report_window")?;
        let report = usage_report::query(local.as_ref(), remote.as_ref(), machine, window)
            .step("usage.report_query")?;
        Ok(report
            .rows
            .iter()
            .map(usage_report::ReportRow::cost_breakdown)
            .collect())
    }

    fn remote_machines(&self) -> anyhow::Result<Vec<(String, String)>> {
        let Some(data) = &self.usage_data_dir else {
            return Ok(Vec::new());
        };
        let Some(identity) = Self::load_usage_identity(data)? else {
            return Ok(Vec::new());
        };
        match Self::open_remote_usage(data, &identity)? {
            Some(remote) => remote.peers().step("usage.remote.read_peers"),
            _ => Ok(Vec::new()),
        }
    }

    fn by_project(&self, period: Period, limit: usize) -> anyhow::Result<Vec<CostBreakdown>> {
        self.with_store(|store| store.stats_by_project(period, limit))
    }

    fn archive_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        self.with_store(|store| store.archive_health())
    }

    fn activity(&self) -> anyhow::Result<Vec<ProviderActivity>> {
        let providers: Vec<&dyn SourceProvider> = self
            .providers
            .iter()
            .map(|p| -> &dyn SourceProvider { p.as_ref() })
            .collect();
        self.with_store(|store| store.activity(&providers))
    }

    fn quota(&self) -> anyhow::Result<Vec<QuotaSnapshot>> {
        // 两个信号互不依赖；一个读不到不影响另一个，但错误不吞：记到返回值里由 UI 展示。
        let mut out = Vec::new();
        // Claude 额度只走官方 /usage（后台每 15 分钟读一次）。旧版装过的 statusline 包装脚本
        // 写的快照只在 /usage 还没有任何数据时兜底（例如 claude CLI 尚未登录）。
        let usage = agenthome_core::signals::claude_usage_snapshot(None)?;
        let has_usage = usage.as_ref().is_some_and(|s| !s.windows.is_empty());
        let fallback = if has_usage {
            None
        } else {
            claude_statusline(None)?
        };
        if let Some(s) = agenthome_core::signals::merge_claude(fallback, usage) {
            out.push(s);
        }
        if let Some(mut s) = codex_rollout_quota(None)? {
            // 重置额度只在用户手动查询后才有（设置页「查询」），查过就并进来。
            if let Some(reset) = agenthome_core::signals::load_reset_credits(None)? {
                agenthome_core::signals::apply_reset_credits(
                    &mut s,
                    &reset,
                    crate::format::now_ms(),
                );
            }
            out.push(s);
        }
        // Antigravity CLI 额度：后台每 15 分钟跑一次官方 `agy -p /usage`。
        if let Some(s) = agenthome_core::signals::agy_usage_snapshot(None)? {
            out.push(s);
        }
        Ok(out)
    }
}
