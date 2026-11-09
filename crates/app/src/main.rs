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
mod search_palette;
mod settings;
mod sqlite_source;
mod sync;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry, TitleBar};
use gpui_kit::{
    App, AppContext as _, Pixels, Point, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions, point, px, size,
};

use assets::AppAssets;
use data::{DataSource, MockDataSource};
use habitat::{HabitatSize, HabitatView};
use sqlite_source::SqliteDataSource;
use workspace::Workspace;

/// 像素栖息地主题（`docs/design/canvas.json` 的设计令牌）。
const PIXEL_HABITAT_THEME: &str = include_str!("../assets/themes/pixel-habitat.json");

/// 红绿灯位置：垂直居中在左栏顶行（`workspace::TOP_ROW_HEIGHT` = 44，灯高约 12）。
const TRAFFIC_LIGHT_POSITION: Point<Pixels> = point(px(16.), px(16.));

const USAGE: &str = "用法：agenthome-app [--db PATH] [--mock] [--habitat-demo] [--appearance light|dark] [--page dashboard|archive|settings] [--search TEXT] [--session ID] [--capture PNG]\n  --db PATH            索引库路径，默认 ~/AgentHome/index/agenthome.db（不存在时启动后自动建库）\n  --mock               使用内存 mock 数据源\n  --habitat-demo       只开一个栖息地演示窗口\n  --appearance MODE    固定明 / 暗主题，不跟随系统（调试与截图用）\n  --page PAGE          启动后打开的页面\n  --search TEXT        启动后弹出 ⌘K 面板并搜索 TEXT\n  --session ID         启动后打开该会话\n  --capture PATH       等待后把窗口离屏渲染成 PNG 并退出（需 --features capture）\n  --capture-delay SEC  截图前等待秒数，默认 4";

fn main() {
    let mut mock = false;
    let mut habitat_demo = false;
    let mut appearance: Option<ThemeMode> = None;
    let mut db: Option<PathBuf> = None;
    let mut start_page: Option<String> = None;
    let mut start_search: Option<String> = None;
    let mut start_session: Option<String> = None;
    let mut capture: Option<(PathBuf, u64)> = None;
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
            "--page" => match arguments.next().as_deref() {
                Some(page @ ("dashboard" | "archive" | "settings")) => {
                    start_page = Some(page.to_string())
                }
                _ => {
                    eprintln!("--page 只接受 dashboard、archive 或 settings。\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--session" => match arguments.next() {
                Some(id) => start_session = Some(id),
                None => {
                    eprintln!("--session 需要会话 id。\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--search" => match arguments.next() {
                Some(text) => start_search = Some(text),
                None => {
                    eprintln!("--search 需要一个关键词。\n{USAGE}");
                    std::process::exit(2);
                }
            },
            // 离屏截图：等待 N 秒后把窗口渲染成 PNG 并退出（需 `--features capture`）。
            "--capture" => match arguments.next() {
                Some(path) => {
                    let delay = capture.as_ref().map_or(4, |(_, delay)| *delay);
                    capture = Some((PathBuf::from(path), delay));
                }
                None => {
                    eprintln!("--capture 需要一个输出路径。\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--capture-delay" => match arguments.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(delay) => {
                    let path = capture.take().map(|(path, _)| path).unwrap_or_default();
                    capture = Some((path, delay));
                }
                None => {
                    eprintln!("--capture-delay 需要秒数。\n{USAGE}");
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
                titlebar: Some(TitlebarOptions {
                    traffic_light_position: Some(TRAFFIC_LIGHT_POSITION),
                    ..TitleBar::title_bar_options()
                }),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                if appearance.is_none() {
                    follow_system_appearance(window);
                }
                let workspace = cx.new(|cx| Workspace::new(source, demo, sync_db, window, cx));
                if start_page.is_some() || start_search.is_some() || start_session.is_some() {
                    let workspace = workspace.clone();
                    let page = start_page.clone();
                    let search = start_search.clone();
                    let session = start_session.clone();
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.apply_startup(page.as_deref(), search, session, window, cx)
                        });
                    });
                }
                if let Some((path, delay)) = capture.clone() {
                    schedule_capture(path, delay, window, cx);
                }
                workspace
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

/// 等 `delay` 秒（数据加载、动画落定）后把当前帧渲染成 PNG，然后退出。
#[cfg(feature = "capture")]
fn schedule_capture(path: PathBuf, delay: u64, window: &mut gpui_kit::Window, cx: &mut App) {
    let timer = cx
        .background_executor()
        .timer(std::time::Duration::from_secs(delay));
    window
        .spawn(cx, async move |cx| {
            timer.await;
            // 锁屏或窗口不可见时系统不驱动重绘，`rendered_frame` 会停在很早的一帧；
            // 主动画两帧（中间让出一点时间给异步任务），再取最新一帧。
            for _ in 0..2 {
                cx.update(|window, cx| window.draw(cx).clear(cx)).ok();
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
            }
            let result = cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.render_to_image()
            });
            match result {
                Ok(Ok(image)) => match image.save(&path) {
                    Ok(()) => eprintln!("已截图：{}", path.display()),
                    Err(error) => eprintln!("截图保存失败：{error}"),
                },
                Ok(Err(error)) => eprintln!("截图渲染失败：{error:#}"),
                Err(error) => eprintln!("截图时窗口已关闭：{error:#}"),
            }
            cx.update(|_, cx| cx.quit()).ok();
        })
        .detach();
}

#[cfg(not(feature = "capture"))]
fn schedule_capture(_: PathBuf, _: u64, _: &mut gpui_kit::Window, _: &mut App) {
    eprintln!("--capture 需要用 `cargo build -p agenthome-app --features capture` 构建。");
}
