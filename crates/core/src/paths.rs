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

/// `~/AgentHome/raw`：raw 层原件归档。
pub fn raw_dir() -> PathBuf {
    default_data_dir().join("raw")
}

/// 与索引库配套的 raw 层目录：`<X>/index/agenthome.db` 对应 `<X>/raw`，
/// 其他位置的库（测试、`--db`）对应同目录下的 `raw`。
pub fn raw_dir_for_db(db: &std::path::Path) -> PathBuf {
    let parent = db.parent().unwrap_or(std::path::Path::new("."));
    if parent.file_name().is_some_and(|n| n == "index")
        && let Some(data_dir) = parent.parent()
    {
        return data_dir.join("raw");
    }
    parent.join("raw")
}

/// `~/.claude/projects`
pub fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

/// `~/.codex/sessions`
pub fn codex_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

/// `~/AgentHome/signals`：statusline 包装脚本等外部信号落盘目录。
pub fn signals_dir() -> PathBuf {
    default_data_dir().join("signals")
}

/// `~/AgentHome/signals/claude-statusline.json`
pub fn claude_statusline_path() -> PathBuf {
    signals_dir().join("claude-statusline.json")
}

/// `~/.claude/settings.json`：Claude Code 用户级设置（statusLine 在这里配置）。
pub fn claude_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

/// `~/AgentHome/bin`：AgentHome 托管的可执行脚本（statusline 包装脚本）。
pub fn agenthome_bin_dir() -> PathBuf {
    default_data_dir().join("bin")
}
