//! 匿名使用统计（Umami）与崩溃 / 错误上报（Sentry）。设置页「匿名使用统计与崩溃报告」可关（默认开），
//! 关掉后立即生效：Umami 不再发，Sentry 在 `before_send` 里丢弃。
//!
//! 只发枚举 / 数字 / 布尔：页面名、版本号、结果（ok / failed）。不发会话内容、路径、项目名、token 用量。
//! 被处理掉的错误只发固定标题、代码里定义的步骤名和错误类别，错误原文（常带路径）不出设备。
//! 崩溃（panic）的文字在发出前把路径换成 `<path>`，不带主机名、不带用户信息。
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

use agenthome_core::diagnostics::ErrorDiagnostic;
use agenthome_core::model::{Period, ProviderId};
use agenthome_core::remote_ssh::ResponseDiagnostic;
use agenthome_core::usage_report::MachineFilter;
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
    Some(sentry::init((dsn, client_options(test_run))))
}

/// 上报用的客户端配置（不含 DSN）：release、environment、去个人信息。
fn client_options(test_run: bool) -> sentry::ClientOptions {
    let mut options = sentry::ClientOptions::default();
    options.release = Some(format!("agenthome@{VERSION}").into());
    options.environment = Some(environment(cfg!(debug_assertions), test_run).into());
    options.send_default_pii = false;
    // capture_message 默认不带堆栈；带上当前线程的堆栈，被处理掉的错误才能定位到调用处。
    options.attach_stacktrace = true;
    // 不用 contexts 默认的主机名（常带真名）。
    options.server_name = Some("agenthome".into());
    options.before_send = Some(Arc::new(|event| active().then(|| scrub(event))));
    options
}

/// 上报一个被处理掉的错误：`context` 是固定的英文标题（决定分组）。`error` 只取代码里定义的步骤名和
/// 错误类别（如 `io.NotFound`）；原文常带会话文件路径，Gemini CLI 的目录名就是项目名，所以不发。
pub fn report_error(context: &'static str, error: &anyhow::Error) {
    report_diagnostic(context, &ErrorDiagnostic::from_error(error));
}

fn report_diagnostic(context: &'static str, diagnostic: &ErrorDiagnostic) {
    sentry::with_scope(
        |scope| set_error_diagnostic(scope, diagnostic),
        || sentry::capture_message(context, sentry::Level::Error),
    );
}

/// 只有一段文字、拿不到错误类型的失败（如文件监听后端报错）：只发固定标题。
pub fn report_failure(context: &'static str) {
    sentry::capture_message(context, sentry::Level::Error);
}

/// 一次扫描里新出现的单文件失败：发个数和第一条的错误类别，不发文件路径和错误原文。
pub fn report_file_failures(context: &'static str, count: usize, first: &ErrorDiagnostic) {
    sentry::with_scope(
        |scope| {
            scope.set_extra("failed_files", json!(count));
            set_error_diagnostic(scope, first);
        },
        || sentry::capture_message(context, sentry::Level::Error),
    );
}

pub fn machine_filter_kind(machine: &MachineFilter) -> &'static str {
    match machine {
        MachineFilter::All => "all",
        MachineFilter::Local => "local",
        MachineFilter::Remote(_) => "remote",
    }
}

pub fn report_cost_failure(period: Period, machine_kind: &'static str, error: &anyhow::Error) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("cost.period", period.as_str());
            scope.set_tag("cost.machine", machine_kind);
            set_error_diagnostic(scope, &ErrorDiagnostic::from_error(error));
        },
        || sentry::capture_message("Cost summary query failed", sentry::Level::Error),
    );
}

/// 用量同步失败：附失败类别、本机错误诊断或拒收的 SSH 响应摘要。
pub fn report_usage_failure(
    stage: &'static str,
    kind: &'static str,
    diagnostic: Option<(&ErrorDiagnostic, Option<&ProviderId>)>,
    response: Option<ResponseDiagnostic>,
) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("remote.stage", stage);
            scope.set_tag("remote.kind", kind);
            if let Some(response) = response {
                scope.set_extra("ssh.response", json!(response));
            }
            if let Some((error, provider)) = diagnostic {
                set_error_diagnostic(scope, error);
                if let Some(provider) = provider {
                    let label = match provider {
                        ProviderId::Other(_) => "other",
                        provider => provider.as_str(),
                    };
                    scope.set_tag("source.provider", label);
                }
            }
        },
        || sentry::capture_message(&usage_failure_title(stage, kind), sentry::Level::Error),
    );
}

fn set_error_diagnostic(scope: &mut sentry::Scope, diagnostic: &ErrorDiagnostic) {
    scope.set_tag("code.operation", diagnostic.operation);
    scope.set_extra("error.chain", json!(diagnostic.chain));
}

fn usage_failure_title(stage: &str, kind: &str) -> String {
    format!("Usage sync failed: {stage}/{kind}")
}

/// 首页花费里出现了定价表没有的模型：每个模型名每次运行只报一次。
/// 有意不计价的（没有公开 API 单价，见 `cost::has_no_public_price`）不报。模型名不含个人信息。
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
        sentry::with_scope(
            |scope| scope.set_extra("model", Value::String(model.model.clone())),
            || sentry::capture_message("Model missing from pricing table", sentry::Level::Error),
        );
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

/// 自由文本（panic 文字、面包屑）里的绝对路径和 `~/` 路径整段换成 `<path>`：文件名、会话 id、
/// 项目目录名（Gemini CLI 的 `~/.gemini/tmp/<项目名>/`、外置盘上的 Claude 项目）都不出设备。
/// 路径到空白或引号 / 括号为止；路径前面紧挨着引号时到配对的引号为止（带空格的路径也整段去掉）。
/// 没加引号又带空格的路径，第一个空格之后的部分去不掉。URL 不动。
fn redact_paths(text: &str) -> String {
    let ends_path = |c: char| c.is_whitespace() || "\"'`(),;=[]{}<>".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let starts_path = (rest.starts_with('/') || rest.starts_with("~/"))
            && prev.is_none_or(|p| p == ':' || ends_path(p))
            && rest
                .trim_start_matches('~')
                .chars()
                .nth(1)
                .is_some_and(|next| !ends_path(next) && next != '/');
        if starts_path {
            let end = match prev.filter(|p| "\"'`".contains(*p)) {
                Some(quote) => rest.find(quote),
                None => rest.find(ends_path),
            }
            .unwrap_or(rest.len());
            // 「读 /a/b: 权限不足」里的冒号是句子的，不算路径。
            let end = rest[..end].trim_end_matches(':').len();
            out.push_str("<path>");
            rest = &rest[end..];
            prev = Some('>');
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
        prev = Some(c);
    }
    out
}

/// 去掉事件里的路径、用户主目录、Claude 项目目录名与主机名。
fn scrub(mut event: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    let home = dirs::home_dir().map(|h| h.display().to_string());
    // 堆栈帧和调试镜像里是源码 / 可执行文件的路径，符号化要用，只换主目录。
    let clean_location = |text: &mut String| *text = scrub_text(text, home.as_deref());
    let clean = |text: &mut String| *text = scrub_text(&redact_paths(text), home.as_deref());
    let clean_stack = |stacktrace: &mut sentry::protocol::Stacktrace| {
        for frame in stacktrace.frames.iter_mut() {
            if let Some(path) = frame.abs_path.as_mut() {
                clean_location(path);
            }
            if let Some(path) = frame.filename.as_mut() {
                clean_location(path);
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
    // attach_stacktrace 给被处理掉的错误挂的是线程堆栈。
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
            DebugImage::Symbolic(image) => clean_location(&mut image.name),
            DebugImage::Apple(image) => clean_location(&mut image.name),
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
    use agenthome_core::collector::{Collector, Limits};
    use agenthome_core::device::DeviceIdentity;
    use agenthome_core::diagnostics::ResultStep;
    use agenthome_core::usage_store::{FailureKind, UsageStore};

    fn emit_handled_failure_examples() -> [sentry::protocol::Uuid; 2] {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("device.json");
        let identity =
            DeviceIdentity::load_or_create(&identity_path, "synthetic-private-host").unwrap();
        let bad_db = dir.path().join("synthetic-private-db");
        std::fs::write(&bad_db, "synthetic-private-content").unwrap();
        let Err(error) = UsageStore::open_read_only(&bad_db, &identity).step("cost.read_models")
        else {
            panic!("malformed cost fixture must fail");
        };
        report_cost_failure(
            Period::Days(30),
            machine_filter_kind(&MachineFilter::Remote("synthetic-private-machine".into())),
            &error,
        );
        let cost_id = sentry::last_event_id().unwrap();

        let gemini = dir.path().join("gemini");
        let file = gemini.join("tmp/project/chats/session-2026-11-01T08-00-aaaaaaaa.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            concat!(
                r#"{"sessionId":"aaaaaaaa-1111-4222-8333-000000000001","startTime":"2026-11-01T08:00:00.000Z","kind":"main"}"#, "\n",
                r#"{"id":"r1","timestamp":"2026-11-01T08:00:01.000Z","type":"gemini","content":"synthetic-private-content","model":"gemini-3-flash","tokens":{"input":100,"output":10,"total":110}}"#, "\n",
            ),
        )
        .unwrap();
        let db = dir.path().join("usage/usage.db");
        let mut collector = Collector::open(
            &identity_path,
            &db,
            vec![Box::new(agenthome_core::GeminiProvider::new(gemini))],
            Limits::default(),
        )
        .unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_usage BEFORE INSERT ON usage
                 BEGIN SELECT RAISE(ABORT, 'synthetic-private-content'); END;",
            )
            .unwrap();
        let report = collector.run_round().unwrap();
        assert_eq!(report.failures[&FailureKind::Store], 1);
        assert_eq!(collector.store().high_water().unwrap(), 0);
        let (provider, diagnostic) = &report.failure_diagnostics[&FailureKind::Store];
        report_usage_failure("collect", "store", Some((diagnostic, Some(provider))), None);
        [cost_id, sentry::last_event_id().unwrap()]
    }

    #[test]
    fn handled_failures_include_steps_codes_and_safe_stacktraces() {
        let mut options = client_options(true);
        options.before_send = Some(Arc::new(|event| Some(scrub(event))));
        let events = sentry::test::with_captured_events_options(
            || {
                emit_handled_failure_examples();
            },
            sentry::apply_defaults(options),
        );
        assert_eq!(events.len(), 2);
        let cost = &events[0];
        assert_eq!(cost.tags["code.operation"], "usage.local.schema");
        assert_eq!(cost.tags["cost.machine"], "remote");
        assert_eq!(
            cost.extra["error.chain"],
            json!([
                "cost.read_models",
                "usage.local.schema",
                "sqlite.NotADatabase (26)"
            ])
        );

        let usage = &events[1];
        assert_eq!(usage.tags["code.operation"], "usage.local.upsert_record");
        assert_eq!(usage.tags["source.provider"], "gemini-cli");
        assert_eq!(
            usage.extra["error.chain"],
            json!([
                "usage.local.commit_file",
                "usage.local.upsert_record",
                "sqlite.ConstraintViolation (1811)"
            ])
        );

        for event in events {
            assert!(event.threads.values.iter().any(|thread| {
                thread
                    .stacktrace
                    .as_ref()
                    .is_some_and(|trace| !trace.frames.is_empty())
            }));
            let payload = serde_json::to_string(&event).unwrap();
            assert!(!payload.contains("synthetic-private") && !payload.contains("gemini-3-flash"));
            assert!(event.user.is_none());
        }
    }

    #[test]
    #[ignore = "Sends two synthetic events to Sentry in the test environment"]
    fn handled_failures_sentry_selftest() {
        let dsn = option_env!("SENTRY_DSN_AGENTHOME")
            .filter(|dsn| !dsn.is_empty())
            .expect("set SENTRY_DSN_AGENTHOME at build time");
        assert!(active(), "set AGENTHOME_TELEMETRY_DEV=1 for a debug build");
        let mut options = client_options(true);
        options.environment = Some("test".into());
        let client = sentry::init((dsn, options));
        let ids = sentry::with_scope(
            |scope| {
                scope.set_fingerprint(Some(&[
                    "agenthome-handled-errors-selftest",
                    "{{ default }}",
                ]));
                scope.set_tag("telemetry.selftest", "handled-errors");
            },
            emit_handled_failure_examples,
        );
        assert!(client.flush(Some(std::time::Duration::from_secs(8))));
        for id in ids {
            println!("Sentry test event: {}", id.simple());
        }
    }

    /// README 与 SECURITY.md 对外承诺上报里没有文件路径和项目名。被处理掉的错误原文常带会话文件路径：
    /// Gemini CLI 的目录名就是项目名，外置盘上的 Claude 项目目录名也认不出来，所以原文整段不发，
    /// 只留代码里定义的步骤名和错误类别。
    #[test]
    fn handled_errors_never_carry_paths_or_project_names() {
        let leaky = [
            "/Users/alice/.gemini/tmp/secret-client/chats/session-2026-11-01T08-00-aaaaaaaa.jsonl",
            "/Users/alice/.claude/projects/-Volumes-work-secret-client/abc.jsonl",
            "/Users/alice/.codex/sessions/2026/11/01/rollout-secret-client.jsonl",
        ];
        let mut options = client_options(false);
        options.before_send = Some(Arc::new(|event| Some(scrub(event))));
        let events = sentry::test::with_captured_events_options(
            || {
                for path in leaky {
                    let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
                    let error = anyhow::Error::new(io).context(format!("读 {path}"));
                    report_error("sync: open index failed", &error);
                    report_file_failures(
                        "sync: ingest failed",
                        3,
                        &ErrorDiagnostic::from_error(&error),
                    );
                }
                report_failure("sync: watcher error");
            },
            options,
        );
        assert_eq!(events.len(), leaky.len() * 2 + 1);
        for event in &events {
            let payload = serde_json::to_string(event).unwrap();
            for word in ["secret-client", "alice", ".gemini", ".jsonl", "session-"] {
                assert!(!payload.contains(word), "{word} 出现在上报里：{payload}");
            }
        }
        // 路径去掉后仍要能看出是哪一类失败。
        let first = &events[0];
        assert_eq!(first.message.as_deref(), Some("sync: open index failed"));
        assert_eq!(first.extra["error.chain"], json!(["io.PermissionDenied"]));
        assert_eq!(events[1].extra["failed_files"], json!(3));
        assert_eq!(
            events[1].extra["error.chain"],
            json!(["io.PermissionDenied"])
        );
        assert!(events.last().unwrap().extra.is_empty());
    }

    /// 崩溃的文字是自由文本，可能带索引库 / 会话文件的路径：路径整段换成 `<path>`，主机名也不能带出去；
    /// 堆栈帧里的源码路径符号化要用，只换主目录。
    #[test]
    fn scrub_redacts_paths_in_free_text_and_hostname() {
        let home = dirs::home_dir().unwrap().display().to_string();
        let mut event = sentry::protocol::Event {
            message: Some(format!("open {home}/AgentHome/index/agenthome.db failed")),
            server_name: Some(Cow::Owned("Peters-MacBook".into())),
            ..Default::default()
        };
        event.exception.values.push(sentry::protocol::Exception {
            ty: "panic".into(),
            value: Some(format!(
                "read {home}/.gemini/tmp/secret-client/chats/s.jsonl: denied"
            )),
            stacktrace: Some(sentry::protocol::Stacktrace {
                frames: vec![sentry::protocol::Frame {
                    abs_path: Some(format!("{home}/code/agenthome/crates/app/src/sync.rs")),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let event = scrub(event);
        assert_eq!(event.message.as_deref(), Some("open <path> failed"));
        let exception = &event.exception.values[0];
        assert_eq!(exception.value.as_deref(), Some("read <path>: denied"));
        let frames = &exception.stacktrace.as_ref().unwrap().frames;
        assert_eq!(
            frames[0].abs_path.as_deref(),
            Some("~/code/agenthome/crates/app/src/sync.rs")
        );
        assert_eq!(event.server_name.as_deref(), Some("agenthome"));
    }

    #[test]
    fn redact_paths_handles_quotes_urls_and_plain_text() {
        // Debug 格式的路径带引号：带空格的目录名也要整段去掉。
        assert_eq!(
            redact_paths(r#"open "/Volumes/work/My Client/a.jsonl" failed"#),
            r#"open "<path>" failed"#
        );
        assert_eq!(
            redact_paths("a ~/.claude/projects/-Volumes-work-client/abc.jsonl, b (/tmp/x)"),
            "a <path>, b (<path>)"
        );
        assert_eq!(redact_paths("db:/Users/alice/x.db busy"), "db:<path> busy");
        // 不是路径的不动：URL、除号、单独的斜杠、模型名。
        for text in [
            "GET https://api.github.com/repos/x/y failed",
            "ratio 3 / 4 and and/or",
            "model openrouter/some-model",
            "index out of bounds: the len is 3 but the index is 7",
        ] {
            assert_eq!(redact_paths(text), text);
        }
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

    /// 网络连接失败发阶段、类别、版本和环境。
    #[test]
    fn remote_failure_event_carries_only_category_version_and_env() {
        let mut options = client_options(false);
        // 调试构建里 `active()` 为假会丢弃事件；这里只验证事件本身长什么样。
        options.before_send = Some(Arc::new(|event| Some(scrub(event))));
        let events = sentry::test::with_captured_events_options(
            || report_usage_failure("sync", "unreachable", None, None),
            options,
        );
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let json = serde_json::to_string_pretty(event).unwrap();
        println!("{json}");
        assert_eq!(
            event.message.as_deref(),
            Some("Usage sync failed: sync/unreachable")
        );
        assert_eq!(event.tags["remote.stage"], "sync");
        assert_eq!(event.tags["remote.kind"], "unreachable");
        assert!(event.extra.is_empty(), "不带错误详情：{:?}", event.extra);
        assert_eq!(
            event.release.as_deref(),
            Some(format!("agenthome@{VERSION}").as_str())
        );
        assert_eq!(event.environment.as_deref(), Some("development"));
        assert_eq!(event.server_name.as_deref(), Some("agenthome"));
        assert!(event.user.is_none());
        let home = dirs::home_dir().unwrap().display().to_string();
        assert!(!json.contains(&home));
    }

    #[test]
    fn remote_bad_response_event_carries_only_bounded_response_metadata() {
        use agenthome_core::remote_ssh::{ResponseFailure, ResponseLine, ResponseStream};

        let response = ResponseDiagnostic {
            reason: ResponseFailure::ExitStatus,
            exit_code: Some(2),
            stdout: ResponseStream {
                captured_bytes: 0,
                first_line: ResponseLine::Empty,
            },
            stderr: ResponseStream {
                captured_bytes: 42,
                first_line: ResponseLine::ExporterDatabaseOpenError,
            },
        };
        let mut options = client_options(false);
        options.before_send = Some(Arc::new(|event| Some(scrub(event))));
        let events = sentry::test::with_captured_events_options(
            || report_usage_failure("sync", "bad_response", None, Some(response)),
            options,
        );
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.tags["remote.stage"], "sync");
        assert_eq!(event.tags["remote.kind"], "bad_response");
        assert_eq!(event.extra.len(), 1);
        assert_eq!(
            event.extra["ssh.response"],
            json!({
                "reason": "exit_status",
                "exit_code": 2,
                "stdout": {"captured_bytes": 0, "first_line": "empty"},
                "stderr": {"captured_bytes": 42, "first_line": "exporter_database_open_error"}
            })
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
