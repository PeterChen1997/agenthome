//! AgentHome 桌面端入口：解析参数、装主题、开主窗口。

#[macro_use]
extern crate rust_i18n;

// 界面文案在 `crates/app/locales/*.yml`（按模块拆文件，`_version: 2` 同一文件里放中英两份）。
rust_i18n::i18n!("locales", fallback = "en");

mod archive;
mod assets;
mod config;
mod dashboard;
mod data;
mod format;
mod habitat;
mod live;
mod menubar;
mod message_row;
mod pressure_badge;
mod search_palette;
mod settings;
mod sqlite_source;
mod sync;
mod updater;
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

const USAGE: &str = "Usage: agenthome-app [--db PATH] [--mock] [--habitat-demo] [--appearance light|dark] [--lang zh-CN|en] [--page dashboard|archive|settings] [--search TEXT] [--session ID] [--menubar-popup] [--capture PNG]
  --db PATH            index database, default ~/AgentHome/index/agenthome.db (created on first launch)
  --mock               use the in-memory demo data source
  --habitat-demo       open only the habitat demo window
  --appearance MODE    pin light / dark theme instead of following the system (debugging and screenshots)
  --lang LANG          pin the UI language without saving it (screenshots)
  --page PAGE          page to open on launch
  --search TEXT        open the Cmd-K palette and search TEXT on launch
  --session ID         open this session on launch
  --menubar-popup      open only the menu bar popup as a normal window (screenshots)
  --capture PATH       render the window offscreen to PNG after a delay, then quit (needs --features capture)
  --capture-delay SEC  seconds to wait before capturing, default 4";

gpui_kit::actions!(agenthome_app, [Quit]);

/// 应用菜单只放「退出 ⌘Q」：关主窗口只是隐藏（菜单栏常驻），退出要有标准入口。
fn install_app_menu(cx: &mut gpui_kit::App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([gpui_kit::KeyBinding::new("cmd-q", Quit, None)]);
    set_app_menus(cx);
}

/// 切换界面语言后重新调用，让菜单文字跟着变。
pub(crate) fn set_app_menus(cx: &mut gpui_kit::App) {
    cx.set_menus([
        gpui_kit::Menu::new("AgentHome").items([gpui_kit::MenuItem::action(
            t!("menu.quit").to_string(),
            Quit,
        )]),
    ]);
}

fn main() {
    let mut mock = false;
    let mut habitat_demo = false;
    let mut menubar_popup = false;
    let mut appearance: Option<ThemeMode> = None;
    let mut db: Option<PathBuf> = None;
    let mut start_page: Option<String> = None;
    let mut start_search: Option<String> = None;
    let mut start_session: Option<String> = None;
    let mut capture: Option<(PathBuf, u64)> = None;
    let mut language: Option<config::Language> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mock" => mock = true,
            "--habitat-demo" => habitat_demo = true,
            "--menubar-popup" => menubar_popup = true,
            "--appearance" => match arguments.next().as_deref() {
                Some("light") => appearance = Some(ThemeMode::Light),
                Some("dark") => appearance = Some(ThemeMode::Dark),
                _ => {
                    eprintln!("--appearance takes light or dark.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--lang" => match arguments.next().as_deref() {
                Some("zh-CN" | "zh") => language = Some(config::Language::ZhCn),
                Some("en") => language = Some(config::Language::En),
                _ => {
                    eprintln!("--lang takes zh-CN or en.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--db" => match arguments.next() {
                Some(path) => db = Some(PathBuf::from(path)),
                None => {
                    eprintln!("--db needs a path.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--page" => match arguments.next().as_deref() {
                Some(page @ ("dashboard" | "archive" | "settings")) => {
                    start_page = Some(page.to_string())
                }
                _ => {
                    eprintln!("--page takes dashboard, archive or settings.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--session" => match arguments.next() {
                Some(id) => start_session = Some(id),
                None => {
                    eprintln!("--session needs a session id.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--search" => match arguments.next() {
                Some(text) => start_search = Some(text),
                None => {
                    eprintln!("--search needs a query.\n{USAGE}");
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
                    eprintln!("--capture needs an output path.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--capture-delay" => match arguments.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(delay) => {
                    let path = capture.take().map(|(path, _)| path).unwrap_or_default();
                    capture = Some((path, delay));
                }
                None => {
                    eprintln!("--capture-delay needs a number of seconds.\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("Unknown argument: {other}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    // 语言最先定：之后建的数据源（包括 mock 文案）和窗口都按它渲染。命令行 --lang 不写偏好。
    match language {
        Some(language) => config::apply_language(language),
        None => match config::load_language() {
            Ok(saved) => config::apply_language(saved),
            Err(error) => {
                config::apply_language(config::Language::System);
                eprintln!("{error:#}");
            }
        },
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

    let application = gpui_kit::application().with_assets(AppAssets);
    // 主窗口关闭只是隐藏（见 `menubar::keep_running_on_close`）；点 Dock 图标时把它带回来。
    #[cfg(target_os = "macos")]
    application.on_reopen(menubar::reopen);
    application.run(move |cx| {
        gpui_kit::init(cx);
        install_app_menu(cx);
        register_fonts(cx);
        apply_pixel_habitat_theme(cx);
        // 命令行 --appearance 优先（截图用，不写偏好）；否则用设置页保存的偏好。
        match appearance {
            Some(mode) => Theme::change(mode, None, cx),
            None => match config::load_appearance() {
                Ok(saved) => config::apply_appearance(saved, None, cx),
                Err(error) => eprintln!(
                    "Could not read the appearance preference, following the system: {error:#}"
                ),
            },
        }
        if menubar_popup {
            open_menubar_popup(source, demo, appearance.is_none(), capture.clone(), cx);
            cx.activate(true);
            return;
        }
        // 截图模式不装菜单栏，也不拦关闭。
        let menubar = capture.is_none();
        #[cfg(target_os = "macos")]
        let menubar_source = source.clone();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1400.), px(860.)), cx)),
            app_id: Some("agenthome".into()),
            titlebar: Some(TitlebarOptions {
                traffic_light_position: Some(TRAFFIC_LIGHT_POSITION),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        };
        let (main_window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
            if appearance.is_none() {
                follow_system_appearance(window);
            }
            #[cfg(target_os = "macos")]
            if menubar {
                menubar::keep_running_on_close(window, cx);
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
        .expect("failed to open the main window");
        #[cfg(target_os = "macos")]
        if menubar {
            menubar::install(menubar_source, demo, main_window, workspace, cx);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (menubar, main_window, workspace);
        cx.activate(true);
    });
}

/// `--menubar-popup`：把菜单栏弹窗的内容开成普通窗口（截图、核对用，不装状态栏图标）。
fn open_menubar_popup(
    source: Arc<dyn DataSource>,
    demo: bool,
    follow_system: bool,
    capture: Option<(PathBuf, u64)>,
    cx: &mut App,
) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(px(menubar::POPUP_WIDTH), px(menubar::POPUP_HEIGHT)),
            cx,
        )),
        app_id: Some("agenthome".into()),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("AgentHome");
        if follow_system {
            follow_system_appearance(window);
        }
        if let Some((path, delay)) = capture {
            schedule_capture(path, delay, window, cx);
        }
        cx.new(|cx| {
            let mut panel = menubar::MenuBarPanel::new(source, demo, cx);
            panel.set_visible(true, cx);
            panel
        })
    })
    .expect("failed to open the menu bar popup window");
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
        .expect("failed to open the habitat demo window");
        cx.activate(true);
    });
}

/// 注册随 app 打包的像素字体（VT323、Pixelify Sans）。
fn register_fonts(cx: &mut App) {
    let fonts = assets::FONTS
        .iter()
        .map(|bytes| std::borrow::Cow::Borrowed(*bytes))
        .collect();
    cx.text_system()
        .add_fonts(fonts)
        .expect("failed to register bundled fonts");
}

/// 把内置的 Pixel Habitat 注册进主题表，并设为当前明/暗模式的默认主题。
fn apply_pixel_habitat_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(PIXEL_HABITAT_THEME)
        .expect("failed to parse the bundled Pixel Habitat theme");
    let find = |name: &str| {
        registry
            .themes()
            .get(&SharedString::from(name.to_string()))
            .cloned()
            .unwrap_or_else(|| panic!("bundled theme is missing {name}"))
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
                    Ok(()) => eprintln!("Captured: {}", path.display()),
                    Err(error) => eprintln!("Failed to save the capture: {error}"),
                },
                Ok(Err(error)) => eprintln!("Failed to render the capture: {error:#}"),
                Err(error) => eprintln!("The window closed before capture: {error:#}"),
            }
            cx.update(|_, cx| cx.quit()).ok();
        })
        .detach();
}

#[cfg(not(feature = "capture"))]
fn schedule_capture(_: PathBuf, _: u64, _: &mut gpui_kit::Window, _: &mut App) {
    eprintln!("--capture needs a build with `cargo build -p agenthome-app --features capture`.");
}
