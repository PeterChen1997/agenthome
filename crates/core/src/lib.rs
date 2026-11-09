//! AgentHome 数据层。零 UI 依赖，可单独 `cargo test`。
//!
//! 分层（见 docs/01-产品与架构方案.md §3）：
//! - `model`：规范化会话模型，所有 Provider 的产出都是它。
//! - `store`：SQLite 索引层（可随时从原件重建），含仪表盘查询。
//! - `providers`：各 agent 的发现与解析（`SourceProvider`）。
//! - `ingest`：扫描 + 解析 + 入库的调度。
//! - `pricing`：内嵌定价表与成本估算。
//! - `signals`：额度信号（Claude statusline 文件、Codex rollout 快照）。
//! - `paths`：默认目录。
//! - `statusline_setup`：Claude statusline 包装脚本的一键安装 / 卸载（改 ~/.claude/settings.json）。
//! - `raw`：raw 层原件归档（逐字节保存、只增不改、zstd 压缩）。
//! - `watch`：来源目录的文件监听（notify / FSEvents）。

pub mod ingest;
pub mod model;
pub mod paths;
pub mod pricing;
pub mod providers;
pub mod raw;
pub mod signals;
pub mod statusline_setup;
pub mod store;
pub mod watch;

pub use ingest::{
    IngestCache, Progress, Report, ingest_all, ingest_all_cached, ingest_claude, ingest_paths,
};
pub use providers::claude_code::ClaudeCodeProvider;
pub use providers::codex::CodexProvider;
pub use providers::gemini::GeminiProvider;
pub use providers::{Discovery, Parsed, SourceProvider};
pub use raw::{RawArchive, RawOutcome, RawReport, RawStats};
pub use signals::{claude_statusline, codex_rollout_quota};
pub use statusline_setup::{InstallReport, StatuslinePaths, StatuslineStatus, UninstallReport};
pub use store::{ResumePoint, SourceRecord, Store, WriteStats};
pub use watch::{RootWatcher, WatchEvent, watch_roots};
