//! Themes: `.muqun-theme` packs as rune-kit palettes (architecture §5).
//!
//! * [`pack`]: read and check a package or a `theme.json` (the App's rules).
//! * [`store`]: the built-in set and `<config>/themes/<id>/`.
//! * [`gallery`]: the published catalogue at muqun.dev.
//! * [`service`]: the tokio side of every `Command::Theme*`.
//! * [`apply`]: the switch itself, on the main thread: the current palettes.
//! * [`muqun_theme`] / [`tokens`]: the colours as a rune-kit `Kit`.
//! * [`agents`]: the agents' brand colours, the same in every theme.
//!
//! `ui::view` builds its kit from [`apply::palette`] each frame and falls
//! back to `Kit::neutral` when no theme was activated.

#[allow(dead_code)]
pub mod agents;
pub mod apply;
pub mod color_scheme;
pub mod gallery;
pub mod look;
pub mod muqun_theme;
pub mod pack;
pub mod service;
pub mod store;
pub mod tokens;

use std::path::Path;
use std::time::Instant;

use crate::config::Appearance;

pub const DEFAULT_THEME_ID: &str = "cover-courier";

/// The App's Cover Courier manifest, copied verbatim (CC0-1.0). The
/// reference the token mapping is tested against; the built-in pack
/// (`assets/themes/builtin/cover-courier.muqun-theme`) is what runs.
#[cfg(test)]
pub const COVER_COURIER_JSON: &str = include_str!("../../assets/themes/cover-courier.json");

/// Install the configured theme before the window opens; the default when
/// it is missing or does not read (said in the log, never fatal).
pub fn install(config_dir: &Path, state_dir: &Path, appearance: &Appearance) -> anyhow::Result<()> {
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
    Ok(())
}
