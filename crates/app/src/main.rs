//! AgentHome 桌面端入口：解析参数、装主题、开主窗口。

mod data;
mod format;
mod message_row;
mod sqlite_source;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeRegistry, TitleBar};
use gpui_kit::{App, AppContext as _, SharedString, WindowBounds, WindowOptions, px, size};

use data::{DataSource, MockDataSource};
use sqlite_source::SqliteDataSource;
use workspace::Workspace;

/// gpui-kit 仓库自带的 macOS Classic 主题（crate 本身不打包主题文件，所以随 app 内置一份）。
const MACOS_CLASSIC_THEME: &str = include_str!("../assets/themes/macos-classic.json");

const USAGE: &str = "用法：agenthome-app [--db PATH] [--mock]\n  --db PATH  索引库路径，默认 ~/AgentHome/index/agenthome.db（先用 `agenthome scan` 生成）\n  --mock     使用内存 mock 数据源";

fn main() {
    let mut mock = false;
    let mut db: Option<PathBuf> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mock" => mock = true,
            "--db" => match arguments.next() {
                Some(path) => db = Some(PathBuf::from(path)),
                None => {
                    eprintln!("--db 需要一个路径。\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("未知参数：{other}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let source: Arc<dyn DataSource> = if mock {
        Arc::new(MockDataSource::new())
    } else {
        let path = db.unwrap_or_else(agenthome_core::paths::default_index_db);
        if !path.exists() {
            eprintln!(
                "索引库不存在：{}\n先运行 `agenthome scan` 生成，或用 --mock 启动。",
                path.display()
            );
            std::process::exit(2);
        }
        match SqliteDataSource::open(&path) {
            Ok(source) => Arc::new(source),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_macos_classic_theme(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1400.), px(860.)), cx)),
                app_id: Some("agenthome".into()),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(source, window, cx))
            })
            .expect("打开主窗口失败");
            cx.activate(true);
        });
}

/// 把内置的 macOS Classic 注册进主题表，并设为当前明/暗模式的默认主题。
fn apply_macos_classic_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(MACOS_CLASSIC_THEME)
        .expect("内置 macOS Classic 主题解析失败");
    let find = |name: &str| {
        registry
            .themes()
            .get(&SharedString::from(name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("内置主题缺少 {name}"))
    };
    let light = find("macOS Classic Light");
    let dark = find("macOS Classic Dark");
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    // `change` 会按当前模式重新加载刚换上的主题。
    let mode = cx.theme().mode;
    Theme::change(mode, None, cx);
}
