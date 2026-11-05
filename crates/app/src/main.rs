//! AgentHome 桌面端入口：解析参数、装主题、开主窗口。

mod archive;
mod assets;
mod dashboard;
mod data;
mod format;
mod habitat;
mod live;
mod message_row;
mod sqlite_source;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry, TitleBar};
use gpui_kit::{App, AppContext as _, SharedString, WindowBounds, WindowOptions, px, size};

use assets::AppAssets;
use data::{DataSource, MockDataSource};
use habitat::{HabitatSize, HabitatView};
use sqlite_source::SqliteDataSource;
use workspace::Workspace;

/// 像素栖息地主题（`docs/design/canvas.json` 的设计令牌）。
const PIXEL_HABITAT_THEME: &str = include_str!("../assets/themes/pixel-habitat.json");

const USAGE: &str = "用法：agenthome-app [--db PATH] [--mock] [--habitat-demo] [--appearance light|dark]\n  --db PATH            索引库路径，默认 ~/AgentHome/index/agenthome.db（先用 `agenthome scan` 生成）\n  --mock               使用内存 mock 数据源\n  --habitat-demo       只开一个栖息地演示窗口\n  --appearance MODE    固定明 / 暗主题，不跟随系统（调试与截图用）";

fn main() {
    let mut mock = false;
    let mut habitat_demo = false;
    let mut appearance: Option<ThemeMode> = None;
    let mut db: Option<PathBuf> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mock" => mock = true,
            "--habitat-demo" => habitat_demo = true,
            "--appearance" => match arguments.next().as_deref() {
                Some("light") => appearance = Some(ThemeMode::Light),
                Some("dark") => appearance = Some(ThemeMode::Dark),
                _ => {
                    eprintln!("--appearance 只接受 light 或 dark。\n{USAGE}");
                    std::process::exit(2);
                }
            },
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
    if habitat_demo {
        run_habitat_demo();
        return;
    }
    let demo = mock;
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
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            register_fonts(cx);
            apply_pixel_habitat_theme(cx);
            if let Some(mode) = appearance {
                Theme::change(mode, None, cx);
            }
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1400.), px(860.)), cx)),
                app_id: Some("agenthome".into()),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                if appearance.is_none() {
                    follow_system_appearance(window);
                }
                cx.new(|cx| Workspace::new(source, demo, window, cx))
            })
            .expect("打开主窗口失败");
            cx.activate(true);
        });
}

/// 栖息地演示窗口：720×372，只放一个 Hero 尺寸的 `HabitatView` 跑剧本。
fn run_habitat_demo() {
    gpui_kit::application().with_assets(AppAssets).run(|cx| {
        gpui_kit::init(cx);
        register_fonts(cx);
        apply_pixel_habitat_theme(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(720.), px(372.)), cx)),
            app_id: Some("agenthome".into()),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |_window, cx| {
            cx.new(|cx| {
                let mut view = HabitatView::new(HabitatSize::Hero, cx);
                view.set_demo(true, cx);
                view
            })
        })
        .expect("打开栖息地演示窗口失败");
        cx.activate(true);
    });
}

/// 注册随 app 打包的像素字体（VT323、Pixelify Sans）。
fn register_fonts(cx: &mut App) {
    let fonts = assets::FONTS
        .iter()
        .map(|bytes| std::borrow::Cow::Borrowed(*bytes))
        .collect();
    cx.text_system().add_fonts(fonts).expect("内置字体注册失败");
}

/// 把内置的 Pixel Habitat 注册进主题表，并设为当前明/暗模式的默认主题。
fn apply_pixel_habitat_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(PIXEL_HABITAT_THEME)
        .expect("内置 Pixel Habitat 主题解析失败");
    let find = |name: &str| {
        registry
            .themes()
            .get(&SharedString::from(name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("内置主题缺少 {name}"))
    };
    let light = find("Pixel Habitat Light");
    let dark = find("Pixel Habitat Dark");
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    // 按系统当前外观（明 / 暗）加载刚换上的主题；窗口打开后再监听变化。
    Theme::sync_system_appearance(None, cx);
}

/// 窗口外观随系统明暗切换时同步主题。订阅挂在窗口上，窗口关闭即释放。
fn follow_system_appearance(window: &mut gpui_kit::Window) {
    window
        .observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        })
        .detach();
}
