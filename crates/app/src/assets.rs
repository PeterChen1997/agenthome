//! 内置素材：栖息地背景 / 精灵、头像、字体。
//!
//! `AppAssets` 把 app 自己的图片与 gpui-kit 的默认图标合成一个 `AssetSource`，
//! `img("habitat/bg-day.png")` 这类相对路径会先查内置表，再回退到 gpui-kit。

use std::borrow::Cow;

use gpui_kit::assets::Assets as KitAssets;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, AssetSource, Result, SharedString, Styled, px};

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
/// 像素字体只有字体像素落在整屏幕像素上才清晰，在 1 倍屏的小字号上会发虚，所以只用在：
/// VT323 —— 栖息地场景里的文字（场景本身是像素画）；
pub const FONT_PIXEL_MONO: &str = "VT323";
/// Pixelify Sans —— 品牌字「AgentHome」，字号用 11px 的整数倍（它一个像素格约为字号的 1/11）。
/// 数字、倒计时、标签用 [`MonoText`]（主题等宽字体），标题用正文字体加粗。
pub const FONT_PIXEL_SANS: &str = "Pixelify Sans";

/// 像素字体（VT323 / Pixelify Sans）没有中日韩字形：含这类字符的文字会退回系统字体，
/// 却仍按像素字的大字号画（VT323 字面小，字号普遍比正文大），比周围文字大一圈。
/// 这里把 U+2E80 之后（CJK 部首、汉字、假名、谚文、全角标点）都算作「像素字体画不了」。
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| c >= '\u{2E80}')
}

/// 数字、倒计时、短标签：主题等宽字体（macOS 默认 Menlo），任何分辨率都清晰；含中文时用正文字体，
/// 否则汉字会退回系统字体却按等宽字体的度量排。像素字体（VT323 / Pixelify）在 1 倍屏上发虚，
/// 只留给品牌字与栖息地场景，见 docs/09 §1。
pub trait MonoText: Styled + Sized {
    fn mono_text(self, text: &str, size: f32, cx: &App) -> Self {
        let family = if has_cjk(text) {
            cx.theme().font_family.clone()
        } else {
            cx.theme().mono_font_family.clone()
        };
        self.font_family(family).text_size(px(size))
    }
}

impl<T: Styled> MonoText for T {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_text_never_goes_to_pixel_fonts() {
        // QA：「重置信用 3 张」的「张」、「合规 · 官方 /usage」用 VT323 时退回大号系统字体。
        assert!(has_cjk("3 张"));
        assert!(has_cjk("合规 · 官方 /usage"));
        assert!(has_cjk("+1 处"));
        assert!(has_cjk("，"));
        // 拉丁文里常见的符号仍可用像素字体。
        assert!(!has_cjk("Official /usage"));
        assert!(!has_cjk("62 · 86 incl. sub-agents"));
        assert!(!has_cjk("…⌘K 02:09:51"));
    }
}
