//! The product's name and mark.
//!
//! The mark is the App's default icon (`app/assets/icons/mint/icon.png`).
//! Rune sets no window icon of its own; Wayland compositors and macOS take
//! the `.desktop` entry's or the bundle's.

pub const APP_NAME: &str = "Muqun";

/// The application id: the Wayland `app_id` / X11 class, the `.desktop`
/// file's name (`muqun-desktop.desktop`) and the icon name, so the
/// compositor, the launcher and notifications all find the same entry.
pub const APP_ID: &str = "muqun-desktop";

/// The Rune revision the desktop is built against (short), for About.
pub const RUNE_REV: &str = "6a56722";

/// Compact mark for the sidebar title bar (and similar chrome).
pub const APP_MARK_PNG: &[u8] = include_bytes!("../assets/icons/app-mark-64.png");

/// The full-size icon, 1024×1024, for packaging and the launch mark.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icons/app-icon.png");
