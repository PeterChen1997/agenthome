//! 远程用量的受限 SSH 导出与配对（CARM-295）。规则见 `agenthome_core::remote_ssh`。

use agenthome_core::device::DeviceIdentity;
use agenthome_core::paths;
use agenthome_core::remote_pull;
use agenthome_core::remote_ssh::{self, EXPORT_TIMEOUT, PairRequest};
use agenthome_core::usage_import::ImportStore;
use agenthome_core::usage_store::UsageStore;
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum RemoteAction {
    /// 采集端：给 Mac 的专用公钥授权只读导出（写 ~/.ssh/authorized_keys，带 restrict、固定命令、来源网段）
    Authorize {
        /// 这把钥匙的名字（字母数字和 . _ -），撤销时用
        #[arg(long)]
        name: String,
        /// Mac `remote pair` 打印的公钥文件；`-` 从标准输入读
        #[arg(long)]
        pubkey: PathBuf,
        /// 允许连接的来源网段，逗号分隔，只接受内网 / Tailscale 地址，如 192.168.1.0/24,100.64.0.0/10
        #[arg(long)]
        from: String,
        /// authorized_keys 路径，默认 ~/.ssh/authorized_keys
        #[arg(long)]
        authorized_keys: Option<PathBuf>,
        /// 导出命令用的数据目录（device.json、usage/），默认 ~/AgentHome
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// 固定命令里写的 agenthome 路径，默认当前可执行文件
        #[arg(long)]
        exe: Option<PathBuf>,
    },
    /// 采集端：撤销某把专用钥匙的授权
    Revoke {
        #[arg(long)]
        name: String,
        #[arg(long)]
        authorized_keys: Option<PathBuf>,
    },
    /// 采集端：列出 AgentHome 授权过的钥匙
    List {
        #[arg(long)]
        authorized_keys: Option<PathBuf>,
    },
    /// 采集端：打印本机 SSH 主机密钥指纹，经可信渠道告诉 Mac 配对时核对
    HostFingerprint {
        /// sshd 主机密钥目录，默认 /etc/ssh
        #[arg(long, default_value = "/etc/ssh")]
        dir: PathBuf,
    },
    /// Mac：与一台采集端配对（核对主机密钥指纹、生成专用钥匙），打印要交给采集端授权的公钥
    Pair {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// 采集端 `remote host-fingerprint` 给出的 SHA256:… 指纹
        #[arg(long)]
        fingerprint: String,
        /// 允许连接的目标网段，逗号分隔，只接受内网 / Tailscale 地址
        #[arg(long)]
        allow: String,
        /// 默认 ~/AgentHome
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Mac：删除一台远端的配对（专用钥匙与固定的主机密钥）
    Unpair {
        #[arg(long)]
        name: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Mac：从远端拉取用量并导入远端用量库（与 App 后台拉取同一套逻辑），拉到没有新变更为止
    Pull {
        #[arg(long)]
        name: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Mac：从远端拉一页用量，原样打印 JSON（导入在单元 7）
    Fetch {
        #[arg(long)]
        name: String,
        /// 游标：上一页的 next，首次为 0
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

fn authorized_keys_path(p: Option<PathBuf>) -> Result<PathBuf> {
    match p {
        Some(p) => Ok(p),
        None => Ok(dirs_home()?.join(".ssh").join("authorized_keys")),
    }
}

fn dirs_home() -> Result<PathBuf> {
    let data = paths::default_data_dir();
    data.parent().map(Path::to_path_buf).context("找不到主目录")
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("读取 {}", path.display())),
    }
}

pub fn run(action: RemoteAction) -> Result<()> {
    match action {
        RemoteAction::Authorize {
            name,
            pubkey,
            from,
            authorized_keys,
            data_dir,
            exe,
        } => {
            let key = if pubkey.as_os_str() == "-" {
                let mut s = String::new();
                std::io::stdin().take(4096).read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(&pubkey)
                    .with_context(|| format!("读取公钥 {}", pubkey.display()))?
            };
            let from = remote_ssh::parse_private_nets(&from)?;
            let exe = match exe {
                Some(p) => p,
                None => std::env::current_exe()?.canonicalize()?,
            };
            let data_dir = data_dir.map(|d| d.canonicalize()).transpose()?;
            let line = remote_ssh::authorized_line(&name, &key, &from, &exe, data_dir.as_deref())?;
            let path = authorized_keys_path(authorized_keys)?;
            let text = remote_ssh::upsert_authorized(&read_or_empty(&path)?, &name, &line);
            remote_ssh::write_private(&path, &text)?;
            println!(
                "已授权 {name}：只能执行用量导出，来源限 {}",
                from.iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!("{line}");
            Ok(())
        }
        RemoteAction::Revoke {
            name,
            authorized_keys,
        } => {
            let path = authorized_keys_path(authorized_keys)?;
            let (text, n) = remote_ssh::revoke_authorized(&read_or_empty(&path)?, &name);
            ensure!(n > 0, "没有名为 {name} 的授权");
            remote_ssh::write_private(&path, &text)?;
            println!("已撤销 {name}（删除 {n} 行）");
            Ok(())
        }
        RemoteAction::List { authorized_keys } => {
            let path = authorized_keys_path(authorized_keys)?;
            for e in remote_ssh::list_authorized(&read_or_empty(&path)?) {
                println!("{}\tfrom={}", e.name, e.from);
            }
            Ok(())
        }
        RemoteAction::HostFingerprint { dir } => {
            let fps = remote_ssh::host_fingerprints(&dir)?;
            ensure!(
                !fps.is_empty(),
                "{} 下没有 ssh_host_*_key.pub",
                dir.display()
            );
            for fp in fps {
                println!("{fp}");
            }
            Ok(())
        }
        RemoteAction::Pair {
            name,
            host,
            user,
            port,
            fingerprint,
            allow,
            data_dir,
        } => {
            let data = data_dir.unwrap_or_else(paths::default_data_dir);
            let dir = remote_ssh::remote_dir(&data, &name)?;
            let pubkey = remote_ssh::pair(
                &dir,
                &PairRequest {
                    name: &name,
                    host: &host,
                    port,
                    user: &user,
                    fingerprint: &fingerprint,
                    allow: &allow,
                },
            )?;
            println!("主机密钥指纹核对通过，已固定。专用公钥：");
            println!("{pubkey}");
            println!();
            println!(
                "在采集端把上面这行存成文件后执行（--from 填本 Mac 的内网 / Tailscale 地址或网段）："
            );
            println!(
                "  agenthome remote authorize --name <本 Mac 的名字> --pubkey <公钥文件> --from <网段>"
            );
            Ok(())
        }
        RemoteAction::Unpair { name, data_dir } => {
            let data = data_dir.unwrap_or_else(paths::default_data_dir);
            let dir = remote_ssh::remote_dir(&data, &name)?;
            remote_ssh::load_config(&dir)?;
            std::fs::remove_dir_all(&dir)?;
            println!("已删除与 {name} 的配对；采集端那把钥匙请用 remote revoke 撤销");
            Ok(())
        }
        RemoteAction::Pull { name, data_dir } => {
            let data = data_dir.unwrap_or_else(paths::default_data_dir);
            let local = DeviceIdentity::load(&data.join("device.json"))?
                .context("本机还没有设备身份（先打开一次 App 或跑 scan）")?;
            let mut store = ImportStore::open(&data.join("remote").join("usage.db"), &local)?;
            let dir = remote_ssh::remote_dir(&data, &name)?;
            let now = || chrono::Utc::now().timestamp_millis();
            loop {
                let outcome =
                    remote_pull::pull_ssh(&data, &mut store, &local, &name, &|| false, now())
                        .map_err(|e| anyhow::anyhow!("{e}（已提交 {} 页）", e.pages))?;
                remote_pull::save_last_ok(&dir, now())?;
                eprintln!(
                    "{} 页：新增 {}，替换 {}，完整快照 {}，积压 {}",
                    outcome.pages,
                    outcome.inserted,
                    outcome.replaced,
                    outcome.snapshots,
                    outcome.backlog
                );
                if !outcome.more {
                    break;
                }
            }
            let machine_id = store.peer(&name)?.context("还没有配对记录")?;
            let t = store.totals(Some(&machine_id))?;
            println!(
                "{name}\tevents={}\trequests={}\tinput={}\toutput={}\tcache_read={}\tcache_write_5m={}\tcache_write_1h={}\treasoning={}",
                t.events,
                t.requests,
                t.input_tokens,
                t.output_tokens,
                t.cache_read_tokens,
                t.cache_write_5m_tokens,
                t.cache_write_1h_tokens,
                t.reasoning_tokens
            );
            Ok(())
        }
        RemoteAction::Fetch {
            name,
            after,
            data_dir,
        } => {
            let data = data_dir.unwrap_or_else(paths::default_data_dir);
            let dir = remote_ssh::remote_dir(&data, &name)?;
            let local = DeviceIdentity::load(&data.join("device.json"))?;
            let page = remote_ssh::fetch(&dir, after, local.as_ref().map(|i| i.machine_id()))?;
            println!("{}", serde_json::to_string(&page)?);
            eprintln!(
                "{} 条，next={}，high_water={}，more={}",
                page.page.records.len(),
                page.next,
                page.high_water,
                page.more
            );
            Ok(())
        }
    }
}

/// authorized_keys 固定命令的入口。只认 `SSH_ORIGINAL_COMMAND` 里的导出请求，
/// 只读本机用量库，输出一页 JSON。报错只写类别，不带路径。
pub fn ssh_export(data_dir: Option<PathBuf>) -> ! {
    std::thread::spawn(|| {
        std::thread::sleep(EXPORT_TIMEOUT);
        eprintln!("agenthome ssh-export: 超时");
        std::process::exit(124);
    });
    match export(data_dir) {
        Ok(bytes) => {
            let mut out = std::io::stdout().lock();
            let ok = out.write_all(&bytes).and_then(|_| out.flush());
            std::process::exit(if ok.is_ok() { 0 } else { 1 });
        }
        Err(e) => {
            eprintln!("agenthome ssh-export: {e}");
            std::process::exit(2);
        }
    }
}

fn export(data_dir: Option<PathBuf>) -> Result<Vec<u8>> {
    let Ok(command) = std::env::var("SSH_ORIGINAL_COMMAND") else {
        bail!("只接受用量导出请求（不提供交互 shell）");
    };
    let after = remote_ssh::parse_request(&command)?;
    let data = data_dir.unwrap_or_else(paths::default_data_dir);
    let identity = DeviceIdentity::load(&data.join("device.json"))
        .map_err(|_| anyhow::anyhow!("设备身份文件读不了"))?
        .context("采集端还没有设备身份（先跑采集）")?;
    let db = data.join("usage").join("usage.db");
    // The collector owns schema initialization and writes. Export reads the committed WAL snapshot.
    let store = UsageStore::open_read_only(&db, &identity)
        .map_err(|_| anyhow::anyhow!("打不开用量库"))?
        .context("采集端还没有用量库（先跑采集）")?;
    remote_ssh::export_page(&store, after)?.encode()
}
