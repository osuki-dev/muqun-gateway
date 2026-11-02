//! The hero band's cover art: the theme's own art when it has some (Cover
//! Courier's railway paper and courier figure, per scheme); themes without
//! it get a wash generated in CSS from their own tokens (`styles.css`,
//! `.mq-home-art--generated`). No colour is computed here.
//!
//! The images are handed to Blinc as file paths, not `data:` URIs: Blinc
//! keys its image cache and compares render props by the source string, and
//! a half-megabyte URI made every rebuild of the page cost ~80 ms. They are
//! written once to the cache directory from the bytes compiled in.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use blinc_theme::ColorScheme;

use crate::theme::DEFAULT_THEME_ID;

/// Cover Courier's art, from the App theme's `assets/` (CC0-1.0), as PNG
/// because Blinc's default image decoders read PNG and JPEG, not WebP:
/// - the paper, cropped to its railway band (1024×220) with the hero's
///   scrim baked into its alpha: 8 % at the left edge rising to full at
///   55 % (behind the text the page shows through), and full until 60 % of
///   the height, fading to nothing at the bottom (into the page). Blinc's
///   CSS masks reach only an image's direct parent and take two alphas, so
///   the two fades cannot be composed at run time;
/// - the figure, resized to 720×480 (480 px is the band's height at 2×);
///   its own art already dissolves at the bottom.
const FILES: [(&str, &[u8]); 4] = [
    (
        "paper-light.png",
        include_bytes!("../../../assets/themes/cover-courier/paper-light.png"),
    ),
    (
        "paper-dark.png",
        include_bytes!("../../../assets/themes/cover-courier/paper-dark.png"),
    ),
    (
        "courier-light.png",
        include_bytes!("../../../assets/themes/cover-courier/courier-light.png"),
    ),
    (
        "courier-dark.png",
        include_bytes!("../../../assets/themes/cover-courier/courier-dark.png"),
    ),
];

/// The figure's width over its height.
pub const FIGURE_ASPECT: f32 = 1.5;
/// The paper's opacity behind the page (the theme's `home.wallpaper` is
/// 0.35; the hero carries text, so a little quieter).
pub const PAPER_OPACITY: f32 = 0.3;

/// A theme's cover art, as image paths.
#[derive(Debug, Clone)]
pub struct Art {
    pub paper: String,
    pub figure: String,
}

/// Write the compiled-in files under `dir` (skipping any already there at
/// the right size). `None` when the directory cannot be written.
fn materialise(dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir).ok()?;
    for (name, bytes) in FILES {
        let path = dir.join(name);
        let current = std::fs::metadata(&path).map(|m| m.len()).ok();
        if current != Some(bytes.len() as u64) {
            let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes).ok()?;
            std::fs::rename(&tmp, &path).ok()?;
        }
    }
    Some(())
}

fn art_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = dirs::cache_dir()?
            .join(crate::platform::DIR_NAME)
            .join("themes")
            .join(DEFAULT_THEME_ID);
        match materialise(&dir) {
            Some(()) => Some(dir),
            None => {
                tracing::warn!(dir = %dir.display(), "cover art unavailable; using the generated wash");
                None
            }
        }
    })
    .as_ref()
}

/// The cover for `theme_id` in `scheme`, if the theme has one and it could
/// be prepared.
pub fn art(theme_id: &str, scheme: ColorScheme) -> Option<Art> {
    if theme_id != DEFAULT_THEME_ID {
        return None;
    }
    let dir = art_dir()?;
    let variant = match scheme {
        ColorScheme::Light => "light",
        ColorScheme::Dark => "dark",
    };
    let path = |name: String| dir.join(name).to_string_lossy().into_owned();
    Some(Art {
        paper: path(format!("paper-{variant}.png")),
        figure: path(format!("courier-{variant}.png")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_art_is_written_once_and_found_per_scheme() {
        let dir = std::env::temp_dir().join(format!("muqun-cover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        materialise(&dir).unwrap();
        for (name, bytes) in FILES {
            assert_eq!(std::fs::read(dir.join(name)).unwrap(), bytes);
        }
        materialise(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(art("some-other-theme", ColorScheme::Dark).is_none());
        assert_eq!(&FILES[2].1[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&FILES[1].1[..8], b"\x89PNG\r\n\x1a\n");
    }
}
