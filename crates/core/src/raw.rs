//! raw 层：原件归档（docs/01 §2 方案 C；落地清单 10.1–10.3）。
//!
//! agent 会自己删原件（Claude Code 默认 30 天清理），index 层只是解析结果，
//! 所以这里把原件内容逐字节存一份，放在 `~/AgentHome/raw/<provider>/<machineId>/<key>/`：
//!
//! - `0001.jsonl.zst`、`0002.jsonl.zst` ……：原件内容的 zstd 压缩。解压后与原件逐字节相同；
//!   Codex 的 `.jsonl.zst` 原件也是先解压再存，所以「内容」统一指解压后的 jsonl 字节。
//! - `manifest.json`：每个版本的内容长度、sha256、存储长度、来源路径 / inode / size / mtime、拷贝时间。
//!
//! 规则：
//! - **只增不改。** 原件变长且前段内容不变（append-only 的常态），把新增的字节作为一个新的
//!   zstd 帧追加到当前版本文件末尾（多帧 zstd 解压后就是拼接的内容）。
//! - 原件变短、前段被改写、或被替换：当前版本原样保留，整份另存为下一个版本。
//! - Codex 把 `.jsonl` 压成 `.jsonl.zst` 改名：key 去掉扩展名后相同，内容相同就只记下新路径，不重复存。
//! - 每次写完数据再原子写清单。上次追加中途退出导致版本文件比清单记录长的，下次先截回清单长度。
//! - 清单读不懂就报错，不覆盖。
//!
//! key 取原件相对来源根目录的路径去掉 `.jsonl` / `.jsonl.zst`：Claude 是 `<项目目录>/<会话 id>`
//! （子代理 `<项目目录>/<会话 id>/subagents/agent-*`），Codex 是 `YYYY/MM/DD/rollout-*`。

use crate::model::ProviderId;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MANIFEST: &str = "manifest.json";
const MANIFEST_SCHEMA: u32 = 1;
const MACHINE_ID_FILE: &str = "machine-id";
const LOCK_FILE: &str = ".lock";
const ZSTD_LEVEL: i32 = 3;
const CHUNK: usize = 1 << 20;

/// 某一时刻看到的原件。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceStamp {
    pub path: PathBuf,
    pub inode: Option<u64>,
    pub size: u64,
    pub mtime_ms: i64,
    /// 原件本身是 `.zst`。
    pub compressed: bool,
}

/// 一个版本：一份连续的原件内容。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RawVersion {
    /// 版本文件名，如 `0001.jsonl.zst`。
    pub file: String,
    /// 解压后的字节数，即已归档的原件内容长度。
    pub content_size: u64,
    pub content_sha256: String,
    /// 版本文件在磁盘上的长度（压缩后）。
    pub stored_size: u64,
    /// 写入次数：首次整份 + 每次追加各算一段。
    pub segments: u32,
    pub first_copied_ms: i64,
    pub last_copied_ms: i64,
    /// 内容与本版本一致（或是其前缀）的原件路径；最后一条是最近一次看到的。
    pub sources: Vec<SourceStamp>,
    /// 为什么开了这个新版本（第一个版本没有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub schema: u32,
    pub provider: String,
    pub machine_id: String,
    pub key: String,
    pub versions: Vec<RawVersion>,
}

/// 一次归档的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawOutcome {
    /// 原件的 size / mtime / 路径都和上次一样，没读文件。
    Unchanged,
    /// 第一次归档。
    Created { bytes: u64 },
    /// 原件变长，追加了新增字节。
    Appended { bytes: u64 },
    /// 内容没变，只是路径 / mtime 变了（如 Codex 压缩改名），记下新来源。
    Restamped,
    /// 原件被截断、改写或替换，另存了一个版本。
    NewVersion { bytes: u64, reason: String },
}

/// 一次扫描里 raw 层的汇总。失败逐条列出，不吞掉。
#[derive(Clone, Debug, Default)]
pub struct RawReport {
    pub unchanged: usize,
    pub created: usize,
    pub appended: usize,
    pub restamped: usize,
    pub new_versions: usize,
    /// 本次写入的原件内容字节数（解压后）。
    pub bytes_copied: u64,
    pub failed: Vec<(PathBuf, String)>,
}

impl RawReport {
    pub fn record(&mut self, outcome: &RawOutcome) {
        match outcome {
            RawOutcome::Unchanged => self.unchanged += 1,
            RawOutcome::Created { bytes } => {
                self.created += 1;
                self.bytes_copied += bytes;
            }
            RawOutcome::Appended { bytes } => {
                self.appended += 1;
                self.bytes_copied += bytes;
            }
            RawOutcome::Restamped => self.restamped += 1,
            RawOutcome::NewVersion { bytes, .. } => {
                self.new_versions += 1;
                self.bytes_copied += bytes;
            }
        }
    }

    pub fn merge(&mut self, other: &RawReport) {
        self.unchanged += other.unchanged;
        self.created += other.created;
        self.appended += other.appended;
        self.restamped += other.restamped;
        self.new_versions += other.new_versions;
        self.bytes_copied += other.bytes_copied;
        self.failed.extend(other.failed.iter().cloned());
    }

    /// 本次有没有写入任何东西。
    pub fn wrote_anything(&self) -> bool {
        self.created + self.appended + self.restamped + self.new_versions > 0
    }
}

/// raw 层整体体积，给设置页用。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawStats {
    /// 有清单的会话（原件文件）数。
    pub sessions: usize,
    pub versions: usize,
    /// 原件内容总字节（解压后）。
    pub content_bytes: u64,
    /// 磁盘占用（压缩后）。
    pub stored_bytes: u64,
    /// 读不懂的清单数。
    pub unreadable: usize,
}

/// raw 层仓库。多个进程（app 同步线程、CLI `scan`）可以同时打开，写入时用文件锁串行。
pub struct RawArchive {
    root: PathBuf,
    machine_id: String,
    lock: File,
    /// 版本文件 → 已归档内容的 sha256 中间状态。活跃原件每追加几行就归档一次，
    /// 没有它就得每次把已归档的几十 MB 前缀重读重算一遍（sha256 状态无法从摘要恢复）。
    hashes: Mutex<HashMap<PathBuf, HashState>>,
}

/// 某个版本文件归档到 `content_size` 时的哈希状态，以及当时原件的身份（路径、inode、首尾指纹）。
struct HashState {
    content_size: u64,
    content_sha256: String,
    hasher: Sha256,
    path: PathBuf,
    inode: Option<u64>,
    fingerprint: [u8; 32],
}

impl RawArchive {
    /// 打开（不存在就创建）raw 层根目录。machineId 第一次取主机名后固定写进 `machine-id`，
    /// 之后主机名变了也不换目录。
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| format!("创建 {}", root.display()))?;
        let id_path = root.join(MACHINE_ID_FILE);
        let machine_id = match fs::read_to_string(&id_path) {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            Ok(_) => bail!("{} 是空的", id_path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let id = sanitize_component(&crate::providers::machine_id());
                fs::write(&id_path, format!("{id}\n"))
                    .with_context(|| format!("写入 {}", id_path.display()))?;
                id
            }
            Err(error) => return Err(error).context(format!("读取 {}", id_path.display())),
        };
        if sanitize_component(&machine_id) != machine_id {
            bail!("{} 内容不是合法目录名：{machine_id:?}", id_path.display());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(LOCK_FILE))
            .with_context(|| format!("打开 {}", root.join(LOCK_FILE).display()))?;
        Ok(Self {
            root,
            machine_id,
            lock,
            hashes: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// 原件对应的归档目录。
    pub fn session_dir(
        &self,
        provider: &ProviderId,
        source_root: &Path,
        path: &Path,
    ) -> Result<PathBuf> {
        Ok(self
            .root
            .join(provider.as_str())
            .join(&self.machine_id)
            .join(session_key(source_root, path)?))
    }

    /// 读某个原件的清单；没归档过返回 `None`。
    pub fn manifest(
        &self,
        provider: &ProviderId,
        source_root: &Path,
        path: &Path,
    ) -> Result<Option<Manifest>> {
        read_manifest(
            &self
                .session_dir(provider, source_root, path)?
                .join(MANIFEST),
        )
    }

    /// 归档一个原件：没变跳过，变长追加，被改写另存新版本。
    pub fn archive(
        &self,
        provider: &ProviderId,
        source_root: &Path,
        path: &Path,
    ) -> Result<RawOutcome> {
        let _guard = LockGuard::acquire(&self.lock)?;
        let meta = fs::metadata(path).with_context(|| format!("读取元数据 {}", path.display()))?;
        let stamp = SourceStamp {
            path: path.to_path_buf(),
            inode: inode(&meta),
            size: meta.len(),
            mtime_ms: crate::providers::mtime_ms(&meta),
            compressed: is_zst(path),
        };
        let key = session_key(source_root, path)?;
        let dir = self
            .root
            .join(provider.as_str())
            .join(&self.machine_id)
            .join(&key);
        let manifest_path = dir.join(MANIFEST);
        let mut manifest = match read_manifest(&manifest_path)? {
            Some(manifest) => manifest,
            None => Manifest {
                schema: MANIFEST_SCHEMA,
                provider: provider.as_str().to_string(),
                machine_id: self.machine_id.clone(),
                key: key.to_string_lossy().into_owned(),
                versions: Vec::new(),
            },
        };
        if manifest
            .versions
            .last()
            .is_some_and(|latest| latest.sources.contains(&stamp))
        {
            return Ok(RawOutcome::Unchanged);
        }
        fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
        let now = now_ms();

        let extend = match manifest.versions.last_mut() {
            Some(latest) => Some(try_extend(&dir, latest, path, &stamp, now, &self.hashes)?),
            None => None,
        };
        let outcome = match extend {
            Some(Extend::Same) => RawOutcome::Restamped,
            Some(Extend::Appended(bytes)) => RawOutcome::Appended { bytes },
            Some(Extend::Diverged(reason)) => {
                let number = manifest.versions.len() + 1;
                let version = write_version(&dir, number, path, stamp, now, Some(reason.clone()))?;
                let bytes = version.content_size;
                manifest.versions.push(version);
                RawOutcome::NewVersion { bytes, reason }
            }
            None => {
                let version = write_version(&dir, 1, path, stamp, now, None)?;
                let bytes = version.content_size;
                manifest.versions.push(version);
                RawOutcome::Created { bytes }
            }
        };
        write_manifest(&manifest_path, &manifest)?;
        Ok(outcome)
    }

    /// 解压出某个版本的完整内容（校验 sha256）。
    pub fn read_version(&self, dir: &Path, version: &RawVersion) -> Result<Vec<u8>> {
        let path = dir.join(&version.file);
        let file = File::open(&path).with_context(|| format!("打开 {}", path.display()))?;
        let mut reader = zstd::Decoder::new(file)?.take(version.content_size);
        let mut out = Vec::with_capacity(usize::try_from(version.content_size).unwrap_or(0));
        reader.read_to_end(&mut out)?;
        let sha = format!("{:x}", Sha256::digest(&out));
        if out.len() as u64 != version.content_size || sha != version.content_sha256 {
            bail!("{} 内容与清单记录不符", path.display());
        }
        Ok(out)
    }

    /// 遍历所有清单统计体积。
    pub fn stats(&self) -> RawStats {
        let mut stats = RawStats::default();
        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && e.file_name() == MANIFEST)
        {
            match read_manifest(entry.path()) {
                Ok(Some(manifest)) => {
                    stats.sessions += 1;
                    stats.versions += manifest.versions.len();
                    for version in &manifest.versions {
                        stats.content_bytes += version.content_size;
                        stats.stored_bytes += version.stored_size;
                    }
                }
                Ok(None) | Err(_) => stats.unreadable += 1,
            }
        }
        stats
    }
}

enum Extend {
    /// 内容和当前版本完全一样。
    Same,
    /// 追加了这么多字节。
    Appended(u64),
    /// 接不上，要另存新版本。
    Diverged(String),
}

/// 尝试把原件接到当前版本末尾。
///
/// 要先确认原件前段与已归档内容一致。同一原件（路径、inode 不变、没变短）且已归档部分首尾各 4 KB 没变时，
/// 直接用内存里缓存的 sha256 中间状态，只读新增的字节；否则（程序刚启动、原件被替换等）重读前缀重算。
fn try_extend(
    dir: &Path,
    latest: &mut RawVersion,
    path: &Path,
    stamp: &SourceStamp,
    now: i64,
    hashes: &Mutex<HashMap<PathBuf, HashState>>,
) -> Result<Extend> {
    let stored = dir.join(&latest.file);
    match fs::metadata(&stored).map(|m| m.len()) {
        Err(_) => return Ok(Extend::Diverged("上一版本的归档文件不见了".into())),
        Ok(len) if len < latest.stored_size => {
            return Ok(Extend::Diverged("上一版本的归档文件比清单记录短".into()));
        }
        Ok(len) if len > latest.stored_size => {
            // 上次追加写了数据但没来得及写清单：截回清单记录的长度再接。
            OpenOptions::new()
                .write(true)
                .open(&stored)?
                .set_len(latest.stored_size)
                .with_context(|| format!("截断 {}", stored.display()))?;
        }
        Ok(_) => {}
    }

    let cached = if stamp.compressed {
        None
    } else {
        hashes.lock().ok().and_then(|mut h| h.remove(&stored))
    }
    .filter(|c| {
        c.content_size == latest.content_size
            && c.content_sha256 == latest.content_sha256
            && c.path == stamp.path
            && c.inode == stamp.inode
            && stamp.size >= c.content_size
            && source_fingerprint(path, c.content_size).ok() == Some(c.fingerprint)
    });

    let mut buf = vec![0u8; CHUNK];
    let (mut reader, mut hasher): (Box<dyn Read>, Sha256) = match cached {
        Some(state) => {
            let mut file = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
            file.seek(SeekFrom::Start(state.content_size))?;
            (Box::new(file), state.hasher)
        }
        None => {
            let mut reader = open_content(path)?;
            let mut hasher = Sha256::new();
            let mut remaining = latest.content_size;
            while remaining > 0 {
                let want = usize::try_from(remaining.min(CHUNK as u64)).unwrap_or(CHUNK);
                let n = read_some(&mut reader, &mut buf[..want])?;
                if n == 0 {
                    return Ok(Extend::Diverged("原件变短了（被截断或替换）".into()));
                }
                hasher.update(&buf[..n]);
                remaining -= n as u64;
            }
            if format!("{:x}", hasher.clone().finalize()) != latest.content_sha256 {
                return Ok(Extend::Diverged("原件已归档的部分被改写了".into()));
            }
            (reader, hasher)
        }
    };

    let first = read_some(&mut reader, &mut buf)?;
    if first == 0 {
        upsert_stamp(&mut latest.sources, stamp.clone());
        remember_hash(hashes, &stored, latest, hasher, path, stamp);
        return Ok(Extend::Same);
    }
    let file = OpenOptions::new()
        .append(true)
        .open(&stored)
        .with_context(|| format!("打开 {}", stored.display()))?;
    let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL)?;
    let mut added = 0u64;
    let mut n = first;
    while n > 0 {
        hasher.update(&buf[..n]);
        encoder.write_all(&buf[..n])?;
        added += n as u64;
        n = read_some(&mut reader, &mut buf)?;
    }
    encoder.finish()?.sync_all()?;

    latest.content_size += added;
    latest.content_sha256 = format!("{:x}", hasher.clone().finalize());
    latest.stored_size = fs::metadata(&stored)?.len();
    latest.segments += 1;
    latest.last_copied_ms = now;
    upsert_stamp(&mut latest.sources, stamp.clone());
    remember_hash(hashes, &stored, latest, hasher, path, stamp);
    Ok(Extend::Appended(added))
}

/// 记下版本文件当前的哈希中间状态，供下次追加时跳过前缀重读。压缩原件不缓存（不能按偏移续读）。
fn remember_hash(
    hashes: &Mutex<HashMap<PathBuf, HashState>>,
    stored: &Path,
    latest: &RawVersion,
    hasher: Sha256,
    path: &Path,
    stamp: &SourceStamp,
) {
    if stamp.compressed {
        return;
    }
    let Ok(fingerprint) = source_fingerprint(path, latest.content_size) else {
        return;
    };
    if let Ok(mut h) = hashes.lock() {
        h.insert(
            stored.to_path_buf(),
            HashState {
                content_size: latest.content_size,
                content_sha256: latest.content_sha256.clone(),
                hasher,
                path: path.to_path_buf(),
                inode: stamp.inode,
                fingerprint,
            },
        );
    }
}

/// 原件 `[0, len)` 首尾各 4 KB 的 sha256：只在末尾追加的原件这两段不会变。
fn source_fingerprint(path: &Path, len: u64) -> io::Result<[u8; 32]> {
    const SPAN: u64 = 4096;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut head = vec![0u8; len.min(SPAN) as usize];
    file.read_exact(&mut head)?;
    hasher.update(&head);
    let tail_start = len.saturating_sub(SPAN);
    let mut tail = vec![0u8; (len - tail_start) as usize];
    file.seek(SeekFrom::Start(tail_start))?;
    file.read_exact(&mut tail)?;
    hasher.update(&tail);
    hasher.update(len.to_le_bytes());
    Ok(hasher.finalize().into())
}

/// 整份写一个新版本：先写临时文件，压完再改名。
fn write_version(
    dir: &Path,
    number: usize,
    path: &Path,
    stamp: SourceStamp,
    now: i64,
    reason: Option<String>,
) -> Result<RawVersion> {
    let file = format!("{number:04}.jsonl.zst");
    let target = dir.join(&file);
    let tmp = dir.join(format!("{file}.tmp"));
    let mut reader = open_content(path)?;
    let out = File::create(&tmp).with_context(|| format!("创建 {}", tmp.display()))?;
    let mut encoder = zstd::Encoder::new(out, ZSTD_LEVEL)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = read_some(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        encoder.write_all(&buf[..n])?;
        size += n as u64;
    }
    encoder.finish()?.sync_all()?;
    fs::rename(&tmp, &target).with_context(|| format!("改名到 {}", target.display()))?;
    Ok(RawVersion {
        stored_size: fs::metadata(&target)?.len(),
        file,
        content_size: size,
        content_sha256: format!("{:x}", hasher.finalize()),
        segments: 1,
        first_copied_ms: now,
        last_copied_ms: now,
        sources: vec![stamp],
        reason,
    })
}

fn upsert_stamp(sources: &mut Vec<SourceStamp>, stamp: SourceStamp) {
    sources.retain(|s| s.path != stamp.path);
    sources.push(stamp);
}

fn read_manifest(path: &Path) -> Result<Option<Manifest>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(format!("读取 {}", path.display())),
    };
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("{} 不是合法清单，未改动", path.display()))?;
    if manifest.schema > MANIFEST_SCHEMA {
        bail!(
            "{} 的清单版本 {} 比本程序新（{MANIFEST_SCHEMA}），未改动",
            path.display(),
            manifest.schema
        );
    }
    Ok(Some(manifest))
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("写入 {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("改名到 {}", path.display()))?;
    Ok(())
}

/// 原件相对来源根目录、去掉 `.jsonl` / `.jsonl.zst` 后的路径。
fn session_key(source_root: &Path, path: &Path) -> Result<PathBuf> {
    let rel = path.strip_prefix(source_root).with_context(|| {
        format!(
            "{} 不在来源目录 {} 下",
            path.display(),
            source_root.display()
        )
    })?;
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("{} 的相对路径不规范", path.display());
    }
    let text = rel.to_string_lossy();
    let stem = text
        .strip_suffix(".jsonl.zst")
        .or_else(|| text.strip_suffix(".jsonl"))
        .unwrap_or(&text);
    if stem.is_empty() {
        bail!("{} 算不出归档 key", path.display());
    }
    Ok(PathBuf::from(stem))
}

fn open_content(path: &Path) -> Result<Box<dyn Read>> {
    let file = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    if is_zst(path) {
        Ok(Box::new(zstd::Decoder::new(file).with_context(|| {
            format!("zstd 解压 {}", path.display())
        })?))
    } else {
        Ok(Box::new(file))
    }
}

fn read_some(reader: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

fn is_zst(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

#[cfg(unix)]
fn inode(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode(_meta: &fs::Metadata) -> Option<u64> {
    None
}

fn sanitize_component(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "unknown-host".to_string()
    } else {
        trimmed.to_string()
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 持有期间独占 raw 层写入。
struct LockGuard<'a>(&'a File);

impl<'a> LockGuard<'a> {
    fn acquire(file: &'a File) -> Result<Self> {
        file.lock().context("锁定 raw 层")?;
        Ok(Self(file))
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        source_root: PathBuf,
        archive: RawArchive,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("projects");
        fs::create_dir_all(source_root.join("-Users-me-proj")).unwrap();
        let archive = RawArchive::open(tmp.path().join("raw")).unwrap();
        Fixture {
            _tmp: tmp,
            source_root,
            archive,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn restore_all(fx: &Fixture, path: &Path) -> Vec<Vec<u8>> {
        let provider = ProviderId::ClaudeCode;
        let dir = fx
            .archive
            .session_dir(&provider, &fx.source_root, path)
            .unwrap();
        let manifest = fx
            .archive
            .manifest(&provider, &fx.source_root, path)
            .unwrap()
            .unwrap();
        manifest
            .versions
            .iter()
            .map(|v| fx.archive.read_version(&dir, v).unwrap())
            .collect()
    }

    /// 产品立足点：原件被 agent 清理之后，归档里仍能逐字节还原。
    #[test]
    fn archived_content_survives_source_deletion() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let body = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
        write(&path, body);
        let outcome = fx
            .archive
            .archive(&ProviderId::ClaudeCode, &fx.source_root, &path)
            .unwrap();
        assert_eq!(
            outcome,
            RawOutcome::Created {
                bytes: body.len() as u64
            }
        );
        fs::remove_file(&path).unwrap();
        assert_eq!(restore_all(&fx, &path), vec![body.to_vec()]);
    }

    /// 没变的原件不重读不重写，否则每次扫描都会把几 GB 原件读一遍。
    #[test]
    fn unchanged_source_is_skipped() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        write(&path, b"line1\n");
        let provider = ProviderId::ClaudeCode;
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        assert_eq!(
            fx.archive
                .archive(&provider, &fx.source_root, &path)
                .unwrap(),
            RawOutcome::Unchanged
        );
    }

    /// append-only 原件只追加新增字节，仍是一个版本，还原结果等于完整原件。
    #[test]
    fn growing_source_appends_segments() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let provider = ProviderId::ClaudeCode;
        write(&path, b"line1\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        append(&path, b"line2\nline3\n");
        assert_eq!(
            fx.archive
                .archive(&provider, &fx.source_root, &path)
                .unwrap(),
            RawOutcome::Appended { bytes: 12 }
        );
        append(&path, b"line4\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();

        let manifest = fx
            .archive
            .manifest(&provider, &fx.source_root, &path)
            .unwrap()
            .unwrap();
        assert_eq!(manifest.versions.len(), 1);
        assert_eq!(manifest.versions[0].segments, 3);
        assert_eq!(
            restore_all(&fx, &path),
            vec![b"line1\nline2\nline3\nline4\n".to_vec()]
        );
    }

    /// 原件被截断或改写时，旧内容一个字节都不能丢：旧版本原样保留，新内容另存。
    /// 追加时用缓存的哈希状态跳过前缀重读；但同一文件（同 inode）的开头被改写仍要发现，另存新版本。
    #[test]
    fn cached_hash_still_detects_rewritten_head() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let provider = ProviderId::ClaudeCode;
        write(&path, b"line1\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        append(&path, b"line2\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        // 原地改写开头（不换 inode）并变长。
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"LINE1").unwrap();
        drop(f);
        append(&path, b"line3\n");
        let outcome = fx
            .archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        assert!(
            matches!(outcome, RawOutcome::NewVersion { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            restore_all(&fx, &path),
            vec![
                b"line1\nline2\n".to_vec(),
                b"LINE1\nline2\nline3\n".to_vec()
            ]
        );
    }

    #[test]
    fn rewritten_source_keeps_old_version() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let provider = ProviderId::ClaudeCode;
        write(&path, b"original long history\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        write(&path, b"short\n");
        let outcome = fx
            .archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        assert!(matches!(outcome, RawOutcome::NewVersion { bytes: 6, .. }));

        write(&path, b"Xhort\nmore\n");
        let outcome = fx
            .archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        assert!(
            matches!(outcome, RawOutcome::NewVersion { .. }),
            "{outcome:?}"
        );

        assert_eq!(
            restore_all(&fx, &path),
            vec![
                b"original long history\n".to_vec(),
                b"short\n".to_vec(),
                b"Xhort\nmore\n".to_vec()
            ]
        );
    }

    /// Codex 把 `.jsonl` 压成 `.jsonl.zst`：内容相同只记新路径，不重复存；之后变长照常追加。
    #[test]
    fn codex_compression_rename_is_not_a_new_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let archive = RawArchive::open(tmp.path().join("raw")).unwrap();
        let provider = ProviderId::Codex;
        let plain = root.join("2026/11/08/rollout-x.jsonl");
        write(&plain, b"a\nb\n");
        archive.archive(&provider, &root, &plain).unwrap();

        let zst = root.join("2026/11/08/rollout-x.jsonl.zst");
        write(&zst, &zstd::encode_all(&b"a\nb\n"[..], 3).unwrap());
        fs::remove_file(&plain).unwrap();
        assert_eq!(
            archive.archive(&provider, &root, &zst).unwrap(),
            RawOutcome::Restamped
        );
        assert_eq!(
            archive.archive(&provider, &root, &zst).unwrap(),
            RawOutcome::Unchanged
        );

        write(&zst, &zstd::encode_all(&b"a\nb\nc\n"[..], 3).unwrap());
        assert_eq!(
            archive.archive(&provider, &root, &zst).unwrap(),
            RawOutcome::Appended { bytes: 2 }
        );
        let manifest = archive.manifest(&provider, &root, &zst).unwrap().unwrap();
        assert_eq!(manifest.versions.len(), 1);
        assert_eq!(manifest.key, "2026/11/08/rollout-x");
    }

    /// 上次追加写了数据却没写清单（进程被杀）：多出来的半截要截掉，否则还原内容会重复。
    #[test]
    fn interrupted_append_is_repaired() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let provider = ProviderId::ClaudeCode;
        write(&path, b"line1\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        let dir = fx
            .archive
            .session_dir(&provider, &fx.source_root, &path)
            .unwrap();
        append(
            &dir.join("0001.jsonl.zst"),
            &zstd::encode_all(&b"line2\n"[..], 3).unwrap(),
        );

        append(&path, b"line2\n");
        fx.archive
            .archive(&provider, &fx.source_root, &path)
            .unwrap();
        assert_eq!(restore_all(&fx, &path), vec![b"line1\nline2\n".to_vec()]);
    }

    /// 清单坏了不能被覆盖：那等于把已归档的版本记录全丢了。
    #[test]
    fn corrupt_manifest_is_reported_not_overwritten() {
        let fx = fixture();
        let path = fx.source_root.join("-Users-me-proj/abc.jsonl");
        let provider = ProviderId::ClaudeCode;
        write(&path, b"line1\n");
        let dir = fx
            .archive
            .session_dir(&provider, &fx.source_root, &path)
            .unwrap();
        write(&dir.join(MANIFEST), b"{ not json");
        assert!(
            fx.archive
                .archive(&provider, &fx.source_root, &path)
                .is_err()
        );
        assert_eq!(fs::read(dir.join(MANIFEST)).unwrap(), b"{ not json");
        assert_eq!(fx.archive.stats().unreadable, 1);
    }

    /// 子代理放在父会话目录下，与 docs/01 §2 的目录结构一致。
    #[test]
    fn keys_follow_source_layout() {
        let root = Path::new("/r");
        assert_eq!(
            session_key(root, Path::new("/r/p/abc.jsonl")).unwrap(),
            PathBuf::from("p/abc")
        );
        assert_eq!(
            session_key(root, Path::new("/r/p/abc/subagents/agent-1.jsonl")).unwrap(),
            PathBuf::from("p/abc/subagents/agent-1")
        );
        assert_eq!(
            session_key(root, Path::new("/r/2026/11/08/rollout-x.jsonl.zst")).unwrap(),
            PathBuf::from("2026/11/08/rollout-x")
        );
        assert!(session_key(root, Path::new("/elsewhere/abc.jsonl")).is_err());
    }

    /// machineId 第一次固定后不随主机名变，避免同一台机器的归档分裂成两份。
    #[test]
    fn machine_id_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("raw");
        let first = RawArchive::open(root.clone()).unwrap();
        fs::write(root.join(MACHINE_ID_FILE), "pinned-host\n").unwrap();
        drop(first);
        assert_eq!(RawArchive::open(root).unwrap().machine_id(), "pinned-host");
    }

    #[test]
    fn stats_sum_versions() {
        let fx = fixture();
        let provider = ProviderId::ClaudeCode;
        let a = fx.source_root.join("-Users-me-proj/a.jsonl");
        let b = fx.source_root.join("-Users-me-proj/b.jsonl");
        write(&a, b"aaaa\n");
        write(&b, b"bb\n");
        fx.archive.archive(&provider, &fx.source_root, &a).unwrap();
        fx.archive.archive(&provider, &fx.source_root, &b).unwrap();
        write(&b, b"c\n");
        fx.archive.archive(&provider, &fx.source_root, &b).unwrap();
        let stats = fx.archive.stats();
        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.versions, 3);
        assert_eq!(stats.content_bytes, 5 + 3 + 2);
        assert!(stats.stored_bytes > 0);
    }
}
