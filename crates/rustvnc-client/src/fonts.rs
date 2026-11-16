use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use ab_glyph::Font;
use egui::{FontData, FontDefinitions, FontFamily};

/// Install a system CJK font as a fallback for both proportional and
/// monospace text. Egui's bundled fonts have no Chinese glyphs, so without
/// this every Chinese label renders as a tofu box.
pub fn install_cjk_font(ctx: &egui::Context) -> Option<PathBuf> {
    let (path, bytes, index) = load_cjk_font()?;
    let mut fonts = FontDefinitions::default();
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    fonts
        .font_data
        .insert("rustvnc-cjk".to_string(), Arc::new(data));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("rustvnc-cjk".to_string());
    }
    ctx.set_fonts(fonts);
    Some(path)
}

fn load_cjk_font() -> Option<(PathBuf, Vec<u8>, u32)> {
    for path in candidates() {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if let Some(index) = pick_face(&bytes) {
            return Some((path, bytes, index));
        }
    }
    None
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"));
        paths.push(PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"));
        paths.push(PathBuf::from("/System/Library/Fonts/Songti.ttc"));
        paths.push(PathBuf::from("/System/Library/Fonts/STHeiti Medium.ttc"));
    }
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        let fonts = PathBuf::from(windir).join("Fonts");
        paths.push(fonts.join("msyh.ttc"));
        paths.push(fonts.join("msyhbd.ttc"));
        paths.push(fonts.join("simhei.ttf"));
        paths.push(fonts.join("simsun.ttc"));
        paths.push(fonts.join("Deng.ttf"));
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        paths.push(PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ));
        paths.push(PathBuf::from(
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ));
        paths.push(PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
        ));
    }
    paths
}

/// Pick the first face in the file that actually contains a common Chinese
/// glyph. This handles both plain TTF/OTF and TTC collections such as
/// Hiragino/微软雅黑.
fn pick_face(bytes: &[u8]) -> Option<u32> {
    for index in 0..16 {
        let Ok(font) = ab_glyph::FontRef::try_from_slice_and_index(bytes, index) else {
            break;
        };
        if font.glyph_id('中').0 != 0 {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn finds_a_chinese_face_on_this_host() {
        let (path, bytes, index) = load_cjk_font().expect("system CJK font");
        assert!(path.exists());
        let font = ab_glyph::FontRef::try_from_slice_and_index(&bytes, index).expect("font");
        assert_ne!(font.glyph_id('中').0, 0);
        assert_ne!(font.glyph_id('文').0, 0);
    }
}
