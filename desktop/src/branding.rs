//! The product's name and mark.
//!
//! The mark is the App's default icon (`app/assets/icons/mint/icon.png`),
//! also the OS window icon ([`window_icon`]).

use std::sync::OnceLock;

use base64::Engine as _;

pub const APP_NAME: &str = "Muqun Desktop";

/// The full-size icon, 1024×1024, for packaging and the window icon.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icons/app-icon.png");

/// The same mark at 64×64 (`magick app-icon.png -resize 64x64 -strip`), for
/// drawing at ~20 px: decoding and sampling the 1024² original for a toolbar
/// glyph would cost ~4 MB of texture for nothing.
pub const APP_MARK_PNG: &[u8] = include_bytes!("../assets/icons/app-mark-64.png");

/// The mark as a `data:` URI, the form Blinc's `image()` takes for in-memory
/// bytes (`blinc_layout/src/image.rs`, `Image::new`). Encoded once.
pub fn mark_data_uri() -> &'static str {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(APP_MARK_PNG)
        )
    })
}

/// Side of the window icon: taskbars and title bars draw it at 16-64 px,
/// and the full 1024² would hand the window system 4 MB of pixels.
const WINDOW_ICON_SIDE: u32 = 256;

/// The title-bar / taskbar icon. winit applies it on Windows and X11; macOS
/// takes the bundle icon and Wayland compositors the `.desktop` entry's.
pub fn window_icon() -> Option<blinc_app::WindowIcon> {
    let decoded = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)
        .inspect_err(|err| tracing::warn!(%err, "window icon unreadable"))
        .ok()?;
    let rgba = decoded
        .resize_exact(
            WINDOW_ICON_SIDE,
            WINDOW_ICON_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    blinc_app::WindowIcon::from_rgba(rgba.into_raw(), width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_marks_are_pngs() {
        for png in [APP_ICON_PNG, APP_MARK_PNG] {
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }
        assert!(mark_data_uri().starts_with("data:image/png;base64,iVBOR"));
    }

    #[test]
    fn the_window_icon_decodes_to_a_square_rgba_image() {
        let icon = window_icon().expect("icon");
        assert_eq!(
            (icon.width, icon.height),
            (WINDOW_ICON_SIDE, WINDOW_ICON_SIDE)
        );
        assert_eq!(
            icon.rgba.len(),
            (WINDOW_ICON_SIDE * WINDOW_ICON_SIDE * 4) as usize
        );
    }
}
