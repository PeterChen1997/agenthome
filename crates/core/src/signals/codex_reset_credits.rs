//! ChatGPT / Codex 的「重置额度」（可兑换的一次性额度重置，带过期时间）。
//!
//! 本机文件里没有这份数据（rollout 快照只有 credits 余额）。只有 OpenAI 后台知道，
//! 所以由**用户手动触发**：AgentHome 启动官方 `codex app-server`，用 JSON-RPC 发一次只读的
//! `account/rateLimits/read`，由官方程序自己用已登录的账号去查。AgentHome 不读任何凭据，
//! 也绝不调用兑换（consume）接口。
//!
//! 结果写到 `~/AgentHome/signals/codex-reset-credits.json`，读取时并进 Codex 的额度快照。

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Credits, QuotaSnapshot};

/// 等官方 CLI 启动并查完。
const TIMEOUT: Duration = Duration::from_secs(40);

/// 一张未用的重置额度。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResetCredit {
    pub title: Option<String>,
    /// 过期时刻（Unix 毫秒）；None 表示不过期。
    pub expires_at_ms: Option<i64>,
}

/// 一次查询结果。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResetCreditsSnapshot {
    /// 后台给的可用张数（明细可能被截断，以它为准）。
    pub available: u32,
    /// 可用的明细，按过期时间先后。
    pub credits: Vec<ResetCredit>,
    pub fetched_at_ms: i64,
    /// 查询所用的 codex 可执行文件。
    pub source: String,
}

impl ResetCreditsSnapshot {
    /// 现在仍未过期的明细。
    pub fn live_credits(&self, now_ms: i64) -> Vec<&ResetCredit> {
        self.credits
            .iter()
            .filter(|c| c.expires_at_ms.is_none_or(|at| at > now_ms))
            .collect()
    }

    /// 现在还剩几张：后台张数减去查询后已过期的明细。
    pub fn live_count(&self, now_ms: i64) -> u32 {
        let expired = self.credits.len() - self.live_credits(now_ms).len();
        self.available
            .saturating_sub(u32::try_from(expired).unwrap_or(u32::MAX))
    }
}

/// `~/AgentHome/signals/codex-reset-credits.json`
pub fn codex_reset_credits_path() -> PathBuf {
    crate::paths::signals_dir().join("codex-reset-credits.json")
}

/// 可用来查询的官方 codex：先用 ChatGPT 桌面版自带的（通常更新），再用独立安装的 CLI。
/// 从 Finder 启动的 app 没有用户 shell 的 PATH，所以先查固定位置。
pub fn codex_cli_candidates() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(
        "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    )];
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".local/bin/codex"));
    }
    out.push(PathBuf::from("/opt/homebrew/bin/codex"));
    out.push(PathBuf::from("/usr/local/bin/codex"));
    if let Some(path) = std::env::var_os("PATH") {
        out.extend(std::env::split_paths(&path).map(|dir| dir.join("codex")));
    }
    let mut seen = std::collections::HashSet::new();
    out.into_iter()
        .filter(|p| p.is_file() && seen.insert(p.clone()))
        .collect()
}

/// 读上次查询的结果；没查过返回 None。
pub fn load_reset_credits(path: Option<&Path>) -> Result<Option<ResetCreditsSnapshot>> {
    let default = codex_reset_credits_path();
    let path = path.unwrap_or(&default);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("{} 不是合法的重置额度快照", path.display()))
}

/// 查询并落盘。依次尝试各个官方 codex，旧版不支持这个查询时换下一个。
pub fn refresh_reset_credits(out: Option<&Path>) -> Result<ResetCreditsSnapshot> {
    let candidates = codex_cli_candidates();
    if candidates.is_empty() {
        bail!("没找到官方 codex（ChatGPT 桌面版自带的或独立安装的 Codex CLI）");
    }
    let mut errors = Vec::new();
    for binary in &candidates {
        match query(binary) {
            Ok(result) => {
                let snapshot = parse_reset_credits(&result, now_ms(), &binary.to_string_lossy())?;
                let default = codex_reset_credits_path();
                let out = out.unwrap_or(&default);
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = out.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
                std::fs::rename(&tmp, out)?;
                return Ok(snapshot);
            }
            Err(error) => errors.push(format!("{}：{error:#}", binary.display())),
        }
    }
    bail!("查询重置额度失败：{}", errors.join("；"))
}

fn now_ms() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// 启动 `codex app-server`，握手后发一次 `account/rateLimits/read`，返回其 result。
fn query(binary: &Path) -> Result<Value> {
    let mut child = Command::new(binary)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动 {}", binary.display()))?;
    let result = (|| {
        let mut stdin = child.stdin.take().context("拿不到 stdin")?;
        let stdout = child.stdout.take().context("拿不到 stdout")?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut send = |message: Value| -> Result<()> {
            writeln!(stdin, "{message}")?;
            stdin.flush()?;
            Ok(())
        };
        send(json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {
                "name": "agenthome",
                "title": "AgentHome",
                "version": env!("CARGO_PKG_VERSION"),
            }},
        }))?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx
                .recv_timeout(left)
                .map_err(|_| anyhow!("等了 {} 秒没有结果", TIMEOUT.as_secs()))?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match message.get("id").and_then(Value::as_i64) {
                Some(1) => {
                    if let Some(error) = message.get("error") {
                        bail!("握手失败：{error}");
                    }
                    send(json!({"method": "initialized"}))?;
                    send(json!({"id": 2, "method": "account/rateLimits/read"}))?;
                }
                Some(2) => {
                    if let Some(error) = message.get("error") {
                        bail!("{}", error_text(error));
                    }
                    return message.get("result").cloned().context("返回里没有 result");
                }
                _ => {}
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn error_text(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

/// 纯解析 `account/rateLimits/read` 的 result，便于测试。
/// 旧版 codex 不带 `rateLimitResetCredits` 字段，视为不支持（报错，让调用方换下一个）。
pub fn parse_reset_credits(
    result: &Value,
    fetched_at_ms: i64,
    source: &str,
) -> Result<ResetCreditsSnapshot> {
    let summary = match result.get("rateLimitResetCredits") {
        None => bail!("这个版本的 codex 不支持查询重置额度，请更新"),
        Some(Value::Null) => {
            return Ok(ResetCreditsSnapshot {
                available: 0,
                credits: Vec::new(),
                fetched_at_ms,
                source: source.to_string(),
            });
        }
        Some(summary) => summary,
    };
    let available = summary
        .get("availableCount")
        .and_then(Value::as_u64)
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    let mut credits: Vec<ResetCredit> = summary
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("status").and_then(Value::as_str) == Some("available"))
        .map(|c| ResetCredit {
            title: c.get("title").and_then(Value::as_str).map(str::to_string),
            expires_at_ms: c.get("expiresAt").and_then(Value::as_i64).map(|s| s * 1000),
        })
        .collect();
    credits.sort_by_key(|c| c.expires_at_ms.unwrap_or(i64::MAX));
    Ok(ResetCreditsSnapshot {
        available,
        credits,
        fetched_at_ms,
        source: source.to_string(),
    })
}

/// 把查到的重置额度并进 Codex 额度快照：张数、最早过期时刻、每张的过期时刻。
pub fn apply_reset_credits(
    snapshot: &mut QuotaSnapshot,
    reset: &ResetCreditsSnapshot,
    now_ms: i64,
) {
    let live = reset.live_credits(now_ms);
    let expiries: Vec<i64> = live.iter().filter_map(|c| c.expires_at_ms).collect();
    let credits = snapshot.credits.get_or_insert(Credits {
        balance: None,
        unlimited: false,
        has_credits: false,
        reset_credits: None,
        expires_at_ms: None,
        reset_credit_expiries_ms: Vec::new(),
    });
    credits.reset_credits = Some(reset.live_count(now_ms));
    credits.expires_at_ms = expiries.first().copied();
    credits.reset_credit_expiries_ms = expiries;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机 2026-11-10 用 codex 0.166 实际查到的形状（id 换成占位）。
    fn sample() -> Value {
        json!({
            "rateLimitResetCredits": {
                "availableCount": 2,
                "credits": [
                    {"id": "b", "resetType": "codexRateLimits", "status": "available",
                     "grantedAt": 1792000000, "expiresAt": 1796162164,
                     "title": "Full reset", "description": null},
                    {"id": "a", "resetType": "codexRateLimits", "status": "available",
                     "grantedAt": 1792000000, "expiresAt": 1796068996,
                     "title": "Full reset", "description": null},
                    {"id": "c", "resetType": "codexRateLimits", "status": "redeemed",
                     "grantedAt": 1791000000, "expiresAt": 1795000000,
                     "title": "Full reset", "description": null}
                ]
            }
        })
    }

    #[test]
    fn keeps_only_available_credits_sorted_by_expiry() {
        let s = parse_reset_credits(&sample(), 0, "codex").unwrap();
        assert_eq!(s.available, 2);
        let expiries: Vec<_> = s.credits.iter().map(|c| c.expires_at_ms).collect();
        // 已兑换的那张不算；先过期的排前面（12-01 早于 12-02）。
        assert_eq!(expiries, [Some(1_796_068_996_000), Some(1_796_162_164_000)]);
    }

    #[test]
    fn old_codex_without_the_field_is_unsupported() {
        let error = parse_reset_credits(&json!({"rateLimits": {}}), 0, "codex").unwrap_err();
        assert!(error.to_string().contains("不支持"));
    }

    #[test]
    fn null_summary_means_none_available() {
        let s = parse_reset_credits(&json!({"rateLimitResetCredits": null}), 0, "codex").unwrap();
        assert_eq!(s.available, 0);
        assert!(s.credits.is_empty());
    }

    #[test]
    fn credits_that_expire_after_the_query_stop_counting() {
        let s = parse_reset_credits(&sample(), 0, "codex").unwrap();
        // 第一张 12-01 过期之后：只剩一张，最早过期变成 12-02。
        let after_first = 1_796_068_996_000 + 1;
        assert_eq!(s.live_count(after_first), 1);
        let mut snapshot = QuotaSnapshot {
            provider: crate::model::ProviderId::Codex,
            plan: None,
            windows: Vec::new(),
            credits: None,
            fetched_at_ms: 0,
            source: String::new(),
            freshness: crate::model::Freshness::Live,
        };
        apply_reset_credits(&mut snapshot, &s, after_first);
        let credits = snapshot.credits.unwrap();
        assert_eq!(credits.reset_credits, Some(1));
        assert_eq!(credits.expires_at_ms, Some(1_796_162_164_000));
        assert_eq!(credits.reset_credit_expiries_ms, [1_796_162_164_000]);
    }
}
