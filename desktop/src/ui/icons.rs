//! App glyphs ([`crate::ui::glyphs`]), tinted from the theme. Chrome must not
//! invent Lucide paths — the App set in `assets/glyphs` is the single source
//! (`scripts/sync-glyphs.sh`).

use blinc_cn::prelude::{ColorToken, ThemeState};
use blinc_layout::prelude::*;

use super::glyphs::{self, Glyph};

/// Theme-tinted App glyph at `size`.
pub fn tinted(glyph: Glyph, size: f32) -> Svg {
    glyphs::icon(glyph, size)
        .tint(ThemeState::get().color(ColorToken::TextSecondary))
        .flex_shrink_0()
}

/// A disclosure chevron: down when `open`, right when closed (App glyphs).
pub fn chevron(open: bool) -> impl ElementBuilder {
    tinted(
        if open {
            Glyph::DisclosureDown
        } else {
            Glyph::Next
        },
        14.0,
    )
}

/// The settings gear.
pub fn settings() -> impl ElementBuilder {
    tinted(Glyph::Settings, 16.0)
}
