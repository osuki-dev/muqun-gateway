//! Text in the app's two families (architecture §8.2 R7: Blinc has no global
//! default font, so every text goes through these wrappers).

use blinc_layout::prelude::*;

use crate::platform::fonts::{self, FontRole};

/// The UI family: Noto Sans CJK covers Latin and CJK with one face, so mixed
/// Chinese/English lines share one baseline and weight.
pub const UI_FONT: &str = "Noto Sans CJK SC";
pub const MONO_FONT: &str = "JetBrainsMono Nerd Font";
/// The display family for editorial headlines (Home's greeting): a serif
/// with the same CJK coverage, so a Chinese greeting reads as designed type.
pub const DISPLAY_FONT: &str = "Noto Serif CJK SC";

/// Load the families from the system, before the window opens.
pub fn register_fonts() {
    for (family, role) in [
        (UI_FONT, FontRole::Ui),
        (MONO_FONT, FontRole::Mono),
        (DISPLAY_FONT, FontRole::Display),
    ] {
        let loaded = fonts::candidates(role).iter().find_map(|path| {
            let data = std::fs::read(path).ok()?;
            let faces = blinc_app::register_font(data);
            (faces > 0).then_some((*path, faces))
        });
        match loaded {
            Some((path, faces)) => tracing::info!(family, path, faces, "font registered"),
            None => tracing::warn!(
                family,
                "font not found on this system; text falls back to the platform default"
            ),
        }
    }
}

pub fn ui_text(content: impl Into<String>) -> Text {
    text(content).font(UI_FONT)
}

pub fn display_text(content: impl Into<String>) -> Text {
    text(content).font(DISPLAY_FONT)
}

pub fn mono_text(content: impl Into<String>) -> Text {
    text(content).font(MONO_FONT)
}
