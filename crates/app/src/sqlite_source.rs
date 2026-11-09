//! 基于 `agenthome_core::Store` 的数据源。`rusqlite::Connection` 只 `Send` 不 `Sync`，
//! 用 `Mutex` 包一层满足 `DataSource: Send + Sync`；查询都在后台线程发生，UI 线程不阻塞。

use std::path::Path;
use std::sync::Mutex;

use agenthome_core::model::{
    CostBreakdown, DailyUsage, Message, Period, ProviderActivity, ProviderHealth, ProviderId,
    QuotaSnapshot, SearchHit, SessionQuery, SessionSummary, UsageTotals,
};
use agenthome_core::{
    ClaudeCodeProvider, CodexProvider, GeminiProvider, SourceProvider, Store, claude_statusline,
    codex_rollout_quota, paths,
};
use anyhow::{Context as _, anyhow};

use crate::data::{DataSource, ProjectRow};

pub struct SqliteDataSource {
    store: Mutex<Store>,
    claude: ClaudeCodeProvider,
    codex: CodexProvider,
    gemini: GeminiProvider,
}

impl SqliteDataSource {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let store =
            Store::open(path).with_context(|| format!("打开索引库失败：{}", path.display()))?;
        Ok(Self {
            store: Mutex::new(store),
            claude: ClaudeCodeProvider::new(paths::claude_projects_dir()),
            codex: CodexProvider::new(paths::codex_sessions_dir()),
            gemini: GeminiProvider::new(paths::gemini_dir()),
        })
    }

    fn with_store<T>(&self, f: impl FnOnce(&Store) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let guard = self.store.lock().map_err(|_| anyhow!("索引库锁已中毒"))?;
        f(&guard)
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

    fn by_model(&self, period: Period) -> anyhow::Result<Vec<CostBreakdown>> {
        self.with_store(|store| store.stats_by_model(period))
    }

    fn by_project(&self, period: Period, limit: usize) -> anyhow::Result<Vec<CostBreakdown>> {
        self.with_store(|store| store.stats_by_project(period, limit))
    }

    fn archive_health(&self) -> anyhow::Result<Vec<ProviderHealth>> {
        self.with_store(|store| store.archive_health())
    }

    fn activity(&self) -> anyhow::Result<Vec<ProviderActivity>> {
        let providers: [&dyn SourceProvider; 3] = [&self.claude, &self.codex, &self.gemini];
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
        if let Some(s) = codex_rollout_quota(None)? {
            out.push(s);
        }
        Ok(out)
    }
}
