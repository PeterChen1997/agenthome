//! 检查更新与应用内更新：查本仓库 GitHub Release 的最新正式版；用户点「立即更新」后下载、
//! 验证并在应用退出后替换自身。
//!
//! 网络只用系统自带的 `/usr/bin/curl`，不带任何会话或账号数据。安装前逐项验证，任何一项不对都不装：
//! 发布附带的 sha256、签名完整（`codesign --verify --deep --strict`）、bundle id 与开发者团队
//! （[`EXPECTED_BUNDLE_ID`] / [`EXPECTED_TEAM_ID`]）、苹果公证（`spctl`）、版本号与发布一致。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 正式包的 bundle id；开发版（`.dev`）与别人 fork 出来的包都不接受。
pub const EXPECTED_BUNDLE_ID: &str = "cn.peterchen.agenthome";
/// 正式包的签名团队（Developer ID Application: YANG CHEN）。
pub const EXPECTED_TEAM_ID: &str = "R85345VS2J";

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
    /// 安装包 `AgentHome-<版本>-macos-arm64.zip` 的下载地址；发布里没有就不能应用内更新。
    pub zip_url: Option<String>,
    /// 同名 `.sha256` 文件的下载地址。
    pub sha256_url: Option<String>,
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
        zip_url: asset_url(json, |name| name.ends_with("-macos-arm64.zip")),
        sha256_url: asset_url(json, |name| name.ends_with("-macos-arm64.zip.sha256")),
    })
}

fn asset_url(json: &Value, wanted: impl Fn(&str) -> bool) -> Option<String> {
    json.get("assets")?.as_array()?.iter().find_map(|asset| {
        let name = asset.get("name")?.as_str()?;
        wanted(name)
            .then(|| {
                asset
                    .get("browser_download_url")?
                    .as_str()
                    .map(str::to_string)
            })
            .flatten()
    })
}

fn curl_to(url: &str, out: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--max-time", "600", "--output"])
        .arg(out)
        .arg(url)
        .output()
        .context("启动 curl")?;
    if !output.status.success() {
        bail!(
            "下载 {url} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// 下载发布的安装包到 `dir`、核对 sha256、解压并验证，返回解压出的新 `AgentHome.app`。
pub fn download_and_verify(release: &Release, dir: &Path) -> Result<PathBuf> {
    let zip_url = release
        .zip_url
        .as_deref()
        .context("这个发布没有安装包附件")?;
    let sha_url = release
        .sha256_url
        .as_deref()
        .context("这个发布没有 sha256 附件，不安装未经核对的包")?;
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("清理 {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir)?;
    let zip = dir.join("update.zip");
    let sha_file = dir.join("update.zip.sha256");
    curl_to(sha_url, &sha_file)?;
    curl_to(zip_url, &zip)?;
    let expected = std::fs::read_to_string(&sha_file)?
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .context("sha256 文件是空的")?;
    let actual = format!("{:x}", Sha256::digest(std::fs::read(&zip)?));
    if actual != expected {
        bail!("安装包校验和不符（期望 {expected}，实际 {actual}），已丢弃");
    }
    let unpacked = dir.join("unpacked");
    run("/usr/bin/ditto", &["-x", "-k"], &[&zip, &unpacked])?;
    let app = unpacked.join("AgentHome.app");
    if !app.is_dir() {
        bail!("安装包里没有 AgentHome.app");
    }
    verify_app(&app, &release.version)?;
    Ok(app)
}

fn run(program: &str, args: &[&str], paths: &[&Path]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .args(paths)
        .output()
        .with_context(|| format!("启动 {program}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("{program} 失败：{}", text.trim());
    }
    Ok(text)
}

/// 验证一个 `.app`：签名完整、bundle id 与团队对得上、经过苹果公证、版本号是 `version`。
pub fn verify_app(app: &Path, version: &str) -> Result<()> {
    run(
        "/usr/bin/codesign",
        &["--verify", "--deep", "--strict"],
        &[app],
    )
    .context("签名校验没通过")?;
    let info = run("/usr/bin/codesign", &["-dv"], &[app])?;
    check_signature_info(&info)?;
    let assessment = run("/usr/sbin/spctl", &["-a", "-vv", "-t", "exec"], &[app])
        .context("没有通过 Gatekeeper（未公证？）")?;
    if !assessment.contains("Notarized Developer ID") {
        bail!("安装包没有经过苹果公证：{}", assessment.trim());
    }
    let plist = app.join("Contents/Info.plist");
    let found = run(
        "/usr/bin/plutil",
        &["-extract", "CFBundleShortVersionString", "raw", "-o", "-"],
        &[&plist],
    )?;
    if found.trim() != version {
        bail!("安装包版本是 {}，与发布的 {version} 不符", found.trim());
    }
    Ok(())
}

/// `codesign -dv` 的输出里 bundle id 与团队必须是正式包的。
fn check_signature_info(info: &str) -> Result<()> {
    let field = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key).map(str::trim).map(str::to_string))
    };
    let id = field("Identifier=").unwrap_or_default();
    let team = field("TeamIdentifier=").unwrap_or_default();
    if id != EXPECTED_BUNDLE_ID || team != EXPECTED_TEAM_ID {
        bail!(
            "安装包的签名身份不对（{id} / {team}，应为 {EXPECTED_BUNDLE_ID} / {EXPECTED_TEAM_ID}）"
        );
    }
    Ok(())
}

/// 当前进程所在的 `.app`（`…/AgentHome.app/Contents/MacOS/agenthome-app` 往上三级）；
/// 不是从 `.app` 运行时为 None。
pub fn running_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?.to_path_buf();
    bundle
        .extension()
        .is_some_and(|x| x == "app")
        .then_some(bundle)
}

/// `target` 能不能被替换：是正式包，且所在目录可写。
pub fn can_replace(target: &Path) -> bool {
    let id = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(target.join("Contents/Info.plist"))
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let writable = target.parent().is_some_and(|dir| {
        let probe = dir.join(format!(".agenthome-write-test-{}", std::process::id()));
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    });
    id.as_deref() == Some(EXPECTED_BUNDLE_ID) && writable
}

/// 交给一个独立脚本在本进程退出后完成替换：等 `pid` 退出 → 旧版挪到 `backup` → 新版放到 `target`
/// → 打开新版。替换失败就把旧版放回去再打开，不留下一个打不开的空位置。调用方随后应退出应用。
pub fn spawn_replace_and_relaunch(
    new_app: &Path,
    target: &Path,
    backup: &Path,
    relaunch: bool,
) -> Result<()> {
    let script = new_app
        .parent()
        .and_then(Path::parent)
        .context("找不到临时目录")?
        .join("install.sh");
    std::fs::write(&script, INSTALL_SCRIPT)?;
    let log = script.with_extension("log");
    Command::new("/bin/bash")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(new_app)
        .arg(target)
        .arg(backup)
        .arg(if relaunch { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(log.with_extension("err"))?)
        .spawn()
        .context("启动安装脚本")?;
    Ok(())
}

const INSTALL_SCRIPT: &str = r#"#!/bin/bash
# AgentHome 应用内更新：等旧进程退出后替换 .app 并重新打开。参数：pid 新包 目标 备份 是否重开
pid="$1"; new="$2"; target="$3"; backup="$4"; relaunch="$5"
for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
mkdir -p "$(dirname "$backup")"
rm -rf "$backup"
if ! mv "$target" "$backup"; then echo "挪走旧版失败"; [ "$relaunch" = 1 ] && open "$target"; exit 1; fi
if ! /usr/bin/ditto "$new" "$target"; then
  echo "放入新版失败，恢复旧版"; rm -rf "$target"; mv "$backup" "$target"
fi
[ "$relaunch" = 1 ] && open "$target"
echo "完成"
"#;

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

    /// 联网：下载真实的最新发布并走完整验证（校验和、签名、团队、公证、版本）。
    /// `cargo test -p agenthome-core --lib real_latest_release -- --ignored`
    #[test]
    #[ignore]
    fn real_latest_release_downloads_and_verifies() {
        let release = fetch_latest_release("test").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let app = download_and_verify(&release, &dir.path().join("update")).unwrap();
        assert!(app.ends_with("AgentHome.app"));
    }

    #[test]
    fn 只接受正式包的签名身份() {
        let ok = "Executable=/x\nIdentifier=cn.peterchen.agenthome\nFormat=app bundle\nTeamIdentifier=R85345VS2J\n";
        assert!(check_signature_info(ok).is_ok());
        // 开发版、别的团队签的同名包都不装。
        assert!(check_signature_info(&ok.replace("agenthome\n", "agenthome.dev\n")).is_err());
        assert!(check_signature_info(&ok.replace("R85345VS2J", "ABCDE12345")).is_err());
        assert!(
            check_signature_info("Identifier=cn.peterchen.agenthome\nTeamIdentifier=not set")
                .is_err()
        );
    }

    #[test]
    fn 解析发布信息() {
        let r = parse_release(&json!({
            "tag_name": "v0.2.3",
            "html_url": "https://github.com/PeterChen1997/agenthome/releases/tag/v0.2.3",
            "published_at": "2026-11-11T02:00:00Z",
            "draft": false,
            "assets": [
                {"name": "AgentHome-0.2.3-macos-arm64.zip.sha256", "browser_download_url": "https://x/a.zip.sha256"},
                {"name": "AgentHome-0.2.3-macos-arm64.zip", "browser_download_url": "https://x/a.zip"}
            ]
        }))
        .unwrap();
        assert_eq!(r.version, "0.2.3");
        assert_eq!(r.zip_url.as_deref(), Some("https://x/a.zip"));
        assert_eq!(r.sha256_url.as_deref(), Some("https://x/a.zip.sha256"));
        assert!(r.url.ends_with("/v0.2.3"));
        assert!(parse_release(&json!({"tag_name": "latest-build"})).is_err());
        assert!(parse_release(&json!({"message": "Not Found"})).is_err());
    }
}
