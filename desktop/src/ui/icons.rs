//! Unified icon registry generated from standalone SVG files under
//! `desktop/assets/glyphs/` (App-synced glyphs) and
//! `desktop/assets/glyphs/desktop/` (Desktop-specific glyphs).
//!
//! To swap or add an icon, replace or drop a standard `.svg` file in those
//! directories; `desktop/build.rs` recompiles the catalog automatically.

use rune::Icon;

#[allow(dead_code)]
pub mod catalog {
    include!(concat!(env!("OUT_DIR"), "/glyph_catalog.rs"));
}

pub use catalog::{
    by_name, ADD_PANE, AGENT, APPEARANCE, BRANCH, CONTROLS, DELETE, FORK, GRID, INBOX, INFORMATION,
    KEYBOARD, MORE, MOVE_DOWN, OPEN_EXTERNAL, PLUS, REFRESH, SETTINGS, STORAGE, TASK,
    WORKING_SPINNER as RUNNING,
};

pub(crate) fn header_button(
    glyph: &'static Icon,
    label: impl Into<String>,
) -> rune_kit::IconButton {
    let label = label.into();
    rune_kit::IconButton::new(glyph)
        .variant(rune_kit::IconButtonVariant::GhostMuted)
        .control_size(rune_kit::ControlSize::Regular)
        .label(label.clone())
        .help(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_catalog_covers_app_and_desktop_svgs() {
        assert!(
            catalog::ALL.len() >= 100,
            "expected full SVG catalog, got {}",
            catalog::ALL.len()
        );
        for icon in catalog::ALL {
            assert!(!icon.name.is_empty());
            assert!(icon.view > 0.0, "{} must have positive view", icon.name);
            assert!(
                !icon.body.is_empty() && !icon.body.contains("<svg"),
                "{} body must be unwrapped inner SVG elements",
                icon.name
            );
        }
        assert_eq!(by_name("branch").unwrap().name, BRANCH.name);
        assert_eq!(by_name("add_pane").unwrap().name, ADD_PANE.name);
        assert_eq!(by_name("window-tile").unwrap().name, "window-tile");
        assert_eq!(PLUS.view, 16.0);
        assert_eq!(catalog::MUQUN_RIBBON_LEFT.view, 32.0);
        // Child overrides like `choice.svg`'s filled center dot must stay intact.
        assert!(catalog::CHOICE.body.contains(r#"fill="currentColor""#));
        assert!(catalog::CHOICE.body.contains(r#"stroke="none""#));
    }
}
