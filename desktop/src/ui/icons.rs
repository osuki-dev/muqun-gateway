//! Lucide icons (through `blinc_cn`), tinted from the theme. Drawn as SVG so
//! they never depend on a font having the glyph.

use blinc_cn::prelude::{icons, to_svg, ColorToken, ThemeState};
use blinc_layout::prelude::*;

const SIZE: f32 = 14.0;

fn tinted(path: &str, size: f32) -> impl ElementBuilder {
    svg(to_svg(path, size))
        .size(size, size)
        .tint(ThemeState::get().color(ColorToken::TextSecondary))
        .flex_shrink_0()
}

/// A disclosure chevron: down when `open`, right when closed.
pub fn chevron(open: bool) -> impl ElementBuilder {
    tinted(
        if open {
            icons::CHEVRON_DOWN
        } else {
            icons::CHEVRON_RIGHT
        },
        SIZE,
    )
}

/// The settings gear.
pub fn settings() -> impl ElementBuilder {
    tinted(icons::SETTINGS, 16.0)
}
