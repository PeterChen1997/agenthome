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
pub const FONT_PIXEL_MONO: &str = "VT323";
/// 区块标题用，C 块接线前暂无调用方。
#[allow(dead_code)]
pub const FONT_PIXEL_SANS: &str = "Pixelify Sans";

/// 像素字体（VT323 / Pixelify Sans）没有中日韩字形：含这类字符的文字会退回系统字体，
/// 却仍按像素字的大字号画（VT323 字面小，字号普遍比正文大），比周围文字大一圈。
/// 这里把 U+2E80 之后（CJK 部首、汉字、假名、谚文、全角标点）都算作「像素字体画不了」。
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| c >= '\u{2E80}')
}

/// 和 VT323 字号视觉等大的正文字号：VT323 字面约为同字号正文的七成。
pub fn body_size_for_pixel(size: f32) -> f32 {
    (size * 0.72).round().max(11.)
}

/// 用 VT323 显示一段可能含中文的文字：纯拉丁字符用 VT323 与 `size`；
/// 含中文时改用主题正文字体、字号换成视觉等大的正文字号。
pub trait PixelMono: Styled + Sized {
    fn pixel_mono(self, text: &str, size: f32, cx: &App) -> Self {
        if has_cjk(text) {
            self.font_family(cx.theme().font_family.clone())
                .text_size(px(body_size_for_pixel(size)))
        } else {
            self.font_family(FONT_PIXEL_MONO).text_size(px(size))
        }
    }
}

impl<T: Styled> PixelMono for T {}

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
