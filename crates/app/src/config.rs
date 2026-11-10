//! app 偏好（设置页 9.8 主题、界面语言）：存在 `~/AgentHome/config/app.json`，只放 UI 偏好，
//! 与可重建的 index 层分开。读坏了就用默认值并把原因交给调用方展示，不静默覆盖。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window};
use serde_json::{Map, Value};

/// 外观偏好。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn label(self) -> String {
        match self {
            Appearance::System => t!("settings.follow_system").into(),
            Appearance::Light => t!("settings.appearance_light").into(),
            Appearance::Dark => t!("settings.appearance_dark").into(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == key)
    }

    fn code(self) -> u8 {
        match self {
            Appearance::System => 0,
            Appearance::Light => 1,
            Appearance::Dark => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Appearance::Light,
            2 => Appearance::Dark,
            _ => Appearance::System,
        }
    }
}

/// 当前生效的外观偏好；系统外观变化的回调读它决定要不要跟随。
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn current_appearance() -> Appearance {
    Appearance::from_code(CURRENT.load(Ordering::Relaxed))
}

/// 设为当前偏好并立即套用主题。
pub fn apply_appearance(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    CURRENT.store(appearance.code(), Ordering::Relaxed);
    match appearance {
        Appearance::System => Theme::sync_system_appearance(window, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, window, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// `~/AgentHome/config/app.json`
pub fn config_path() -> PathBuf {
    agenthome_core::paths::default_data_dir()
        .join("config")
        .join("app.json")
}

/// 界面语言偏好。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    System,
    ZhCn,
    En,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::System, Language::ZhCn, Language::En];

    /// 语言名用各自的语言写，「跟随系统」随当前界面语言。
    pub fn label(self) -> String {
        match self {
            Language::System => t!("settings.follow_system").into(),
            Language::ZhCn => "简体中文".into(),
            Language::En => "English".into(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::ZhCn => "zh-CN",
            Language::En => "en",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.key() == key)
    }

    /// 实际使用的语言代码（rust-i18n 的 locale）。跟随系统时：系统首选语言是中文就用中文，否则英文。
    pub fn locale(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-CN",
            Language::En => "en",
            Language::System => system_locale(),
        }
    }
}

fn system_locale() -> &'static str {
    match sys_locale::get_locale() {
        Some(tag) if tag.to_ascii_lowercase().starts_with("zh") => "zh-CN",
        _ => "en",
    }
}

/// 设为当前界面语言（同时作用于 gpui-kit 组件，它们共用 rust-i18n 的全局 locale）。
pub fn apply_language(language: Language) {
    rust_i18n::set_locale(language.locale());
}

/// 读已保存的外观偏好。文件不存在返回默认值；文件坏了返回错误，由调用方展示。
pub fn load_appearance() -> anyhow::Result<Appearance> {
    Ok(load_key("appearance")?
        .as_deref()
        .and_then(Appearance::from_key)
        .unwrap_or_default())
}

/// 读已保存的界面语言。规则同外观。
pub fn load_language() -> anyhow::Result<Language> {
    Ok(load_key("language")?
        .as_deref()
        .and_then(Language::from_key)
        .unwrap_or_default())
}

pub fn save_appearance(appearance: Appearance) -> anyhow::Result<()> {
    save_key("appearance", appearance.key())
}

pub fn save_language(language: Language) -> anyhow::Result<()> {
    save_key("language", language.key())
}

/// 用户是否已同意「由官方 codex 查询 ChatGPT 重置额度」。只问一次；读不到按未同意。
pub fn reset_credits_consented() -> bool {
    load_key("codex_reset_credits_consent")
        .ok()
        .flatten()
        .is_some_and(|v| v == "yes")
}

pub fn save_reset_credits_consent() -> anyhow::Result<()> {
    save_key("codex_reset_credits_consent", "yes")
}

/// 是否自动检查更新（默认开；设置页可关）。读不到按开。
pub fn auto_update_check() -> bool {
    !load_key("auto_update_check")
        .ok()
        .flatten()
        .is_some_and(|v| v == "off")
}

pub fn save_auto_update_check(on: bool) -> anyhow::Result<()> {
    save_key("auto_update_check", if on { "on" } else { "off" })
}

fn load_key(key: &str) -> anyhow::Result<Option<String>> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "{}",
                t!("config.read_failed", path = path.display(), error = error)
            ));
        }
    };
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        anyhow::anyhow!(
            "{}",
            t!("config.invalid_json", path = path.display(), error = error)
        )
    })?;
    Ok(value.get(key).and_then(Value::as_str).map(str::to_string))
}

/// 保存一项偏好；保留文件里其他键。写临时文件再改名，避免写一半。
fn save_key(key: &str, value: &str) -> anyhow::Result<()> {
    let path = config_path();
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            // 坏文件不覆盖，交给用户处理。
            _ => anyhow::bail!("{}", t!("config.unparsable", path = path.display())),
        },
        Err(_) => Map::new(),
    };
    root.insert(key.to_string(), Value::String(value.to_string()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(root))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
