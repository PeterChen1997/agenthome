//! 检查更新：查本仓库 GitHub Release 的最新正式版，与当前版本比较。
//!
//! 只发一个读取请求（`GET /repos/PeterChen1997/agenthome/releases/latest`），不带任何会话或账号数据；
//! 用系统自带的 `/usr/bin/curl`，不为一个请求引入 HTTP 依赖。只提醒、不下载、不安装。

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// 最新正式版的发布 API（`latest` 自动排除草稿与预发布）。
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/PeterChen1997/agenthome/releases/latest";
/// 所有发布的页面（API 不可用时让用户自己去看）。
pub const RELEASES_PAGE: &str = "https://github.com/PeterChen1997/agenthome/releases";

/// 一个已发布的版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// 不带 `v` 前缀的版本号，如 `0.2.3`。
    pub version: String,
    /// 发布页地址。
    pub url: String,
    /// 发布时间（RFC 3339），可能缺。
    pub published_at: Option<String>,
}

/// 查 GitHub 上的最新正式版。`current` 只用于 User-Agent（GitHub API 要求带 UA）。
pub fn fetch_latest_release(current: &str) -> Result<Release> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-time",
            "15",
            "--header",
            "Accept: application/vnd.github+json",
            "--user-agent",
            &format!("AgentHome/{current}"),
            LATEST_RELEASE_API,
        ])
        .output()
        .context("启动 curl")?;
    if !output.status.success() {
        bail!(
            "查询最新版本失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: Value = serde_json::from_slice(&output.stdout).context("GitHub 返回的不是 JSON")?;
    parse_release(&json)
}

/// GitHub release JSON → [`Release`]。
pub fn parse_release(json: &Value) -> Result<Release> {
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .context("发布信息里没有 tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    if parse_version(&version).is_none() {
        bail!("看不懂的版本号 {tag:?}");
    }
    Ok(Release {
        version,
        url: json
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or(RELEASES_PAGE)
            .to_string(),
        published_at: json
            .get("published_at")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// `x.y.z`（可带 `v` 前缀与 `-local` 之类后缀，后缀忽略）→ 数字三元组。
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    );
    parts.next().is_none().then_some(version)
}

/// `latest` 是否比 `current` 新。后缀不参与比较：本地构建的 `0.2.2-local` 与正式的 `0.2.2` 视为同一版，
/// 不该提示「有新版本 0.2.2」。看不懂的版本号一律当作不新，宁可不提醒也不乱提醒。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 只有真正更高的版本才提醒() {
        assert!(is_newer("0.2.3", "0.2.2"));
        assert!(is_newer("v0.3.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.10.0"), "按数字比，不按字符串比");
        assert!(!is_newer("0.2.2", "0.2.2"));
        assert!(!is_newer("0.2.1", "0.2.2"));
        // 本地构建带后缀，与同号正式版是同一版。
        assert!(!is_newer("0.2.2", "0.2.2-local"));
        assert!(is_newer("0.2.3", "0.2.2-local"));
        // 看不懂的版本号不提醒。
        assert!(!is_newer("nightly", "0.2.2"));
        assert!(!is_newer("0.2.3", ""));
    }

    #[test]
    fn 解析发布信息() {
        let r = parse_release(&json!({
            "tag_name": "v0.2.3",
            "html_url": "https://github.com/PeterChen1997/agenthome/releases/tag/v0.2.3",
            "published_at": "2026-11-11T02:00:00Z",
            "draft": false
        }))
        .unwrap();
        assert_eq!(r.version, "0.2.3");
        assert!(r.url.ends_with("/v0.2.3"));
        assert!(parse_release(&json!({"tag_name": "latest-build"})).is_err());
        assert!(parse_release(&json!({"message": "Not Found"})).is_err());
    }
}
