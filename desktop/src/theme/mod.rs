//! Theme packs — service stubbed for the Rune port (no Blinc token binding yet).

pub mod service;

use std::path::Path;

use crate::config::Appearance;

pub const DEFAULT_THEME_ID: &str = "cover-courier";

pub fn install(
    config_dir: &Path,
    state_dir: &Path,
    _appearance: &Appearance,
) -> anyhow::Result<()> {
    service::set_paths(config_dir, state_dir);
    Ok(())
}
