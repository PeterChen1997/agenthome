//! 设置页（落地清单 §9）：来源与同步状态、额度来源、归档位置与索引、外观、合规说明、定价表版本。
//!
//! Claude 额度只走官方 /usage（后台每 15 分钟读一次），不再提供 statusline 安装；
//! 检测到旧版装过的 statusline 包装脚本时只给一个卸载入口，不自动卸载。
//!
//! 所有文件 IO（读 settings.json、算库体积、卸 statusline）都在后台线程；
//! 结果回前台写字段再 notify。

use std::path::PathBuf;
use std::time::Duration;

use agenthome_core::model::{Freshness, ProviderId};
use agenthome_core::statusline_setup::{self, StatuslinePaths, StatuslineStatus};
use agenthome_core::{paths, pricing};
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
};

use crate::assets::FONT_PIXEL_SANS;
use crate::config::{self, Appearance, Language};
use crate::format::{now_ms, relative_time};
use crate::live::LiveState;
use crate::message_row::provider_avatar;
use crate::sync::{ProviderSync, RawSync, SyncHandle, SyncState};

/// 库体积等慢变化信息的刷新间隔。
const DISK_POLL: Duration = Duration::from_secs(30);

/// 一次操作（安装、卸载、保存偏好）的结果提示。
#[derive(Clone)]
struct Notice {
    ok: bool,
    text: SharedString,
}

/// 后台读到的磁盘信息。
#[derive(Clone, Default)]
struct DiskInfo {
    /// 索引库 + WAL + SHM 的字节数。
    index_bytes: Option<u64>,
    statusline: Option<StatuslineStatus>,
}

pub struct SettingsView {
    sync: Entity<SyncState>,
    sync_handle: Option<SyncHandle>,
    live: Entity<LiveState>,
    index_db: Option<PathBuf>,
    disk: DiskInfo,
    appearance: Appearance,
    language: Language,
    notice: Option<Notice>,
    busy: bool,
    _disk_task: Task<()>,
    _action_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(
        sync: Entity<SyncState>,
        sync_handle: Option<SyncHandle>,
        live: Entity<LiveState>,
        index_db: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&sync, |_, _, cx| cx.notify()),
            cx.observe(&live, |_, _, cx| cx.notify()),
        ];
        let db = index_db.clone();
        let disk_task = cx.spawn(async move |this, cx| {
            loop {
                let db = db.clone();
                let info = cx
                    .background_spawn(async move { read_disk_info(db.as_deref()) })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.disk = info;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(DISK_POLL).await;
            }
        });
        Self {
            sync,
            sync_handle,
            live,
            index_db,
            disk: DiskInfo::default(),
            appearance: config::current_appearance(),
            language: config::load_language().unwrap_or_default(),
            notice: None,
            busy: false,
            _disk_task: disk_task,
            _action_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    fn reload_disk(&mut self, cx: &mut Context<Self>) {
        let db = self.index_db.clone();
        self._action_task = cx.spawn(async move |this, cx| {
            let info = cx
                .background_spawn(async move { read_disk_info(db.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.disk = info;
                cx.notify();
            });
        });
    }

    fn set_appearance(
        &mut self,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.appearance == appearance {
            return;
        }
        self.appearance = appearance;
        config::apply_appearance(appearance, Some(window), cx);
        self.notice = match config::save_appearance(appearance) {
            Ok(()) => None,
            Err(error) => Some(Notice {
                ok: false,
                text: format!("外观已切换，但保存偏好失败：{error:#}").into(),
            }),
        };
        cx.notify();
    }

    fn set_language(&mut self, language: Language, window: &mut Window, cx: &mut Context<Self>) {
        if self.language == language {
            return;
        }
        self.language = language;
        config::apply_language(language);
        self.notice = match config::save_language(language) {
            Ok(()) => None,
            Err(error) => Some(Notice {
                ok: false,
                text: t!(
                    "settings.language_save_failed",
                    error = format!("{error:#}")
                )
                .to_string()
                .into(),
            }),
        };
        // 渲染时取的文案靠整窗重绘更新；各页缓存的文案（仪表盘模型、归档列表）由外壳重新加载。
        cx.emit(SettingsEvent::LanguageChanged);
        window.refresh();
        cx.notify();
    }

    /// 卸载旧版 AgentHome 装过的 statusline 包装脚本。点按钮本身就是用户的确认；
    /// 动作前后都在页面上写明改了什么（还原成什么、备份在哪）。
    fn uninstall_statusline(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice = None;
        cx.notify();
        self._action_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    statusline_setup::uninstall(&StatuslinePaths::defaults())
                        .map(|r| uninstall_summary(&r))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.notice = Some(match result {
                    Ok(summary) => Notice {
                        ok: true,
                        text: summary.into(),
                    },
                    Err(error) => Notice {
                        ok: false,
                        text: format!("卸载失败，未改动任何文件：{error:#}").into(),
                    },
                });
                this.reload_disk(cx);
                cx.notify();
            });
        });
    }

    fn render_sources(&self, cx: &mut Context<Self>) -> AnyElement {
        let sync = self.sync.read(cx).clone();
        let now = now_ms();
        let scanning = sync.scanning.clone();
        let rows: Vec<AnyElement> = sync
            .providers
            .iter()
            .map(|p| self.render_source_row(p, now, cx))
            .collect();
        let status: SharedString = match (&scanning, sync.last_full_scan_ms) {
            (Some(scan), _) => format!(
                "正在入库 {} {}/{}",
                provider_name(&scan.provider),
                scan.done,
                scan.total
            )
            .into(),
            (None, Some(at)) => format!(
                "文件监听实时入库；最近一次全量扫描 {}，之后每小时兜底一次",
                relative_time(at, now)
            )
            .into(),
            (None, None) if self.sync_handle.is_none() => "演示数据，不扫描本机目录".into(),
            (None, None) => "启动扫描进行中…".into(),
        };
        section(
            "数据来源",
            "只读各 agent 的本机会话文件，从不修改原目录",
            cx,
        )
        .child(v_flex().gap(px(8.)).children(rows))
        .child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(
                    Button::new("scan-now")
                        .outline()
                        .small()
                        .label("立即扫描")
                        .disabled(self.sync_handle.is_none() || scanning.is_some())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(handle) = &this.sync_handle {
                                handle.scan_all();
                            }
                            cx.notify();
                        })),
                )
                .child(muted(status, cx)),
        )
        .into_any_element()
    }

    fn render_source_row(&self, p: &ProviderSync, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let state = if !p.detected {
            chip("未检测到", theme.muted_foreground, cx)
        } else if p.watching {
            chip("监听中", theme.success, cx)
        } else {
            chip("仅定时扫描", theme.warning, cx)
        };
        let mut detail = match p.last_scan_ms {
            Some(at) => format!(
                "{} 个会话文件 · 最近扫描 {} · 本次入库 {} · 失败 {} · 警告 {}",
                p.files,
                relative_time(at, now),
                p.parsed,
                p.failed,
                p.warnings
            ),
            None if p.detected => "等待首次扫描".to_string(),
            None => "目录不存在：该 agent 未安装或从未运行".to_string(),
        };
        if let Some(note) = p.watch_note.as_ref().filter(|_| p.detected) {
            detail.push_str(&format!(" · 监听未启用：{note}"));
        }
        let name = provider_name(&p.provider);
        let root = crate::format::tilde(&p.root);
        let failure = p.last_failure.clone();
        source_row(&p.provider, name, state, root.into(), detail.into(), cx)
            .when_some(failure, |row, failure| {
                row.child(
                    div()
                        .pl(px(34.))
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(format!("最近失败：{failure}")),
                )
            })
            .into_any_element()
    }

    fn render_quota(&self, cx: &mut Context<Self>) -> AnyElement {
        let live = self.live.read(cx);
        let now = now_ms();
        let snapshot_text = |provider: ProviderId| -> String {
            match live.quota.iter().find(|q| q.provider == provider) {
                Some(q) => format!(
                    "最近快照 {}（{}）",
                    relative_time(q.fetched_at_ms, now),
                    freshness_label(q.freshness)
                ),
                None => "暂无快照".to_string(),
            }
        };
        let claude_snapshot = snapshot_text(ProviderId::ClaudeCode);
        let codex_snapshot = snapshot_text(ProviderId::Codex);
        let legacy = legacy_statusline(self.disk.statusline.as_ref());
        let theme = cx.theme();

        let probe = self.sync.read(cx).claude_probe.clone();
        let probe_view = probe_view(&probe, self.sync_handle.is_some(), now);
        let probe_color = match probe_view.tone {
            Tone::Ok => theme.success,
            Tone::Bad => theme.danger,
            Tone::Muted => theme.muted_foreground,
        };
        let probe_chip = chip(probe_view.chip, probe_color, cx);
        let can_probe = self.sync_handle.is_some() && probe.binary.is_some() && !probe.running;

        section("额度来源", "只用官方程序与本机文件，不读取任何账号凭据", cx)
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .items_center()
                            .child(provider_avatar(&ProviderId::ClaudeCode, 24.))
                            .child(strong("Claude Code · 官方 /usage（后台）"))
                            .child(probe_chip)
                            .child(div().flex_1())
                            .child(muted(claude_snapshot.clone(), cx)),
                    )
                    .child(div().pl(px(34.)).child(muted(probe_view.text, cx)))
                    .child(div().pl(px(34.)).child(muted(
                        "Claude 额度来自官方 /usage，每 15 分钟后台读取：启动一次官方 claude 命令\
                         （约 10 秒），不发对话、不耗 token、不读凭据，桌面版用户也能看到订阅额度。\
                         不需要再装 statusline。",
                        cx,
                    )))
                    .child(
                        h_flex().pl(px(34.)).child(
                            Button::new("claude-usage-refresh")
                                .outline()
                                .small()
                                .label("立即刷新")
                                .loading(probe.running)
                                .disabled(!can_probe)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(handle) = &this.sync_handle {
                                        handle.refresh_claude_quota();
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .when_some(legacy, |col, detail| {
                        col.child(
                            h_flex()
                                .pl(px(34.))
                                .gap(px(8.))
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().warning)
                                        .child(format!(
                                            "检测到旧的 statusline 包装脚本，可卸载（{detail}）。\
                                             卸载会把 ~/.claude/settings.json 的 statusLine 还原，改前先备份。"
                                        )),
                                )
                                .child(
                                    Button::new("statusline-uninstall")
                                        .outline()
                                        .small()
                                        .label("卸载")
                                        .loading(self.busy)
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.uninstall_statusline(cx)
                                        })),
                                ),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .items_center()
                            .child(provider_avatar(&ProviderId::Codex, 24.))
                            .child(strong("Codex · rollout 快照"))
                            .child(chip("无需安装", cx.theme().success, cx))
                            .child(div().flex_1())
                            .child(muted(codex_snapshot, cx)),
                    )
                    .child(div().pl(px(34.)).child(muted(
                        "读取 ~/.codex/sessions 最新会话文件里的 rate_limits；\
                     ChatGPT 桌面版里的 Codex 也写在这里。",
                        cx,
                    ))),
            )
            .into_any_element()
    }

    fn render_storage(&self, cx: &mut Context<Self>) -> AnyElement {
        let data_dir = crate::format::tilde(&paths::default_data_dir());
        let index = match (&self.index_db, self.disk.index_bytes) {
            (Some(db), Some(bytes)) => {
                format!("{} · {}", crate::format::tilde(db), human_bytes(bytes))
            }
            (Some(db), None) => format!("{} · 计算中", crate::format::tilde(db)),
            (None, _) => "演示数据，无索引库".to_string(),
        };
        let raw = self.sync.read(cx).raw.clone();
        let raw_line = raw_summary(&raw, self.sync_handle.is_some());
        let mut section = section(
            "归档位置",
            "raw 层逐字节保存原件，agent 自己清理了也还在；index 层可随时从头重建",
            cx,
        )
        .child(kv("归档目录", data_dir, cx))
        .child(kv("索引库", index, cx))
        .child(kv("原件（raw 层）", raw_line, cx));
        if let Some(failure) = raw.last_failure.or(raw.open_error) {
            section = section.child(
                div()
                    .pl(px(4.))
                    .child(muted(format!("最近的归档错误：{failure}"), cx)),
            );
        }
        section.into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = Appearance::ALL
            .iter()
            .position(|a| *a == self.appearance)
            .unwrap_or(0);
        section("外观", "保存在 ~/AgentHome/config/app.json", cx)
            .child(
                h_flex().child(
                    TabBar::new("appearance")
                        .segmented()
                        .xsmall()
                        .selected_index(selected)
                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                            if let Some(appearance) = Appearance::ALL.get(*ix) {
                                this.set_appearance(*appearance, window, cx);
                            }
                        }))
                        .children(Appearance::ALL.iter().map(|a| Tab::new().label(a.label()))),
                ),
            )
            .into_any_element()
    }

    fn render_language(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = Language::ALL
            .iter()
            .position(|l| *l == self.language)
            .unwrap_or(0);
        section(
            t!("settings.language").to_string(),
            t!("settings.language_hint").to_string(),
            cx,
        )
        .child(
            h_flex().child(
                TabBar::new("language")
                    .segmented()
                    .xsmall()
                    .selected_index(selected)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                        if let Some(language) = Language::ALL.get(*ix) {
                            this.set_language(*language, window, cx);
                        }
                    }))
                    .children(Language::ALL.iter().map(|l| Tab::new().label(l.label()))),
            ),
        )
        .into_any_element()
    }

    fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        section("合规与定价", "", cx)
            .child(muted(
                "· 额度只走官方程序与本机文件：Claude 由官方 claude 命令的 /usage 提供，\
                 Codex 走会话 rollout。AgentHome 不读取凭据，不提供 Claude OAuth usage 端点、Codex wham 端点等需要账号凭据的高风险来源。",
                cx,
            ))
            .child(muted(
                "· 不读取、不保存任何登录凭据；只读取会话文件：原件拷贝进 ~/AgentHome/raw、解析进索引，不改动 agent 原目录。",
                cx,
            ))
            .child(kv(
                "定价表",
                format!(
                    "{}（内嵌快照，成本为估算；Claude 有官方成本时以官方为准）",
                    pricing::pricing_version()
                ),
                cx,
            ))
            .into_any_element()
    }
}

/// 设置页发给外壳的事件。
pub enum SettingsEvent {
    /// 界面语言变了：外壳让各页重新加载带文案的数据。
    LanguageChanged,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.notice.clone();
        div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(920.))
                    .mx_auto()
                    .px(px(24.))
                    .pt(px(18.))
                    .pb(px(32.))
                    .gap(px(14.))
                    .when_some(notice, |page, notice| {
                        let color = if notice.ok {
                            cx.theme().success
                        } else {
                            cx.theme().danger
                        };
                        page.child(
                            div()
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(color)
                                .text_color(color)
                                .child(notice.text),
                        )
                    })
                    .child(self.render_sources(cx))
                    .child(self.render_quota(cx))
                    .child(self.render_storage(cx))
                    .child(self.render_appearance(cx))
                    .child(self.render_language(cx))
                    .child(self.render_about(cx)),
            )
    }
}

fn read_disk_info(db: Option<&std::path::Path>) -> DiskInfo {
    let index_bytes = db.map(|db| {
        ["", "-wal", "-shm"]
            .iter()
            .filter_map(|suffix| {
                let mut path = db.as_os_str().to_owned();
                path.push(suffix);
                std::fs::metadata(PathBuf::from(path)).ok().map(|m| m.len())
            })
            .sum()
    });
    // 演示数据（没有索引库）不读本机的 Claude 配置，界面只反映演示状态。
    let statusline = db.map(|_| statusline_setup::status(&StatuslinePaths::defaults()));
    DiskInfo {
        index_bytes,
        statusline,
    }
}

enum Tone {
    Ok,
    Bad,
    Muted,
}

/// 旧版 AgentHome 装过的 statusline 包装脚本：装着时返回一句描述（包装了什么），否则 None。
/// 其他状态（没装、别人的命令、配置读不懂）都不出现卸载入口。
fn legacy_statusline(status: Option<&StatuslineStatus>) -> Option<String> {
    let wrapped = |command: &Option<String>| match command {
        Some(command) => format!("它包装着原命令 {command}"),
        None => "安装前没有 statusline".to_string(),
    };
    match status? {
        StatuslineStatus::Installed {
            wrapped_command, ..
        } => Some(wrapped(wrapped_command)),
        StatuslineStatus::InstalledElsewhere {
            script_path,
            wrapped_command,
        } => Some(format!("{script_path}，{}", wrapped(wrapped_command))),
        _ => None,
    }
}

/// 后台 /usage 状态 → 标签与说明。
struct ProbeView {
    chip: &'static str,
    tone: Tone,
    text: String,
}

fn probe_view(probe: &crate::sync::ClaudeProbe, syncing: bool, now: i64) -> ProbeView {
    let view = |chip, tone, text: String| ProbeView { chip, tone, text };
    if !syncing {
        return view("演示数据", Tone::Muted, "mock 模式不运行后台刷新".into());
    }
    if probe.running {
        return view(
            "刷新中",
            Tone::Muted,
            "正在后台运行官方 claude /usage…".into(),
        );
    }
    match (&probe.last_error, probe.last_ok_ms, &probe.binary) {
        (Some(error), _, _) => view("失败", Tone::Bad, format!("上次刷新失败：{error}")),
        (None, Some(at), Some(bin)) => view(
            "正常",
            Tone::Ok,
            format!(
                "{} 成功读取（{}）",
                relative_time(at, now),
                crate::format::tilde(bin)
            ),
        ),
        _ => view("等待中", Tone::Muted, "启动后马上运行第一次".into()),
    }
}

fn uninstall_summary(report: &statusline_setup::UninstallReport) -> String {
    if !report.settings_changed {
        return "本来就没有安装，未改动任何文件。".to_string();
    }
    let restored = match &report.restored_command {
        Some(command) => format!("statusLine 已还原为 {command}。"),
        None => "已删除 statusLine 一项（安装前没有）。".to_string(),
    };
    let backup = report
        .backup_path
        .as_ref()
        .map(|p| format!("改动前的文件备份在 {}。", crate::format::tilde(p)))
        .unwrap_or_default();
    format!("已卸载：{restored}{backup}已收集的额度数据保留在 ~/AgentHome/signals。")
}

fn provider_name(provider: &ProviderId) -> &'static str {
    match provider {
        ProviderId::ClaudeCode => "Claude Code",
        ProviderId::Codex => "Codex",
        ProviderId::GeminiCli => "Gemini CLI",
        ProviderId::OpenCode => "OpenCode",
        ProviderId::Cursor => "Cursor",
        ProviderId::Other(_) => "其他",
    }
}

fn freshness_label(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Live => "实时",
        Freshness::Cached => "缓存",
        Freshness::Stale => "已过期",
    }
}

/// 设置页「原件（raw 层）」一行。
fn raw_summary(raw: &RawSync, live: bool) -> String {
    if !live {
        return "演示数据，不归档".to_string();
    }
    if let Some(error) = &raw.open_error {
        return format!("打不开，只入索引、不归档：{error}");
    }
    let Some(stats) = &raw.stats else {
        return "正在首次归档原件…".to_string();
    };
    let mut line = format!(
        "{} 个原件 · {} 个版本 · 内容 {} · 占用 {}",
        stats.sessions,
        stats.versions,
        human_bytes(stats.content_bytes),
        human_bytes(stats.stored_bytes)
    );
    if raw.failed > 0 {
        line.push_str(&format!(" · 最近一次扫描 {} 个归档失败", raw.failed));
    }
    if stats.unreadable > 0 {
        line.push_str(&format!(" · {} 个清单读不懂", stats.unreadable));
    }
    line
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 设置页区块：卡片 + Pixelify Sans 标题 + 次要说明。
fn section(
    title: impl Into<SharedString>,
    sub: impl Into<SharedString>,
    cx: &Context<SettingsView>,
) -> gpui_kit::Div {
    let (title, sub): (SharedString, SharedString) = (title.into(), sub.into());
    v_flex()
        .gap(px(12.))
        .px(px(18.))
        .py(px(16.))
        .rounded(px(4.))
        .bg(cx.theme().group_box)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .gap(px(8.))
                .items_center()
                .child(
                    div()
                        .font_family(FONT_PIXEL_SANS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(14.))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(sub),
                ),
        )
}

fn source_row(
    provider: &ProviderId,
    name: &str,
    state: AnyElement,
    root: SharedString,
    detail: SharedString,
    cx: &Context<SettingsView>,
) -> gpui_kit::Div {
    v_flex()
        .gap(px(2.))
        .child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(provider_avatar(provider, 24.))
                .child(strong(name.to_string()))
                .child(state)
                .child(div().flex_1())
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(root),
                ),
        )
        .child(div().pl(px(34.)).child(muted(detail, cx)))
}

fn chip(text: &'static str, color: gpui_kit::Hsla, _cx: &Context<SettingsView>) -> AnyElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(2.))
        .border_1()
        .border_color(color)
        .text_size(px(11.))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn strong(text: impl Into<SharedString>) -> gpui_kit::Div {
    div().font_weight(FontWeight::SEMIBOLD).child(text.into())
}

fn muted(text: impl Into<SharedString>, cx: &Context<SettingsView>) -> gpui_kit::Div {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn kv(key: &'static str, value: String, cx: &Context<SettingsView>) -> gpui_kit::Div {
    h_flex()
        .gap(px(12.))
        .items_start()
        .child(
            div()
                .w(px(110.))
                .flex_none()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(key),
        )
        .child(div().flex_1().min_w_0().text_size(px(12.)).child(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_summary_reports_failures_instead_of_hiding_them() {
        let mut raw = RawSync::default();
        assert_eq!(raw_summary(&raw, false), "演示数据，不归档");
        assert_eq!(raw_summary(&raw, true), "正在首次归档原件…");
        raw.stats = Some(agenthome_core::RawStats {
            sessions: 2,
            versions: 3,
            content_bytes: 2048,
            stored_bytes: 512,
            unreadable: 1,
        });
        raw.failed = 4;
        assert_eq!(
            raw_summary(&raw, true),
            "2 个原件 · 3 个版本 · 内容 2.0 KB · 占用 512 B · 最近一次扫描 4 个归档失败 · 1 个清单读不懂"
        );
        raw.open_error = Some("权限不足".into());
        assert_eq!(
            raw_summary(&raw, true),
            "打不开，只入索引、不归档：权限不足"
        );
    }

    #[test]
    fn index_size_is_readable() {
        // 设置页用它展示索引库体积；单位要随量级变化，不能把 1.4 GB 显示成一长串字节。
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1_432_059_904), "1.3 GB");
    }

    #[test]
    fn only_agenthome_wrapper_offers_uninstall() {
        // Claude 额度改走 /usage 后不再提供安装；只有装着 AgentHome 自己的包装脚本时才给卸载入口，
        // 用户自己的 statusline、读不懂的配置都不能出现能改 settings.json 的按钮。
        let installed = StatuslineStatus::Installed {
            wrapped_command: Some("ccline".into()),
            script_up_to_date: false,
        };
        assert!(legacy_statusline(Some(&installed)).is_some_and(|t| t.contains("ccline")));
        let elsewhere = StatuslineStatus::InstalledElsewhere {
            script_path: "/repo/script/agenthome-statusline.sh".into(),
            wrapped_command: None,
        };
        assert!(legacy_statusline(Some(&elsewhere)).is_some());
        for status in [
            StatuslineStatus::SettingsMissing,
            StatuslineStatus::NotConfigured,
            StatuslineStatus::OtherCommand {
                command: "ccline".into(),
            },
            StatuslineStatus::SettingsUnreadable {
                error: "bad json".into(),
            },
            StatuslineStatus::Unrecognized {
                reason: "type 不是 command".into(),
            },
        ] {
            assert_eq!(legacy_statusline(Some(&status)), None);
        }
        assert_eq!(legacy_statusline(None), None);
    }
}
