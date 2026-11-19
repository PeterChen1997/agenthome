//! 匿名使用统计（Umami）与崩溃 / 错误上报（Sentry）。设置页「匿名使用统计与崩溃报告」可关（默认开），
//! 关掉后立即生效：Umami 不再发，Sentry 在 `before_send` 里丢弃。
//!
//! 只发枚举 / 数字 / 布尔：页面名、版本号、结果（ok / failed）。不发会话内容、路径、项目名、token 用量。
//! 错误上报前把用户主目录替换成 `~`，不带主机名、不带用户信息。
//!
//! Sentry DSN 在构建时从环境变量 `SENTRY_DSN_AGENTHOME` 注入（`script/release-macos.sh` 从
//! `~/.config/sentry-dsns.env` 读），开源仓库里没有；没注入时 Sentry 不启用。调试构建默认都不发，
//! 设 `AGENTHOME_TELEMETRY_DEV=1` 才发（environment = development），用于核对。
//!
//! 非真实使用的运行（`--mock`、`--capture`、`--menubar-popup`、`--db`、`AGENTHOME_TELEMETRY_TEST`）
//! 算测试运行：Sentry 照发但 environment = test，不混进 production；Umami 完全不发。

use std::borrow::Cow;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sentry::protocol::DebugImage;
use serde_json::{Value, json};

use crate::config;

const UMAMI_ENDPOINT: &str = "https://a.hweb.peterchen97.cn/api/send";
/// Umami 后台的 `AgentHome` website。
const UMAMI_WEBSITE: &str = "5bc30eaf-ce3e-4e80-a6d5-86eb219da3a7";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 当前是否允许上报（设置页开关）。启动时从偏好读，开关切换时更新。
static ENABLED: AtomicBool = AtomicBool::new(true);
/// 测试运行（截图、演示数据、临时库、自检 panic）：Umami 不发。
static TEST_RUN: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 开关打开且不是（未显式允许的）调试构建。
fn active() -> bool {
    ENABLED.load(Ordering::Relaxed)
        && (!cfg!(debug_assertions) || std::env::var_os("AGENTHOME_TELEMETRY_DEV").is_some())
}

/// Sentry environment：调试构建 development；正式包里的测试运行 test；其余才是 production。
fn environment(debug_build: bool, test_run: bool) -> &'static str {
    if debug_build {
        "development"
    } else if test_run {
        "test"
    } else {
        "production"
    }
}

/// 启动时调用一次：读开关偏好、初始化 Sentry。返回的 guard 要活到进程结束（drop 时把队列发完）。
/// `test_run`：这次不是真实使用（见模块说明），上报不能算进 production。
pub fn init(test_run: bool) -> Option<sentry::ClientInitGuard> {
    set_enabled(config::telemetry_enabled());
    TEST_RUN.store(test_run, Ordering::Relaxed);
    let dsn = option_env!("SENTRY_DSN_AGENTHOME").filter(|d| !d.is_empty())?;
    let mut options = sentry::ClientOptions::default();
    options.release = Some(format!("agenthome@{VERSION}").into());
    options.environment = Some(environment(cfg!(debug_assertions), test_run).into());
    options.send_default_pii = false;
    // capture_message 默认不带堆栈；带上当前线程的堆栈，report_error 的事件才能定位到调用处。
    options.attach_stacktrace = true;
    // 不用 contexts 默认的主机名（常带真名）。
    options.server_name = Some("agenthome".into());
    options.before_send = Some(Arc::new(|event| active().then(|| scrub(event))));
    Some(sentry::init((dsn, options)))
}

/// 上报一个被处理掉的错误：`context` 是固定的英文标题（决定分组），`error` 是详情（会去掉主目录）。
pub fn report_error(context: &'static str, error: &str) {
    sentry::with_scope(
        |scope| scope.set_extra("error", Value::String(error.to_string())),
        || sentry::capture_message(context, sentry::Level::Error),
    );
}

/// 首页花费里出现了定价表没有的模型：每个模型名每次运行只报一次。
/// 有意不计价的（没有公开 API 单价，见 `cost::has_no_public_price`）不报。模型名不含个人信息。
#[expect(dead_code, reason = "CARM-283 首页花费卡接入后删掉")]
pub fn report_unpriced_models(summary: &agenthome_core::cost::CostSummary) {
    static REPORTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut reported) = REPORTED.lock() else {
        return;
    };
    for model in summary.unpriced_models() {
        if agenthome_core::cost::has_no_public_price(&model.model)
            || reported.contains(&model.model)
        {
            continue;
        }
        reported.push(model.model.clone());
        report_error("Model missing from pricing table", &model.model);
    }
}

/// 页面浏览（Umami pageview）。`page` 是固定的页面名。
pub fn page_view(page: &'static str) {
    send(json!({ "url": format!("/{page}"), "title": page }));
}

/// 自定义事件。`data` 只能放枚举 / 数字 / 布尔。
pub fn event(name: &'static str, data: Value) {
    send(json!({ "url": "/", "name": name, "data": data }));
}

fn send(mut payload: Value) {
    if !active() || TEST_RUN.load(Ordering::Relaxed) {
        return;
    }
    if let Value::Object(map) = &mut payload {
        map.insert("website".into(), UMAMI_WEBSITE.into());
        map.insert("hostname".into(), "agenthome.app".into());
        map.insert("language".into(), rust_i18n::locale().to_string().into());
    }
    let body = json!({ "type": "event", "payload": payload }).to_string();
    // 和检查更新一样用系统 curl，不多引 HTTP 依赖。Umami 会丢弃像爬虫的 User-Agent（如 curl/x），
    // 所以带一个浏览器样式的 UA，里面写上版本。发完不等结果，失败不重试。
    let spawned = Command::new("/usr/bin/curl")
        .args(["-sS", "-m", "10", "-o", "/dev/null", "-X", "POST"])
        .args(["-H", "Content-Type: application/json"])
        .args([
            "-A",
            &format!("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AgentHome/{VERSION}"),
        ])
        .args(["--data-binary", &body, UMAMI_ENDPOINT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // 在后台线程等它退出，不留僵尸进程。
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
    }
}

/// Claude 把项目目录压成一段（`~/.claude/projects/-Users-<用户名>-<项目路径>/`），
/// 主目录替换不到它；以这些前缀开头的路径段整段换成 `<project>`。
const DASHED_PREFIXES: [&str; 2] = ["-Users-", "-home-"];

/// 文本里的主目录换成 `~`，Claude 项目目录名（含用户名与项目名）换成 `<project>`。
fn scrub_text(text: &str, home: Option<&str>) -> String {
    let text = match home.filter(|h| h.len() > 1) {
        Some(home) => text.replace(home, "~"),
        None => text.to_string(),
    };
    // 自定义主目录（不在 /Users、/home 下）的压缩形式：非字母数字都换成 `-`。
    let dashed_home: Option<String> = home.filter(|h| h.len() > 1).map(|h| {
        h.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    });
    let is_boundary = |c: char| c == '/' || c.is_whitespace() || "\"'`(),:;=[]{}<>".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    let mut at_boundary = true;
    while let Some(c) = rest.chars().next() {
        let dashed = at_boundary
            && (DASHED_PREFIXES.iter().any(|p| rest.starts_with(p))
                || dashed_home.as_deref().is_some_and(|d| rest.starts_with(d)));
        if dashed {
            let end = rest.find(is_boundary).unwrap_or(rest.len());
            out.push_str("<project>");
            rest = &rest[end..];
            at_boundary = false;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
        at_boundary = is_boundary(c);
    }
    out
}

/// 去掉事件里的用户主目录、Claude 项目目录名与主机名。
fn scrub(mut event: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    let home = dirs::home_dir().map(|h| h.display().to_string());
    let clean = |text: &mut String| *text = scrub_text(text, home.as_deref());
    let clean_stack = |stacktrace: &mut sentry::protocol::Stacktrace| {
        for frame in stacktrace.frames.iter_mut() {
            if let Some(path) = frame.abs_path.as_mut() {
                clean(path);
            }
            if let Some(path) = frame.filename.as_mut() {
                clean(path);
            }
        }
    };
    event.server_name = Some(Cow::Borrowed("agenthome"));
    event.user = None;
    if let Some(message) = event.message.as_mut() {
        clean(message);
    }
    for value in event.extra.values_mut() {
        if let Value::String(text) = value {
            clean(text);
        }
    }
    for exception in event.exception.values.iter_mut() {
        if let Some(value) = exception.value.as_mut() {
            clean(value);
        }
        if let Some(stacktrace) = exception.stacktrace.as_mut() {
            clean_stack(stacktrace);
        }
    }
    // attach_stacktrace 给 report_error 的事件挂的是线程堆栈。
    if let Some(stacktrace) = event.stacktrace.as_mut() {
        clean_stack(stacktrace);
    }
    for thread in event.threads.values.iter_mut() {
        if let Some(stacktrace) = thread.stacktrace.as_mut() {
            clean_stack(stacktrace);
        }
    }
    // 调试镜像里是可执行文件的完整路径（app 不在 /Applications 时带用户名）。
    for image in event.debug_meta.to_mut().images.iter_mut() {
        match image {
            DebugImage::Symbolic(image) => clean(&mut image.name),
            DebugImage::Apple(image) => clean(&mut image.name),
            _ => {}
        }
    }
    for breadcrumb in event.breadcrumbs.values.iter_mut() {
        if let Some(message) = breadcrumb.message.as_mut() {
            clean(message);
        }
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误详情常带索引库 / 会话文件的绝对路径，里面是用户名；上报前必须换成 `~`，主机名也不能带出去。
    #[test]
    fn scrub_removes_home_and_hostname() {
        let home = dirs::home_dir().unwrap().display().to_string();
        let mut event = sentry::protocol::Event {
            message: Some(format!("open {home}/AgentHome/index/agenthome.db failed")),
            server_name: Some(Cow::Owned("Peters-MacBook".into())),
            ..Default::default()
        };
        event.extra.insert(
            "error".into(),
            Value::String(format!("{home}/.claude/projects/x.jsonl")),
        );
        let event = scrub(event);
        let message = event.message.unwrap();
        assert!(!message.contains(&home), "{message}");
        assert!(message.contains("~/AgentHome/index"));
        assert_eq!(
            event.extra["error"],
            Value::String("~/.claude/projects/x.jsonl".into())
        );
        assert_eq!(event.server_name.as_deref(), Some("agenthome"));
    }

    /// Claude 会话原件在 `~/.claude/projects/-Users-<用户名>-<项目路径>/` 下，入库失败的报错常带这个目录名；
    /// 主目录替换管不到这种压缩形式，用户名和项目名（可能是客户 / 未公开项目）不能随报错发出去。
    #[test]
    fn scrub_removes_dashed_claude_project_dir() {
        let home = "/Users/alice";
        let text = "parse /Users/alice/.claude/projects/-Users-alice-code-hub-secret-client/abc.jsonl: bad line";
        let cleaned = scrub_text(text, Some(home));
        assert_eq!(
            cleaned,
            "parse ~/.claude/projects/<project>/abc.jsonl: bad line"
        );
        assert!(!cleaned.contains("alice") && !cleaned.contains("secret-client"));
        // 已被换成 `~` 的路径、别的用户的目录、不在路径段开头的连字符都要处理对。
        assert_eq!(
            scrub_text("\"-Users-bob-proj\" and ~/x-Users-y", Some(home)),
            "\"<project>\" and ~/x-Users-y"
        );
        // 自定义主目录（不在 /Users 下）也按压缩形式识别。
        assert_eq!(
            scrub_text(
                "dir -opt-homes-carol-work/s.jsonl",
                Some("/opt/homes/carol")
            ),
            "dir <project>/s.jsonl"
        );
    }

    /// 测试运行（截图、演示数据、自检 panic）的上报不能混进 production 的崩溃率与报错列表。
    #[test]
    fn test_runs_never_report_as_production() {
        assert_eq!(environment(false, true), "test");
        assert_eq!(environment(false, false), "production");
        assert_eq!(environment(true, true), "development");
        assert_eq!(environment(true, false), "development");
    }
}
