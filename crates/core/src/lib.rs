//! AgentHome 数据层。零 UI 依赖，可单独 `cargo test`。
//!
//! 分层（见 docs/01-产品与架构方案.md §3）：
//! - `model`：规范化会话模型，所有 Provider 的产出都是它。
//! - `store`：SQLite 索引层（可随时从原件重建）。
//! - `providers`：各 agent 的发现与解析。
//! - `ingest`：扫描 + 解析 + 入库的调度。
//! - `paths`：默认目录。

pub mod ingest;
pub mod model;
pub mod paths;
pub mod providers;
pub mod store;

pub use ingest::{Progress, Report, ingest_claude};
pub use providers::claude_code::{ClaudeCodeProvider, Discovery, Parsed};
pub use store::Store;
