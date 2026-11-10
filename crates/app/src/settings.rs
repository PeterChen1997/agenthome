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
use agenthome_core::signals::ResetCreditsSnapshot;
use agenthome_core::statusline_setup::{self, StatuslinePaths, StatuslineStatus};
use agenthome_core::{paths, pricing};
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
};

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

/// ChatGPT 重置额度：只在用户点「查询」时由官方 codex 查一次。
struct ResetCreditsState {
    running: bool,
    snapshot: Option<ResetCreditsSnapshot>,
    error: Option<SharedString>,
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
    reset: ResetCreditsState,
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
            reset: ResetCreditsState {
                running: false,
                snapshot: agenthome_core::signals::load_reset_credits(None)
                    .ok()
                    .flatten(),
                error: None,
            },
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
                text: t!(
                    "settings.appearance_save_failed",
                    error = format!("{error:#}")
                )
                .to_string()
                .into(),
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
        crate::set_app_menus(cx);
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
                        text: t!("settings.uninstall_failed", error = format!("{error:#}"))
                            .to_string()
                            .into(),
                    },
                });
                this.reload_disk(cx);
                cx.notify();
            });
        });
    }

    /// 点「查询重置额度」：第一次先弹授权说明，同意后才查；之后每次点直接查。
    fn on_reset_credits_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reset.running {
            return;
        }
        if config::reset_credits_consented() {
            self.query_reset_credits(cx);
            return;
        }
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title(t!("settings.reset_consent_title").to_string())
                .description(t!("settings.reset_consent_body").to_string())
                .show_cancel(true)
                .ok_text(t!("settings.reset_consent_ok").to_string())
                .cancel_text(t!("settings.reset_consent_cancel").to_string())
                .on_ok(move |_, _, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if let Err(error) = config::save_reset_credits_consent() {
                            this.reset.error = Some(format!("{error:#}").into());
                        }
                        this.query_reset_credits(cx);
                    });
                    true
                })
        });
    }

    fn query_reset_credits(&mut self, cx: &mut Context<Self>) {
        self.reset.running = true;
        self.reset.error = None;
        cx.notify();
        self._action_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { agenthome_core::signals::refresh_reset_credits(None) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.reset.running = false;
                match result {
                    Ok(snapshot) => this.reset.snapshot = Some(snapshot),
                    Err(error) => this.reset.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            });
        });
    }

    /// 额度来源里 Codex 下面的「ChatGPT 重置额度」一段。
    fn render_reset_credits(&self, now: i64, cx: &mut Context<Self>) -> AnyElement {
        const DAY: i64 = 86_400_000;
        let state = &self.reset;
        let theme = cx.theme();
        let (chip_text, chip_color): (String, gpui_kit::Hsla) =
            match (&state.snapshot, &state.error) {
                (_, Some(_)) => (t!("settings.reset_failed_chip").into(), theme.danger),
                (Some(snapshot), None) => (
                    t!("settings.reset_count", count = snapshot.live_count(now)).into(),
                    theme.success,
                ),
                (None, None) => (
                    t!("settings.reset_not_queried").into(),
                    theme.muted_foreground,
                ),
            };
        let fetched = state.snapshot.as_ref().map(|snapshot| {
            t!(
                "settings.reset_fetched",
                time = relative_time(snapshot.fetched_at_ms, now)
            )
            .to_string()
        });
        let rows: Vec<AnyElement> = state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.live_credits(now))
            .unwrap_or_default()
            .into_iter()
            .map(|credit| {
                let title = credit
                    .title
                    .clone()
                    .unwrap_or_else(|| t!("settings.reset_untitled").to_string());
                let (text, color) = match credit.expires_at_ms {
                    Some(at) => {
                        let left = at - now;
                        let when = chrono::TimeZone::timestamp_millis_opt(&chrono::Local, at)
                            .single()
                            .map(|t| t.format("%m-%d %H:%M").to_string())
                            .unwrap_or_default();
                        let color = if left < DAY {
                            cx.theme().danger
                        } else if left < 3 * DAY {
                            cx.theme().warning
                        } else {
                            cx.theme().foreground
                        };
                        (
                            t!(
                                "settings.reset_expires",
                                title = title,
                                time = when,
                                left = crate::format::countdown(left)
                            )
                            .to_string(),
                            color,
                        )
                    }
                    None => (
                        t!("settings.reset_no_expiry", title = title).to_string(),
                        cx.theme().foreground,
                    ),
                };
                div()
                    .text_size(px(12.5))
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .child(div().w(px(24.)))
                    .child(strong(t!("settings.reset_title").to_string()))
                    .child(chip(chip_text, chip_color, cx))
                    .child(div().flex_1())
                    .when_some(fetched, |row, text| row.child(muted(text, cx))),
            )
            .child(
                div()
                    .pl(px(34.))
                    .child(muted(t!("settings.reset_desc").to_string(), cx)),
            )
            .when(!rows.is_empty(), |col| {
                col.child(v_flex().pl(px(34.)).gap(px(2.)).children(rows))
            })
            .when_some(state.error.clone(), |col, error| {
                col.child(
                    div()
                        .pl(px(34.))
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                h_flex().pl(px(34.)).child(
                    Button::new("codex-reset-credits-query")
                        .outline()
                        .small()
                        .label(t!("settings.reset_query").to_string())
                        .loading(state.running)
                        .disabled(state.running)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_reset_credits_click(window, cx)
                        })),
                ),
            )
            .into_any_element()
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
            (Some(scan), _) => t!(
                "settings.ingesting",
                provider = provider_name(&scan.provider),
                done = scan.done,
                total = scan.total
            )
            .to_string()
            .into(),
            (None, Some(at)) => t!("settings.sync_live", time = relative_time(at, now))
                .to_string()
                .into(),
            (None, None) if self.sync_handle.is_none() => {
                t!("settings.sync_demo").to_string().into()
            }
            (None, None) => t!("settings.sync_starting").to_string().into(),
        };
        section(
            t!("settings.sources_title").to_string(),
            t!("settings.sources_hint").to_string(),
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
                        .label(t!("settings.scan_now").to_string())
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
            chip(t!("settings.source_not_found"), theme.muted_foreground, cx)
        } else if p.watching {
            chip(t!("settings.source_watching"), theme.success, cx)
        } else {
            chip(t!("settings.source_scheduled"), theme.warning, cx)
        };
        let mut detail = match p.last_scan_ms {
            Some(at) => {
                let files = if p.files == 1 {
                    t!("settings.source_files_one", count = p.files)
                } else {
                    t!("settings.source_files_other", count = p.files)
                };
                t!(
                    "settings.source_detail",
                    files = files,
                    time = relative_time(at, now),
                    parsed = p.parsed,
                    failed = p.failed,
                    warnings = p.warnings
                )
                .to_string()
            }
            None if p.detected => t!("settings.source_waiting").to_string(),
            None => t!("settings.source_missing").to_string(),
        };
        if let Some(note) = p.watch_note.as_ref().filter(|_| p.detected) {
            detail.push_str(" · ");
            detail.push_str(&t!("settings.watch_off", note = note));
        }
        let name = provider_name(&p.provider);
        let root = crate::format::tilde(&p.root);
        let failure = p.last_failure.clone();
        source_row(&p.provider, &name, state, root.into(), detail.into(), cx)
            .when_some(failure, |row, failure| {
                row.child(
                    div()
                        .pl(px(34.))
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(t!("settings.last_failure", failure = failure).to_string()),
                )
            })
            .into_any_element()
    }

    fn render_quota(&self, cx: &mut Context<Self>) -> AnyElement {
        let live = self.live.read(cx);
        let now = now_ms();
        let snapshot_text = |provider: ProviderId| -> String {
            match live.quota.iter().find(|q| q.provider == provider) {
                Some(q) => t!(
                    "settings.snapshot",
                    time = relative_time(q.fetched_at_ms, now),
                    freshness = freshness_label(q.freshness)
                )
                .to_string(),
                None => t!("settings.no_snapshot").to_string(),
            }
        };
        let claude_snapshot = snapshot_text(ProviderId::ClaudeCode);
        let codex_snapshot = snapshot_text(ProviderId::Codex);
        let agy_snapshot = snapshot_text(ProviderId::AntigravityCli);
        let legacy = legacy_statusline(self.disk.statusline.as_ref());
        let theme = cx.theme();

        let probe = self.sync.read(cx).claude_probe.clone();
        let claude_view = probe_view(&probe, self.sync_handle.is_some(), now, "claude");
        let probe_color = match claude_view.tone {
            Tone::Ok => theme.success,
            Tone::Bad => theme.danger,
            Tone::Muted => theme.muted_foreground,
        };
        let probe_chip = chip(claude_view.chip, probe_color, cx);
        let can_probe = self.sync_handle.is_some() && probe.binary.is_some() && !probe.running;
        let agy = self.sync.read(cx).agy_probe.clone();
        let agy_view = probe_view(&agy, self.sync_handle.is_some(), now, "agy");
        let agy_color = match agy_view.tone {
            Tone::Ok => theme.success,
            Tone::Bad => theme.danger,
            Tone::Muted => theme.muted_foreground,
        };
        let agy_chip = chip(agy_view.chip, agy_color, cx);
        let can_agy = self.sync_handle.is_some() && agy.binary.is_some() && !agy.running;
        let reset_credits = self.render_reset_credits(now, cx);

        section(
            t!("settings.quota_title").to_string(),
            t!("settings.quota_hint").to_string(),
            cx,
        )
        .child(
            v_flex()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(10.))
                        .items_center()
                        .child(provider_avatar(&ProviderId::ClaudeCode, 24.))
                        .child(strong(t!("settings.claude_usage_title").to_string()))
                        .child(probe_chip)
                        .child(div().flex_1())
                        .child(muted(claude_snapshot.clone(), cx)),
                )
                .child(div().pl(px(34.)).child(muted(claude_view.text, cx)))
                .child(
                    div()
                        .pl(px(34.))
                        .child(muted(t!("settings.claude_usage_desc").to_string(), cx)),
                )
                .child(
                    h_flex().pl(px(34.)).child(
                        Button::new("claude-usage-refresh")
                            .outline()
                            .small()
                            .label(t!("settings.refresh_now").to_string())
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
                                    .child(
                                        t!("settings.legacy_statusline", detail = detail)
                                            .to_string(),
                                    ),
                            )
                            .child(
                                Button::new("statusline-uninstall")
                                    .outline()
                                    .small()
                                    .label(t!("settings.uninstall").to_string())
                                    .loading(self.busy)
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.uninstall_statusline(cx)),
                                    ),
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
                        .child(strong(t!("settings.codex_rollout_title").to_string()))
                        .child(chip(t!("settings.no_setup_needed"), cx.theme().success, cx))
                        .child(div().flex_1())
                        .child(muted(codex_snapshot, cx)),
                )
                .child(
                    div()
                        .pl(px(34.))
                        .child(muted(t!("settings.codex_rollout_desc").to_string(), cx)),
                ),
        )
        .child(reset_credits)
        .child(
            v_flex()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(10.))
                        .items_center()
                        .child(provider_avatar(&ProviderId::AntigravityCli, 24.))
                        .child(strong(t!("settings.agy_usage_title").to_string()))
                        .child(agy_chip)
                        .child(div().flex_1())
                        .child(muted(agy_snapshot, cx)),
                )
                .child(div().pl(px(34.)).child(muted(agy_view.text, cx)))
                .child(
                    div()
                        .pl(px(34.))
                        .child(muted(t!("settings.agy_usage_desc").to_string(), cx)),
                )
                .child(
                    h_flex().pl(px(34.)).child(
                        Button::new("agy-usage-refresh")
                            .outline()
                            .small()
                            .label(t!("settings.refresh_now").to_string())
                            .loading(agy.running)
                            .disabled(!can_agy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(handle) = &this.sync_handle {
                                    handle.refresh_agy_quota();
                                }
                                cx.notify();
                            })),
                    ),
                ),
        )
        .into_any_element()
    }

    fn render_storage(&self, cx: &mut Context<Self>) -> AnyElement {
        let data_dir = crate::format::tilde(&paths::default_data_dir());
        let index = match (&self.index_db, self.disk.index_bytes) {
            (Some(db), Some(bytes)) => {
                format!("{} · {}", crate::format::tilde(db), human_bytes(bytes))
            }
            (Some(db), None) => format!(
                "{} · {}",
                crate::format::tilde(db),
                t!("settings.calculating")
            ),
            (None, _) => t!("settings.demo_no_index").to_string(),
        };
        let raw = self.sync.read(cx).raw.clone();
        let raw_line = raw_summary(&raw, self.sync_handle.is_some());
        let mut section = section(
            t!("settings.storage_title").to_string(),
            t!("settings.storage_hint").to_string(),
            cx,
        )
        .child(kv(t!("settings.archive_dir"), data_dir, cx))
        .child(kv(t!("settings.index_db"), index, cx))
        .child(kv(t!("settings.raw_layer"), raw_line, cx));
        if let Some(failure) = raw.last_failure.or(raw.open_error) {
            section = section.child(div().pl(px(4.)).child(muted(
                t!("settings.raw_last_error", failure = failure).to_string(),
                cx,
            )));
        }
        section.into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = Appearance::ALL
            .iter()
            .position(|a| *a == self.appearance)
            .unwrap_or(0);
        section(
            t!("settings.appearance").to_string(),
            t!("settings.appearance_hint").to_string(),
            cx,
        )
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
        section(t!("settings.about_title").to_string(), "", cx)
            .child(muted(t!("settings.about_quota").to_string(), cx))
            .child(muted(t!("settings.about_credentials").to_string(), cx))
            .child(kv(
                t!("settings.pricing_table"),
                t!(
                    "settings.pricing_value",
                    version = pricing::pricing_version()
                )
                .to_string(),
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
        Some(command) => t!("settings.legacy_wraps", command = command).to_string(),
        None => t!("settings.legacy_no_previous").to_string(),
    };
    match status? {
        StatuslineStatus::Installed {
            wrapped_command, ..
        } => Some(wrapped(wrapped_command)),
        StatuslineStatus::InstalledElsewhere {
            script_path,
            wrapped_command,
        } => Some(
            t!(
                "settings.legacy_elsewhere",
                path = script_path,
                detail = wrapped(wrapped_command)
            )
            .to_string(),
        ),
        _ => None,
    }
}

/// 后台 /usage 状态 → 标签与说明。
struct ProbeView {
    chip: SharedString,
    tone: Tone,
    text: String,
}

/// `agent` 是官方命令名（`claude` / `agy`），只用于「正在运行」的说明。
fn probe_view(probe: &crate::sync::QuotaProbe, syncing: bool, now: i64, agent: &str) -> ProbeView {
    let view =
        |chip: std::borrow::Cow<'static, str>, tone, text: std::borrow::Cow<str>| ProbeView {
            chip: chip.to_string().into(),
            tone,
            text: text.into_owned(),
        };
    if !syncing {
        return view(
            t!("settings.probe_demo"),
            Tone::Muted,
            t!("settings.probe_demo_text"),
        );
    }
    if probe.running {
        return view(
            t!("settings.probe_running"),
            Tone::Muted,
            t!("settings.probe_running_text", agent = agent),
        );
    }
    if probe.not_installed {
        return view(
            t!("settings.probe_not_installed"),
            Tone::Muted,
            t!("settings.probe_not_installed_text", agent = agent),
        );
    }
    match (&probe.last_error, probe.last_ok_ms, &probe.binary) {
        (Some(error), _, _) => view(
            t!("settings.probe_failed"),
            Tone::Bad,
            t!("settings.probe_failed_text", error = error),
        ),
        (None, Some(at), Some(bin)) => view(
            t!("settings.probe_ok"),
            Tone::Ok,
            t!(
                "settings.probe_ok_text",
                time = relative_time(at, now),
                path = crate::format::tilde(bin)
            ),
        ),
        _ => view(
            t!("settings.probe_waiting"),
            Tone::Muted,
            t!("settings.probe_waiting_text"),
        ),
    }
}

fn uninstall_summary(report: &statusline_setup::UninstallReport) -> String {
    if !report.settings_changed {
        return t!("settings.uninstall_noop").to_string();
    }
    let restored = match &report.restored_command {
        Some(command) => t!("settings.uninstall_restored", command = command).to_string(),
        None => t!("settings.uninstall_removed").to_string(),
    };
    let backup = report
        .backup_path
        .as_ref()
        .map(|p| t!("settings.uninstall_backup", path = crate::format::tilde(p)).to_string())
        .unwrap_or_default();
    t!(
        "settings.uninstall_done",
        restored = restored,
        backup = backup
    )
    .to_string()
}

fn provider_name(provider: &ProviderId) -> String {
    agenthome_core::providers::registry::display_name(provider)
}

fn freshness_label(freshness: Freshness) -> String {
    match freshness {
        Freshness::Live => t!("settings.freshness_live").into(),
        Freshness::Cached => t!("settings.freshness_cached").into(),
        Freshness::Stale => t!("settings.freshness_stale").into(),
    }
}

/// 设置页「原件（raw 层）」一行。
fn raw_summary(raw: &RawSync, live: bool) -> String {
    if !live {
        return t!("settings.raw_demo").to_string();
    }
    if let Some(error) = &raw.open_error {
        return t!("settings.raw_open_failed", error = error).to_string();
    }
    let Some(stats) = &raw.stats else {
        return t!("settings.raw_first_run").to_string();
    };
    let originals = if stats.sessions == 1 {
        t!("settings.raw_originals_one", count = stats.sessions)
    } else {
        t!("settings.raw_originals_other", count = stats.sessions)
    };
    let versions = if stats.versions == 1 {
        t!("settings.raw_versions_one", count = stats.versions)
    } else {
        t!("settings.raw_versions_other", count = stats.versions)
    };
    let sizes = t!(
        "settings.raw_sizes",
        content = human_bytes(stats.content_bytes),
        stored = human_bytes(stats.stored_bytes)
    );
    let mut line = format!("{originals} · {versions} · {sizes}");
    if raw.failed > 0 {
        line.push_str(" · ");
        line.push_str(&t!("settings.raw_failed", count = raw.failed));
    }
    if stats.unreadable > 0 {
        line.push_str(" · ");
        line.push_str(&if stats.unreadable == 1 {
            t!("settings.raw_unreadable_one", count = stats.unreadable)
        } else {
            t!("settings.raw_unreadable_other", count = stats.unreadable)
        });
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

fn chip(
    text: impl Into<SharedString>,
    color: gpui_kit::Hsla,
    _cx: &Context<SettingsView>,
) -> AnyElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(2.))
        .border_1()
        .border_color(color)
        .text_size(px(11.))
        .text_color(color)
        .child(text.into())
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

fn kv(key: impl Into<SharedString>, value: String, cx: &Context<SettingsView>) -> gpui_kit::Div {
    h_flex()
        .gap(px(12.))
        .items_start()
        .child(
            div()
                .w(px(110.))
                .flex_none()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(key.into()),
        )
        .child(div().flex_1().min_w_0().text_size(px(12.)).child(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_summary_reports_failures_instead_of_hiding_them() {
        let mut raw = RawSync::default();
        assert_eq!(raw_summary(&raw, false), "Demo data; not archived");
        assert_eq!(
            raw_summary(&raw, true),
            "Archiving originals for the first time…"
        );
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
            "2 originals · 3 versions · content 2.0 KB · on disk 512 B · 4 failed to archive in the last scan · 1 unreadable manifest"
        );
        raw.open_error = Some("权限不足".into());
        assert_eq!(
            raw_summary(&raw, true),
            "Can't open; indexing only, not archiving: 权限不足"
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
