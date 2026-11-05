//! Claude Code statusline JSON（docs/research/04-usage-apis.md §1.5）。
//!
//! 字段：`rate_limits.{five_hour,seven_day,seven_day_opus}.{used_percentage, resets_at(秒)}`、
//! `rate_limits.spend_limit.{used_percentage, resets_at, used_usd, limit_usd}`。
//! 只有 claude.ai 订阅且会话已有一次 API 响应后才有 `rate_limits`。

use crate::model::{ProviderId, QuotaSnapshot, QuotaWindow};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// 读 statusline 快照文件。文件不存在返回 `Ok(None)`；JSON 坏了报错。
/// 默认路径 `~/AgentHome/signals/claude-statusline.json`。
pub fn claude_statusline(path: Option<&Path>) -> Result<Option<QuotaSnapshot>> {
    let default = crate::paths::claude_statusline_path();
    let path = path.unwrap_or(&default);
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).with_context(|| format!("读取 {}", path.display()))?;
    let v: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} 不是合法 JSON", path.display()))?;
    let mtime_ms = crate::providers::mtime_ms(&meta);
    Ok(Some(parse_statusline(
        &v,
        mtime_ms,
        chrono::Utc::now().timestamp_millis(),
        &path.display().to_string(),
    )))
}

/// 纯解析，便于测试。`now_ms` 用于判断新鲜度。
pub fn parse_statusline(v: &Value, mtime_ms: i64, now_ms: i64, source: &str) -> QuotaSnapshot {
    let mut windows = Vec::new();
    if let Some(rl) = v.get("rate_limits").and_then(Value::as_object) {
        for (key, label, minutes) in [
            ("five_hour", "5 小时", Some(300)),
            ("seven_day", "7 天", Some(10080)),
            ("seven_day_opus", "7 天 (Opus)", Some(10080)),
            ("spend_limit", "消费上限", None),
        ] {
            let Some(w) = rl.get(key) else { continue };
            let Some(used) = w.get("used_percentage").and_then(Value::as_f64) else {
                continue;
            };
            windows.push(QuotaWindow {
                id: key.to_string(),
                label: label.to_string(),
                used_percent: used as f32,
                resets_at_ms: w.get("resets_at").and_then(Value::as_i64).map(|s| s * 1000),
                window_minutes: minutes,
            });
        }
    }
    QuotaSnapshot {
        provider: ProviderId::ClaudeCode,
        plan: None,
        windows,
        credits: None,
        fetched_at_ms: mtime_ms,
        source: source.to_string(),
        // 没有 rate_limits（API key 用户 / 会话首次响应前）也算一次快照，只是窗口为空。
        freshness: super::freshness(mtime_ms, now_ms, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Freshness;
    use serde_json::json;

    #[test]
    fn 解析两窗口与新鲜度() {
        let v = json!({
            "session_id": "x",
            "model": {"id": "claude-opus-5-5"},
            "rate_limits": {
                "five_hour": {"used_percentage": 12.5, "resets_at": 1_800_000_000},
                "seven_day": {"used_percentage": 40, "resets_at": 1_800_500_000},
                "seven_day_opus": {"used_percentage": 3}
            }
        });
        let now = 1_000_000_000_000;
        let s = parse_statusline(&v, now - 60_000, now, "f");
        assert_eq!(s.provider, ProviderId::ClaudeCode);
        assert_eq!(s.freshness, Freshness::Live);
        assert_eq!(s.windows.len(), 3);
        let five = s.windows.iter().find(|w| w.id == "five_hour").unwrap();
        assert_eq!(five.used_percent, 12.5);
        assert_eq!(five.resets_at_ms, Some(1_800_000_000_000));
        assert_eq!(five.window_minutes, Some(300));
        let opus = s.windows.iter().find(|w| w.id == "seven_day_opus").unwrap();
        assert_eq!(opus.resets_at_ms, None);

        let stale = parse_statusline(&v, now - 11 * 60_000, now, "f");
        assert_eq!(stale.freshness, Freshness::Stale);
    }

    #[test]
    fn 无_rate_limits_时窗口为空() {
        let v = json!({"session_id": "x", "cost": {"total_cost_usd": 1.0}});
        let s = parse_statusline(&v, 0, 0, "f");
        assert!(s.windows.is_empty());
        assert_eq!(s.freshness, Freshness::Live);
    }

    #[test]
    fn 文件不存在返回_none() {
        let p = std::env::temp_dir().join("agenthome-no-such-statusline.json");
        assert!(claude_statusline(Some(&p)).unwrap().is_none());
    }
}
