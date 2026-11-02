//! Themes: `.muqun-theme` colours as a Blinc theme (architecture §5).
//!
//! Blinc exports every `ColorToken` of the installed theme as a CSS variable
//! (`var(--primary)`, `var(--surface-overlay)`, `var(--text-primary)` ...),
//! which both blinc_cn's stylesheet and `ui/styles.css` read. So this module
//! never writes `:root { --primary: … }` itself: a stylesheet variable would
//! shadow the theme's, and `add_css` cannot take one back.

pub mod agents;
pub mod muqun_theme;
pub mod tokens;

use blinc_theme::{ColorScheme, SystemSchemeWatcher, ThemeState};

use crate::config::Scheme;

pub const DEFAULT_THEME_ID: &str = "cover-courier";

/// The App's Cover Courier theme manifest, copied verbatim (CC0-1.0).
pub const COVER_COURIER_JSON: &str = include_str!("../../assets/themes/cover-courier.json");

/// Install the built-in theme in `scheme`. `System` follows the OS: the
/// returned watcher keeps doing so until it is dropped or stopped.
pub fn install(scheme: Scheme) -> anyhow::Result<Option<SystemSchemeWatcher>> {
    let bundle = muqun_theme::ThemeFile::parse(COVER_COURIER_JSON)?.bundle()?;
    let (initial, watch) = match scheme {
        Scheme::Light => (ColorScheme::Light, false),
        Scheme::Dark => (ColorScheme::Dark, false),
        Scheme::System => (blinc_theme::detect_system_color_scheme(), true),
    };
    ThemeState::init(bundle, initial);
    Ok(watch.then(SystemSchemeWatcher::start))
}
