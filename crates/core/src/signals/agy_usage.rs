//! Antigravity CLI（`agy`）的订阅额度：跑官方 `agy -p /usage --output-format json` 读它自己的额度。
//!
//! 与 Claude 的 `/usage` 同一思路（CLAUDE.md 已拍板决策 3）：由官方程序用它自己的登录去查，
//! AgentHome 不读任何凭据。实测（2026-11-10，agy 1.5.3）这个斜杠命令在打印模式下直接本地处理：
//! 不建对话（`conversation_id` 为空）、不耗 token（usage 全 0），约 12 秒。
//!
//! 输出 `command.data.groups[]`：每组模型共享一个周额度和一个 5 小时额度（`buckets[]`：`id`、
//! `window` = `weekly` / `5h`、`remaining_fraction`、`reset_time`）；Pro 套餐的 Gemini 4 Argon 另有单独周额度。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::model::{Freshness, ProviderId, QuotaSnapshot, QuotaWindow};

/// `agy` 一次 `/usage` 的上限；正常约 12 秒，首次启动可能要做迁移。
const TIMEOUT: Duration = Duration::from_secs(90);

/// 快照文件 `~/AgentHome/signals/agy-usage.json`。
pub fn agy_usage_path() -> PathBuf {
    crate::paths::signals_dir().join("agy-usage.json")
}

/// 找官方 `agy`。从 Finder 启动的 app 没有用户 shell 的 PATH，所以先查常见安装位置。
pub fn find_agy_binary() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/bin/agy"),
        PathBuf::from("/opt/homebrew/bin/agy"),
        PathBuf::from("/usr/local/bin/agy"),
    ];
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("agy"))
            .find(|p| p.is_file())
    })
}

/// 读上次的快照；文件不存在返回 None。新鲜度按采集时刻重算。
pub fn agy_usage_snapshot(path: Option<&Path>) -> Result<Option<QuotaSnapshot>> {
    let default = agy_usage_path();
    let path = path.unwrap_or(&default);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    let mut snapshot: QuotaSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} 不是合法的额度快照", path.display()))?;
    snapshot.freshness = super::freshness(
        snapshot.fetched_at_ms,
        chrono::Utc::now().timestamp_millis(),
        true,
    );
    Ok(Some(snapshot))
}

/// 跑一次 `agy -p /usage` 并写快照。`cwd` 是 `agy` 的工作目录（只影响它记录「上次对话」，/usage 不建对话）。
pub fn refresh_agy_usage(agy: &Path, cwd: &Path, out: &Path) -> Result<QuotaSnapshot> {
    let mut child = Command::new(agy)
        .args(["-p", "/usage", "--output-format", "json"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("启动 {}", agy.display()))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("agy /usage 超过 {} 秒没有结束", TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).with_context(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!(
            "agy /usage 的输出不是 JSON（可能未登录，先在终端运行一次 agy）：{}",
            stderr.trim().lines().last().unwrap_or("")
        )
    })?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let windows = parse_agy_usage(&json)?;
    let snapshot = QuotaSnapshot {
        provider: ProviderId::AntigravityCli,
        plan: None,
        windows,
        credits: None,
        fetched_at_ms: now_ms,
        source: format!("{} -p /usage", agy.display()),
        freshness: Freshness::Live,
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
    std::fs::rename(&tmp, out)?;
    Ok(snapshot)
}

/// `agy -p /usage --output-format json` 的输出 → 额度窗口。顺序：Gemini 组、Argon、Claude / GPT 组，组内 5 小时在前。
pub fn parse_agy_usage(json: &Value) -> Result<Vec<QuotaWindow>> {
    let status = json.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "SUCCESS" {
        let error = json.get("error").and_then(Value::as_str).unwrap_or("");
        bail!("agy /usage 没有成功（status = {status:?}）{error}");
    }
    // 斜杠命令不该建对话；建了说明它把 /usage 当成了普通提问，会耗额度，必须停下。
    if json
        .get("conversation_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        bail!("agy 把 /usage 当成了一次对话（新版行为变了？），已停止，请反馈");
    }
    let groups = json
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .context("agy /usage 的输出里没有 command.data.groups（格式变了？）")?;
    let mut windows = Vec::new();
    for group in groups {
        let name = group.get("name").and_then(Value::as_str).unwrap_or("");
        // 主力的 Gemini 组不带后缀，其余组在窗口名后标出来，与 Claude 的「7 天 (Fable)」同样写法。
        let suffix = match name {
            "Gemini Models" => String::new(),
            "Claude and GPT models" => " (Claude/GPT)".to_string(),
            "Gemini 4 Argon" => " (Argon)".to_string(),
            other => format!(" ({other})"),
        };
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = bucket.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(remaining) = bucket.get("remaining_fraction").and_then(Value::as_f64) else {
                continue;
            };
            let (label, minutes) = match bucket.get("window").and_then(Value::as_str) {
                Some("5h") => ("5 小时", Some(300)),
                Some("weekly") => ("7 天", Some(10_080)),
                _ => ("窗口", None),
            };
            windows.push(QuotaWindow {
                id: id.to_string(),
                label: format!("{label}{suffix}"),
                used_percent: ((1.0 - remaining.clamp(0.0, 1.0)) * 100.0) as f32,
                resets_at_ms: bucket
                    .get("reset_time")
                    .and_then(Value::as_str)
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp_millis()),
                window_minutes: minutes,
            });
        }
    }
    if windows.is_empty() {
        bail!("agy /usage 没有返回任何额度窗口（可能未登录）");
    }
    let rank = |w: &QuotaWindow| {
        let group = if w.label.contains("(Argon)") {
            1
        } else if w.label.contains('(') {
            2
        } else {
            0
        };
        (group, w.window_minutes.unwrap_or(u32::MAX))
    };
    windows.sort_by_key(rank);
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-11-10 本机真实输出（agy 1.5.3，Google AI Pro）去掉说明文字。
    fn sample() -> Value {
        json!({"conversation_id": "", "status": "SUCCESS", "num_turns": 0,
        "usage": {"input_tokens": 0, "output_tokens": 0},
        "command": {"name": "usage", "data": {"groups": [
          {"name": "Gemini Models", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.9994890689849854, "reset_time": "2026-11-14T00:00:11Z"},
            {"id": "gemini-5h", "window": "5h", "remaining_fraction": 1, "reset_time": "2026-11-10T18:35:27Z"}]},
          {"name": "Claude and GPT models", "buckets": [
            {"id": "3p-weekly", "window": "weekly", "remaining_fraction": 1, "reset_time": "2026-11-17T13:35:27Z"},
            {"id": "3p-5h", "window": "5h", "remaining_fraction": 1, "reset_time": "2026-11-10T18:35:27Z"}]},
          {"name": "Gemini 4 Argon", "buckets": [
            {"id": "gemini-argon-wtu-weekly", "window": "weekly", "remaining_fraction": 0.9898666739463806, "reset_time": "2026-11-17T13:03:01Z"}]}
        ]}}})
    }

    #[test]
    fn 剩余比例换成已用百分比_窗口长度与重置时刻齐全() {
        // 窗口长度与重置时刻是使用压力的输入，缺一个徽标就不显示。
        let w = parse_agy_usage(&sample()).unwrap();
        let ids: Vec<&str> = w.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-5h",
                "gemini-weekly",
                "gemini-argon-wtu-weekly",
                "3p-5h",
                "3p-weekly"
            ]
        );
        let argon = &w[2];
        assert_eq!(argon.label, "7 天 (Argon)");
        assert!(
            (argon.used_percent - 1.0133).abs() < 0.01,
            "{}",
            argon.used_percent
        );
        assert_eq!(argon.window_minutes, Some(10_080));
        assert_eq!(argon.resets_at_ms, Some(1_794_920_581_000));
        assert_eq!(w[0].label, "5 小时");
        assert_eq!(w[0].used_percent, 0.0);
        assert_eq!(w[3].label, "5 小时 (Claude/GPT)");
    }

    #[test]
    fn 被当成对话或失败时报错而不是写空快照() {
        let mut talked = sample();
        talked["conversation_id"] = json!("abc");
        assert!(parse_agy_usage(&talked).is_err());
        let failed = json!({"conversation_id": "", "status": "ERROR", "error": "authentication failed or timed out"});
        let e = parse_agy_usage(&failed).unwrap_err().to_string();
        assert!(e.contains("authentication failed"), "{e}");
        let mut empty = sample();
        empty["command"]["data"]["groups"] = json!([]);
        assert!(parse_agy_usage(&empty).is_err());
    }
}
