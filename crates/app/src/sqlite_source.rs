//! 基于 `agenthome_core::Store` 的数据源。`rusqlite::Connection` 只 `Send` 不 `Sync`，
//! 用 `Mutex` 包一层满足 `DataSource: Send + Sync`；查询都在后台线程发生，UI 线程不阻塞。

use std::path::Path;
use std::sync::Mutex;

use agenthome_core::Store;
use agenthome_core::model::{DailyUsage, Message, SessionQuery, SessionSummary};
use anyhow::{Context as _, anyhow};

use crate::data::{DataSource, ProjectRow};

pub struct SqliteDataSource {
    store: Mutex<Store>,
}

impl SqliteDataSource {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let store =
            Store::open(path).with_context(|| format!("打开索引库失败：{}", path.display()))?;
        Ok(Self {
            store: Mutex::new(store),
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

    fn messages(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        self.with_store(|store| store.messages(session_id))
    }

    fn daily(&self, days: u32) -> anyhow::Result<Vec<DailyUsage>> {
        self.with_store(|store| store.stats_daily(days))
    }
}
