//! The product's name and mark.
//!
//! The mark is the App's default icon (`app/assets/icons/mint/icon.png`).
//! Rune sets no window icon of its own; Wayland compositors and macOS take
//! the `.desktop` entry's or the bundle's.

pub const APP_NAME: &str = "Muqun";

/// The full-size icon, 1024×1024, for packaging.
#[cfg(test)]
pub const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icons/app-icon.png");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_png() {
        assert_eq!(&APP_ICON_PNG[..8], b"\x89PNG\r\n\x1a\n");
    }
}
