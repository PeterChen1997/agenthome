//! 内置素材：栖息地背景 / 精灵、头像、字体。
//!
//! `AppAssets` 把 app 自己的图片与 gpui-kit 的默认图标合成一个 `AssetSource`，
//! `img("habitat/bg-day.png")` 这类相对路径会先查内置表，再回退到 gpui-kit。

use std::borrow::Cow;

use gpui_kit::assets::Assets as KitAssets;
use gpui_kit::{AssetSource, Result, SharedString};

/// 一条内置素材：资源路径 → 编译期嵌入的字节。
macro_rules! embedded {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../assets/", $path)) as &'static [u8])),*]
    };
}

/// 图片素材表，路径即 `img()` 里用的字符串。
const IMAGES: &[(&str, &[u8])] = embedded![
    "habitat/bg-day.png",
    "habitat/bg-evening.png",
    "habitat/bg-night.png",
    "habitat/claude-working.png",
    "habitat/claude-thinking.png",
    "habitat/claude-sleeping.png",
    "habitat/claude-tired.png",
    "habitat/claude-happy.png",
    "habitat/codex-working.png",
    "habitat/codex-thinking.png",
    "habitat/codex-sleeping.png",
    "habitat/codex-tired.png",
    "habitat/codex-happy.png",
    "habitat/gemini-working.png",
    "habitat/gemini-thinking.png",
    "habitat/gemini-sleeping.png",
    "habitat/gemini-tired.png",
    "habitat/gemini-happy.png",
    "avatars/avatar-claude.png",
    "avatars/avatar-codex.png",
    "avatars/avatar-gemini.png",
    "avatars/app-icon.png",
];

/// 随 app 打包的 OFL 字体（VT323 Regular，Pixelify Sans Regular / Medium / SemiBold）。
pub const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/VT323-Regular.ttf"),
    include_bytes!("../assets/fonts/PixelifySans-Regular.ttf"),
    include_bytes!("../assets/fonts/PixelifySans-Medium.ttf"),
    include_bytes!("../assets/fonts/PixelifySans-SemiBold.ttf"),
];

/// 字体文件里的 family 名，`font_family()` 时用。
pub const FONT_PIXEL_MONO: &str = "VT323";
/// 区块标题用，C 块接线前暂无调用方。
#[allow(dead_code)]
pub const FONT_PIXEL_SANS: &str = "Pixelify Sans";

/// app 的素材源：内置图片 + gpui-kit 默认图标。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match IMAGES.iter().find(|(name, _)| *name == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => KitAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = IMAGES
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect();
        names.extend(KitAssets.list(path)?);
        Ok(names)
    }
}
