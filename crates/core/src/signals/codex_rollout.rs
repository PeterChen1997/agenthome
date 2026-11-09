//! Codex 额度快照：最新 rollout 里最后一条 `event_msg.token_count.payload.rate_limits`
//! （docs/research/04-usage-apis.md §2.3）。无需网络与鉴权。

use crate::model::{Credits, ProviderId, QuotaSnapshot, QuotaWindow};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 从新到旧最多看这么多个 rollout（新开的会话可能还没有 token_count）。
const MAX_FILES: usize = 10;
/// 候选列表多久重新遍历一次目录兜底（入库线程会随时补充最新路径，这只防止漏掉）。
const REWALK: Duration = Duration::from_secs(600);

/// 每个来源根目录「最近写过的 rollout」候选：(mtime, 路径)，按 mtime 倒序，最多 [`MAX_FILES`] 个。
struct Recent {
    root: PathBuf,
    files: Vec<(i64, PathBuf)>,
    walked_at: Option<Instant>,
}

/// app 每 5 秒读一次额度，原来每次都遍历 `~/.codex/sessions`（几百个文件）并整读最新的 rollout。
/// 现在：候选路径由入库线程在处理 Codex 原件时登记（[`note_rollout`]），目录只在没有候选或
/// 超过 [`REWALK`] 时遍历；每个候选只从上次读到的位置往后读。
static RECENT: Mutex<Vec<Recent>> = Mutex::new(Vec::new());
static TAILS: Mutex<Option<HashMap<PathBuf, Tail>>> = Mutex::new(None);

/// 某个 rollout 上次读到哪、最后一条 rate_limits 是什么。
struct Tail {
    size: u64,
    mtime_ms: i64,
    /// 已读完整行的末尾字节偏移（`.zst` 不续读）。
    offset: u64,
    last: Option<Value>,
}

/// 入库线程处理（或跳过）一个 Codex 原件时登记它，额度读取据此找最新的 rollout，不必遍历目录。
pub fn note_rollout(root: &Path, path: &Path, mtime_ms: i64) {
    let Ok(mut recent) = RECENT.lock() else {
        return;
    };
    let idx = match recent.iter().position(|r| r.root == root) {
        Some(i) => i,
        None => {
            recent.push(Recent {
                root: root.to_path_buf(),
                files: Vec::new(),
                walked_at: None,
            });
            recent.len() - 1
        }
    };
    let entry = &mut recent[idx];
    entry.files.retain(|(_, p)| p != path);
    entry.files.push((mtime_ms, path.to_path_buf()));
    entry
        .files
        .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    entry.files.truncate(MAX_FILES);
}

/// 默认根目录 `~/.codex/sessions`。目录不存在或没有任何 rate_limits 时返回 `Ok(None)`。
pub fn codex_rollout_quota(root: Option<&Path>) -> Result<Option<QuotaSnapshot>> {
    let default = crate::paths::codex_sessions_dir();
    let root = root.unwrap_or(&default);
    if !root.is_dir() {
        return Ok(None);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    for (mtime_ms, path) in candidates(root) {
        if let Some(rl) = last_rate_limits(&path)? {
            return Ok(Some(parse_rate_limits(
                &rl,
                mtime_ms,
                now_ms,
                &path.display().to_string(),
            )));
        }
    }
    Ok(None)
}

/// 最近写过的 rollout，按 mtime 倒序。登记过且不太旧就用登记的（只 stat 这几个），否则遍历目录。
fn candidates(root: &Path) -> Vec<(i64, PathBuf)> {
    let known: Option<Vec<PathBuf>> = RECENT.lock().ok().and_then(|recent| {
        recent
            .iter()
            .find(|r| r.root == root)
            .filter(|r| !r.files.is_empty() && r.walked_at.is_none_or(|t| t.elapsed() < REWALK))
            .map(|r| r.files.iter().map(|(_, p)| p.clone()).collect())
    });
    let mut files: Vec<(i64, PathBuf)> = match known {
        Some(paths) => paths
            .into_iter()
            .filter_map(|p| {
                let m = std::fs::metadata(&p).ok()?;
                Some((crate::providers::mtime_ms(&m), p))
            })
            .collect(),
        None => Vec::new(),
    };
    if files.is_empty() {
        files = walk(root);
        if let Ok(mut recent) = RECENT.lock() {
            recent.retain(|r| r.root != root);
            recent.push(Recent {
                root: root.to_path_buf(),
                files: files.iter().take(MAX_FILES).cloned().collect(),
                walked_at: Some(Instant::now()),
            });
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.truncate(MAX_FILES);
    files
}

/// 遍历根目录下全部 rollout。
fn walk(root: &Path) -> Vec<(i64, PathBuf)> {
    // 文件名带时间戳，按名倒序即按开始时间倒序；再按 mtime 挑真正最新写入的。
    let mut files: Vec<(i64, PathBuf)> = walkdir::WalkDir::new(root)
        .min_depth(4)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let n = e.file_name().to_string_lossy();
            n.starts_with("rollout-") && (n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"))
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((crate::providers::mtime_ms(&m), e.into_path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files
}

/// 一个 rollout 里最后一条非空 `rate_limits`。没变就用上次的结果；变长了只读新增的完整行。
fn last_rate_limits(path: &Path) -> Result<Option<Value>> {
    let meta = std::fs::metadata(path).with_context(|| format!("读取元数据 {}", path.display()))?;
    let (size, mtime_ms) = (meta.len(), crate::providers::mtime_ms(&meta));
    let compressed = path.extension().is_some_and(|e| e == "zst");
    let prev = TAILS.lock().ok().and_then(|t| {
        t.as_ref()?
            .get(path)
            .map(|t| (t.size, t.mtime_ms, t.offset, t.last.clone()))
    });
    let (from, mut last) = match prev {
        Some((s, m, _, last)) if s == size && m == mtime_ms => return Ok(last),
        Some((s, _, offset, last)) if !compressed && size > s => (offset, last),
        _ => (0, None),
    };
    let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut offset = from;
    if compressed {
        let mut reader = BufReader::new(zstd::Decoder::new(file)?);
        scan_lines(&mut reader, &mut last, &mut offset)?;
    } else {
        let mut file = file;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::new(file.take(size - from));
        scan_lines(&mut reader, &mut last, &mut offset)?;
    }
    if let Ok(mut tails) = TAILS.lock() {
        tails.get_or_insert_with(HashMap::new).insert(
            path.to_path_buf(),
            Tail {
                size,
                mtime_ms,
                offset,
                last: last.clone(),
            },
        );
    }
    Ok(last)
}

/// 逐行找 `event_msg` 的 `rate_limits`，更新 `last`；`offset` 推进到最后一个完整行之后。坏行跳过（这是信号，不是归档）。
fn scan_lines(reader: &mut dyn BufRead, last: &mut Option<Value>, offset: &mut u64) -> Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(n) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if n == 0 || line.last() != Some(&b'\n') {
            // 写了一半的末行留到下次。
            if n > 0 {
                consider(&line, last);
            }
            break;
        }
        *offset += n as u64;
        consider(&line, last);
    }
    Ok(())
}

fn consider(line: &[u8], last: &mut Option<Value>) {
    // 粗筛，避免对每行做完整 JSON 解析。
    if !line.windows(11).any(|w| w == b"rate_limits") {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line.trim_ascii_end()) else {
        return;
    };
    if v.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if let Some(rl) = v.pointer("/payload/rate_limits")
        && !rl.is_null()
    {
        *last = Some(rl.clone());
    }
}

/// 纯解析，便于测试。
pub fn parse_rate_limits(rl: &Value, mtime_ms: i64, now_ms: i64, source: &str) -> QuotaSnapshot {
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(w) = rl.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let minutes = w
            .get("window_minutes")
            .and_then(Value::as_u64)
            .map(|m| m as u32);
        windows.push(QuotaWindow {
            id: key.to_string(),
            label: super::window_label(minutes),
            used_percent: used as f32,
            resets_at_ms: w.get("resets_at").and_then(Value::as_i64).map(|s| s * 1000),
            window_minutes: minutes,
        });
    }
    let credits = rl.get("credits").filter(|v| !v.is_null()).map(|c| Credits {
        balance: c.get("balance").and_then(Value::as_str).map(str::to_string),
        unlimited: c.get("unlimited").and_then(Value::as_bool).unwrap_or(false),
        has_credits: c
            .get("has_credits")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reset_credits: c
            .get("reset_credits")
            .and_then(Value::as_u64)
            .map(|n| n as u32),
        expires_at_ms: c
            .get("expires_at")
            .and_then(Value::as_i64)
            .map(|s| s * 1000),
    });
    QuotaSnapshot {
        provider: ProviderId::Codex,
        plan: rl
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        credits,
        fetched_at_ms: mtime_ms,
        source: source.to_string(),
        freshness: super::freshness(mtime_ms, now_ms, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Freshness;
    use serde_json::json;

    #[test]
    fn 解析_primary_与_credits() {
        let rl = json!({
            "limit_id": "codex",
            "primary": {"used_percent": 2.0, "window_minutes": 10080, "resets_at": 1_794_392_689},
            "secondary": null,
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            "plan_type": "prolite"
        });
        let now = 1_000_000_000_000;
        let s = parse_rate_limits(&rl, now - 3_600_000, now, "f");
        assert_eq!(s.provider, ProviderId::Codex);
        assert_eq!(s.plan.as_deref(), Some("prolite"));
        assert_eq!(s.windows.len(), 1);
        assert_eq!(s.windows[0].label, "7 天");
        assert_eq!(s.windows[0].resets_at_ms, Some(1_794_392_689_000));
        assert_eq!(s.credits.as_ref().unwrap().balance.as_deref(), Some("0"));
        assert_eq!(s.freshness, Freshness::Cached);
        assert_eq!(
            parse_rate_limits(&rl, now - 2 * 86_400_000, now, "f").freshness,
            Freshness::Stale
        );
    }

    #[test]
    fn 从_rollout_文件取最后一条() {
        let dir =
            std::env::temp_dir().join(format!("agenthome-codex-quota-{}", std::process::id()));
        let day = dir.join("2026").join("11").join("05");
        std::fs::create_dir_all(&day).unwrap();
        let p = day.join("rollout-2026-11-05T10-00-00-00000000-0000-7000-8000-000000000001.jsonl");
        let lines = [
            json!({"timestamp":"2026-11-05T10:00:00.000Z","type":"session_meta","payload":{"id":"s1"}}),
            json!({"timestamp":"2026-11-05T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":null,
                "rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":100}}}}),
            json!({"timestamp":"2026-11-05T10:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":null,
                "rate_limits":{"primary":{"used_percent":5.5,"window_minutes":300,"resets_at":200},"plan_type":"plus"}}}),
        ];
        let body: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        std::fs::write(&p, body.join("\n")).unwrap();
        let s = codex_rollout_quota(Some(&dir)).unwrap().expect("应有快照");
        assert_eq!(s.windows[0].used_percent, 5.5);
        assert_eq!(s.windows[0].label, "5 小时");
        assert_eq!(s.plan.as_deref(), Some("plus"));
        assert_eq!(s.freshness, Freshness::Live);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 入库线程登记过的 rollout 直接用，不遍历目录（这里故意放在遍历找不到的层级）；
    /// 文件变长后只读新增的行，拿到最新一条 rate_limits。
    #[test]
    fn 登记的_rollout_不遍历目录且续读新增行() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let odd = root.join("not-a-date");
        std::fs::create_dir_all(&odd).unwrap();
        let p = odd.join("rollout-2026-11-05T10-00-00-00000000-0000-7000-8000-000000000009.jsonl");
        let rl = |pct: f64| {
            json!({"timestamp":"2026-11-05T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":null,
                "rate_limits":{"primary":{"used_percent":pct,"window_minutes":300,"resets_at":100}}}})
            .to_string()
                + "\n"
        };
        std::fs::write(&p, rl(3.0)).unwrap();
        assert!(
            codex_rollout_quota(Some(&root)).unwrap().is_none(),
            "没登记时遍历目录，按规则找不到这个层级的文件"
        );
        note_rollout(&root, &p, 1);
        let s = codex_rollout_quota(Some(&root))
            .unwrap()
            .expect("登记后应找到");
        assert_eq!(s.windows[0].used_percent, 3.0);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, rl(7.5).as_bytes()).unwrap();
        drop(f);
        let s = codex_rollout_quota(Some(&root)).unwrap().unwrap();
        assert_eq!(s.windows[0].used_percent, 7.5);
    }

    #[test]
    fn 目录不存在返回_none() {
        let p = std::env::temp_dir().join("agenthome-no-such-codex-root");
        assert!(codex_rollout_quota(Some(&p)).unwrap().is_none());
    }
}
