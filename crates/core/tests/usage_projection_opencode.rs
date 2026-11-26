//! OpenCode 只读用量适配（CARM-293，方案单元 5）。
//!
//! 为什么要这些用例：
//! - OpenCode 落盘的 `output` 已扣掉推理、`input` 已扣掉缓存，口径一错 Mac 端计价就和 OpenCode 自己的统计对不上；
//!   fixture 用 CARM-289 从 Runner 真实快照取的两组数字和它给的预期值。
//! - OpenCode 是 SQLite + WAL，进行中的请求只在 `-wal` 里：读不到就会少算，进度只看主文件也会一直跳过；
//!   而采集端不能改 OpenCode 的目录（不建 `-wal` / `-shm`、不往它的 `-shm` 写读标记）。
//! - fork 会用新 id、新时间复制整段步骤：不去重就把一次请求的花费算两遍。
//! - 坏行（孤儿步骤、token 不是整数）不能静默吞掉，也不能混进统计。

use agenthome_core::OpenCodeProvider;
use agenthome_core::device::DeviceIdentity;
use agenthome_core::providers::SourceProvider;
use agenthome_core::usage_projection::{dedup, records};
use agenthome_core::usage_store::{UsageStore, collect_source};
use agenthome_core::usage_v1::UsageRecord;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, tokens_input INTEGER DEFAULT 0, tokens_output INTEGER DEFAULT 0,
  tokens_reasoning INTEGER DEFAULT 0, tokens_cache_read INTEGER DEFAULT 0, tokens_cache_write INTEGER DEFAULT 0);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
";

fn identity(dir: &Path) -> DeviceIdentity {
    DeviceIdentity::load_or_create(&dir.join("device.json"), "box").unwrap()
}

/// 新建一个 OpenCode 数据目录，返回保持打开的写连接（模拟 OpenCode 正在运行、不自动合并 WAL）。
fn opencode_root(dir: &Path) -> (PathBuf, Connection) {
    let root = dir.join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let conn = Connection::open(root.join("opencode.db")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    (root, conn)
}

fn assistant(conn: &Connection, id: &str, session: &str, created: i64, model: &str) {
    let data = json!({"role": "assistant", "providerID": "example-provider", "modelID": model,
                      "time": {"created": created}, "path": {"cwd": "/SECRET-PATH-7f3a"}});
    conn.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
        params![id, session, created, data.to_string()],
    )
    .unwrap();
}

fn part(conn: &Connection, id: &str, msg: &str, session: &str, ts: i64, data: Value) {
    conn.execute(
        "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        params![id, msg, session, ts, data.to_string()],
    )
    .unwrap();
}

fn step(input: u64, output: u64, reasoning: u64, read: u64, write: u64) -> Value {
    json!({"type": "step-finish", "reason": "stop", "cost": 0,
           "tokens": {"total": input + output + reasoning + read + write, "input": input, "output": output,
                      "reasoning": reasoning, "cache": {"write": write, "read": read}}})
}

/// CARM-289 脱敏样本：A 缓存命中 + 推理，B 缓存写入（不分 TTL）。
fn sample(conn: &Connection) {
    assistant(
        conn,
        "msg_fixture_a",
        "ses_fixture_a",
        1_699_999_990_000,
        "example-model",
    );
    part(
        conn,
        "prt_text_a",
        "msg_fixture_a",
        "ses_fixture_a",
        1_699_999_995_000,
        json!({"type": "text", "text": "SECRET-REPLY-7f3a"}),
    );
    part(
        conn,
        "prt_fixture_a",
        "msg_fixture_a",
        "ses_fixture_a",
        1_700_000_000_000,
        step(717, 51, 67, 42_240, 0),
    );
    assistant(
        conn,
        "msg_fixture_b",
        "ses_fixture_b",
        1_699_999_999_000,
        "example-model",
    );
    part(
        conn,
        "prt_fixture_b",
        "msg_fixture_b",
        "ses_fixture_b",
        1_700_000_001_000,
        step(6, 47, 0, 0, 15_916),
    );
}

fn project(root: &Path, id: &DeviceIdentity) -> (Vec<UsageRecord>, Vec<String>) {
    let provider = OpenCodeProvider::new(root.to_path_buf());
    let files = provider.discover().files;
    assert_eq!(files, vec![root.join("opencode.db")]);
    let file = provider.usage_events(&files[0]).unwrap().unwrap();
    let (recs, mut warnings) = records(id, &provider.id(), &file);
    warnings.extend(file.warnings);
    (dedup(recs), warnings)
}

fn total(r: &UsageRecord) -> u64 {
    r.input_tokens
        + r.output_tokens
        + r.cache_read_tokens
        + r.cache_write_5m_tokens
        + r.cache_write_1h_tokens
}

#[test]
fn fixture_matches_carm289_expected_values() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let (root, conn) = opencode_root(dir.path());
    sample(&conn);
    let (recs, warnings) = project(&root, &id);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(recs.len(), 2, "只有 step-finish 计请求，文本 part 不算");

    // (请求, 新鲜输入, 含推理输出, 推理, 缓存读, 缓存写 5m, 缓存写 1h, 总 token)，取自 CARM-289 预期表。
    let expected = [
        (1, 717, 118, 67, 42_240, 0, 0, 43_075),
        (1, 6, 47, 0, 0, 15_916, 0, 15_969),
    ];
    for (r, e) in recs.iter().zip(expected) {
        r.validate().unwrap();
        let got = (
            r.requests,
            r.input_tokens,
            r.output_tokens,
            r.reasoning_tokens,
            r.cache_read_tokens,
            r.cache_write_5m_tokens,
            r.cache_write_1h_tokens,
            total(r),
        );
        assert_eq!(got, e);
        assert_eq!(r.source, "opencode");
        assert_eq!(r.model.as_deref(), Some("example-model"));
        assert_eq!(
            r.vendor, None,
            "providerID 是路由名，认不出厂商就留空，不猜"
        );
    }
    assert_eq!(recs[0].ts_ms, 1_700_000_000_000, "时间取完成步骤写入时间");
    assert_eq!(recs.iter().map(total).sum::<u64>(), 59_044);

    // 载荷里没有正文、路径、源 id。
    let payload = serde_json::to_string(&recs).unwrap();
    for secret in [
        "SECRET",
        "prt_fixture",
        "msg_fixture",
        "ses_fixture",
        "example-provider",
    ] {
        assert!(!payload.contains(secret), "载荷泄露了 {secret}");
    }
}

/// 目录里每个文件的 (名字, 大小, 修改时间, 内容)。
fn dir_state(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime, String)> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let meta = e.metadata().unwrap();
            let name = e.file_name().into_string().unwrap();
            (
                name,
                (
                    meta.len(),
                    meta.modified().unwrap(),
                    sha(&std::fs::read(e.path()).unwrap()),
                ),
            )
        })
        .collect()
}

/// 子进程扮演正在运行的 OpenCode（SQLite 在同一进程内共用 `-shm` 映射，必须分进程才测得到读标记）。
/// 父进程经 stdin 发指令：`more` 再写一步，EOF 退出。
#[test]
#[ignore]
fn opencode_writer_child() {
    let Some(db) = std::env::var_os("OPENCODE_WRITER_DB") else {
        return;
    };
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    sample(&conn);
    println!("WRITER-READY");
    for line in std::io::stdin().lines() {
        if line.unwrap() == "more" {
            assistant(
                &conn,
                "msg_c",
                "ses_fixture_a",
                1_700_000_002_000,
                "example-model",
            );
            part(
                &conn,
                "prt_c",
                "msg_c",
                "ses_fixture_a",
                1_700_000_003_000,
                step(1, 2, 0, 3, 0),
            );
            println!("WRITER-MORE");
        }
    }
    std::mem::forget(conn); // 不正常关闭：-wal / -shm 留着，像 OpenCode 被杀。
}

struct Writer {
    child: std::process::Child,
    out: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Writer {
    fn spawn(db: &Path) -> Self {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "opencode_writer_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OPENCODE_WRITER_DB", db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut w = Self { child, out };
        w.wait_for("WRITER-READY");
        w
    }

    fn wait_for(&mut self, marker: &str) {
        for line in self.out.by_ref() {
            if line.unwrap().contains(marker) {
                return;
            }
        }
        panic!("写进程没有输出 {marker}");
    }

    fn more(&mut self) {
        use std::io::Write;
        writeln!(self.child.stdin.as_mut().unwrap(), "more").unwrap();
        self.wait_for("WRITER-MORE");
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn reads_committed_wal_without_touching_source_dir() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let root = dir.path().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let mut writer = Writer::spawn(&root.join("opencode.db"));
    // 写进程一直开着、不合并：样本只在 -wal 里。
    let before = dir_state(&root);
    assert!(before.contains_key("opencode.db-wal") && before.contains_key("opencode.db-shm"));
    assert!(before["opencode.db-wal"].0 > 0, "fixture 应该还在 WAL 里");

    let (recs, _) = project(&root, &id);
    assert_eq!(recs.len(), 2, "WAL 里已提交的请求要读到");
    assert_eq!(
        dir_state(&root),
        before,
        "读取不能改源目录里的任何文件（含 -shm 内容）"
    );

    // OpenCode 继续写，再读能看到新提交的，也照样不改文件。
    writer.more();
    let before = dir_state(&root);
    assert_eq!(project(&root, &id).0.len(), 3);
    assert_eq!(dir_state(&root), before);

    // OpenCode 被杀、-wal / -shm 留下：没有活的写者也照样读到、不改文件。
    drop(writer);
    let before = dir_state(&root);
    assert!(before["opencode.db-wal"].0 > 0);
    assert_eq!(project(&root, &id).0.len(), 3);
    assert_eq!(dir_state(&root), before);
}

#[test]
fn clean_db_without_wal_files_is_read_without_creating_them() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let (root, conn) = opencode_root(dir.path());
    sample(&conn);
    drop(conn); // OpenCode 正常退出：合并并删掉 -wal / -shm。
    let before = dir_state(&root);
    assert_eq!(before.keys().collect::<Vec<_>>(), ["opencode.db"]);

    let (recs, _) = project(&root, &id);
    assert_eq!(recs.len(), 2);
    assert_eq!(dir_state(&root), before, "不能建 -wal / -shm");
}

#[test]
fn forked_copy_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let (root, conn) = opencode_root(dir.path());
    sample(&conn);
    // fork：新会话、新消息 id、新 part id、新写入时间，消息数据（含创建时间）和步骤内容原样复制。
    assistant(
        &conn,
        "msg_fork_a",
        "ses_fork",
        1_699_999_990_000,
        "example-model",
    );
    part(
        &conn,
        "prt_fork_a",
        "msg_fork_a",
        "ses_fork",
        1_700_000_500_000,
        step(717, 51, 67, 42_240, 0),
    );
    // 同一条消息里的第二步，token 恰好一样：是另一次请求，要计入。
    part(
        &conn,
        "prt_fixture_a2",
        "msg_fixture_a",
        "ses_fixture_a",
        1_700_000_000_500,
        step(717, 51, 67, 42_240, 0),
    );

    let (recs, _) = project(&root, &id);
    assert_eq!(recs.len(), 3, "fork 副本只计一次，同消息的第二步照计");
    let a: Vec<_> = recs.iter().filter(|r| r.input_tokens == 717).collect();
    assert_eq!(a.len(), 2);
    assert_eq!(
        a[0].ts_ms, 1_700_000_000_000,
        "保留原请求的时间，不用 fork 的写入时间"
    );
    assert_ne!(a[0].event_key, a[1].event_key);
}

#[test]
fn bad_rows_are_reported_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let (root, conn) = opencode_root(dir.path());
    sample(&conn);
    // 找不到消息的孤儿步骤。
    part(
        &conn,
        "prt_orphan",
        "msg_gone",
        "ses_x",
        1_700_000_004_000,
        step(9, 9, 0, 0, 0),
    );
    // 挂在 user 消息下。
    conn.execute(
        "INSERT INTO message VALUES ('msg_user', 'ses_x', 1, 1, ?1)",
        [json!({"role": "user", "time": {"created": 1}}).to_string()],
    )
    .unwrap();
    part(
        &conn,
        "prt_user",
        "msg_user",
        "ses_x",
        1_700_000_005_000,
        step(9, 9, 0, 0, 0),
    );
    // token 是小数 / 负数 / 整个缺失。
    assistant(
        &conn,
        "msg_bad",
        "ses_x",
        1_700_000_006_000,
        "example-model",
    );
    let mut odd = step(9, 9, 0, 0, 0);
    odd["tokens"]["input"] = json!(1.5);
    part(
        &conn,
        "prt_float",
        "msg_bad",
        "ses_x",
        1_700_000_007_000,
        odd,
    );
    let mut neg = step(9, 9, 0, 0, 0);
    neg["tokens"]["output"] = json!(-1);
    part(&conn, "prt_neg", "msg_bad", "ses_x", 1_700_000_008_000, neg);
    part(
        &conn,
        "prt_none",
        "msg_bad",
        "ses_x",
        1_700_000_009_000,
        json!({"type": "step-finish"}),
    );
    // 缺推理字段按 0（OpenCode 缺明细时本来就存 0），照计。
    let mut no_reasoning = step(5, 5, 0, 0, 0);
    no_reasoning["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("reasoning");
    part(
        &conn,
        "prt_noreason",
        "msg_bad",
        "ses_x",
        1_700_000_010_000,
        no_reasoning,
    );

    let (recs, warnings) = project(&root, &id);
    assert_eq!(recs.len(), 3, "两条样本 + 缺推理那条");
    assert_eq!(warnings.len(), 5, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|w| !w.contains("prt_") && !w.contains("msg_")),
        "警告里不带源 id"
    );
}

#[test]
fn incremental_collect_sees_wal_growth() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());
    let (root, conn) = opencode_root(dir.path());
    sample(&conn);
    let provider = OpenCodeProvider::new(root.clone());
    let mut store = UsageStore::in_memory(&id).unwrap();

    let first = collect_source(&mut store, &id, &provider).unwrap();
    assert_eq!(
        (first.files, first.failed(), first.stats.inserted),
        (1, 0, 2)
    );
    let again = collect_source(&mut store, &id, &provider).unwrap();
    assert_eq!(again.skipped, 1, "没变就跳过");

    // 新请求只进了 -wal，主文件大小和修改时间都没变。
    let db_meta = std::fs::metadata(root.join("opencode.db")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assistant(
        &conn,
        "msg_c",
        "ses_fixture_a",
        1_700_000_002_000,
        "example-model",
    );
    part(
        &conn,
        "prt_c",
        "msg_c",
        "ses_fixture_a",
        1_700_000_003_000,
        step(1, 2, 0, 3, 0),
    );
    let db_after = std::fs::metadata(root.join("opencode.db")).unwrap();
    assert_eq!(
        (db_meta.len(), db_meta.modified().unwrap()),
        (db_after.len(), db_after.modified().unwrap())
    );

    let third = collect_source(&mut store, &id, &provider).unwrap();
    assert_eq!(
        (third.skipped, third.stats.inserted, third.stats.unchanged),
        (0, 1, 2)
    );
}

#[test]
fn parse_file_refuses_and_raw_is_not_archived() {
    // 只接用量：误进本地归档流程时要报错，而不是产出空会话；整库也不拷进 raw 层。
    let dir = tempfile::tempdir().unwrap();
    let (root, _conn) = opencode_root(dir.path());
    let provider = OpenCodeProvider::new(root.clone());
    let db = root.join("opencode.db");
    assert!(provider.parse_file(&db).is_err());
    assert!(!provider.archive_raw(&db));
    assert_eq!(provider.accepts(&db), Some(true));
    assert_eq!(provider.accepts(&root.join("opencode.db-wal")), Some(false));
}

/// 真实数据核对（`REAL_OPENCODE=<opencode.db> cargo test -p agenthome-core --test usage_projection_opencode -- --ignored --nocapture`）。
/// 直接读活库（验证 WAL 与不写源目录），再用 OpenCode 自己维护的 `session.tokens_*` 汇总和 assistant 消息数对账；
/// `opencode stats` 的数字就是从这两处来的。
#[test]
#[ignore]
fn real_data_matches_opencode_own_totals() {
    let db = PathBuf::from(std::env::var("REAL_OPENCODE").expect("REAL_OPENCODE=<opencode.db>"));
    let root = db.parent().unwrap().to_path_buf();
    let dir = tempfile::tempdir().unwrap();
    let id = identity(dir.path());

    let sidecars = |db: &Path| {
        ["-wal", "-shm"].map(|s| {
            let mut p = db.as_os_str().to_owned();
            p.push(s);
            std::fs::read(PathBuf::from(p))
                .ok()
                .map(|b| (b.len(), sha(&b)))
        })
    };
    let before = sidecars(&db);
    let provider = OpenCodeProvider::new(root);
    let file = provider.usage_events(&db).unwrap().unwrap();
    let after = sidecars(&db);
    let (recs, warnings) = records(&id, &provider.id(), &file);
    let raw = recs.len();
    let recs = dedup(recs);
    eprintln!("WAL 读前 / 读后（大小, 摘要）：{before:?} / {after:?}");
    eprintln!(
        "完成步骤 {raw}，去重后 {}，警告 {}（{:?}）",
        recs.len(),
        warnings.len() + file.warnings.len(),
        file.warnings
    );

    let mut by_model: BTreeMap<String, [u64; 6]> = BTreeMap::new();
    for r in &recs {
        let e = by_model
            .entry(r.model.clone().unwrap_or_else(|| "未知".into()))
            .or_default();
        for (slot, v) in e.iter_mut().zip([
            u64::from(r.requests),
            r.input_tokens,
            r.output_tokens,
            r.reasoning_tokens,
            r.cache_read_tokens,
            r.cache_write_5m_tokens + r.cache_write_1h_tokens,
        ]) {
            *slot += v;
        }
    }
    eprintln!("AgentHome 投影：模型 | 请求 | 新鲜输入 | 输出(含推理) | 推理 | 缓存读 | 缓存写");
    for (m, v) in &by_model {
        eprintln!("  {m} | {v:?}");
    }
    let sum = |i: usize| by_model.values().map(|v| v[i]).sum::<u64>();
    let mine = [sum(1), sum(2), sum(4), sum(5)];

    // OpenCode 自己的口径：stats 的总 token 读 session 汇总（输出 = output + reasoning），
    // 按模型的 Messages 数 assistant 行。用同一时刻的只读连接读，避免两次读之间又写入。
    // 与适配器同样的打开方式：没有 -wal / -shm 时用 immutable，否则 readonly_shm 打不开还会建出 -wal。
    let mode = if before.iter().any(Option::is_some) {
        "readonly_shm=1"
    } else {
        "immutable=1"
    };
    let conn = Connection::open_with_flags(
        format!("file:{}?mode=ro&{mode}", db.display()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let own: [i64; 4] = conn
        .query_row(
            "SELECT sum(tokens_input), sum(tokens_output) + sum(tokens_reasoning), sum(tokens_cache_read), sum(tokens_cache_write) FROM session",
            [],
            |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]),
        )
        .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(data,'$.modelID'), count(*) FROM message
             WHERE json_extract(data,'$.role')='assistant' GROUP BY 1 ORDER BY 1",
        )
        .unwrap();
    let messages: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    eprintln!("OpenCode session 汇总：新鲜输入 / 输出(含推理) / 缓存读 / 缓存写 = {own:?}");
    eprintln!("AgentHome 投影合计：                                          {mine:?}");
    eprintln!("OpenCode assistant 消息数（stats 的 Messages）：{messages:?}");
    assert_eq!(
        mine.map(|v| v as i64),
        own,
        "token 合计要与 OpenCode 自己的汇总一致"
    );
    assert_eq!(before, after, "读取不能改 -wal / -shm，也不能新建");
}

fn sha(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b)
        .iter()
        .take(6)
        .map(|x| format!("{x:02x}"))
        .collect()
}
