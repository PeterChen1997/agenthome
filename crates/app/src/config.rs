//! app 偏好（设置页 9.8 主题）：存在 `~/AgentHome/config/app.json`，只放 UI 偏好，
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

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "跟随系统",
            Appearance::Light => "浅色",
            Appearance::Dark => "深色",
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

/// 读已保存的外观偏好。文件不存在返回默认值；文件坏了返回错误，由调用方展示。
pub fn load_appearance() -> anyhow::Result<Appearance> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Appearance::System);
        }
        Err(error) => return Err(anyhow::anyhow!("读取 {} 失败：{error}", path.display())),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("{} 不是合法 JSON：{error}", path.display()))?;
    Ok(value
        .get("appearance")
        .and_then(Value::as_str)
        .and_then(Appearance::from_key)
        .unwrap_or_default())
}

/// 保存外观偏好；保留文件里其他键。写临时文件再改名，避免写一半。
pub fn save_appearance(appearance: Appearance) -> anyhow::Result<()> {
    let path = config_path();
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            // 坏文件不覆盖，交给用户处理。
            _ => anyhow::bail!("{} 内容无法解析，未保存", path.display()),
        },
        Err(_) => Map::new(),
    };
    root.insert(
        "appearance".to_string(),
        Value::String(appearance.key().to_string()),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(root))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
