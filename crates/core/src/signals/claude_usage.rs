//! Claude 订阅额度的第二个合规来源：在伪终端里启动**官方** `claude`，执行 `/usage`，
//! 读出屏幕上的各窗口百分比与重置时间（调研 docs/research/04-usage-apis.md 的 A7）。
//!
//! 为什么需要它：statusline 只由终端交互界面在「有过一次 API 响应」后调用，只用 Claude 桌面版
//! 的人永远拿不到快照；而 `/usage` 由官方程序自己去查，AgentHome 不碰任何凭据、不发对话、不耗 token。
//!
//! 结果写到 `~/AgentHome/signals/claude-usage.json`（序列化的 `QuotaSnapshot`），读取时按
//! `fetched_at_ms` 重算新鲜度。屏幕格式（2.1.x）：
//!
//! ```text
//! Current week (all models)
//! ██████░░░░░░░░░░░░░░░░░░   18% used
//! Resets Nov 14 at 2am (Asia/Shanghai)
//! ```

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Datelike as _, Local, NaiveDate, NaiveTime, TimeZone as _};

use crate::model::{Freshness, ProviderId, QuotaSnapshot, QuotaWindow};

/// 伪终端尺寸：够高，`/usage` 面板不用滚动就能完整显示。
const ROWS: u16 = 80;
const COLS: u16 = 120;
/// 等 CLI 启动到可输入。
const READY_TIMEOUT: Duration = Duration::from_secs(25);
/// 等 `/usage` 的额度部分加载完（它是异步拉取的，比面板本身晚出现）。
const USAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// 画面多久不变算「渲染完」。
const SETTLE: Duration = Duration::from_millis(1500);

/// `~/AgentHome/signals/claude-usage.json`
pub fn claude_usage_path() -> PathBuf {
    crate::paths::signals_dir().join("claude-usage.json")
}

/// 找官方 `claude` 可执行文件。从 Finder 启动的 app 没有用户 shell 的 PATH，所以先查常见安装位置。
pub fn find_claude_binary() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("claude"))
            .find(|p| p.is_file())
    })
}

/// 读上次 `/usage` 的快照；文件不存在返回 None。新鲜度按采集时刻重算。
pub fn claude_usage_snapshot(path: Option<&Path>) -> Result<Option<QuotaSnapshot>> {
    let default = claude_usage_path();
    let path = path.unwrap_or(&default);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    let mut snapshot: QuotaSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} 不是合法的额度快照", path.display()))?;
    // 周窗口变化慢，24 小时内都算「缓存可用」。
    snapshot.freshness = super::freshness(
        snapshot.fetched_at_ms,
        chrono::Utc::now().timestamp_millis(),
        true,
    );
    Ok(Some(snapshot))
}

/// 跑一次 `/usage` 并写快照。`cwd` 是 CLI 的工作目录（需已被 Claude Code 信任，否则报错退出）。
pub fn refresh_claude_usage(claude: &Path, cwd: &Path, out: &Path) -> Result<QuotaSnapshot> {
    let started = std::time::SystemTime::now();
    let screen = capture_usage_screen(claude, cwd)?;
    let now = Local::now();
    let windows = parse_usage_screen(&screen, now);
    if windows.is_empty() {
        bail!(
            "/usage 没有显示订阅额度（可能未登录 Claude 订阅账号）。屏幕末尾：{}",
            tail(&screen, 6)
        );
    }
    let snapshot = QuotaSnapshot {
        provider: ProviderId::ClaudeCode,
        plan: claude_plan_name(),
        windows,
        credits: None,
        fetched_at_ms: now.timestamp_millis(),
        source: format!("{} /usage", claude.display()),
        freshness: Freshness::Live,
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
    std::fs::rename(&tmp, out)?;
    if let Some(left) = transcript_left_behind(cwd, started) {
        bail!(
            "额度已刷新，但官方 CLI 这次保存了一个会话文件 {}（不保存记录的标记可能已失效，请反馈）",
            left.display()
        );
    }
    Ok(snapshot)
}

/// 本次运行后，`cwd` 对应的 Claude 项目目录里是否出现了新的会话文件。
fn transcript_left_behind(cwd: &Path, since: std::time::SystemTime) -> Option<PathBuf> {
    let dir_name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = crate::paths::claude_projects_dir().join(dir_name);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|e| e == "jsonl")
                && std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since)
        })
}

/// 套餐名：只读 `~/.claude.json` 的 `oauthAccount.organizationType`（如 `claude_max`），
/// 这是账号资料而非凭据；读不到就不显示，不猜。
pub fn claude_plan_name() -> Option<String> {
    let path = dirs::home_dir()?.join(".claude.json");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    plan_label(
        value
            .get("oauthAccount")?
            .get("organizationType")?
            .as_str()?,
    )
}

fn plan_label(organization_type: &str) -> Option<String> {
    let name = organization_type.strip_prefix("claude_")?;
    let mut chars = name.chars();
    let first = chars.next()?.to_ascii_uppercase();
    Some(std::iter::once(first).chain(chars).collect())
}

/// 合并两路 Claude 快照（statusline 与 `/usage`）：有窗口的那份里取较新的为主；
/// 主快照没有的窗口（例如 `/usage` 才有的按模型周窗口），从另一份 1 小时内的快照补上。
pub fn merge_claude(
    statusline: Option<QuotaSnapshot>,
    usage: Option<QuotaSnapshot>,
) -> Option<QuotaSnapshot> {
    let has = |s: &Option<QuotaSnapshot>| s.as_ref().is_some_and(|s| !s.windows.is_empty());
    let (mut main, other) = match (has(&statusline), has(&usage)) {
        (true, true) => {
            let (a, b) = (statusline?, usage?);
            if a.fetched_at_ms >= b.fetched_at_ms {
                (a, Some(b))
            } else {
                (b, Some(a))
            }
        }
        (true, false) => (statusline?, None),
        (false, true) => (usage?, None),
        // 都没有窗口：保留 statusline 的空快照，UI 据此显示「已接入但还没数据」。
        (false, false) => return statusline.or(usage),
    };
    if main.plan.is_none() {
        main.plan = other.as_ref().and_then(|o| o.plan.clone());
    }
    if let Some(other) = other
        && main.fetched_at_ms - other.fetched_at_ms <= 3_600_000
    {
        for window in other.windows {
            if !main.windows.iter().any(|w| w.id == window.id) {
                main.windows.push(window);
            }
        }
    }
    Some(main)
}

/// 在伪终端里启动官方 CLI，执行 `/usage`，返回额度加载完后的屏幕文字。进程随后被结束。
pub fn capture_usage_screen(claude: &Path, cwd: &Path) -> Result<String> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("打开伪终端失败")?;
    let mut command = CommandBuilder::new(claude);
    // 只对这一次启动覆盖设置：官方 CLI 启动时会按 cleanupPeriodDays（默认 30 天）删除旧会话原件，
    // 后台每 15 分钟启动一次会让清理远比用户自己用 CLI 时频繁。不改用户的全局配置。
    command.args(["--settings", r#"{"cleanupPeriodDays": 36500}"#]);
    command.cwd(cwd);
    // 干净环境：不继承调用方的 ANTHROPIC_* / CLAUDE_CODE_*（例如从 Claude 会话里启动时），
    // 让 CLI 按用户自己的登录状态运行。
    command.env_clear();
    for key in ["HOME", "USER", "LOGNAME", "SHELL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let bin_dir = claude.parent().map(Path::to_path_buf).unwrap_or_default();
    command.env(
        "PATH",
        format!(
            "{}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            bin_dir.display()
        ),
    );
    command.env("LANG", "en_US.UTF-8");
    command.env("TERM", "xterm-256color");
    // 官方 CLI 的子会话标记：不把这次 /usage 存成会话记录，否则每次刷新都会在用户的
    // Claude 历史（与 AgentHome 归档）里多一个空会话。未写入文档，失效时由
    // `refresh_claude_usage` 的事后检查报出来。
    command.env("CLAUDE_CODE_CHILD_SESSION", "1");

    let mut child = pair
        .slave
        .spawn_command(command)
        .with_context(|| format!("启动 {} 失败", claude.display()))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let size = |n: u16| std::num::NonZeroU16::new(n).unwrap_or(std::num::NonZeroU16::MIN);
    let mut parser = vt100::Parser::new(size(ROWS), size(COLS), 0);
    let result = (|| -> Result<String> {
        // 1. 等到输入框出现。信任提示不自动确认：那是用户的决定。
        let screen = wait_for(&rx, &mut parser, READY_TIMEOUT, |s| {
            s.contains("trust this folder") || s.contains('❯')
        })?;
        if screen.contains("trust this folder") {
            bail!(
                "官方 CLI 要求先信任 {}：在终端里 cd 到该目录运行一次 claude 并选择信任",
                cwd.display()
            );
        }
        // 2. 输入 /usage；先等补全菜单出来再回车。
        writer.write_all(b"/usage")?;
        writer.flush()?;
        pump(&rx, &mut parser, Duration::from_millis(800));
        writer.write_all(b"\r")?;
        writer.flush()?;
        // 3. 等额度部分出现且画面稳定。
        wait_for(&rx, &mut parser, USAGE_TIMEOUT, |s| s.contains("% used"))
            .context("等待 /usage 显示额度超时")
    })();

    // 无论成功与否都结束 CLI：先 Esc 关面板、/exit，再兜底 kill。
    let _ = writer.write_all(b"\x1b");
    let _ = writer.write_all(b"/exit\r");
    let _ = writer.flush();
    pump(&rx, &mut parser, Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// 不断读输出喂给终端模拟器，直到 `done(屏幕)` 成立且画面稳定 [`SETTLE`]，或超时。
fn wait_for(
    rx: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
    timeout: Duration,
    done: impl Fn(&str) -> bool,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut last_change = Instant::now();
    let mut last = String::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => parser.process(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("claude 提前退出。屏幕末尾：{}", tail(&last, 6))
            }
        }
        let screen = parser.screen().contents();
        if screen != last {
            last = screen;
            last_change = Instant::now();
        }
        if done(&last) && last_change.elapsed() >= SETTLE {
            return Ok(last);
        }
        if Instant::now() >= deadline {
            bail!("超时。屏幕末尾：{}", tail(&last, 6));
        }
    }
}

fn pump(rx: &mpsc::Receiver<Vec<u8>>, parser: &mut vt100::Parser, wait: Duration) {
    let deadline = Instant::now() + wait;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(bytes) => parser.process(&bytes),
            Err(_) => break,
        }
    }
}

fn tail(screen: &str, lines: usize) -> String {
    let kept: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..].join(" / ")
}

/// 从 `/usage` 屏幕文字解析各额度窗口。纯函数，便于测试。
///
/// 规则：`Current …` 标题行开始一个窗口；其后第一处 `N% used` 是已用百分比；
/// 再其后第一行 `Resets …` 是重置时间（按本机时区解释，CLI 与 app 在同一台机器上）。
pub fn parse_usage_screen(screen: &str, now: DateTime<Local>) -> Vec<QuotaWindow> {
    let mut windows: Vec<QuotaWindow> = Vec::new();
    let mut current: Option<QuotaWindow> = None;
    for raw in screen.lines() {
        let line = raw.trim();
        if line.starts_with("Current ") {
            windows.extend(current.take().filter(|w| w.used_percent >= 0.0));
            let (id, label, minutes) = classify_title(line);
            current = Some(QuotaWindow {
                id,
                label,
                used_percent: -1.0,
                resets_at_ms: None,
                window_minutes: minutes,
            });
            continue;
        }
        let Some(window) = current.as_mut() else {
            continue;
        };
        if window.used_percent < 0.0 {
            if let Some(percent) = percent_used(line) {
                window.used_percent = percent;
            }
        } else if window.resets_at_ms.is_none()
            && let Some(text) = line.strip_prefix("Resets ")
        {
            window.resets_at_ms = parse_reset(text, now).map(|t| t.timestamp_millis());
        }
    }
    windows.extend(current.filter(|w| w.used_percent >= 0.0));
    windows
}

/// 标题 → (窗口 id, 中文标签, 窗口分钟数)。id 与 statusline 的命名对齐，方便两路来源互换。
fn classify_title(title: &str) -> (String, String, Option<u32>) {
    let rest = title.trim_start_matches("Current ").trim();
    let (kind, scope) = match rest.split_once('(') {
        Some((kind, scope)) => (kind.trim(), Some(scope.trim_end_matches(')').trim())),
        None => (rest, None),
    };
    match (kind, scope) {
        ("session", _) => ("five_hour".into(), "5 小时".into(), Some(300)),
        ("week", None) | ("week", Some("all models")) => {
            ("seven_day".into(), "7 天".into(), Some(10_080))
        }
        ("week", Some(model)) => (
            format!("seven_day_{}", slug(model)),
            format!("7 天 ({model})"),
            Some(10_080),
        ),
        _ => (format!("other_{}", slug(rest)), rest.to_string(), None),
    }
}

fn slug(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// 找 `18% used` / `18.5% used`。
fn percent_used(line: &str) -> Option<f32> {
    let end = line.find("% used")?;
    let digits: String = line[..end]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// 解析 `Nov 14 at 2am (Asia/Shanghai)`、`2:30pm (…)`、`Nov 14 (…)`。
/// 只有时刻时取今天，已过则明天；没有年份时取最近的未来日期（容忍 1 天的回退）。
fn parse_reset(text: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let text = match text.find(" (") {
        Some(ix) => &text[..ix],
        None => text,
    }
    .trim();
    let (date_part, time_part) = match text.split_once(" at ") {
        Some((d, t)) => (Some(d.trim()), Some(t.trim())),
        None if text.chars().next()?.is_ascii_digit() => (None, Some(text)),
        None => (Some(text), None),
    };
    let time = match time_part {
        Some(t) => parse_clock(t)?,
        None => NaiveTime::from_hms_opt(0, 0, 0)?,
    };
    let local = |date: NaiveDate| Local.from_local_datetime(&date.and_time(time)).earliest();
    match date_part {
        None => {
            let today = local(now.date_naive())?;
            if today >= now {
                Some(today)
            } else {
                local(now.date_naive().succ_opt()?)
            }
        }
        Some(date) => {
            let mut parts = date.split_whitespace();
            let month = month_number(parts.next()?)?;
            let day: u32 = parts.next()?.trim_end_matches(',').parse().ok()?;
            let year = parts
                .next()
                .and_then(|y| y.parse::<i32>().ok())
                .unwrap_or(now.year());
            let candidate = local(NaiveDate::from_ymd_opt(year, month, day)?)?;
            if candidate < now - chrono::Duration::days(1) && parts.next().is_none() {
                local(NaiveDate::from_ymd_opt(year + 1, month, day)?)
            } else {
                Some(candidate)
            }
        }
    }
}

/// `2am`、`2:30pm`、`14:00`。
fn parse_clock(text: &str) -> Option<NaiveTime> {
    let lower = text.to_ascii_lowercase();
    let (body, pm) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let (h, m) = match body.trim().split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.trim().parse::<u32>().ok()?, 0),
    };
    let hour = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    };
    NaiveTime::from_hms_opt(hour, m, 0)
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let key = name.get(..3)?.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == key).map(|i| i as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    /// 按 2026-11-08 用户截图里的 `/usage` 面板合成（数值与格式一致，不含其他内容）。
    const SCREEN: &str = "
 Settings  Status   Config   Usage   Stats

 Session

 Total cost:             $0.0000
 Usage:                  0 input, 0 output, 0 cache read, 0 cache write

 Current week (all models)
 █████████                                                      18% used
 Resets Nov 14 at 2am (Asia/Shanghai)

 Current week (Fable)
 ██████████████                                                 28% used
 Resets Nov 14 at 2am (Asia/Shanghai)

 What's contributing to your limits usage?
";

    #[test]
    fn weekly_windows_from_usage_panel() {
        // 只用桌面版的人就靠这条路拿 Claude 额度；两个周窗口都要出来，id 与 statusline 对齐。
        let now = at(2026, 11, 8, 14, 0);
        let windows = parse_usage_screen(SCREEN, now);
        assert_eq!(windows.len(), 2);
        let all = &windows[0];
        assert_eq!(all.id, "seven_day");
        assert_eq!(all.used_percent, 18.0);
        assert_eq!(all.window_minutes, Some(10_080));
        assert_eq!(
            all.resets_at_ms,
            Some(at(2026, 11, 14, 2, 0).timestamp_millis())
        );
        let fable = &windows[1];
        assert_eq!(fable.id, "seven_day_fable");
        assert_eq!(fable.label, "7 天 (Fable)");
        assert_eq!(fable.used_percent, 28.0);
    }

    #[test]
    fn session_window_maps_to_five_hour_with_time_only_reset() {
        // 5 小时窗口只给时刻：已过的时刻必须落到明天，否则倒计时会是负数。
        let screen = "Current session\n███ 41.5% used\nResets 1am (Asia/Shanghai)\n";
        let now = at(2026, 11, 8, 23, 30);
        let windows = parse_usage_screen(screen, now);
        assert_eq!(windows[0].id, "five_hour");
        assert_eq!(windows[0].used_percent, 41.5);
        assert_eq!(
            windows[0].resets_at_ms,
            Some(at(2026, 11, 9, 1, 0).timestamp_millis())
        );
    }

    #[test]
    fn year_rolls_over_for_january_reset_seen_in_december() {
        let now = at(2026, 12, 30, 10, 0);
        let reset = parse_reset("Jan 2 at 3:30pm (Asia/Shanghai)", now).unwrap();
        assert_eq!(reset, at(2027, 1, 2, 15, 30));
    }

    fn snap(at_ms: i64, ids: &[&str]) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: ProviderId::ClaudeCode,
            plan: None,
            windows: ids
                .iter()
                .map(|id| QuotaWindow {
                    id: (*id).into(),
                    label: (*id).into(),
                    used_percent: 10.0,
                    resets_at_ms: None,
                    window_minutes: None,
                })
                .collect(),
            credits: None,
            fetched_at_ms: at_ms,
            source: String::new(),
            freshness: Freshness::Live,
        }
    }

    #[test]
    fn merge_prefers_newer_and_fills_missing_windows() {
        // 终端里刚回复过（statusline 更新）时以它为准，但按模型的周窗口只有 /usage 有，不能丢。
        let statusline = snap(2_000_000, &["seven_day"]);
        let usage = snap(1_000_000, &["seven_day", "seven_day_fable"]);
        let merged = merge_claude(Some(statusline), Some(usage)).unwrap();
        assert_eq!(merged.fetched_at_ms, 2_000_000);
        let ids: Vec<&str> = merged.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["seven_day", "seven_day_fable"]);
    }

    #[test]
    fn empty_statusline_does_not_hide_usage_probe() {
        // 后台跑 /usage 时 CLI 启动也会触发 statusline，写下一份没有窗口的更新快照；它不能盖掉真数据。
        let statusline = snap(5_000_000, &[]);
        let usage = snap(1_000_000, &["seven_day"]);
        let merged = merge_claude(Some(statusline), Some(usage)).unwrap();
        assert_eq!(merged.windows.len(), 1);
        assert_eq!(merged.fetched_at_ms, 1_000_000);
    }

    #[test]
    fn plan_label_from_organization_type() {
        assert_eq!(plan_label("claude_max").as_deref(), Some("Max"));
        assert_eq!(plan_label("claude_pro").as_deref(), Some("Pro"));
        // 不认识的格式不猜。
        assert_eq!(plan_label("api"), None);
    }

    #[test]
    fn panel_without_quota_yields_nothing() {
        // API key 登录时只有会话花费，没有任何额度窗口：不能凭空造出 0%。
        let screen = " Session\n Total cost: $0.0000\n Usage: 0 input\n";
        assert!(parse_usage_screen(screen, at(2026, 11, 8, 12, 0)).is_empty());
    }
}
