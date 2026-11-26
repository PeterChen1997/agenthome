//! 受限 SSH 导出与配对（CARM-284 方案单元 9，传输方式 A：Mac 经 SSH 拉取）。
//!
//! 两端各一半：
//! - **采集端**（Runner）：`authorized_keys` 里给 Mac 的专用公钥加一行
//!   `restrict,from="<内网网段>",command="'<agenthome>' ssh-export" ssh-ed25519 … agenthome:<名字>`。
//!   `restrict` 关掉 pty、端口 / agent / X11 转发和 `~/.ssh/rc`；`command=` 让 sshd 无论客户端要执行什么
//!   都只跑导出命令，客户端原本要的命令放在 `SSH_ORIGINAL_COMMAND` 里，导出命令只认一种写法
//!   （[`parse_request`]），其余一律拒绝；交互 shell、`cat` 任意文件、scp / sftp 都走不到 shell。
//! - **Mac**：每个远端一个目录 `~/AgentHome/remote/<名字>/`，放专用密钥、只含这一台主机密钥的 known_hosts
//!   和 `remote.json`。配对时用户从可信渠道拿到主机密钥指纹（在 Runner 上跑 `ssh-keygen -lf` 看），
//!   扫到的密钥指纹对不上就拒绝；之后每次连接 `StrictHostKeyChecking=yes` 固定校验。
//!
//! 网络范围：配对时必须显式给出允许的网段，且只能是内网 / Tailscale / 回环这类私有地址；每次连接前
//! 解析目标地址，任何一个解析结果不在允许网段内就不连，并直接连解析出的 IP（避免 DNS 中途换地址）。
//! 采集端的 `from=` 同样只接受私有网段。默认什么都不开：不授权就没有任何钥匙能读，不监听端口、不自动发现。
//!
//! 导出：一次一页，序列化后整页不超过 [`usage_v1::MAX_PAGE_BYTES`]；两端都有超时。

use crate::usage_import::PageCursor;
use crate::usage_store::UsageStore;
use crate::usage_v1::{self, UsagePage};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 导出请求的固定写法：`agenthome-export v1 after=<seq>`。
pub const REQUEST_PREFIX: &str = "agenthome-export v1 after=";
/// 采集端导出命令的总时限，超时直接退出（退出码 124）。
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(20);
/// Mac 端一次拉取的总时限（含连接），超时杀掉 ssh。
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// authorized_keys 行注释的前缀，`revoke` / `list` 只认带它的行。
const KEY_COMMENT_PREFIX: &str = "agenthome:";
/// 导出时一次最多从库里取多少条，再按页大小截断。
const FETCH_ROWS: usize = 2000;

// ---------------------------------------------------------------------------
// 导出（采集端）

/// 解析 `SSH_ORIGINAL_COMMAND`。只接受 `agenthome-export v1 after=<十进制>`，多一个字符都拒绝。
pub fn parse_request(command: &str) -> Result<u64> {
    ensure!(command.len() <= 64, "请求过长");
    let Some(after) = command.strip_prefix(REQUEST_PREFIX) else {
        bail!("只接受用量导出请求");
    };
    ensure!(
        !after.is_empty() && after.len() <= 20 && after.bytes().all(|c| c.is_ascii_digit()),
        "游标不是十进制整数"
    );
    after.parse().context("游标超出范围")
}

/// 客户端发出的请求字符串。
pub fn request_command(after: u64) -> String {
    format!("{REQUEST_PREFIX}{after}")
}

/// 一页导出。整体（含外层字段）序列化后不超过 `MAX_PAGE_BYTES`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPage {
    pub v: u32,
    /// 采集端用量库的标识；和上次不同说明库重建过，旧游标作废（单元 8 走完整快照）。
    pub store_id: String,
    /// 本页从哪个游标之后开始（即请求里的 `after`）。
    pub after: u64,
    /// 下一次请求用的游标：本页最后一条的 `seq`；本页为空时等于 `after`。
    pub next: u64,
    /// 采集端当前的高水位。
    pub high_water: u64,
    /// `next` 之后还有没有变更。
    pub more: bool,
    pub page: UsagePage,
}

impl ExportPage {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.v == usage_v1::VERSION, "不支持的版本 {}", self.v);
        ensure!(
            self.store_id.len() == 32 && self.store_id.bytes().all(|c| c.is_ascii_hexdigit()),
            "store_id 不是 32 位 hex"
        );
        ensure!(
            self.after <= self.next && self.next <= self.high_water.max(self.after),
            "游标不一致"
        );
        ensure!(
            self.page.records.is_empty() == (self.next == self.after),
            "游标与记录数不一致"
        );
        self.page.validate()
    }

    /// 交给 `usage_import::ImportStore::import_page` 的游标区间 `(after, next]`。
    pub fn cursor(&self) -> PageCursor {
        PageCursor {
            store_id: self.store_id.clone(),
            after: self.after,
            through: self.next,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= usage_v1::MAX_PAGE_BYTES,
            "页大小 {} 超过上限 {}",
            bytes.len(),
            usage_v1::MAX_PAGE_BYTES
        );
        Ok(bytes)
    }

    /// Mac 端入口：先看大小再解析；未知字段、超限、校验不过整页拒收。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= usage_v1::MAX_PAGE_BYTES,
            "页大小 {} 超过上限 {}",
            bytes.len(),
            usage_v1::MAX_PAGE_BYTES
        );
        let page: Self = serde_json::from_slice(bytes)?;
        page.validate()?;
        Ok(page)
    }
}

/// 取 `seq > after` 的变更，按页大小截断成一页。
pub fn export_page(store: &UsageStore, after: u64) -> Result<ExportPage> {
    let changes = store.changes_since(after, FETCH_ROWS)?;
    let mut page = ExportPage {
        v: usage_v1::VERSION,
        store_id: store.store_id().to_string(),
        after,
        next: after,
        high_water: 0,
        more: false,
        page: UsagePage {
            v: usage_v1::VERSION,
            machine_id: store.machine_id().to_string(),
            records: Vec::new(),
        },
    };
    // 外层字段按最大位数估算，留足余量；每条记录多一个逗号。
    let mut size = serde_json::to_vec(&page)?.len() + 3 * 20;
    let mut taken = 0;
    for change in &changes {
        let len = serde_json::to_vec(&change.record)?.len() + 1;
        if size + len > usage_v1::MAX_PAGE_BYTES {
            break;
        }
        size += len;
        page.page.records.push(change.record.clone());
        page.next = change.seq;
        taken += 1;
    }
    ensure!(taken > 0 || changes.is_empty(), "单条记录超过页大小上限");
    // 高水位在取完变更之后读，不会比本页最后一条小。最大的 seq 总属于某条现存记录
    // （记录的 seq 只会换成更大的），所以 `next < high_water` 就说明后面还有。
    page.high_water = store.high_water()?.max(page.next);
    page.more = page.next < page.high_water;
    Ok(page)
}

// ---------------------------------------------------------------------------
// 网段

/// 一个网段：地址 + 前缀长度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

/// 允许出现在配置里的私有范围：RFC 1918、Tailscale（CGNAT 100.64/10）、回环、链路本地、IPv6 ULA。
const PRIVATE: [(IpAddr, u8); 9] = [
    (IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8),
    (IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)), 12),
    (IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16),
    (IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), 10),
    (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8),
    (IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0)), 16),
    (IpAddr::V6(Ipv6Addr::LOCALHOST), 128),
    (IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0)), 7),
    (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)), 10),
];

impl Net {
    /// `10.0.0.0/8`、`192.168.1.5`（单个地址）、`fd7a::/48`。
    pub fn parse(s: &str) -> Result<Self> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .parse()
            .with_context(|| format!("{s:?} 不是 IP 或网段"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .with_context(|| format!("{s:?} 的前缀长度不对"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                masked(u32::from(net).into(), self.prefix, 32)
                    == masked(u32::from(ip).into(), self.prefix, 32)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                masked(u128::from(net), self.prefix, 128)
                    == masked(u128::from(ip), self.prefix, 128)
            }
            _ => false,
        }
    }

    /// 整个网段都落在私有范围内。
    pub fn is_private(&self) -> bool {
        PRIVATE.iter().any(|&(addr, prefix)| {
            let range = Net { addr, prefix };
            self.prefix >= prefix && range.contains(self.addr)
        })
    }
}

fn masked(v: u128, prefix: u8, bits: u8) -> u128 {
    if prefix == 0 { 0 } else { v >> (bits - prefix) }
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// 解析逗号分隔的网段列表；空列表、非私有网段都报错。
pub fn parse_private_nets(list: &str) -> Result<Vec<Net>> {
    let nets: Vec<Net> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Net::parse)
        .collect::<Result<_>>()?;
    ensure!(!nets.is_empty(), "必须给出允许的网段");
    for net in &nets {
        ensure!(
            net.is_private(),
            "{net} 不是内网 / Tailscale 网段，不允许（不做公网同步）"
        );
    }
    Ok(nets)
}

// ---------------------------------------------------------------------------
// authorized_keys（采集端）

/// 名字：字母数字和 `. _ -`，1–64 个字符。用在 authorized_keys 注释、目录名里。
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// 一把已授权的专用钥匙。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedEntry {
    pub name: String,
    pub from: String,
}

/// 校验公钥：单行 `ssh-ed25519 <base64> [注释]`。返回 `(类型, base64)`。
fn parse_pubkey(text: &str) -> Result<(&str, &str)> {
    let text = text.trim();
    ensure!(!text.contains(['\n', '\r']), "公钥必须是单行");
    let mut parts = text.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let blob = parts.next().unwrap_or_default();
    ensure!(
        kind == "ssh-ed25519",
        "只接受 ssh-ed25519 公钥（不能带 authorized_keys 选项）"
    );
    ensure!(
        blob.len() >= 60
            && blob.len() <= 100
            && blob
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c)),
        "公钥内容不对"
    );
    Ok((kind, blob))
}

/// 拼出 authorized_keys 的一行。`exe` 是采集端 agenthome 的绝对路径，`data_dir` 非默认时一起写进命令。
pub fn authorized_line(
    name: &str,
    pubkey: &str,
    from: &[Net],
    exe: &Path,
    data_dir: Option<&Path>,
) -> Result<String> {
    ensure!(is_valid_name(name), "名字只能含字母数字和 . _ -");
    let (kind, blob) = parse_pubkey(pubkey)?;
    ensure!(!from.is_empty(), "必须限制来源网段");
    for net in from {
        ensure!(net.is_private(), "{net} 不是内网 / Tailscale 网段");
    }
    let mut command = format!("{} ssh-export", shell_quote(exe)?);
    if let Some(dir) = data_dir {
        command.push_str(&format!(" --data-dir {}", shell_quote(dir)?));
    }
    let from: Vec<String> = from.iter().map(Net::to_string).collect();
    Ok(format!(
        "restrict,from=\"{}\",command=\"{command}\" {kind} {blob} {KEY_COMMENT_PREFIX}{name}",
        from.join(",")
    ))
}

/// 单引号包起来给 sshd 交给 shell 执行；路径里有引号、反斜杠、换行直接拒绝，不做转义。
fn shell_quote(path: &Path) -> Result<String> {
    let s = path.to_str().context("路径不是 UTF-8")?;
    ensure!(path.is_absolute(), "必须是绝对路径");
    ensure!(
        !s.contains(['\'', '"', '\\', '\n', '\r']),
        "路径里不能有引号、反斜杠或换行"
    );
    Ok(format!("'{s}'"))
}

fn entry_name(line: &str) -> Option<&str> {
    let line = line.trim();
    if !line.starts_with("restrict,") || !line.contains(" ssh-export") {
        return None;
    }
    line.rsplit(' ')
        .next()
        .and_then(|c| c.strip_prefix(KEY_COMMENT_PREFIX))
}

/// 把 `line` 写进 authorized_keys 文本：同名的旧行替换，否则追加。
pub fn upsert_authorized(text: &str, name: &str, line: &str) -> String {
    let mut out: Vec<&str> = text
        .lines()
        .filter(|l| entry_name(l) != Some(name))
        .collect();
    out.push(line);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// 删掉名为 `name` 的授权行。返回新文本和删掉的行数；不动其他行。
pub fn revoke_authorized(text: &str, name: &str) -> (String, usize) {
    let mut removed = 0;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let hit = entry_name(l) == Some(name);
            removed += usize::from(hit);
            !hit
        })
        .collect();
    let mut s = kept.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    (s, removed)
}

/// 列出 AgentHome 写的授权行。
pub fn list_authorized(text: &str) -> Vec<AuthorizedEntry> {
    text.lines()
        .filter_map(|l| {
            let name = entry_name(l)?;
            let from = l
                .split_once("from=\"")
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(f, _)| f.to_string())
                .unwrap_or_default();
            Some(AuthorizedEntry {
                name: name.to_string(),
                from,
            })
        })
        .collect()
}

/// 原子改写文件（临时文件 + 改名），权限 0600；父目录不存在则建成 0700。
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("没有父目录")?;
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        set_mode(dir, 0o700)?;
    }
    let tmp = path.with_extension("agenthome-tmp");
    std::fs::write(&tmp, text)?;
    set_mode(&tmp, 0o600)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// 配对与拉取（Mac）

const SSH: &str = "/usr/bin/ssh";
const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
const SSH_KEYSCAN: &str = "/usr/bin/ssh-keyscan";

/// 一台已配对的远端，存在 `<目录>/remote.json`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    pub schema: u32,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 允许连接的网段（私有地址）。
    pub allow: Vec<String>,
}

impl RemoteConfig {
    fn allow_nets(&self) -> Result<Vec<Net>> {
        parse_private_nets(&self.allow.join(","))
    }

    fn host_key_alias(&self) -> String {
        format!("agenthome-{}", self.name)
    }
}

/// 拉取 / 配对失败的类别。上报时只报类别，不带地址、路径和载荷。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteErrorKind {
    /// 目标地址不在允许网段内。
    NetNotAllowed,
    /// 主机密钥和配对时固定的不一致（或配对时指纹对不上）。
    HostKeyMismatch,
    /// 钥匙未授权或已撤销，或来源不在采集端的 `from=` 内。
    Unauthorized,
    /// 连不上（拒绝连接、超时、解析失败）。
    Unreachable,
    /// 超过总时限。
    Timeout,
    /// 采集端返回错误，或返回内容不合规。
    BadResponse,
    /// 拉到的是本机自己的用量。
    SelfPairing,
    /// 本地配置或工具问题。
    Local,
}

impl RemoteErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetNotAllowed => "net_not_allowed",
            Self::HostKeyMismatch => "host_key_mismatch",
            Self::Unauthorized => "unauthorized",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::BadResponse => "bad_response",
            Self::SelfPairing => "self_pairing",
            Self::Local => "local",
        }
    }
}

#[derive(Debug)]
pub struct RemoteError {
    pub kind: RemoteErrorKind,
    pub detail: String,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.as_str(), self.detail)
    }
}

impl std::error::Error for RemoteError {}

fn fail<T>(kind: RemoteErrorKind, detail: impl Into<String>) -> Result<T> {
    Err(RemoteError {
        kind,
        detail: detail.into(),
    }
    .into())
}

/// 解析目标，所有解析结果都必须在允许网段内；返回第一个地址。
pub fn resolve_allowed(host: &str, port: u16, allow: &[Net]) -> Result<IpAddr> {
    let addrs: Vec<IpAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.map(|a| a.ip()).collect(),
        Err(e) => return fail(RemoteErrorKind::Unreachable, format!("解析主机失败：{e}")),
    };
    let Some(first) = addrs.first().copied() else {
        return fail(RemoteErrorKind::Unreachable, "主机没有地址");
    };
    for ip in &addrs {
        if !allow.iter().any(|n| n.contains(*ip)) {
            return fail(
                RemoteErrorKind::NetNotAllowed,
                format!("目标地址 {ip} 不在允许的网段内，拒绝连接"),
            );
        }
    }
    Ok(first)
}

fn check_fingerprint_format(fp: &str) -> Result<()> {
    let ok = fp.strip_prefix("SHA256:").is_some_and(|b| {
        b.len() == 43
            && b.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
    });
    ensure!(
        ok,
        "指纹格式应为 SHA256:<43 位 base64>（在采集端跑 ssh-keygen -lf <主机公钥> 获得）"
    );
    Ok(())
}

/// 一行公钥（`类型 base64`）的 SHA256 指纹，交给 ssh-keygen 算。
fn fingerprint_of(key_line: &str) -> Result<String> {
    let mut child = Command::new(SSH_KEYGEN)
        .args(["-l", "-E", "sha256", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 ssh-keygen")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("ssh-keygen stdin")?;
        writeln!(stdin, "{key_line}")?;
    }
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "ssh-keygen 算指纹失败");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .nth(1)
        .map(str::to_string)
        .context("ssh-keygen 输出不对")
}

/// 配对参数。
pub struct PairRequest<'a> {
    pub name: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    /// 用户从可信渠道拿到的主机密钥指纹 `SHA256:…`。
    pub fingerprint: &'a str,
    pub allow: &'a str,
}

/// 配对：校验参数与网段 → 扫主机密钥并核对指纹 → 固定到本远端专用的 known_hosts →
/// 生成专用 ed25519 钥匙 → 写 `remote.json`。返回专用公钥（交给采集端 `remote authorize`）。
/// 目录已存在时拒绝（先 `unpair`），不覆盖已有钥匙。
pub fn pair(dir: &Path, req: &PairRequest<'_>) -> Result<String> {
    ensure!(is_valid_name(req.name), "名字只能含字母数字和 . _ -");
    ensure!(
        is_valid_name(req.user) && !req.user.starts_with('-'),
        "用户名只能含字母数字和 . _ -"
    );
    ensure!(
        !req.host.is_empty()
            && !req.host.starts_with('-')
            && req
                .host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-:_".contains(&c)),
        "主机只能是主机名或 IP"
    );
    check_fingerprint_format(req.fingerprint)?;
    let allow = parse_private_nets(req.allow)?;
    ensure!(!dir.exists(), "这个名字已经配对过，先 unpair 再重新配对");
    let ip = resolve_allowed(req.host, req.port, &allow)?;

    let scan = Command::new(SSH_KEYSCAN)
        .args(["-T", "5", "-p", &req.port.to_string(), &ip.to_string()])
        .stderr(Stdio::null())
        .output()
        .context("启动 ssh-keyscan")?;
    let text = String::from_utf8_lossy(&scan.stdout);
    let mut matched = None;
    let mut seen = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.split_whitespace();
        let (Some(_host), Some(kind), Some(blob)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let key = format!("{kind} {blob}");
        let fp = fingerprint_of(&key)?;
        if fp == req.fingerprint {
            matched = Some(key);
        }
        seen.push(fp);
    }
    if seen.is_empty() {
        return fail(
            RemoteErrorKind::Unreachable,
            "没扫到主机密钥（连不上或不是 SSH 服务）",
        );
    }
    let Some(key) = matched else {
        return fail(
            RemoteErrorKind::HostKeyMismatch,
            format!(
                "主机密钥指纹不符，拒绝配对。期望 {}，实际 {}",
                req.fingerprint,
                seen.join(" / ")
            ),
        );
    };

    let config = RemoteConfig {
        schema: 1,
        name: req.name.to_string(),
        host: req.host.to_string(),
        port: req.port,
        user: req.user.to_string(),
        allow: allow.iter().map(Net::to_string).collect(),
    };
    std::fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    write_private(
        &dir.join("known_hosts"),
        &format!("{} {key}\n", config.host_key_alias()),
    )?;
    let status = Command::new(SSH_KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-C")
        .arg(format!("{KEY_COMMENT_PREFIX}{}", req.name))
        .arg("-f")
        .arg(dir.join("id_ed25519"))
        .stdin(Stdio::null())
        .status()
        .context("启动 ssh-keygen")?;
    ensure!(status.success(), "生成专用钥匙失败");
    write_private(
        &dir.join("remote.json"),
        &serde_json::to_string_pretty(&config)?,
    )?;
    Ok(std::fs::read_to_string(dir.join("id_ed25519.pub"))?
        .trim()
        .to_string())
}

pub fn load_config(dir: &Path) -> Result<RemoteConfig> {
    let text =
        std::fs::read_to_string(dir.join("remote.json")).context("没有这个远端（先 pair）")?;
    let config: RemoteConfig = serde_json::from_str(&text).context("remote.json 不对")?;
    ensure!(config.schema == 1, "remote.json 版本不认识");
    ensure!(is_valid_name(&config.name), "remote.json 里的名字不对");
    Ok(config)
}

/// 拉一页。`local_machine_id`：本机身份，拉到自己的用量（把本机当远端）时拒绝。
pub fn fetch(dir: &Path, after: u64, local_machine_id: Option<&str>) -> Result<ExportPage> {
    let config = load_config(dir)?;
    let allow = config.allow_nets()?;
    let ip = resolve_allowed(&config.host, config.port, &allow)?;
    let known_hosts = dir.join("known_hosts");
    let key = dir.join("id_ed25519");
    let mut cmd = Command::new(SSH);
    // -F /dev/null：不读用户 ~/.ssh/config，ProxyCommand、LocalForward 之类都不会被带进来。
    cmd.arg("-F").arg("/dev/null").args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "CheckHostIP=no",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
    ]);
    cmd.arg(format!("HostKeyAlias={}", config.host_key_alias()))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-i")
        .arg(&key)
        .arg("-p")
        .arg(config.port.to_string())
        .arg("-l")
        .arg(&config.user)
        .arg("--")
        .arg(ip.to_string())
        .arg(request_command(after));
    let (status, stdout, stderr) = run_limited(cmd, FETCH_TIMEOUT)?;
    if !status.success() {
        let kind = classify_ssh_error(status.code(), &stderr);
        return fail(kind, first_line(&stderr));
    }
    let page = match ExportPage::decode(&stdout) {
        Ok(page) => page,
        Err(e) => return fail(RemoteErrorKind::BadResponse, format!("返回内容不合规：{e}")),
    };
    if page.after != after {
        return fail(RemoteErrorKind::BadResponse, "返回的游标与请求不一致");
    }
    if local_machine_id == Some(page.page.machine_id.as_str()) {
        return fail(
            RemoteErrorKind::SelfPairing,
            "对端就是本机（machine_id 相同），拒绝把本机当远端汇入",
        );
    }
    Ok(page)
}

fn first_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('@'))
        .unwrap_or("ssh 失败")
        .chars()
        .take(200)
        .collect()
}

fn classify_ssh_error(code: Option<i32>, stderr: &str) -> RemoteErrorKind {
    if code != Some(255) {
        // 连上了，是采集端导出命令自己报错。
        return RemoteErrorKind::BadResponse;
    }
    if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        RemoteErrorKind::HostKeyMismatch
    } else if stderr.contains("Permission denied") {
        RemoteErrorKind::Unauthorized
    } else {
        RemoteErrorKind::Unreachable
    }
}

/// 跑子进程：stdout 最多读 `MAX_PAGE_BYTES + 1` 字节，超时杀掉。
fn run_limited(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, String)> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RemoteError {
            kind: RemoteErrorKind::Local,
            detail: format!("启动 ssh 失败：{e}"),
        })?;
    let stdout = child.stdout.take().context("ssh stdout")?;
    let stderr = child.stderr.take().context("ssh stderr")?;
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take(usage_v1::MAX_PAGE_BYTES as u64 + 1)
            .read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(16 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return fail(RemoteErrorKind::Timeout, "拉取超时");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok((status, stdout, stderr))
}

/// `~/AgentHome/remote/<name>`
pub fn remote_dir(data_dir: &Path, name: &str) -> Result<PathBuf> {
    ensure!(is_valid_name(name), "名字只能含字母数字和 . _ -");
    Ok(data_dir.join("remote").join(name))
}

/// 本机 SSH 主机公钥的指纹，给用户在配对时核对（采集端 `remote host-fingerprint`）。
pub fn host_fingerprints(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for kind in ["ed25519", "ecdsa", "rsa"] {
        let path = dir.join(format!("ssh_host_{kind}_key.pub"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            let key: Vec<&str> = text.split_whitespace().take(2).collect();
            out.push(format!(
                "{} {}",
                fingerprint_of(&key.join(" "))?,
                kind.to_uppercase()
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_grammar_is_strict() {
        assert_eq!(parse_request("agenthome-export v1 after=0").unwrap(), 0);
        assert_eq!(parse_request("agenthome-export v1 after=42").unwrap(), 42);
        for bad in [
            "",
            "bash",
            "cat /etc/passwd",
            "internal-sftp",
            "/usr/libexec/sftp-server",
            "scp -f /etc/passwd",
            "agenthome-export v1 after=",
            "agenthome-export v1 after=-1",
            "agenthome-export v1 after=1 ",
            "agenthome-export v1 after=1;cat /etc/passwd",
            "agenthome-export v1 after=1\ncat /etc/passwd",
            "agenthome-export v1 after=99999999999999999999",
            "agenthome-export v2 after=1",
            " agenthome-export v1 after=1",
        ] {
            assert!(parse_request(bad).is_err(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn nets() {
        let n = Net::parse("192.168.2.0/24").unwrap();
        assert!(n.contains("192.168.2.117".parse().unwrap()));
        assert!(!n.contains("192.168.3.1".parse().unwrap()));
        assert!(!n.contains("::1".parse().unwrap()));
        for ok in [
            "10.0.0.0/8",
            "10.1.2.3",
            "172.16.0.0/12",
            "172.31.255.1",
            "192.168.0.0/16",
            "100.64.0.0/10",
            "100.101.102.103",
            "127.0.0.1",
            "fd7a:115c:a1e0::/48",
            "::1",
        ] {
            assert!(Net::parse(ok).unwrap().is_private(), "{ok} 应算私有");
        }
        for bad in [
            "0.0.0.0/0",
            "8.8.8.8",
            "10.0.0.0/7",
            "172.32.0.1",
            "100.128.0.1",
            "192.169.0.1",
            "2001:db8::/32",
            "::/0",
        ] {
            assert!(!Net::parse(bad).unwrap().is_private(), "{bad} 不应算私有");
        }
        assert!(Net::parse("10.0.0.0/33").is_err());
        assert!(Net::parse("example.com").is_err());
        assert!(parse_private_nets("").is_err(), "必须显式给网段");
        assert!(parse_private_nets("192.168.1.0/24,8.8.8.8").is_err());
        let allow = parse_private_nets("127.0.0.0/8").unwrap();
        assert!(resolve_allowed("127.0.0.1", 22, &allow).is_ok());
        let err = resolve_allowed("127.0.0.1", 22, &parse_private_nets("10.0.0.0/8").unwrap())
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<RemoteError>().unwrap().kind,
            RemoteErrorKind::NetNotAllowed
        );
    }

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVcLq3vEFkJ2kQkqvJ0nP7mX0X8m3RZ4CB1yXbqTZ2o mac";

    #[test]
    fn authorized_line_is_restricted() {
        let from = parse_private_nets("192.168.2.0/24,100.64.0.0/10").unwrap();
        let line = authorized_line(
            "mac",
            KEY,
            &from,
            Path::new("/opt/agent home/agenthome"),
            None,
        )
        .unwrap();
        assert_eq!(
            line,
            "restrict,from=\"192.168.2.0/24,100.64.0.0/10\",command=\"'/opt/agent home/agenthome' ssh-export\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVcLq3vEFkJ2kQkqvJ0nP7mX0X8m3RZ4CB1yXbqTZ2o agenthome:mac"
        );
        let exe = Path::new("/usr/bin/agenthome");
        assert!(
            authorized_line("mac", KEY, &[], exe, None).is_err(),
            "必须限制来源"
        );
        let public = [Net::parse("8.8.8.0/24").unwrap()];
        assert!(authorized_line("mac", KEY, &public, exe, None).is_err());
        assert!(authorized_line("a b", KEY, &from, exe, None).is_err());
        assert!(authorized_line("mac", KEY, &from, Path::new("rel/agenthome"), None).is_err());
        assert!(authorized_line("mac", KEY, &from, Path::new("/x'y/agenthome"), None).is_err());
        // 公钥里夹带 authorized_keys 选项、换行、别的类型都拒绝。
        for bad in [
            format!("command=\"sh\" {KEY}"),
            format!("{KEY}\nssh-ed25519 AAAA x"),
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7 x".to_string(),
        ] {
            assert!(
                authorized_line("mac", &bad, &from, exe, None).is_err(),
                "{bad}"
            );
        }
        let with_dir =
            authorized_line("mac", KEY, &from, exe, Some(Path::new("/data/ah"))).unwrap();
        assert!(
            with_dir.contains("command=\"'/usr/bin/agenthome' ssh-export --data-dir '/data/ah'\"")
        );
    }

    #[test]
    fn authorize_and_revoke_only_touch_own_lines() {
        let from = parse_private_nets("10.0.0.0/8").unwrap();
        let exe = Path::new("/usr/bin/agenthome");
        let other = "ssh-ed25519 AAAAOTHER someone@laptop";
        let line = authorized_line("mac", KEY, &from, exe, None).unwrap();
        let text = upsert_authorized(&format!("{other}\n"), "mac", &line);
        assert_eq!(text, format!("{other}\n{line}\n"));
        // 重复授权替换同名行，不追加第二行。
        let from2 = parse_private_nets("192.168.0.0/16").unwrap();
        let line2 = authorized_line("mac", KEY, &from2, exe, None).unwrap();
        let text = upsert_authorized(&text, "mac", &line2);
        assert_eq!(text, format!("{other}\n{line2}\n"));
        assert_eq!(
            list_authorized(&text),
            vec![AuthorizedEntry {
                name: "mac".into(),
                from: "192.168.0.0/16".into()
            }]
        );
        let (text, n) = revoke_authorized(&text, "mac");
        assert_eq!(n, 1);
        assert_eq!(text, format!("{other}\n"));
        // 用户自己的行即使注释碰巧叫 agenthome:mac 也不动（不是 restrict + ssh-export 的行）。
        let user_line = "ssh-ed25519 AAAAUSER agenthome:mac";
        let (text, n) = revoke_authorized(user_line, "mac");
        assert_eq!((n, text.trim()), (0, user_line));
    }

    #[test]
    fn fingerprint_format() {
        assert!(
            check_fingerprint_format("SHA256:CLq9xOrw8UUHVN0Z419G/8+1o4a1KMSYq6qANYsZRNc").is_ok()
        );
        for bad in [
            "",
            "MD5:aa:bb",
            "SHA256:short",
            "CLq9xOrw8UUHVN0Z419G/8+1o4a1KMSYq6qANYsZRNc",
        ] {
            assert!(check_fingerprint_format(bad).is_err());
        }
    }

    #[test]
    fn ssh_errors_are_classified() {
        use RemoteErrorKind::*;
        let c = classify_ssh_error;
        assert_eq!(
            c(Some(255), "Host key verification failed."),
            HostKeyMismatch
        );
        assert_eq!(
            c(Some(255), "u@h: Permission denied (publickey)."),
            Unauthorized
        );
        assert_eq!(
            c(Some(255), "connect to host x port 22: Connection refused"),
            Unreachable
        );
        assert_eq!(
            c(Some(2), "agenthome ssh-export: 只接受用量导出请求"),
            BadResponse
        );
    }
}
