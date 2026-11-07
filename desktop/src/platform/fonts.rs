//! Where each OS keeps the fonts the UI asks for by family.
//!
//! The only place font paths (and the `cfg(target_os)` that picks them) are
//! allowed to live (architecture §10). Bundling a subset is a later task; a
//! missing file only costs the fallback chain's choice of face.

// `candidates` stays for a bundled subset later; the slots resolve by name.
#![allow(dead_code)]

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
        // Rune's default UI face; CJK comes through its script fallbacks, so
        // the sans slot is left alone (naming a CJK family here would put
        // its Latin in front of Inter's wherever Inter is not installed).
        FontRole::Ui => "Inter",
        FontRole::Mono => "JetBrainsMono Nerd Font, JetBrainsMono NF, ui-monospace",
        FontRole::Display => "Noto Serif CJK SC, Inter",
    }
}

/// The two slot families (`AppSpec::families`): the UI face and the mono
/// face. Names, not bytes: a face registered through `AppSpec::fonts` joins
/// the glyph fallback chain of every text, which put the mono face's
/// punctuation into UI labels; fontconfig resolves the names instead and
/// `ui-monospace` is the platform's own monospace where the Nerd Font is
/// not installed.
pub fn families() -> (&'static str, &'static str) {
    (family(FontRole::Ui), family(FontRole::Mono))
}
