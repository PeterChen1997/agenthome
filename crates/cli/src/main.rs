//! agenthome 命令行：scan / sessions / show / stats。

use agenthome_core::model::{ContentBlock, Role, SessionQuery};
use agenthome_core::{ClaudeCodeProvider, Store, ingest_claude, paths};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

#[derive(Parser)]
#[command(name = "agenthome", about = "AgentHome：本机 agent 会话归档与索引")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 扫描 Claude Code 转录并入库（增量）
    Scan {
        /// Claude Code projects 根目录，默认 ~/.claude/projects
        #[arg(long)]
        root: Option<PathBuf>,
        /// 索引数据库路径，默认 ~/AgentHome/index/agenthome.db
        #[arg(long)]
        db: Option<PathBuf>,
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
    /// 按天 × 模型统计 token
    Stats {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan { root, db } => scan(root, db),
        Cmd::Sessions {
            db,
            limit,
            query,
            project,
        } => sessions(db, limit, query, project),
        Cmd::Show { session, db } => show(db, &session),
        Cmd::Stats { db, days } => stats(db, days),
    }
}

fn open_store(db: Option<PathBuf>) -> Result<Store> {
    let path = db.unwrap_or_else(paths::default_index_db);
    Store::open(&path).with_context(|| format!("打开索引库 {}", path.display()))
}

fn scan(root: Option<PathBuf>, db: Option<PathBuf>) -> Result<()> {
    let provider = ClaudeCodeProvider::new(root.unwrap_or_else(paths::claude_projects_dir));
    if !provider.detect() {
        bail!("未找到 Claude Code 目录：{}", provider.root().display());
    }
    let mut store = open_store(db)?;
    let stderr = std::io::stderr();
    let mut progress = |p: agenthome_core::Progress| {
        if p.done.is_multiple_of(50) || p.done == p.total {
            let _ = write!(stderr.lock(), "\r已处理 {}/{} 个文件", p.done, p.total);
        }
    };
    let report = ingest_claude(&mut store, &provider, &mut progress);
    eprintln!();

    let (sessions, messages) = store.counts()?;
    println!("扫描根目录: {}", provider.root().display());
    println!("发现文件: {}", report.scanned);
    println!("重新解析: {}", report.parsed);
    println!("未变跳过: {}", report.skipped_unchanged);
    println!(
        "旧副本跳过(superseded/orphaned): {}",
        report.skipped_superseded
    );
    println!("解析警告: {} 条", report.warnings);
    for w in &report.warning_samples {
        println!("  警告: {w}");
    }
    if !report.ignored_records.is_empty() {
        let items: Vec<String> = report
            .ignored_records
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect();
        println!("非对话记录(未入库): {}", items.join(", "));
    }
    for (later, first) in &report.id_collisions {
        println!(
            "  警告: 会话 id 冲突，{} 覆盖了 {}",
            later.display(),
            first.display()
        );
    }
    println!("库内会话: {sessions}，消息: {messages}");
    println!("耗时: {:.1}s", report.elapsed.as_secs_f64());
    if !report.failed.is_empty() {
        println!("失败: {} 个文件", report.failed.len());
        for (p, e) in &report.failed {
            println!("  {}: {e}", p.display());
        }
        std::process::exit(1);
    }
    Ok(())
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
        "用量: in={} out={} cache_read={} cache_write={} reasoning={} requests={}",
        fmt_num(u.input_tokens),
        fmt_num(u.output_tokens),
        fmt_num(u.cache_read_tokens),
        fmt_num(u.cache_write_tokens),
        fmt_num(u.reasoning_tokens),
        u.requests
    );
    println!("原件: {}", session.source.path.display());
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

fn stats(db: Option<PathBuf>, days: u32) -> Result<()> {
    let store = open_store(db)?;
    let rows = store.stats_daily(days)?;
    if rows.is_empty() {
        println!("最近 {days} 天没有用量记录");
        return Ok(());
    }
    let header = [
        "日期",
        "provider",
        "模型",
        "消息",
        "input",
        "output",
        "cache_read",
        "cache_write",
        "reasoning",
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
                fmt_num(d.usage.reasoning_tokens),
            ]
        })
        .collect();
    print_table(&header, &body);
    Ok(())
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
