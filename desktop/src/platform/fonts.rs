//! Where each OS keeps the fonts the UI asks for by family.
//!
//! The only place font paths (and the `cfg(target_os)` that picks them) are
//! allowed to live (architecture §10). Bundling a subset is a later task; a
//! missing file only costs the fallback chain's choice of face.

/// The families the app draws with (`ui/text.rs` names them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    /// Latin and CJK in one face, so mixed lines share a baseline.
    Ui,
    /// Code, paths, tool names.
    Mono,
    /// Serif with CJK coverage, for editorial headlines (Home's greeting).
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
