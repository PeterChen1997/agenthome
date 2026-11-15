//! 匿名使用统计（Umami）与崩溃 / 错误上报（Sentry）。设置页「匿名使用统计与崩溃报告」可关（默认开），
//! 关掉后立即生效：Umami 不再发，Sentry 在 `before_send` 里丢弃。
//!
//! 只发枚举 / 数字 / 布尔：页面名、版本号、结果（ok / failed）。不发会话内容、路径、项目名、token 用量。
//! 错误上报前把用户主目录替换成 `~`，不带主机名、不带用户信息。
//!
//! Sentry DSN 在构建时从环境变量 `SENTRY_DSN_AGENTHOME` 注入（`script/release-macos.sh` 从
//! `~/.config/sentry-dsns.env` 读），开源仓库里没有；没注入时 Sentry 不启用。调试构建默认都不发，
//! 设 `AGENTHOME_TELEMETRY_DEV=1` 才发（environment = development），用于核对。

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

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 开关打开且不是（未显式允许的）调试构建。
fn active() -> bool {
    ENABLED.load(Ordering::Relaxed)
        && (!cfg!(debug_assertions) || std::env::var_os("AGENTHOME_TELEMETRY_DEV").is_some())
}

/// 启动时调用一次：读开关偏好、初始化 Sentry。返回的 guard 要活到进程结束（drop 时把队列发完）。
pub fn init() -> Option<sentry::ClientInitGuard> {
    set_enabled(config::telemetry_enabled());
    let dsn = option_env!("SENTRY_DSN_AGENTHOME").filter(|d| !d.is_empty())?;
    let environment = if cfg!(debug_assertions) {
        "development"
    } else {
        "production"
    };
    let mut options = sentry::ClientOptions::default();
    options.release = Some(format!("agenthome@{VERSION}").into());
    options.environment = Some(environment.into());
    options.send_default_pii = false;
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

/// 页面浏览（Umami pageview）。`page` 是固定的页面名。
pub fn page_view(page: &'static str) {
    send(json!({ "url": format!("/{page}"), "title": page }));
}

/// 自定义事件。`data` 只能放枚举 / 数字 / 布尔。
pub fn event(name: &'static str, data: Value) {
    send(json!({ "url": "/", "name": name, "data": data }));
}

fn send(mut payload: Value) {
    if !active() {
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

/// 去掉事件里的用户主目录与主机名。
fn scrub(mut event: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    let home = dirs::home_dir().map(|h| h.display().to_string());
    let clean = |text: &mut String| {
        if let Some(home) = home.as_deref().filter(|h| h.len() > 1) {
            *text = text.replace(home, "~");
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
            for frame in stacktrace.frames.iter_mut() {
                if let Some(path) = frame.abs_path.as_mut() {
                    clean(path);
                }
                if let Some(path) = frame.filename.as_mut() {
                    clean(path);
                }
            }
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
}
