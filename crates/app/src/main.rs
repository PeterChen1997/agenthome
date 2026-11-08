//! AgentHome 桌面端入口：解析参数、装主题、开主窗口。

mod archive;
mod assets;
mod config;
mod dashboard;
mod data;
mod format;
mod habitat;
mod live;
mod message_row;
mod settings;
mod sqlite_source;
mod sync;
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

const USAGE: &str = "用法：agenthome-app [--db PATH] [--mock] [--habitat-demo] [--appearance light|dark]\n  --db PATH            索引库路径，默认 ~/AgentHome/index/agenthome.db（不存在时启动后自动建库）\n  --mock               使用内存 mock 数据源\n  --habitat-demo       只开一个栖息地演示窗口\n  --appearance MODE    固定明 / 暗主题，不跟随系统（调试与截图用）";

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
    // 真实数据源同时启动同步引擎（启动扫描 + 文件监听 + 兜底扫描）；mock 不同步。
    let mut sync_db: Option<PathBuf> = None;
    let source: Arc<dyn DataSource> = if mock {
        Arc::new(MockDataSource::new())
    } else {
        let path = db.unwrap_or_else(agenthome_core::paths::default_index_db);
        // 库不存在时 `Store::open` 会建一个空库，同步引擎随后完成首次全量入库。
        match SqliteDataSource::open(&path) {
            Ok(source) => {
                sync_db = Some(path);
                Arc::new(source)
            }
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
            // 命令行 --appearance 优先（截图用，不写偏好）；否则用设置页保存的偏好。
            match appearance {
                Some(mode) => Theme::change(mode, None, cx),
                None => match config::load_appearance() {
                    Ok(saved) => config::apply_appearance(saved, None, cx),
                    Err(error) => eprintln!("外观偏好读取失败，按跟随系统处理：{error:#}"),
                },
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
                cx.new(|cx| Workspace::new(source, demo, sync_db, window, cx))
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
            // 用户在设置页固定了明 / 暗时不跟随。
            if config::current_appearance() == config::Appearance::System {
                Theme::sync_system_appearance(Some(window), cx);
            }
        })
        .detach();
}
