//! Making a theme the current one.
//!
//! The service reads and checks a theme on the tokio side
//! ([`super::service::load_applied`]); [`activate`] then stores its two
//! palettes where the UI reads them, on the main thread, and the UI redraws.
//! [`palette`] is that read: `ui::view` builds its kit from it each frame and
//! falls back to the neutral kit when no theme is active.
//!
//! Agent brand colours are not theme tokens ([`super::agents`]), so no theme
//! can change their hue (architecture §6.1b).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use super::color_scheme::ColorScheme;
use super::pack::{Manifest, SlotLook};
use super::tokens::ThemePalette;
use crate::config::Scheme;

/// The decoration slots this host draws; their images are read with the
/// theme so a switch never waits on the disk.
pub const DRAWN_SLOTS: &[&str] = &[
    "home.artwork",
    "home.wallpaper",
    "shell.wallpaper",
    "home.composer.background",
    "empty.artwork",
    "launch.artwork",
];

/// The chrome glyphs this host replaces.
pub const DRAWN_ICONS: &[&str] = &["chrome.settings", "home.arrow"];

/// A replacement glyph, decoded: straight RGBA, `template` when its alpha is
/// the shape and the theme supplies the colour.
#[derive(Clone)]
pub struct Glyph {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub template: bool,
    /// Which way it points as drawn, radians clockwise from right, when the
    /// pack says (`iconDirections`).
    pub direction: Option<f32>,
}

/// Everything the main thread needs to switch: read and checked on the
/// tokio side.
#[derive(Clone)]
pub struct AppliedTheme {
    pub manifest: Arc<Manifest>,
    pub scheme: Scheme,
    /// Encoded bytes by asset id, for every asset a drawn slot or the custom
    /// logo names (one copy however many slots share it).
    pub images: BTreeMap<String, Arc<[u8]>>,
    /// Each image's size in pixels (read from its header), so art can be
    /// composed before it has decoded.
    pub sizes: BTreeMap<String, [u32; 2]>,
    /// Decoded glyphs by icon name ([`DRAWN_ICONS`]).
    pub glyphs: BTreeMap<String, Glyph>,
    /// When the owner asked, for the end-to-end timing.
    pub requested: Instant,
}

impl std::fmt::Debug for AppliedTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppliedTheme")
            .field("id", &self.manifest.id)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

struct Current {
    id: String,
    manifest: Arc<Manifest>,
    scheme: Scheme,
    light: ThemePalette,
    dark: ThemePalette,
    images: BTreeMap<String, Arc<[u8]>>,
    sizes: BTreeMap<String, [u32; 2]>,
    glyphs: BTreeMap<String, Glyph>,
}

fn current() -> &'static RwLock<Option<Current>> {
    static CURRENT: OnceLock<RwLock<Option<Current>>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(None))
}

/// A slot's picture and how the pack wants it shown.
#[derive(Clone)]
pub struct Art {
    pub bytes: Arc<[u8]>,
    pub look: SlotLook,
    /// Width and height in pixels.
    pub size: [f32; 2],
}

impl Current {
    fn art(&self, slot: &str, scheme: ColorScheme) -> Option<Art> {
        let dark = scheme == ColorScheme::Dark;
        let asset = self.manifest.slot_asset(slot, dark)?;
        let [w, h] = *self.sizes.get(asset)?;
        Some(Art {
            bytes: self.images.get(asset)?.clone(),
            look: self.manifest.slot_look(slot, dark).unwrap_or_default(),
            size: [w as f32, h as f32],
        })
    }

    fn home_artwork(&self, scheme: ColorScheme) -> Option<Art> {
        let hidden = self
            .manifest
            .home_identity
            .as_ref()
            .and_then(|h| h.artwork.as_ref())
            .is_some_and(|m| m.mode == "hidden");
        (!hidden)
            .then(|| self.art("home.artwork", scheme))
            .flatten()
    }
}

fn read<T>(f: impl FnOnce(&Current) -> Option<T>) -> Option<T> {
    current().read().unwrap().as_ref().and_then(f)
}

/// The current theme's id.
pub fn current_theme_id() -> Option<String> {
    current().read().unwrap().as_ref().map(|c| c.id.clone())
}

/// The current theme and the scheme the owner chose for it.
#[allow(dead_code)]
pub fn current_choice() -> Option<(String, Scheme)> {
    current()
        .read()
        .unwrap()
        .as_ref()
        .map(|c| (c.id.clone(), c.scheme))
}

/// The current theme's palette for `scheme`, or `None` before a theme was
/// activated (the caller falls back to `Kit::neutral`).
pub fn palette(scheme: ColorScheme) -> Option<ThemePalette> {
    let guard = current().read().unwrap();
    let c = guard.as_ref()?;
    Some(match scheme {
        ColorScheme::Light => c.light,
        ColorScheme::Dark => c.dark,
    })
}

/// The current theme's terminal palette for `scheme` (`variants.<scheme>.terminal`),
/// or `None` before a theme was activated.
pub fn terminal(scheme: ColorScheme) -> Option<super::pack::Terminal> {
    read(|c| {
        let v = &c.manifest.variants;
        Some(match scheme {
            ColorScheme::Light => v.light.terminal.clone(),
            ColorScheme::Dark => v.dark.terminal.clone(),
        })
    })
}

/// Decoration `slot`'s picture in `scheme` (`home.composer.background`,
/// `empty.artwork` …), when the pack has one.
pub fn art(slot: &str, scheme: ColorScheme) -> Option<Art> {
    read(|c| c.art(slot, scheme))
}

/// The Home foreground (`home.artwork`), unless the pack turned it off
/// (`homeIdentity.artwork: hidden`).
pub fn home_artwork(scheme: ColorScheme) -> Option<Art> {
    read(|c| c.home_artwork(scheme))
}

/// The wallpaper behind the window: on Home `home.wallpaper`, else the
/// shared `shell.wallpaper`; elsewhere only `shell.wallpaper` (the App's
/// rule: Home's own wallpaper never spills onto the other screens).
pub fn backdrop(scheme: ColorScheme, home: bool) -> Option<Art> {
    read(|c| {
        home.then(|| c.art("home.wallpaper", scheme))
            .flatten()
            .or_else(|| c.art("shell.wallpaper", scheme))
    })
}

/// Startup art: `launch.artwork`, else the Home foreground.
pub fn launch(scheme: ColorScheme) -> Option<Art> {
    read(|c| {
        c.art("launch.artwork", scheme)
            .or_else(|| c.home_artwork(scheme))
    })
}

/// How the pack presents Home's brand block.
#[derive(Clone, Default)]
pub struct Identity {
    /// The Muqun mark shows (no theme, or `logo` default).
    pub mark: bool,
    /// The pack's own logo (`logo: custom`).
    pub logo: Option<Arc<[u8]>>,
    /// The pack's own name for Home (`name: custom`).
    pub name: Option<String>,
    /// Home uses a tall cover band (`homePresentation.header: cover`).
    pub cover_header: bool,
}

/// The current pack's Home identity; Muqun's own when no theme is active.
/// As in the App, a custom theme that says nothing about the logo hides it.
pub fn identity() -> Identity {
    read(|c| {
        let m = &c.manifest;
        let id = m.home_identity.as_ref();
        let mode = |f: fn(&super::pack::HomeIdentity) -> Option<&super::pack::Mode>| {
            id.and_then(f).map(|m| m.mode.as_str())
        };
        let builtin_default = c.id == super::DEFAULT_THEME_ID;
        let logo_mode = mode(|h| h.logo.as_ref());
        Some(Identity {
            mark: logo_mode == Some("default") || (logo_mode.is_none() && builtin_default),
            logo: (logo_mode == Some("custom"))
                .then(|| id?.logo.as_ref()?.asset.as_ref())
                .flatten()
                .and_then(|a| c.images.get(a).cloned()),
            name: (mode(|h| h.name.as_ref()) == Some("custom"))
                .then(|| id?.name.as_ref()?.text.clone())
                .flatten(),
            cover_header: m
                .home_presentation
                .as_ref()
                .is_none_or(|p| p.header != "standard"),
        })
    })
    .unwrap_or(Identity {
        mark: true,
        cover_header: true,
        ..Identity::default()
    })
}

/// The current pack's name and its one-line description (else author).
pub fn current_title() -> Option<(String, Option<String>)> {
    read(|c| {
        let m = &c.manifest;
        Some((
            m.name.clone(),
            m.description.clone().or_else(|| m.author.clone()),
        ))
    })
}

/// The pack's replacement for chrome glyph `name`.
pub fn glyph(name: &str) -> Option<Glyph> {
    read(|c| c.glyphs.get(name).cloned())
}

/// Install `applied` now. Main thread. Returns how long the swap took.
pub fn activate(applied: AppliedTheme) -> anyhow::Result<std::time::Duration> {
    let started = Instant::now();
    let light = applied.manifest.palette(ColorScheme::Light)?;
    let dark = applied.manifest.palette(ColorScheme::Dark)?;
    *current().write().unwrap() = Some(Current {
        id: applied.manifest.id.clone(),
        manifest: applied.manifest.clone(),
        scheme: applied.scheme,
        light,
        dark,
        images: applied.images,
        sizes: applied.sizes,
        glyphs: applied.glyphs,
    });
    let took = started.elapsed();
    tracing::info!(
        id = %applied.manifest.id,
        scheme = ?applied.scheme,
        swap_us = took.as_micros() as u64,
        since_request_ms = applied.requested.elapsed().as_millis() as u64,
        "theme applied"
    );
    Ok(took)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activating_a_builtin_makes_its_palettes_current() {
        let pack = crate::theme::store::builtin(crate::theme::DEFAULT_THEME_ID).unwrap();
        let applied = AppliedTheme {
            manifest: Arc::new(pack.manifest.clone()),
            scheme: Scheme::Dark,
            images: BTreeMap::new(),
            sizes: BTreeMap::new(),
            glyphs: BTreeMap::new(),
            requested: Instant::now(),
        };
        activate(applied).unwrap();
        assert_eq!(
            current_theme_id().as_deref(),
            Some(crate::theme::DEFAULT_THEME_ID)
        );
        let light = palette(ColorScheme::Light).unwrap();
        let dark = palette(ColorScheme::Dark).unwrap();
        assert!(light.background.luminance() > dark.background.luminance());
    }
}
