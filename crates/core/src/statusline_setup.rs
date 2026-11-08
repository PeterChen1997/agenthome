//! Claude statusline 一键安装 / 卸载（设置页 9.4，手工步骤见 docs/06-statusline-安装.md）。
//!
//! 做法与手工安装一致：把内嵌的 `script/agenthome-statusline.sh` 写到 `~/AgentHome/bin/`，
//! 再把 `~/.claude/settings.json` 的 `statusLine.command` 改成
//! `AGENTHOME_ORIG_STATUSLINE='<原命令>' <脚本>`（没有原命令时只写脚本路径）。
//! 脚本靠这个环境变量把 statusline JSON 转交给原命令，用户原有显示不变。
//!
//! 安全约束：
//! - 只动 `statusLine` 一个键；其余键、键顺序原样保留（serde_json `preserve_order`）。
//! - 改之前先把原文件逐字节备份成 `settings.json.agenthome-backup-<时间戳>`。
//! - settings.json 解析失败、或 statusLine 形状看不懂时直接报错，不写任何文件。
//! - 写入走「同目录临时文件 + rename」；settings.json 是符号链接时改它指向的文件，链接本身保留。
//! - 幂等：已经是包装脚本的命令不会再包一层；卸载不碰 signals 数据。

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 包装脚本文件名；命令里出现它就认为是 AgentHome 的包装。
pub const SCRIPT_FILE_NAME: &str = "agenthome-statusline.sh";
/// 脚本读取原 statusline 命令的环境变量（见脚本头部注释）。
pub const ORIG_ENV: &str = "AGENTHOME_ORIG_STATUSLINE";
/// 内嵌的包装脚本内容。
pub const SCRIPT_CONTENT: &str = include_str!("../../../script/agenthome-statusline.sh");
const BACKUP_MARK: &str = ".agenthome-backup-";

/// 安装器涉及的两个位置；全部由调用方传入，便于测试用临时目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslinePaths {
    /// Claude Code 用户设置，默认 `~/.claude/settings.json`。
    pub settings: PathBuf,
    /// 包装脚本目录，默认 `~/AgentHome/bin`。
    pub bin_dir: PathBuf,
}

impl StatuslinePaths {
    /// 默认路径的便捷入口：`~/.claude/settings.json` + `~/AgentHome/bin`。
    pub fn defaults() -> Self {
        Self {
            settings: crate::paths::claude_settings_path(),
            bin_dir: crate::paths::agenthome_bin_dir(),
        }
    }

    /// 托管脚本的完整路径：`<bin_dir>/agenthome-statusline.sh`。
    pub fn script_path(&self) -> PathBuf {
        self.bin_dir.join(SCRIPT_FILE_NAME)
    }
}

/// 当前 statusline 的接入状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatuslineStatus {
    /// settings.json 不存在（Claude Code 还没写过设置）。安装会新建它（前提是所在目录存在）。
    SettingsMissing,
    /// settings.json 读不了或不是 JSON 对象。安装 / 卸载都会拒绝，不写任何文件。
    SettingsUnreadable { error: String },
    /// 没有 `statusLine` 键。安装后只显示 AgentHome 的极简状态行。
    NotConfigured,
    /// statusLine 是别的命令（用户自己的，或安装后被其他工具接管）。安装会把它包进来保留显示。
    OtherCommand { command: String },
    /// 已接入，且指向托管脚本 `<bin_dir>/agenthome-statusline.sh`。
    Installed {
        /// 被包装的原命令；`None` 表示原来没有 statusline。
        wrapped_command: Option<String>,
        /// 托管脚本存在且内容等于当前内嵌版本；`false` 时重新安装即可更新。
        script_up_to_date: bool,
    },
    /// 已接入 AgentHome 包装脚本，但不是托管路径（例如按 docs/06 手工指向仓库里的脚本）。
    /// 安装会改指托管路径并保留原命令；卸载同样能还原。
    InstalledElsewhere {
        script_path: String,
        wrapped_command: Option<String>,
    },
    /// statusLine 形状看不懂（不是对象、type 不是 command、command 不是字符串，
    /// 或命令里出现了包装脚本但格式无法解析）。安装拒绝，避免双重包装或丢配置；需要手工处理。
    Unrecognized { reason: String },
}

impl StatuslineStatus {
    /// 是否已接入 AgentHome 包装（托管路径或其他路径）。
    pub fn is_installed(&self) -> bool {
        matches!(
            self,
            Self::Installed { .. } | Self::InstalledElsewhere { .. }
        )
    }
}

/// 安装结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// 托管脚本路径。
    pub script_path: PathBuf,
    /// 本次是否（重新）写了脚本；内容与权限都已一致时为 `false`。
    pub script_written: bool,
    /// settings.json 是否被改动；已安装再装时为 `false`。
    pub settings_changed: bool,
    /// settings.json 原本不存在、本次新建。
    pub settings_created: bool,
    /// 改动前的备份；没改动或原文件不存在时为 `None`。
    pub backup_path: Option<PathBuf>,
    /// 被包装的原命令；`None` 表示原来没有 statusline。
    pub wrapped_command: Option<String>,
    /// 写入后的 `statusLine.command`。
    pub command: String,
}

/// 卸载结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    /// settings.json 是否被改动；本来就没装时为 `false`。
    pub settings_changed: bool,
    /// 改动前的备份。
    pub backup_path: Option<PathBuf>,
    /// 还原成的命令；`None` 且 `settings_changed` 时表示删掉了 `statusLine` 键。
    pub restored_command: Option<String>,
}

/// 只读：判断当前接入状态。不写任何文件。
pub fn status(paths: &StatuslinePaths) -> StatuslineStatus {
    let root = match load(&paths.settings) {
        Ok(Loaded::Missing) => return StatuslineStatus::SettingsMissing,
        Ok(Loaded::Present { root, .. }) => root,
        Err(e) => {
            return StatuslineStatus::SettingsUnreadable {
                error: format!("{e:#}"),
            };
        }
    };
    let script = paths.script_path();
    match classify(&root) {
        Current::NotConfigured => StatuslineStatus::NotConfigured,
        Current::Other(command) => StatuslineStatus::OtherCommand { command },
        Current::Unrecognized(reason) => StatuslineStatus::Unrecognized { reason },
        Current::Wrapper { script: s, orig } if Path::new(&s) == script => {
            StatuslineStatus::Installed {
                wrapped_command: orig,
                script_up_to_date: script_up_to_date(&script),
            }
        }
        Current::Wrapper { script: s, orig } => StatuslineStatus::InstalledElsewhere {
            script_path: s,
            wrapped_command: orig,
        },
    }
}

/// 安装（或更新）包装脚本并接入 statusLine。幂等。
pub fn install(paths: &StatuslinePaths) -> Result<InstallReport> {
    // 先读、先算，确认能成功再动任何文件：坏 JSON / 看不懂的 statusLine 一个字节都不写。
    let (raw, mut root) = match load(&paths.settings)? {
        Loaded::Present { raw, root } => (Some(raw), root),
        Loaded::Missing => {
            let dir = parent_dir(&paths.settings);
            if !dir.is_dir() {
                bail!("{} 不存在，未检测到 Claude Code 配置目录", dir.display());
            }
            (None, Map::new())
        }
    };
    let wrapped = match classify(&root) {
        Current::Unrecognized(reason) => {
            bail!("statusLine 无法识别，未做任何改动：{reason}")
        }
        Current::NotConfigured => None,
        Current::Other(c) => Some(c),
        Current::Wrapper { orig, .. } => orig,
    };
    let script_path = paths.script_path();
    let command = wrapper_command(&script_path, wrapped.as_deref())?;
    let settings_changed = set_command(&mut root, &command);

    // 先落脚本，再改 settings：settings 永远不会指向一个还不存在的脚本。
    let script_written = write_script(&paths.bin_dir)?;
    let mut backup_path = None;
    if settings_changed {
        if let Some(raw) = &raw {
            backup_path = Some(backup(&paths.settings, raw)?);
        }
        write_settings(&paths.settings, &root, raw.as_deref())?;
    }
    Ok(InstallReport {
        script_path,
        script_written,
        settings_changed,
        settings_created: raw.is_none(),
        backup_path,
        wrapped_command: wrapped,
        command,
    })
}

/// 卸载：把 statusLine 还原成被包装的原命令（原来没有就删掉该键）。
/// 不删除托管脚本（其他 settings 可能还引用它）和 signals 数据。
pub fn uninstall(paths: &StatuslinePaths) -> Result<UninstallReport> {
    let unchanged = UninstallReport {
        settings_changed: false,
        backup_path: None,
        restored_command: None,
    };
    let (raw, mut root) = match load(&paths.settings)? {
        Loaded::Missing => return Ok(unchanged),
        Loaded::Present { raw, root } => (raw, root),
    };
    let Current::Wrapper { orig, .. } = classify(&root) else {
        return Ok(unchanged);
    };
    match &orig {
        Some(c) => {
            set_command(&mut root, c);
        }
        None => {
            root.shift_remove("statusLine");
        }
    }
    let backup_path = backup(&paths.settings, &raw)?;
    write_settings(&paths.settings, &root, Some(&raw))?;
    Ok(UninstallReport {
        settings_changed: true,
        backup_path: Some(backup_path),
        restored_command: orig,
    })
}

// ---------------------------------------------------------------- 读取与识别

enum Loaded {
    Missing,
    Present {
        raw: Vec<u8>,
        root: Map<String, Value>,
    },
}

/// 不存在返回 `Missing`；读不了 / 不是 JSON 对象报错。
fn load(path: &Path) -> Result<Loaded> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e).with_context(|| format!("读取 {}", path.display())),
    };
    let value: Value = serde_json::from_slice(&raw)
        .with_context(|| format!("{} 不是合法 JSON", path.display()))?;
    let Value::Object(root) = value else {
        bail!("{} 顶层不是 JSON 对象", path.display());
    };
    Ok(Loaded::Present { raw, root })
}

enum Current {
    NotConfigured,
    Other(String),
    Wrapper {
        script: String,
        orig: Option<String>,
    },
    Unrecognized(String),
}

fn classify(root: &Map<String, Value>) -> Current {
    let Some(sl) = root.get("statusLine") else {
        return Current::NotConfigured;
    };
    let Value::Object(sl) = sl else {
        return Current::Unrecognized("statusLine 不是对象".into());
    };
    if let Some(t) = sl.get("type")
        && t.as_str() != Some("command")
    {
        return Current::Unrecognized(format!("statusLine.type 是 {t}，不是 \"command\""));
    }
    let Some(Value::String(cmd)) = sl.get("command") else {
        return Current::Unrecognized("statusLine.command 缺失或不是字符串".into());
    };
    if let Some(w) = parse_wrapper(cmd) {
        return w;
    }
    if cmd.contains(SCRIPT_FILE_NAME) {
        // 出现了包装脚本但不是我们写的格式（例如 `bash .../agenthome-statusline.sh | x`）：
        // 当成普通命令再包一层会双重包装，所以拒绝，让用户手工处理。
        return Current::Unrecognized(format!("命令里有 {SCRIPT_FILE_NAME} 但格式无法解析：{cmd}"));
    }
    if cmd.trim().is_empty() {
        return Current::Unrecognized("statusLine.command 为空".into());
    }
    Current::Other(cmd.clone())
}

/// 识别 `[NAME=值 ...] <路径>/agenthome-statusline.sh`，取出 `AGENTHOME_ORIG_STATUSLINE`。
fn parse_wrapper(cmd: &str) -> Option<Current> {
    let words = shell_words(cmd)?;
    let (last, assigns) = words.split_last()?;
    if Path::new(last).file_name()? != SCRIPT_FILE_NAME {
        return None;
    }
    let mut orig = None;
    for w in assigns {
        let (k, v) = w.split_once('=')?;
        let valid_name = !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_name {
            return None;
        }
        if k == ORIG_ENV && !v.trim().is_empty() {
            orig = Some(v.to_string());
        }
    }
    Some(Current::Wrapper {
        script: last.clone(),
        orig,
    })
}

/// 极简 POSIX 分词：单引号、双引号（\ 只转义 " \ $ ` 换行）、反斜杠、空白分隔。
/// 不展开变量、不识别 `;` `|` 等元字符（它们会留在词里，从而让识别失败）。引号不配对返回 `None`。
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        x => cur.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let n = chars.next()?;
                            if !matches!(n, '"' | '\\' | '$' | '`' | '\n') {
                                cur.push('\\');
                            }
                            cur.push(n);
                        }
                        x => cur.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 路径只含安全字符时原样输出，否则单引号包起来。
fn quote_path(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '@' | ',')
        });
    if safe { s.to_string() } else { single_quote(s) }
}

fn wrapper_command(script: &Path, orig: Option<&str>) -> Result<String> {
    let script = script
        .to_str()
        .with_context(|| format!("脚本路径不是 UTF-8：{}", script.display()))?;
    let script = quote_path(script);
    Ok(match orig {
        Some(o) => format!("{ORIG_ENV}={} {script}", single_quote(o)),
        None => script,
    })
}

/// 只改 `statusLine.command`（statusLine 已有的 type / padding / refreshInterval 等保留在原位）。
/// 没有 statusLine 时追加 `{"type":"command","command":…}`。返回是否有变化。
fn set_command(root: &mut Map<String, Value>, command: &str) -> bool {
    if let Some(Value::Object(sl)) = root.get_mut("statusLine") {
        if sl.get("command").and_then(Value::as_str) == Some(command) {
            return false;
        }
        sl.insert("command".into(), Value::String(command.into()));
        return true;
    }
    let mut sl = Map::new();
    sl.insert("type".into(), Value::String("command".into()));
    sl.insert("command".into(), Value::String(command.into()));
    root.insert("statusLine".into(), Value::Object(sl));
    true
}

fn script_up_to_date(script: &Path) -> bool {
    fs::read(script).is_ok_and(|b| b == SCRIPT_CONTENT.as_bytes()) && is_mode_755(script)
}

#[cfg(unix)]
fn is_mode_755(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755)
}

#[cfg(not(unix))]
fn is_mode_755(_path: &Path) -> bool {
    true
}

// ---------------------------------------------------------------- 写入

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// 写托管脚本（原子替换，755）。内容与权限都一致时不写，返回 `false`。
fn write_script(bin_dir: &Path) -> Result<bool> {
    let path = bin_dir.join(SCRIPT_FILE_NAME);
    if script_up_to_date(&path) {
        return Ok(false);
    }
    fs::create_dir_all(bin_dir).with_context(|| format!("创建目录 {}", bin_dir.display()))?;
    atomic_write(&path, SCRIPT_CONTENT.as_bytes(), Some(0o755))?;
    Ok(true)
}

/// 把改动前的原始字节写到同目录 `settings.json.agenthome-backup-<时间戳>`，权限与原文件一致。
fn backup(settings: &Path, raw: &[u8]) -> Result<PathBuf> {
    let name = settings
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let dir = parent_dir(settings);
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let path = dir.join(format!("{name}{BACKUP_MARK}{stamp}{suffix}"));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = match opts.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("创建备份 {}", path.display())),
        };
        f.write_all(raw)
            .and_then(|_| f.sync_all())
            .with_context(|| format!("写备份 {}", path.display()))?;
        if let Ok(meta) = fs::metadata(settings) {
            let _ = fs::set_permissions(&path, meta.permissions());
        }
        return Ok(path);
    }
    bail!("备份文件名冲突过多：{}", dir.display())
}

/// 原子写 settings.json：保留原文件末尾换行习惯与权限；符号链接时写到它指向的文件。
fn write_settings(settings: &Path, root: &Map<String, Value>, raw: Option<&[u8]>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(root).context("序列化 settings.json")?;
    if raw.is_none_or(|r| r.ends_with(b"\n")) {
        text.push('\n');
    }
    let target = if raw.is_some() {
        fs::canonicalize(settings).with_context(|| format!("解析 {}", settings.display()))?
    } else {
        settings.to_path_buf()
    };
    atomic_write(&target, text.as_bytes(), None)
}

/// 同目录临时文件 + fsync + rename。`mode` 为 `None` 时沿用目标文件现有权限。
fn atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let dir = parent_dir(path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{name}.agenthome-tmp-{}-{nanos}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("创建临时文件 {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        set_mode(&tmp, path, mode)?;
        fs::rename(&tmp, path).with_context(|| format!("替换 {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn set_mode(tmp: &Path, target: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = match (mode, fs::metadata(target)) {
        (Some(m), _) => fs::Permissions::from_mode(m),
        (None, Ok(meta)) => meta.permissions(),
        (None, Err(_)) => return Ok(()),
    };
    fs::set_permissions(tmp, perms).with_context(|| format!("设置权限 {}", tmp.display()))
}

#[cfg(not(unix))]
fn set_mode(_tmp: &Path, _target: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup(settings: Option<&str>) -> (TempDir, StatuslinePaths) {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let paths = StatuslinePaths {
            settings: claude.join("settings.json"),
            bin_dir: dir.path().join("AgentHome").join("bin"),
        };
        if let Some(s) = settings {
            fs::write(&paths.settings, s).unwrap();
        }
        (dir, paths)
    }

    fn read_root(p: &StatuslinePaths) -> Map<String, Value> {
        match serde_json::from_slice(&fs::read(&p.settings).unwrap()).unwrap() {
            Value::Object(m) => m,
            other => panic!("不是对象: {other}"),
        }
    }

    fn command_of(p: &StatuslinePaths) -> Option<String> {
        read_root(p)
            .get("statusLine")
            .and_then(|s| s.get("command"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn backups(p: &StatuslinePaths) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(p.settings.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().contains(BACKUP_MARK))
            .collect();
        v.sort();
        v
    }

    // 用户的 settings.json 里有权限、hooks、环境变量等大量配置，顺序也是用户自己排的；
    // 安装器只能碰 statusLine，任何其他键丢失或重排都会破坏用户环境或制造无意义 diff。
    const RICH: &str = r#"{
  "zeta": 1,
  "permissions": {"allow": ["Bash(ls:*)"], "deny": []},
  "statusLine": {"type": "command", "command": "~/.claude/statusline.sh", "padding": 0},
  "alpha": {"nested": [1, 2, {"b": true, "a": null}]},
  "env": {"FOO": "bar"}
}
"#;

    #[test]
    fn 安装后其他键与顺序原样保留() {
        let (_d, p) = setup(Some(RICH));
        let before = read_root(&p);
        install(&p).unwrap();
        let after = read_root(&p);
        let keys = |m: &Map<String, Value>| m.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&before), keys(&after), "顶层键顺序必须不变");
        for (k, v) in &before {
            if k != "statusLine" {
                assert_eq!(after.get(k), Some(v), "{k} 必须原样保留");
            }
        }
        // statusLine 里除 command 外的字段（padding）也保留。
        assert_eq!(after["statusLine"]["padding"], 0);
        assert_eq!(after["statusLine"]["type"], "command");
    }

    #[test]
    fn 卸载还原原有命令且整份设置回到安装前() {
        let (_d, p) = setup(Some(RICH));
        let before = read_root(&p);
        let rep = install(&p).unwrap();
        assert_eq!(
            rep.wrapped_command.as_deref(),
            Some("~/.claude/statusline.sh")
        );
        assert!(
            matches!(status(&p), StatuslineStatus::Installed { wrapped_command: Some(ref c), script_up_to_date: true } if c == "~/.claude/statusline.sh")
        );
        let un = uninstall(&p).unwrap();
        assert!(un.settings_changed);
        assert_eq!(
            un.restored_command.as_deref(),
            Some("~/.claude/statusline.sh")
        );
        let after = read_root(&p);
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        assert!(matches!(status(&p), StatuslineStatus::OtherCommand { .. }));
    }

    #[test]
    fn 原来没有statusline时卸载删掉该键() {
        let (_d, p) = setup(Some("{\n  \"model\": \"opus\"\n}\n"));
        assert_eq!(status(&p), StatuslineStatus::NotConfigured);
        let rep = install(&p).unwrap();
        assert_eq!(rep.wrapped_command, None);
        assert_eq!(command_of(&p).unwrap(), p.script_path().to_str().unwrap());
        uninstall(&p).unwrap();
        let after = read_root(&p);
        assert!(!after.contains_key("statusLine"));
        assert_eq!(after["model"], "opus");
        assert_eq!(status(&p), StatuslineStatus::NotConfigured);
    }

    #[test]
    fn 重复安装不双重包装也不再备份() {
        let (_d, p) = setup(Some(RICH));
        let first = install(&p).unwrap();
        let cmd1 = command_of(&p).unwrap();
        let second = install(&p).unwrap();
        assert_eq!(command_of(&p).unwrap(), cmd1);
        assert_eq!(cmd1.matches(SCRIPT_FILE_NAME).count(), 1);
        assert_eq!(second.wrapped_command, first.wrapped_command);
        assert!(!second.settings_changed && !second.script_written);
        assert_eq!(second.backup_path, None);
        assert_eq!(backups(&p).len(), 1, "无改动时不应再产生备份");
    }

    #[test]
    fn 坏json不写任何文件() {
        let bad = "{ \"statusLine\": { \"command\": \"x\" }, ";
        let (_d, p) = setup(Some(bad));
        assert!(matches!(
            status(&p),
            StatuslineStatus::SettingsUnreadable { .. }
        ));
        assert!(install(&p).is_err());
        assert!(uninstall(&p).is_err());
        assert_eq!(fs::read_to_string(&p.settings).unwrap(), bad);
        assert!(!p.bin_dir.exists(), "坏 JSON 时连脚本也不能写");
        assert!(backups(&p).is_empty());
        let entries = fs::read_dir(p.settings.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1, "不能留下临时文件");
    }

    #[test]
    fn 备份存在且逐字节等于安装前() {
        let (_d, p) = setup(Some(RICH));
        let rep = install(&p).unwrap();
        let b = rep.backup_path.expect("应有备份");
        assert_eq!(b.parent(), p.settings.parent());
        assert!(
            b.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("settings.json.agenthome-backup-")
        );
        assert_eq!(fs::read_to_string(&b).unwrap(), RICH);
        // 卸载的备份等于安装后的内容。
        let installed = fs::read(&p.settings).unwrap();
        let un = uninstall(&p).unwrap();
        assert_eq!(fs::read(un.backup_path.unwrap()).unwrap(), installed);
        assert_eq!(backups(&p).len(), 2);
    }

    #[test]
    fn 脚本内容与权限() {
        let (_d, p) = setup(Some("{}"));
        install(&p).unwrap();
        assert_eq!(fs::read_to_string(p.script_path()).unwrap(), SCRIPT_CONTENT);
        assert!(is_mode_755(&p.script_path()));
        // 脚本被改坏后 status 能看出来，重装会修复。
        fs::write(p.script_path(), "echo broken").unwrap();
        assert!(matches!(
            status(&p),
            StatuslineStatus::Installed {
                script_up_to_date: false,
                ..
            }
        ));
        let rep = install(&p).unwrap();
        assert!(rep.script_written && !rep.settings_changed);
        assert_eq!(fs::read_to_string(p.script_path()).unwrap(), SCRIPT_CONTENT);
    }

    #[test]
    fn 手工安装的其他路径升级为托管路径且不双重包装() {
        // docs/06 的手工安装形态：指向仓库里的脚本。
        let manual = r#"{"statusLine":{"type":"command","command":"AGENTHOME_ORIG_STATUSLINE='ccusage statusline' /repo/script/agenthome-statusline.sh"}}"#;
        let (_d, p) = setup(Some(manual));
        assert_eq!(
            status(&p),
            StatuslineStatus::InstalledElsewhere {
                script_path: "/repo/script/agenthome-statusline.sh".into(),
                wrapped_command: Some("ccusage statusline".into()),
            }
        );
        let rep = install(&p).unwrap();
        assert_eq!(rep.wrapped_command.as_deref(), Some("ccusage statusline"));
        let cmd = command_of(&p).unwrap();
        assert_eq!(cmd.matches(SCRIPT_FILE_NAME).count(), 1);
        assert!(cmd.ends_with(p.script_path().to_str().unwrap()));
        uninstall(&p).unwrap();
        assert_eq!(command_of(&p).as_deref(), Some("ccusage statusline"));
    }

    #[test]
    fn 看不懂的形状拒绝安装且不写文件() {
        for s in [
            r#"{"statusLine":"oops"}"#,
            r#"{"statusLine":{"type":"static","command":"x"}}"#,
            r#"{"statusLine":{"type":"command"}}"#,
            r#"{"statusLine":{"type":"command","command":"bash ~/x/agenthome-statusline.sh | tee"}}"#,
            "[1,2]",
        ] {
            let (_d, p) = setup(Some(s));
            let st = status(&p);
            assert!(
                matches!(
                    st,
                    StatuslineStatus::Unrecognized { .. }
                        | StatuslineStatus::SettingsUnreadable { .. }
                ),
                "{s} => {st:?}"
            );
            assert!(install(&p).is_err(), "{s}");
            assert_eq!(fs::read_to_string(&p.settings).unwrap(), s);
            assert!(!p.bin_dir.exists());
        }
    }

    #[test]
    fn settings不存在时新建且卸载为空操作() {
        let (_d, p) = setup(None);
        assert_eq!(status(&p), StatuslineStatus::SettingsMissing);
        assert!(!uninstall(&p).unwrap().settings_changed);
        let rep = install(&p).unwrap();
        assert!(rep.settings_created && rep.backup_path.is_none());
        assert!(status(&p).is_installed());
        // .claude 目录都不存在时不擅自创建。
        let d = tempfile::tempdir().unwrap();
        let p2 = StatuslinePaths {
            settings: d.path().join("nope").join("settings.json"),
            bin_dir: d.path().join("bin"),
        };
        assert!(install(&p2).is_err());
        assert!(!p2.bin_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn 符号链接的settings保持为链接() {
        // dotfiles 管理器常把 settings.json 软链到仓库；rename 覆盖链接会把它变成普通文件。
        let (d, p) = setup(None);
        let real = d.path().join("dotfiles-settings.json");
        fs::write(&real, RICH).unwrap();
        std::os::unix::fs::symlink(&real, &p.settings).unwrap();
        install(&p).unwrap();
        assert!(
            fs::symlink_metadata(&p.settings)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&real)
                .unwrap()
                .contains(SCRIPT_FILE_NAME)
        );
    }

    #[cfg(unix)]
    #[test]
    fn 安装后的命令经真实shell执行能落盘并转交原命令() {
        // 原命令带单引号、双引号、$()：引号转义错一个字符，用户的 statusline 就会坏掉。
        let orig = r#"printf 'orig[%s]' "$(cat)""#;
        let settings = serde_json::json!({"statusLine": {"type": "command", "command": orig}});
        let (d, p) = setup(Some(&settings.to_string()));
        install(&p).unwrap();
        let cmd = command_of(&p).unwrap();
        let signals = d.path().join("signals");
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("AGENTHOME_SIGNALS_DIR", &signals)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"a":1}"#)
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), r#"orig[{"a":1}]"#);
        assert_eq!(
            fs::read_to_string(signals.join("claude-statusline.json")).unwrap(),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn 包装命令分词往返() {
        let script = Path::new("/Users/a b/AgentHome/bin/agenthome-statusline.sh");
        for orig in ["ccusage statusline", "it's \"q\" $HOME `x` \\n", "a\nb"] {
            let cmd = wrapper_command(script, Some(orig)).unwrap();
            match parse_wrapper(&cmd) {
                Some(Current::Wrapper { script: s, orig: o }) => {
                    assert_eq!(Path::new(&s), script);
                    assert_eq!(o.as_deref(), Some(orig));
                }
                _ => panic!("无法解析 {cmd}"),
            }
        }
    }
}
