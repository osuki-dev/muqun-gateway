//! The product's name and mark.
//!
//! The mark is the App's default icon (`app/assets/icons/mint/icon.png`).
//! The OS window icon is not set: Blinc's `WindowConfig` has no field for
//! it yet (see README, "Later").

use std::sync::OnceLock;

use base64::Engine as _;

pub const APP_NAME: &str = "Muqun Desktop";

/// The full-size icon, 1024×1024, for packaging and the window icon later.
#[allow(dead_code)] // the window icon and packaging arrive later
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
}
