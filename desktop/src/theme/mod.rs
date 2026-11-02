//! Themes: `.muqun-theme` packs as Blinc themes (architecture §5).
//!
//! * [`pack`]: read and check a package or a `theme.json` (the App's rules).
//! * [`store`]: the built-in set and `<config>/themes/<id>/`.
//! * [`gallery`]: the published catalogue at muqun.dev.
//! * [`service`]: the tokio side of every `Command::Theme*`.
//! * [`apply`]: the switch itself, on the main thread.
//! * [`muqun_theme`] / [`tokens`]: the colours as Blinc tokens.
//!
//! Blinc exports every `ColorToken` of the installed theme as a CSS variable
//! (`var(--primary)`, `var(--surface-overlay)`, `var(--text-primary)` ...),
//! which both blinc_cn's stylesheet and `ui/styles.css` read. So this module
//! never writes `:root { --primary: … }` itself: a stylesheet variable would
//! shadow the theme's, and `add_css` cannot take one back.

pub mod agents;
pub mod apply;
pub mod gallery;
pub mod muqun_theme;
pub mod pack;
pub mod service;
pub mod store;
pub mod tokens;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use blinc_theme::SystemSchemeWatcher;

use crate::config::Appearance;

pub const DEFAULT_THEME_ID: &str = "cover-courier";

/// The App's Cover Courier manifest, copied verbatim (CC0-1.0). The
/// reference the token mapping is tested against; the built-in pack
/// (`assets/themes/builtin/cover-courier.muqun-theme`) is what runs.
#[cfg(test)]
pub const COVER_COURIER_JSON: &str = include_str!("../../assets/themes/cover-courier.json");

/// Install the configured theme before the window opens; the default when
/// it is missing or does not read (said in the log, never fatal). Returns
/// the system-scheme follower, running when the scheme is `System`.
pub fn install(
    config_dir: &Path,
    state_dir: &Path,
    appearance: &Appearance,
) -> anyhow::Result<Arc<Mutex<Option<SystemSchemeWatcher>>>> {
    service::set_paths(config_dir, state_dir);
    let store = store::ThemeStore::open(service::store_dir(config_dir));
    let started = Instant::now();
    let applied = service::load_applied(&store, &appearance.theme_id, appearance.scheme, started)
        .or_else(|err| {
        tracing::warn!(
            id = %appearance.theme_id,
            "theme unavailable, using {DEFAULT_THEME_ID}: {err:#}"
        );
        service::load_applied(&store, DEFAULT_THEME_ID, appearance.scheme, started)
    })?;
    apply::activate(applied)?;
    Ok(apply::scheme_watcher())
}
