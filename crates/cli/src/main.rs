//! agenthome 命令行：scan / sessions / show / stats / quota / activity / statusline。

use agenthome_core::model::{
    ContentBlock, CostSource, Freshness, Period, ProviderId, Role, SessionQuery,
};
use agenthome_core::statusline_setup::{self, StatuslinePaths, StatuslineStatus};
use agenthome_core::{
    ClaudeCodeProvider, CodexProvider, GeminiProvider, RawArchive, SourceProvider, Store,
    claude_statusline, codex_rollout_quota, ingest_all, paths, pricing,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Write;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

#[derive(Parser)]
#[command(name = "agenthome", about = "AgentHome：本机 agent 会话归档与索引")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum PeriodArg {
    Today,
    Week,
    Month,
    All,
}

#[derive(Subcommand)]
enum Cmd {
    /// 扫描全部来源（Claude Code、Codex、Gemini）并入库（增量）
    Scan {
        /// Claude Code projects 根目录，默认 ~/.claude/projects
        #[arg(long)]
        root: Option<PathBuf>,
        /// Codex sessions 根目录，默认 ~/.codex/sessions
        #[arg(long)]
        codex_root: Option<PathBuf>,
        /// Gemini 根目录（Gemini CLI 的 tmp/ 与 Antigravity 的 antigravity/），默认 ~/.gemini
        #[arg(long)]
        gemini_root: Option<PathBuf>,
        /// 索引数据库路径，默认 ~/AgentHome/index/agenthome.db
        #[arg(long)]
        db: Option<PathBuf>,
        /// 只入索引，不把原件归档到 raw 层（默认归档到索引库旁的 raw 目录）
        #[arg(long)]
        no_raw: bool,
    },
    /// 列出会话摘要
    Sessions {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 全文搜索（标题 + 消息正文）
        #[arg(long)]
        query: Option<String>,
        /// 按项目名（cwd 最后一段）过滤
        #[arg(long)]
        project: Option<String>,
    },
    /// 打印一个会话的消息
    Show {
        /// 会话 id 或前缀（完整 id / native id 前缀均可）
        session: String,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// 用量与成本：汇总 / 按模型 / 按项目 / 每日
    Stats {
        #[arg(long)]
        db: Option<PathBuf>,
        /// 周期（本地时区）；与 --days 二选一
        #[arg(long, value_enum)]
        period: Option<PeriodArg>,
        /// 最近 N 天（滚动）；默认 30
        #[arg(long)]
        days: Option<u32>,
    },
    /// 打印额度快照（Claude statusline 文件 + Codex rollout）
    Quota {
        /// Codex sessions 根目录，默认 ~/.codex/sessions
        #[arg(long)]
        codex_root: Option<PathBuf>,
        /// statusline 快照文件，默认 ~/AgentHome/signals/claude-statusline.json
        #[arg(long)]
        statusline: Option<PathBuf>,
        /// 先在后台跑一次官方 `claude` 的 /usage 刷新 Claude 订阅额度（约 10 秒，不发对话、不耗 token）
        #[arg(long)]
        refresh_claude: bool,
        /// 启动官方 codex 只读查询一次 ChatGPT 重置额度（由 codex 自己鉴权；不兑换、不读凭据）
        #[arg(long)]
        refresh_codex_reset_credits: bool,
    },
    /// 各来源最近活动
    Activity {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        codex_root: Option<PathBuf>,
        #[arg(long)]
        gemini_root: Option<PathBuf>,
    },
    /// Claude statusline 包装脚本：查看 / 安装 / 卸载（改 ~/.claude/settings.json，改前自动备份）
    Statusline {
        #[command(subcommand)]
        action: StatuslineAction,
        /// Claude Code 设置文件，默认 ~/.claude/settings.json
        #[arg(long, global = true)]
        settings: Option<PathBuf>,
        /// 包装脚本目录，默认 ~/AgentHome/bin
        #[arg(long, global = true)]
        bin_dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum StatuslineAction {
    /// 只读：查看当前接入状态
    Status,
    /// 安装（或更新）包装脚本并接入 statusLine，保留原有 statusline 显示
    Install,
    /// 卸载：statusLine 还原为原命令（原来没有就删掉），不删 signals 数据
    Uninstall,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan {
            root,
            codex_root,
            gemini_root,
            db,
            no_raw,
        } => scan(root, codex_root, gemini_root, db, no_raw),
        Cmd::Sessions {
            db,
            limit,
            query,
            project,
        } => sessions(db, limit, query, project),
        Cmd::Show { session, db } => show(db, &session),
        Cmd::Stats { db, period, days } => stats(db, period, days),
        Cmd::Quota {
            codex_root,
            statusline,
            refresh_claude,
            refresh_codex_reset_credits,
        } => quota(
            codex_root,
            statusline,
            refresh_claude,
            refresh_codex_reset_credits,
        ),
        Cmd::Activity {
            db,
            root,
            codex_root,
            gemini_root,
        } => activity(db, root, codex_root, gemini_root),
        Cmd::Statusline {
            action,
            settings,
            bin_dir,
        } => statusline(action, settings, bin_dir),
    }
}

fn open_store(db: Option<PathBuf>) -> Result<Store> {
    let path = db.unwrap_or_else(paths::default_index_db);
    Store::open(&path).with_context(|| format!("打开索引库 {}", path.display()))
}

fn providers(
    root: Option<PathBuf>,
    codex_root: Option<PathBuf>,
    gemini_root: Option<PathBuf>,
) -> (ClaudeCodeProvider, CodexProvider, GeminiProvider) {
    (
        ClaudeCodeProvider::new(root.unwrap_or_else(paths::claude_projects_dir)),
        CodexProvider::new(codex_root.unwrap_or_else(paths::codex_sessions_dir)),
        GeminiProvider::new(gemini_root.unwrap_or_else(paths::gemini_dir)),
    )
}

fn scan(
    root: Option<PathBuf>,
    codex_root: Option<PathBuf>,
    gemini_root: Option<PathBuf>,
    db: Option<PathBuf>,
    no_raw: bool,
) -> Result<()> {
    let (claude, codex, gemini) = providers(root, codex_root, gemini_root);
    let list: [&dyn SourceProvider; 3] = [&claude, &codex, &gemini];
    let detected: Vec<&dyn SourceProvider> = list.iter().copied().filter(|p| p.detect()).collect();
    if detected.is_empty() {
        bail!(
            "未找到任何来源目录：{} / {} / {}",
            claude.root().display(),
            codex.root().display(),
            gemini.root().display()
        );
    }
    let db = db.unwrap_or_else(paths::default_index_db);
    let raw = if no_raw {
        None
    } else {
        let dir = paths::raw_dir_for_db(&db);
        Some(
            RawArchive::open(dir.clone())
                .with_context(|| format!("打开 raw 层 {}", dir.display()))?,
        )
    };
    let mut store = open_store(Some(db))?;
    let stderr = std::io::stderr();
    let mut progress = |p: agenthome_core::Progress| {
        if p.done.is_multiple_of(50) || p.done == p.total {
            let _ = write!(
                stderr.lock(),
                "\r[{}] 已处理 {}/{} 个文件",
                p.provider.as_str(),
                p.done,
                p.total
            );
        }
    };
    let report = ingest_all(&mut store, &detected, raw.as_ref(), &mut progress);
    eprintln!();

    for (provider, r) in &report.per_provider {
        let root = match provider {
            ProviderId::ClaudeCode => claude.root().display().to_string(),
            ProviderId::Codex => codex.root().display().to_string(),
            ProviderId::GeminiCli => gemini.root().display().to_string(),
            _ => String::new(),
        };
        println!("== {} ({root})", provider.as_str());
        println!(
            "发现文件: {}  重新解析: {}  未变跳过: {}  旧副本跳过: {}",
            r.scanned, r.parsed, r.skipped_unchanged, r.skipped_superseded
        );
        println!(
            "解析警告: {} 条  耗时: {:.1}s",
            r.warnings,
            r.elapsed.as_secs_f64()
        );
        for w in &r.warning_samples {
            println!("  警告: {w}");
        }
        if !r.ignored_records.is_empty() {
            let items: Vec<String> = r
                .ignored_records
                .iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect();
            println!("非对话记录(未入库): {}", items.join(", "));
        }
        for (later, first) in &r.id_collisions {
            println!(
                "  警告: 会话 id 冲突，{} 覆盖了 {}",
                later.display(),
                first.display()
            );
        }
        if raw.is_some() {
            println!(
                "原件归档: 新建 {}  追加 {}  新版本 {}  改名 {}  未变 {}  写入 {}",
                r.raw.created,
                r.raw.appended,
                r.raw.new_versions,
                r.raw.restamped,
                r.raw.unchanged,
                human_bytes(r.raw.bytes_copied)
            );
            for (p, e) in &r.raw.failed {
                println!("  归档失败: {}: {e}", p.display());
            }
        }
        if !r.failed.is_empty() {
            println!("失败: {} 个文件", r.failed.len());
            for (p, e) in &r.failed {
                println!("  {}: {e}", p.display());
            }
        }
    }
    let (sessions, messages) = store.counts()?;
    println!("== 合计");
    println!("库内会话: {sessions}，消息: {messages}");
    if let Some(raw) = &raw {
        let stats = raw.stats();
        println!(
            "raw 层: {} 个原件、{} 个版本，内容 {}，占用 {}（{}）",
            stats.sessions,
            stats.versions,
            human_bytes(stats.content_bytes),
            human_bytes(stats.stored_bytes),
            raw.root().display()
        );
        if stats.unreadable > 0 {
            println!("  警告: {} 个清单读不懂", stats.unreadable);
        }
    }
    println!("总耗时: {:.1}s", report.elapsed.as_secs_f64());
    if !report.failed.is_empty() || !report.raw.failed.is_empty() {
        println!(
            "失败合计: 解析 {} 个文件，归档 {} 个文件",
            report.failed.len(),
            report.raw.failed.len()
        );
        std::process::exit(1);
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn sessions(
    db: Option<PathBuf>,
    limit: usize,
    query: Option<String>,
    project: Option<String>,
) -> Result<()> {
    let store = open_store(db)?;
    let mut rows = store.list_sessions(&SessionQuery {
        text: query,
        limit: if project.is_some() { 0 } else { limit },
        ..Default::default()
    })?;
    if let Some(p) = &project {
        rows.retain(|s| &s.project_name == p);
        rows.truncate(limit);
    }
    if rows.is_empty() {
        println!("没有匹配的会话");
        return Ok(());
    }
    let header = [
        "更新时间",
        "来源",
        "类型",
        "项目",
        "消息",
        "tokens",
        "会话ID(native)",
        "标题",
    ];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|s| {
            vec![
                fmt_time(s.updated_at_ms),
                s.provider.as_str().to_string(),
                s.kind.as_str().to_string(),
                truncate(&s.project_name, 20),
                s.message_count.to_string(),
                fmt_num(s.usage.total_tokens()),
                // 主会话显示 uuid；子代理 native_id 是 "父uuid/agentId"，只显示 agentId。
                truncate(s.id.rsplit(['/', ':']).next().unwrap_or(&s.id), 36),
                truncate(&s.title, 40),
            ]
        })
        .collect();
    print_table(&header, &body);
    Ok(())
}

fn show(db: Option<PathBuf>, prefix: &str) -> Result<()> {
    let store = open_store(db)?;
    let Some(session) = store.find_session(prefix)? else {
        bail!("找不到会话: {prefix}");
    };
    println!("会话: {}", session.id);
    println!("标题: {}", session.title.as_deref().unwrap_or("(无)"));
    println!(
        "类型: {}  父会话: {}",
        session.kind.as_str(),
        session.parent_session_id.as_deref().unwrap_or("-")
    );
    println!(
        "目录: {}  分支: {}",
        session.cwd.as_deref().unwrap_or("-"),
        session.git_branch.as_deref().unwrap_or("-")
    );
    println!(
        "版本: {}  入口: {}",
        session.agent_version.as_deref().unwrap_or("-"),
        session.entrypoint.as_deref().unwrap_or("-")
    );
    println!(
        "时间: {} ~ {}",
        fmt_time(session.started_at_ms),
        fmt_time(session.updated_at_ms)
    );
    println!("模型: {}", session.models.join(", "));
    let u = &session.usage;
    println!(
        "用量: in={} out={} cache_read={} cache_write={} reasoning={} requests={}{}",
        fmt_num(u.input_tokens),
        fmt_num(u.output_tokens),
        fmt_num(u.cache_read_tokens),
        fmt_num(u.cache_write_tokens),
        fmt_num(u.reasoning_tokens),
        u.requests,
        if u.input_includes_cache {
            " (input 含缓存读)"
        } else {
            ""
        }
    );
    println!(
        "成本: 记录={}  估算={}",
        session
            .recorded_cost_usd
            .map(fmt_usd)
            .unwrap_or_else(|| "-".to_string()),
        session
            .estimated_cost_usd
            .map(fmt_usd)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "原件: {}{}",
        session.source.path.display(),
        if session.source.compressed {
            " (zstd)"
        } else {
            ""
        }
    );
    println!("---");
    for m in store.messages(&session.id)? {
        let role = match m.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::System => "系统",
            Role::Tool => "工具",
        };
        let meta = if m.is_meta { " [meta]" } else { "" };
        let tools: Vec<String> = m
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let mut line = format!("[{}] {role}{meta}", fmt_time(m.timestamp_ms));
        if let Some(model) = &m.model {
            line.push_str(&format!(" ({model})"));
        }
        if !tools.is_empty() {
            line.push_str(&format!(" 工具: {}", tools.join(", ")));
        }
        let text = m.text();
        if !text.is_empty() {
            line.push_str(&format!(
                "\n    {}",
                truncate(&text.replace('\n', " "), 200)
            ));
        } else if let Some(b) = m.blocks.first() {
            let kind = match b {
                ContentBlock::Thinking { .. } => "(thinking)",
                ContentBlock::ToolResult { .. } => "(tool_result)",
                ContentBlock::ToolCall { .. } => "",
                ContentBlock::Image { .. } => "(image)",
                ContentBlock::Compaction { .. } => "(compaction)",
                ContentBlock::Unknown { .. } => "(unknown block)",
                ContentBlock::Text { .. } => "",
            };
            if !kind.is_empty() {
                line.push_str(&format!("\n    {kind}"));
            }
        }
        println!("{line}");
    }
    Ok(())
}

fn stats(db: Option<PathBuf>, period: Option<PeriodArg>, days: Option<u32>) -> Result<()> {
    let store = open_store(db)?;
    let period = match (period, days) {
        (Some(_), Some(_)) => bail!("--period 与 --days 只能选一个"),
        (Some(PeriodArg::Today), None) => Period::Today,
        (Some(PeriodArg::Week), None) => Period::Week,
        (Some(PeriodArg::Month), None) => Period::Month,
        (Some(PeriodArg::All), None) => Period::All,
        (None, Some(n)) => Period::Days(n),
        (None, None) => Period::Days(30),
    };
    let daily_days = match period {
        Period::Days(n) => n,
        Period::Today => 1,
        Period::Week => 7,
        Period::Month => 31,
        Period::All => 30,
    };

    println!(
        "== 汇总（周期: {}，定价表: {}）",
        period.as_str(),
        pricing::pricing_version()
    );
    let header = [
        "来源",
        "会话",
        "消息",
        "请求",
        "input",
        "output",
        "cache_read",
        "cache_write",
        "成本(USD)",
        "成本来源",
    ];
    let mut body = Vec::new();
    for p in [
        None,
        Some(ProviderId::ClaudeCode),
        Some(ProviderId::Codex),
        Some(ProviderId::GeminiCli),
    ] {
        let t = store.stats_totals(period, p.as_ref())?;
        if p.is_some() && t.messages == 0 {
            continue;
        }
        body.push(vec![
            p.as_ref()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "全部".to_string()),
            t.sessions.to_string(),
            t.messages.to_string(),
            t.usage.requests.to_string(),
            fmt_num(t.usage.input_tokens),
            fmt_num(t.usage.output_tokens),
            fmt_num(t.usage.cache_read_tokens),
            fmt_num(t.usage.cache_write_tokens),
            fmt_usd(t.cost_usd),
            cost_source_str(t.cost_source).to_string(),
        ]);
    }
    print_table(&header, &body);

    println!("\n== 按模型");
    let rows = store.stats_by_model(period)?;
    if rows.is_empty() {
        println!("(无)");
    } else {
        let header = [
            "来源",
            "模型",
            "请求",
            "input",
            "output",
            "cache_read",
            "cache_write",
            "成本(USD)",
        ];
        let body: Vec<Vec<String>> = rows
            .iter()
            .map(|b| {
                vec![
                    b.provider.as_str().to_string(),
                    b.key.clone(),
                    b.usage.requests.to_string(),
                    fmt_num(b.usage.input_tokens),
                    fmt_num(b.usage.output_tokens),
                    fmt_num(b.usage.cache_read_tokens),
                    fmt_num(b.usage.cache_write_tokens),
                    fmt_usd(b.cost_usd),
                ]
            })
            .collect();
        print_table(&header, &body);
    }

    println!("\n== 按项目（前 15）");
    let rows = store.stats_by_project(period, 15)?;
    if rows.is_empty() {
        println!("(无)");
    } else {
        let header = ["来源", "项目", "请求", "tokens", "成本(USD)"];
        let body: Vec<Vec<String>> = rows
            .iter()
            .map(|b| {
                vec![
                    b.provider.as_str().to_string(),
                    truncate(
                        if b.key.is_empty() {
                            "(无 cwd)"
                        } else {
                            &b.key
                        },
                        30,
                    ),
                    b.usage.requests.to_string(),
                    fmt_num(b.usage.total_tokens()),
                    fmt_usd(b.cost_usd),
                ]
            })
            .collect();
        print_table(&header, &body);
    }

    println!("\n== 每日（最近 {daily_days} 天）");
    let rows = store.stats_daily(daily_days)?;
    if rows.is_empty() {
        println!("(无)");
        return Ok(());
    }
    let header = [
        "日期",
        "来源",
        "模型",
        "消息",
        "input",
        "output",
        "cache_read",
        "cache_write",
        "成本(USD)",
    ];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|d| {
            vec![
                d.day.clone(),
                d.provider.as_str().to_string(),
                d.model.clone(),
                d.message_count.to_string(),
                fmt_num(d.usage.input_tokens),
                fmt_num(d.usage.output_tokens),
                fmt_num(d.usage.cache_read_tokens),
                fmt_num(d.usage.cache_write_tokens),
                fmt_usd(d.cost_usd),
            ]
        })
        .collect();
    print_table(&header, &body);
    Ok(())
}

fn quota(
    codex_root: Option<PathBuf>,
    statusline: Option<PathBuf>,
    refresh_claude: bool,
    refresh_codex_reset_credits: bool,
) -> Result<()> {
    if refresh_codex_reset_credits {
        println!("正在启动官方 codex 查询重置额度 …");
        agenthome_core::signals::refresh_reset_credits(None)?;
    }
    let usage_path = agenthome_core::signals::claude_usage::claude_usage_path();
    if refresh_claude {
        let claude = agenthome_core::signals::find_claude_binary().ok_or_else(|| {
            anyhow::anyhow!("找不到官方 claude 命令（~/.local/bin、/opt/homebrew/bin、PATH）")
        })?;
        println!("正在后台运行 {} /usage …", claude.display());
        agenthome_core::signals::refresh_claude_usage(
            &claude,
            &paths::default_data_dir(),
            &usage_path,
        )?;
    }
    println!("== Claude Code（/usage 快照 {}）", usage_path.display());
    match agenthome_core::signals::claude_usage_snapshot(Some(&usage_path))? {
        Some(s) => print_snapshot(&s),
        None => println!("无快照：用 --refresh-claude 跑一次，或等 app 定时刷新。"),
    }
    let sl_path = statusline.unwrap_or_else(paths::claude_statusline_path);
    println!("\n== Claude Code（statusline 快照 {}）", sl_path.display());
    match claude_statusline(Some(&sl_path))? {
        Some(s) => print_snapshot(&s),
        None => println!(
            "无快照：文件不存在。按 docs/06-statusline-安装.md 装好包装脚本并跑一次 Claude Code 后才有。"
        ),
    }
    let root = codex_root.unwrap_or_else(paths::codex_sessions_dir);
    println!("\n== Codex（rollout {}）", root.display());
    match codex_rollout_quota(Some(&root))? {
        Some(s) => print_snapshot(&s),
        None => println!("无快照：目录不存在，或最近 10 个 rollout 里没有 rate_limits 记录。"),
    }
    let reset_path = agenthome_core::signals::codex_reset_credits::codex_reset_credits_path();
    println!("\n== ChatGPT 重置额度（手动查询 {}）", reset_path.display());
    match agenthome_core::signals::load_reset_credits(Some(&reset_path))? {
        Some(r) => {
            println!(
                "可用 {} 张  查询于: {}  来源: {}",
                r.available,
                fmt_time(r.fetched_at_ms),
                r.source
            );
            for c in &r.credits {
                println!(
                    "  {}  过期: {}",
                    c.title.as_deref().unwrap_or("-"),
                    c.expires_at_ms
                        .map_or_else(|| "不过期".to_string(), fmt_time)
                );
            }
        }
        None => {
            println!("没查过：用 --refresh-codex-reset-credits 查一次，或在 app 设置页点「查询」。")
        }
    }
    Ok(())
}

fn print_snapshot(s: &agenthome_core::model::QuotaSnapshot) {
    println!(
        "套餐: {}  采集于: {}  新鲜度: {}  来源: {}",
        s.plan.as_deref().unwrap_or("-"),
        fmt_time(s.fetched_at_ms),
        match s.freshness {
            Freshness::Live => "live",
            Freshness::Cached => "cached",
            Freshness::Stale => "stale",
        },
        s.source
    );
    if s.windows.is_empty() {
        println!("(快照里没有额度窗口：API key 用户或会话尚无 API 响应)");
    }
    for w in &s.windows {
        println!(
            "  {:<12} 已用 {:>5.1}%  重置: {}",
            w.label,
            w.used_percent,
            w.resets_at_ms
                .map(fmt_time)
                .unwrap_or_else(|| "-".to_string())
        );
    }
    if let Some(c) = &s.credits {
        println!(
            "  credits: balance={} unlimited={} has_credits={}",
            c.balance.as_deref().unwrap_or("-"),
            c.unlimited,
            c.has_credits
        );
    }
}

fn activity(
    db: Option<PathBuf>,
    root: Option<PathBuf>,
    codex_root: Option<PathBuf>,
    gemini_root: Option<PathBuf>,
) -> Result<()> {
    let store = open_store(db)?;
    let (claude, codex, gemini) = providers(root, codex_root, gemini_root);
    let list: [&dyn SourceProvider; 3] = [&claude, &codex, &gemini];
    let rows = store.activity(&list)?;
    let header = ["来源", "检测到", "最近活动", "类型", "10分钟内活跃会话"];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|a| {
            vec![
                a.provider.as_str().to_string(),
                if a.detected { "是" } else { "否" }.to_string(),
                a.last_activity_ms
                    .map(fmt_time)
                    .unwrap_or_else(|| "-".to_string()),
                a.last_kind
                    .map(|k| format!("{k:?}"))
                    .unwrap_or_else(|| "-".to_string()),
                a.active_sessions.to_string(),
            ]
        })
        .collect();
    print_table(&header, &body);
    Ok(())
}

fn statusline(
    action: StatuslineAction,
    settings: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
) -> Result<()> {
    let defaults = StatuslinePaths::defaults();
    let p = StatuslinePaths {
        settings: settings.unwrap_or(defaults.settings),
        bin_dir: bin_dir.unwrap_or(defaults.bin_dir),
    };
    match action {
        StatuslineAction::Status => {
            println!("设置文件: {}", p.settings.display());
            println!("托管脚本: {}", p.script_path().display());
            print_statusline_status(&statusline_setup::status(&p));
        }
        StatuslineAction::Install => {
            let r = statusline_setup::install(&p)?;
            println!(
                "包装脚本: {}（{}）",
                r.script_path.display(),
                if r.script_written {
                    "已写入"
                } else {
                    "已是最新，未改动"
                }
            );
            if r.settings_changed {
                println!(
                    "已{} {} 的 statusLine",
                    if r.settings_created {
                        "新建设置文件并写入"
                    } else {
                        "修改"
                    },
                    p.settings.display()
                );
            } else {
                println!("statusLine 已接入，未改动设置文件");
            }
            if let Some(b) = &r.backup_path {
                println!("改动前的备份: {}", b.display());
            }
            match &r.wrapped_command {
                Some(c) => println!("原 statusline 命令已保留并由包装脚本转交: {c}"),
                None => println!("原来没有 statusline，将显示 AgentHome 的极简状态行"),
            }
            println!("当前命令: {}", r.command);
            println!(
                "下一步: 开一个新的 Claude Code 会话说一句话，再运行 `agenthome quota` 确认能看到 5 小时 / 7 天额度。"
            );
        }
        StatuslineAction::Uninstall => {
            let r = statusline_setup::uninstall(&p)?;
            if !r.settings_changed {
                println!("没有接入 AgentHome 包装脚本，未做任何改动");
                return Ok(());
            }
            match &r.restored_command {
                Some(c) => println!("statusLine 已还原为原命令: {c}"),
                None => println!("原来没有 statusline，已删除 statusLine 键"),
            }
            if let Some(b) = &r.backup_path {
                println!("改动前的备份: {}", b.display());
            }
            println!(
                "未删除托管脚本与 {} 下的额度快照；不需要可手动删除。",
                paths::signals_dir().display()
            );
        }
    }
    Ok(())
}

fn print_statusline_status(s: &StatuslineStatus) {
    match s {
        StatuslineStatus::SettingsMissing => {
            println!("状态: 设置文件不存在（安装时会新建）");
        }
        StatuslineStatus::SettingsUnreadable { error } => {
            println!("状态: 设置文件无法解析，安装 / 卸载都不会改动它");
            println!("原因: {error}");
        }
        StatuslineStatus::NotConfigured => {
            println!("状态: 未安装（当前没有 statusline）");
        }
        StatuslineStatus::OtherCommand { command } => {
            println!("状态: 未安装（当前 statusline 是其他命令，安装后会保留它的显示）");
            println!("当前命令: {command}");
        }
        StatuslineStatus::Installed {
            wrapped_command,
            script_up_to_date,
        } => {
            println!("状态: 已安装");
            println!(
                "被包装的原命令: {}",
                wrapped_command.as_deref().unwrap_or("(无)")
            );
            if !script_up_to_date {
                println!("提示: 托管脚本缺失或不是最新版本，重新运行 install 即可修复");
            }
        }
        StatuslineStatus::InstalledElsewhere {
            script_path,
            wrapped_command,
        } => {
            println!("状态: 已安装（手工接入，脚本不在托管位置）");
            println!("脚本: {script_path}");
            println!(
                "被包装的原命令: {}",
                wrapped_command.as_deref().unwrap_or("(无)")
            );
            println!("提示: 运行 install 会改指托管脚本并保留原命令");
        }
        StatuslineStatus::Unrecognized { reason } => {
            println!("状态: statusLine 无法识别，安装会拒绝，请手工处理");
            println!("原因: {reason}");
        }
    }
}

fn cost_source_str(c: CostSource) -> &'static str {
    match c {
        CostSource::Estimated => "估算",
        CostSource::Recorded => "记录",
        CostSource::Mixed => "混合",
        CostSource::Unknown => "未知",
    }
}

fn fmt_usd(v: f64) -> String {
    format!("${v:.2}")
}

fn fmt_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| ms.to_string())
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 按显示宽度截断（中文占 2 列）。
fn truncate(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max_width.saturating_sub(1) {
            out.push('…');
            return out;
        }
        w += cw;
        out.push(c);
    }
    out
}

fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.width()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(c.width());
        }
    }
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c}{}", " ".repeat(widths[i].saturating_sub(c.width()))))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", line(header.to_vec()));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for r in rows {
        println!("{}", line(r.iter().map(String::as_str).collect()));
    }
}
