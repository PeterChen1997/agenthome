//! 本机设备身份（远程用量同步 v1 的基础，CARM-284 方案单元 1）。
//!
//! `~/AgentHome/device.json` 第一次创建后固定，内容：
//! - `machine_id`：随机生成的安装 UUID。主机名相同的两台机器也不同；改主机名、换 IP 不变。
//!   远程用量按 `(machine_id, 来源, 事件键)` 去重，靠它区分机器。
//! - `session_label`：本机会话 id `{provider}:{label}:{native_id}` 中间那段。创建时取当时的主机名
//!   （与旧版各来源每次现读主机名的结果相同），之后不再随主机名变，已有会话 id 保持不变。
//! - `aliases`：本地索引里已经出现过的其他标签（例如改过主机名的旧会话），都映射到本机。
//! - 事件键密钥：只在本机，用来把来源的请求标识变成不透明的事件键（`usage_v1::event_key`）。
//!
//! 迁机时连同本文件一起恢复；克隆出来的机器要删掉它重新生成。文件含密钥，权限 0600。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceFile {
    schema: u32,
    machine_id: String,
    session_label: String,
    #[serde(default)]
    aliases: Vec<String>,
    /// 32 字节，hex。
    event_key_secret: String,
}

/// 本机身份。
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    machine_id: String,
    session_label: String,
    aliases: Vec<String>,
    secret: [u8; 32],
}

impl std::fmt::Debug for DeviceIdentity {
    // 密钥不进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("machine_id", &self.machine_id)
            .field("session_label", &self.session_label)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn session_label(&self) -> &str {
        &self.session_label
    }

    /// 本地索引里的这个 `machine_id` 是不是本机的（当前标签或旧标签）。
    pub fn is_local_label(&self, label: &str) -> bool {
        self.session_label == label || self.aliases.iter().any(|a| a == label)
    }

    pub(crate) fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// 读身份文件；不存在返回 `None`，内容不对报错（不覆盖）。
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(format!("读取 {}", path.display())),
        };
        let file: DeviceFile = serde_json::from_str(&text)
            .with_context(|| format!("{} 不是合法的设备身份文件", path.display()))?;
        Self::from_file(file)
            .map(Some)
            .with_context(|| format!("{} 内容无效", path.display()))
    }

    /// 读身份文件，不存在就生成：随机 UUID + 随机密钥，会话标签取 `hostname`。
    /// 两个进程同时创建时只有一个写成，另一个读它写好的。
    pub fn load_or_create(path: &Path, hostname: &str) -> Result<Self> {
        if let Some(identity) = Self::load(path)? {
            return Ok(identity);
        }
        let mut seed = [0u8; 48];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("取随机数失败：{e}"))?;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&seed[..16]);
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&seed[16..]);
        let identity = Self {
            machine_id: format_uuid_v4(uuid),
            session_label: hostname.to_string(),
            aliases: Vec::new(),
            secret,
        };
        if create_new(path, &identity.to_json()?)? {
            Ok(identity)
        } else {
            Self::load(path)?.with_context(|| format!("{} 创建后又不见了", path.display()))
        }
    }

    /// 把本地索引里出现过的标签记为本机旧标签（已记过的跳过）。返回是否改了文件。
    pub fn adopt_labels(&mut self, path: &Path, labels: &[String]) -> Result<bool> {
        let mut changed = false;
        for label in labels {
            if !label.is_empty() && !self.is_local_label(label) {
                self.aliases.push(label.clone());
                changed = true;
            }
        }
        if changed {
            crate::statusline_setup::atomic_write(path, self.to_json()?.as_bytes(), Some(0o600))?;
        }
        Ok(changed)
    }

    fn from_file(file: DeviceFile) -> Result<Self> {
        if file.schema != SCHEMA {
            bail!("不认识的 schema {}", file.schema);
        }
        if !is_uuid(&file.machine_id) {
            bail!("machine_id 不是 UUID");
        }
        if file.session_label.is_empty() {
            bail!("session_label 是空的");
        }
        let secret = decode_hex32(&file.event_key_secret).context("event_key_secret")?;
        Ok(Self {
            machine_id: file.machine_id,
            session_label: file.session_label,
            aliases: file.aliases,
            secret,
        })
    }

    fn to_json(&self) -> Result<String> {
        let file = DeviceFile {
            schema: SCHEMA,
            machine_id: self.machine_id.clone(),
            session_label: self.session_label.clone(),
            aliases: self.aliases.clone(),
            event_key_secret: hex(&self.secret),
        };
        Ok(serde_json::to_string_pretty(&file)? + "\n")
    }
}

/// 当前主机名。只在第一次创建身份时用；之后会话标签以身份文件为准。
pub fn current_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

static LOCAL_LABEL: OnceLock<String> = OnceLock::new();

/// 本进程各来源用的会话标签：身份文件里的 `session_label`；还没有身份文件（测试、只读命令）
/// 用当前主机名，和身份文件将来创建时取的值一致。
pub(crate) fn local_session_label() -> String {
    LOCAL_LABEL
        .get_or_init(
            || match DeviceIdentity::load(&crate::paths::device_identity_path()) {
                Ok(Some(identity)) => identity.session_label,
                Ok(None) => current_hostname(),
                Err(error) => {
                    // 身份文件坏了不能停掉会话索引：沿用主机名（旧版行为），并把原因打出来。
                    eprintln!("AgentHome：设备身份文件无效，会话标签改用主机名：{error:#}");
                    current_hostname()
                }
            },
        )
        .clone()
}

/// app / CLI 入库前调用：确保本机身份存在，并把本地索引里已有的标签记为本机。
/// 要在实例化各来源之前调用，本进程的会话标签以它为准。
pub fn ensure_local(store: &crate::Store) -> Result<DeviceIdentity> {
    let identity = ensure_at(
        &crate::paths::device_identity_path(),
        &current_hostname(),
        store,
    )?;
    let _ = LOCAL_LABEL.set(identity.session_label.clone());
    Ok(identity)
}

fn ensure_at(path: &Path, hostname: &str, store: &crate::Store) -> Result<DeviceIdentity> {
    let mut identity = DeviceIdentity::load_or_create(path, hostname)?;
    identity.adopt_labels(path, &store.machine_labels()?)?;
    Ok(identity)
}

/// 只在 `path` 不存在时写入（临时文件 + 硬链接，原子地「不存在才创建」）。已存在返回 false。
fn create_new(path: &Path, text: &str) -> Result<bool> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("创建 {}", dir.display()))?;
    let tmp = dir.join(format!(".device.json.tmp-{}", std::process::id()));
    let result = (|| -> Result<bool> {
        let mut f = fs::File::create(&tmp).with_context(|| format!("创建 {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        match fs::hard_link(&tmp, path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error).context(format!("写入 {}", path.display())),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

fn format_uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// 小写、带连字符的 8-4-4-4-12 UUID。
pub(crate) fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("应为 64 位 hex");
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("AgentHome").join("device.json")
    }

    /// 两台主机名相同的机器（各自的 AgentHome）必须得到不同的 machine_id 和密钥，
    /// 否则 Mac 汇总时会把两台机器的用量当成同一台去重、少算。
    #[test]
    fn same_hostname_gets_distinct_machine_ids() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ia = DeviceIdentity::load_or_create(&path(&a), "build-box").unwrap();
        let ib = DeviceIdentity::load_or_create(&path(&b), "build-box").unwrap();
        assert_ne!(ia.machine_id(), ib.machine_id());
        assert_ne!(ia.secret(), ib.secret());
        assert!(is_uuid(ia.machine_id()) && is_uuid(ib.machine_id()));
        assert_eq!(&ia.machine_id()[14..15], "4", "UUID v4");
        // 会话标签仍是主机名：各自本地的会话 id 与旧版一致。
        assert_eq!(ia.session_label(), "build-box");
        assert_eq!(ib.session_label(), "build-box");
    }

    /// 改主机名后重新打开：machine_id、会话标签、密钥都不变（远端不会被当成新机器重复计费）。
    #[test]
    fn rename_host_keeps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = DeviceIdentity::load_or_create(&path(&dir), "old-name").unwrap();
        let again = DeviceIdentity::load_or_create(&path(&dir), "new-name").unwrap();
        assert_eq!(first, again);
        assert_eq!(again.session_label(), "old-name");
    }

    #[test]
    fn create_does_not_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let first = DeviceIdentity::load_or_create(&p, "h").unwrap();
        assert!(!create_new(&p, "{}").unwrap());
        assert_eq!(DeviceIdentity::load(&p).unwrap().unwrap(), first);
    }

    /// 文件坏了报错，不悄悄重新生成（否则本机会变成一台新机器）。
    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "not json").unwrap();
        assert!(DeviceIdentity::load_or_create(&p, "h").is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "not json");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let mut id = DeviceIdentity::load_or_create(&p, "h").unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        id.adopt_labels(&p, &["other".into()]).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// 本地索引里已有的旧标签（如改名前的主机名）映射到本机，并持久化；重复调用不重写。
    #[test]
    fn adopts_existing_index_labels() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let mut id = DeviceIdentity::load_or_create(&p, "mac").unwrap();
        let labels = vec!["mac".to_string(), "old-mac".to_string()];
        assert!(id.adopt_labels(&p, &labels).unwrap());
        assert!(!id.adopt_labels(&p, &labels).unwrap());
        let reloaded = DeviceIdentity::load(&p).unwrap().unwrap();
        assert!(reloaded.is_local_label("mac"));
        assert!(reloaded.is_local_label("old-mac"));
        assert!(!reloaded.is_local_label("runner"));
        assert_eq!(reloaded.session_label(), "mac");
    }

    /// 升级前的索引：会话 id 用的是当时的主机名（还有一批改名前的旧主机名）。升级后先建身份、
    /// 再改主机名，本机来源算出的会话 id 仍和库里一致，不会重复入库；旧主机名的会话也归本机。
    #[test]
    fn existing_session_ids_stay_valid() {
        use crate::model::{ProviderId, Session, SessionKind, SourceInfo, Usage};
        let session = |label: &str, native: &str| Session {
            id: format!("claude-code:{label}:{native}"),
            provider: ProviderId::ClaudeCode,
            native_id: native.to_string(),
            parent_session_id: None,
            kind: SessionKind::Main,
            title: None,
            cwd: None,
            git_branch: None,
            machine_id: label.to_string(),
            agent_version: None,
            entrypoint: None,
            started_at_ms: 1,
            ended_at_ms: None,
            updated_at_ms: 1,
            models: vec![],
            usage: Usage::default(),
            message_count: 0,
            recorded_cost_usd: None,
            estimated_cost_usd: None,
            source: SourceInfo {
                path: format!("/nonexistent/{native}.jsonl").into(),
                size_bytes: 1,
                mtime_ms: 1,
                compressed: false,
            },
        };
        let mut store = crate::Store::in_memory().unwrap();
        store.upsert_session(&session("my-mac", "s1")).unwrap();
        store.upsert_session(&session("older-mac", "s0")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let first = ensure_at(&p, "my-mac", &store).unwrap();
        // 用户之后改了主机名。
        let id = ensure_at(&p, "renamed-mac", &store).unwrap();
        assert_eq!(id.machine_id(), first.machine_id());
        assert_eq!(id.session_label(), "my-mac");
        let recomputed = format!("claude-code:{}:s1", id.session_label());
        assert!(store.get_session(&recomputed).unwrap().is_some());
        for label in store.machine_labels().unwrap() {
            assert!(id.is_local_label(&label), "{label} 应映射到本机");
        }
        assert!(!id.is_local_label("renamed-mac"));
    }

    #[test]
    fn debug_hides_secret() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(&path(&dir), "h").unwrap();
        assert!(!format!("{id:?}").contains(&hex(id.secret())));
    }
}
