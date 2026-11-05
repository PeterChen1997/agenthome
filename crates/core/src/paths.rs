//! 默认目录。已拍板：归档目录 `~/AgentHome`，index 层在其下，不同步。

use std::path::PathBuf;

/// `~/AgentHome`
pub fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AgentHome")
}

/// `~/AgentHome/index/agenthome.db`
pub fn default_index_db() -> PathBuf {
    default_data_dir().join("index").join("agenthome.db")
}

/// `~/.claude/projects`
pub fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}
