//! 采集服务调度（CARM-297，方案单元 10）。
//!
//! 为什么要这些用例：采集端常驻在别人的机器上，和数据库等服务抢资源。
//! - 首扫可能有几百 MiB 历史：必须分片、随时可停，重启后从没提交的文件续上，结果与一口气采完完全相同；
//! - 状态目录满了不能靠丢数据腾地方：暂停时进度不前移、已有用量和待导出的变更一条不少，腾出空间后补齐；
//! - 一个活跃的大文件每轮都在变，不能让排在后面的文件永远轮不到；
//! - 失败只按类别上报，日志和状态文件里不能出现路径。

use agenthome_core::GeminiProvider;
use agenthome_core::collector::{Collector, Limits};
use agenthome_core::providers::SourceProvider;
use agenthome_core::usage_store::{FailureKind, UsageStore};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

fn line(v: Value) -> String {
    format!("{v}\n")
}

/// 一个 Gemini CLI 会话文件：`replies` 次回复，每次新鲜输入 100、输出 10。
fn session(n: usize, replies: usize) -> String {
    let mut s = line(json!({
        "sessionId": format!("aaaaaaaa-1111-4222-8333-{n:012}"),
        "startTime": "2026-11-01T08:00:00.000Z", "kind": "main"
    }));
    for r in 0..replies {
        s += &reply(n, r);
    }
    s
}

fn reply(n: usize, r: usize) -> String {
    line(json!({
        "id": format!("r-{n}-{r}"), "timestamp": format!("2026-11-01T08:{:02}:{:02}.000Z", n % 60, r % 60),
        "type": "gemini", "content": "SECRET-REPLY", "model": "gemini-3-flash",
        "tokens": {"input": 100, "output": 10, "cached": 0, "thoughts": 0, "tool": 0, "total": 110}
    }))
}

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(files: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gemini");
        let fx = Self { dir, root };
        for n in 0..files {
            fx.write(n, &session(n, 1));
        }
        fx
    }

    fn file(&self, n: usize) -> PathBuf {
        self.root.join(format!(
            "tmp/proj-{n}/chats/session-2026-11-01T08-00-{n:08}.jsonl"
        ))
    }

    fn write(&self, n: usize, body: &str) {
        let p = self.file(n);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn append(&self, n: usize, body: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.file(n))
            .unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("AgentHome")
    }

    fn state_dir(&self) -> PathBuf {
        self.data().join("usage")
    }

    fn open(&self, limits: Limits) -> anyhow::Result<Collector> {
        let providers: Vec<Box<dyn SourceProvider + Send + Sync>> =
            vec![Box::new(GeminiProvider::new(self.root.clone()))];
        Collector::open(
            &self.data().join("device.json"),
            &self.state_dir().join("usage.db"),
            providers,
            limits,
        )
    }
}

/// 不限速、预算宽裕，只按 `shard_bytes` 分片。
fn limits(shard_bytes: u64) -> Limits {
    Limits {
        interval: Duration::from_secs(60),
        read_bytes_per_sec: u64::MAX / 4,
        shard_bytes,
        state_budget_bytes: 1 << 40,
        commit_reserve_bytes: 0,
    }
}

/// 库里全部有效用量：(请求数, 新鲜输入, 输出)。
fn totals(store: &UsageStore) -> (u64, u64, u64) {
    let changes = store.changes_since(0, 100_000).unwrap();
    changes.iter().fold((0, 0, 0), |(n, i, o), c| {
        (
            n + u64::from(c.record.requests),
            i + c.record.input_tokens,
            o + c.record.output_tokens,
        )
    })
}

/// 首扫分片：一轮只读一个文件，其余记积压；中途停进程（丢掉 Collector）再开，
/// 从没提交的文件续上，最终与「一轮读完」逐项相同，`seq` 不多分配。
#[test]
fn first_scan_is_sharded_and_resumes_after_restart() {
    let fx = Fixture::new(5);

    let mut c = fx.open(limits(1)).unwrap();
    let r = c.run_round().unwrap();
    assert_eq!((r.files, r.read_files, r.pending_files), (5, 1, 4));
    assert!(r.pending_bytes > 0);
    assert_eq!(c.store().high_water().unwrap(), 1);
    let r = c.run_round().unwrap();
    assert_eq!((r.read_files, r.unchanged, r.pending_files), (1, 1, 3));
    assert_eq!(c.next_delay(&r), Duration::ZERO, "有积压就立即下一轮");
    drop(c); // systemd 停服务 / 进程被杀

    let mut c = fx.open(limits(1)).unwrap();
    let mut rounds = 0;
    loop {
        let r = c.run_round().unwrap();
        rounds += 1;
        assert_eq!(r.failed(), 0);
        if r.pending_files == 0 {
            assert!(c.next_delay(&r) > Duration::ZERO, "追平后回到常态间隔");
            break;
        }
        assert!(rounds < 10, "积压必须逐轮减少");
    }
    assert_eq!(rounds, 3, "重启后只补剩下的 3 个文件");
    assert_eq!(totals(c.store()), (5, 500, 50));
    assert_eq!(c.store().high_water().unwrap(), 5, "重启不重复分配");

    // 对照：同样的数据一轮读完。
    let other = Fixture::new(5);
    let mut whole = other.open(limits(u64::MAX)).unwrap();
    let r = whole.run_round().unwrap();
    assert_eq!((r.read_files, r.pending_files), (5, 0));
    assert_eq!(totals(whole.store()), totals(c.store()));
}

/// 活跃文件每轮都在变：下一轮从上次读到的位置之后继续，排在后面的文件不会被饿死。
#[test]
fn busy_file_does_not_starve_later_files() {
    let fx = Fixture::new(3);
    let mut c = fx.open(limits(1)).unwrap();
    for round in 0..3 {
        // 第 0 个文件每轮都在追加；一轮只够读一个文件。
        fx.append(0, &reply(0, 10 + round));
        let r = c.run_round().unwrap();
        assert_eq!(r.read_files, 1);
        // 第 0 轮读文件 0，之后从上次之后继续：第 1 轮文件 1、第 2 轮文件 2。
        assert_eq!(r.pending_files, if round == 0 { 2 } else { 2 - round + 1 });
    }
    // 轮回到文件 0，把它三次追加一并补上。
    let r = c.run_round().unwrap();
    assert_eq!((r.read_files, r.pending_files), (1, 0));
    assert_eq!(
        totals(c.store()).0,
        3 + 3,
        "3 个文件各 1 条 + 文件 0 追加的 3 条"
    );
}

/// 并发 1：同一状态目录只允许一个采集进程。
#[test]
fn second_collector_on_same_state_dir_is_refused() {
    let fx = Fixture::new(1);
    let _first = fx.open(limits(1)).unwrap();
    assert!(fx.open(limits(1)).is_err());
}

/// 状态目录接近预算：暂停读新文件，进度不前移，已有用量与待导出的变更一条不少；
/// 腾出空间后下一轮补齐，结果与没暂停过一样。
#[test]
fn budget_pause_keeps_progress_and_resumes_after_space_is_freed() {
    let fx = Fixture::new(2);
    let budget = Limits {
        state_budget_bytes: 64 << 20,
        commit_reserve_bytes: 1 << 20,
        ..limits(u64::MAX)
    };
    let mut c = fx.open(budget).unwrap();
    let r = c.run_round().unwrap();
    assert_eq!((r.read_files, r.paused_for_budget), (2, false));
    let hw = c.store().high_water().unwrap();
    let before = c.store().changes_since(0, 1000).unwrap();

    // 别的东西把状态目录塞满（稀疏文件，不真占盘）。
    let filler = fx.state_dir().join("filler");
    std::fs::File::create(&filler)
        .unwrap()
        .set_len(64 << 20)
        .unwrap();
    fx.append(0, &reply(0, 5));
    fx.write(2, &session(2, 2));

    let r = c.run_round().unwrap();
    assert!(r.paused_for_budget);
    assert_eq!((r.read_files, r.pending_files), (0, 2));
    assert!(r.state_bytes > r.state_budget_bytes - (1 << 20));
    assert_eq!(c.store().high_water().unwrap(), hw, "暂停期间不写新用量");
    assert_eq!(
        c.store().changes_since(0, 1000).unwrap(),
        before,
        "待导出的变更一条不丢"
    );
    assert_eq!(
        c.next_delay(&r),
        Duration::from_secs(60) - Duration::from_millis(r.elapsed_ms)
    );
    c.write_status(&r).unwrap();
    let status = std::fs::read_to_string(fx.state_dir().join("collector-status.json")).unwrap();
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["paused_for_budget"], true);
    assert_eq!(status["pending_files"], 2);

    // 腾出空间。
    std::fs::remove_file(&filler).unwrap();
    let r = c.run_round().unwrap();
    assert_eq!(
        (r.paused_for_budget, r.read_files, r.pending_files),
        (false, 2, 0)
    );
    assert_eq!(totals(c.store()), (5, 500, 50), "2 + 追加 1 + 新文件 2");
}

/// 失败只按类别计数；状态文件里不出现路径。
#[test]
fn failures_are_classified_without_paths() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new(2);
    let unreadable = fx.file(1);
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&unreadable).is_ok() {
        // root 跑测试时权限不生效，这条用例没有意义。
        eprintln!("跳过：当前用户无视文件权限");
        return;
    }
    let mut c = fx.open(limits(u64::MAX)).unwrap();
    let r = c.run_round().unwrap();
    assert_eq!(r.read_files, 1);
    assert_eq!(
        r.failures.get(&FailureKind::PermissionDenied),
        Some(&1),
        "{:?}",
        r.failures
    );
    c.write_status(&r).unwrap();
    let status = std::fs::read_to_string(fx.state_dir().join("collector-status.json")).unwrap();
    assert!(status.contains("\"permission_denied\": 1"), "{status}");
    let root = fx.dir.path().to_str().unwrap();
    assert!(!status.contains(root), "状态文件不能带路径");
    assert!(!status.contains("SECRET"));

    // 权限恢复后下一轮补上，不需要人工干预。
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = c.run_round().unwrap();
    assert_eq!((r.read_files, r.failed()), (1, 0));
    assert_eq!(totals(c.store()).0, 2);
}

/// 磁盘真满时 SQLite / I/O 报的错归到 `disk_full`，不和普通读错混在一起。
#[test]
fn disk_full_errors_are_recognised() {
    let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::StorageFull))
        .context(FailureKind::Store);
    assert_eq!(FailureKind::of(&io), FailureKind::DiskFull);
    let sqlite = anyhow::Error::from(rusqlite_full()).context(FailureKind::Store);
    assert_eq!(FailureKind::of(&sqlite), FailureKind::DiskFull);
    let gone = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound))
        .context(FailureKind::Io);
    assert_eq!(FailureKind::of(&gone), FailureKind::Vanished);
    let parse = anyhow::anyhow!("坏行").context(FailureKind::Parse);
    assert_eq!(FailureKind::of(&parse), FailureKind::Parse);
}

fn rusqlite_full() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL), None)
}
