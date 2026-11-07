//! Where each OS keeps the fonts the UI asks for by family.
//!
//! The only place font paths (and the `cfg(target_os)` that picks them) are
//! allowed to live (architecture §10). Bundling a subset is a later task; a
//! missing file only costs the fallback chain's choice of face.

use std::sync::OnceLock;

/// The families the app draws with (`ui/text.rs` names them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    /// Latin and CJK in one face, so mixed lines share a baseline.
    Ui,
    /// Code, paths, tool names.
    Mono,
    /// Serif with CJK coverage, for editorial headlines (Home's greeting).
    #[allow(dead_code)]
    Display,
}

/// Files to try, in order, for `role` on this OS. The first that loads wins.
pub fn candidates(role: FontRole) -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        match role {
            FontRole::Ui => &[
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            ],
            FontRole::Mono => &[
                "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
                "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMonoNerdFont-Regular.ttf",
            ],
            FontRole::Display => &[
                "/usr/share/fonts/noto-cjk/NotoSerifCJK-Medium.ttc",
                "/usr/share/fonts/opentype/noto/NotoSerifCJK-Medium.ttc",
                "/usr/share/fonts/google-noto-cjk/NotoSerifCJK-Medium.ttc",
            ],
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = role;
        &[]
    }
}

/// The family list a slot asks for by `role`, CSS style: the embedded face
/// first, then what the platform resolves when it is missing.
pub fn family(role: FontRole) -> &'static str {
    match role {
        // Inter (Rune's embedded face) for Latin, the system's CJK face for
        // the glyphs it lacks, so Latin keeps its rhythm and CJK still shows.
        FontRole::Ui => "Inter, Noto Sans CJK SC, Noto Sans CJK JP",
        FontRole::Mono => "JetBrainsMono Nerd Font, JetBrainsMono NF, ui-monospace",
        FontRole::Display => "Noto Serif CJK SC, Inter",
    }
}

/// The first file of `candidates(role)` that reads, kept for the process
/// (Rune takes font bytes for its lifetime). `None` leaves the platform
/// fallback in charge.
pub fn load(role: FontRole) -> Option<&'static [u8]> {
    for path in candidates(role) {
        if let Ok(bytes) = std::fs::read(path) {
            tracing::debug!(?role, path, "font loaded");
            return Some(Box::leak(bytes.into_boxed_slice()));
        }
    }
    None
}

/// Every face the UI embeds at launch (`AppSpec::fonts`): the mono face, so
/// code, commands and diffs share one width wherever the system's monospace
/// differs. The UI face comes from the family list (Rune's Inter, the
/// system's CJK face by glyph); its multi-megabyte TTC is never read here.
pub fn embedded() -> &'static [&'static [u8]] {
    static FONTS: OnceLock<Vec<&'static [u8]>> = OnceLock::new();
    FONTS.get_or_init(|| [FontRole::Mono].into_iter().filter_map(load).collect())
}

/// The two slot families (`AppSpec::families`): the UI face and the mono face.
pub fn families() -> (&'static str, &'static str) {
    (family(FontRole::Ui), family(FontRole::Mono))
}
