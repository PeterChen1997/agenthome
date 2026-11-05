//! Codex 额度快照：最新 rollout 里最后一条 `event_msg.token_count.payload.rate_limits`
//! （docs/research/04-usage-apis.md §2.3）。无需网络与鉴权。

use crate::model::{Credits, ProviderId, QuotaSnapshot, QuotaWindow};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 从新到旧最多看这么多个 rollout（新开的会话可能还没有 token_count）。
const MAX_FILES: usize = 10;

/// 默认根目录 `~/.codex/sessions`。目录不存在或没有任何 rate_limits 时返回 `Ok(None)`。
pub fn codex_rollout_quota(root: Option<&Path>) -> Result<Option<QuotaSnapshot>> {
    let default = crate::paths::codex_sessions_dir();
    let root = root.unwrap_or(&default);
    if !root.is_dir() {
        return Ok(None);
    }
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    for (mtime_ms, path) in files.into_iter().take(MAX_FILES) {
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

/// 扫一个 rollout，返回最后一条非空 `rate_limits`。坏行跳过（这是信号，不是归档）。
fn last_rate_limits(path: &Path) -> Result<Option<Value>> {
    let file = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "zst") {
        Box::new(BufReader::new(zstd::Decoder::new(file)?))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut last = None;
    for line in reader.split(b'\n') {
        let Ok(line) = line else { break };
        // 粗筛，避免对每行做完整 JSON 解析。
        if !line.windows(11).any(|w| w == b"rate_limits") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        if let Some(rl) = v.pointer("/payload/rate_limits")
            && !rl.is_null()
        {
            last = Some(rl.clone());
        }
    }
    Ok(last)
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

    #[test]
    fn 目录不存在返回_none() {
        let p = std::env::temp_dir().join("agenthome-no-such-codex-root");
        assert!(codex_rollout_quota(Some(&p)).unwrap().is_none());
    }
}
