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

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use super::color_scheme::ColorScheme;
use super::pack::Manifest;
use super::tokens::ThemePalette;
use crate::config::Scheme;

/// Everything the main thread needs to switch: read and checked on the
/// tokio side.
#[derive(Clone)]
pub struct AppliedTheme {
    pub manifest: Arc<Manifest>,
    pub scheme: Scheme,
    pub wallpaper_light: Option<Arc<[u8]>>,
    pub wallpaper_dark: Option<Arc<[u8]>>,
    pub artwork_light: Option<Arc<[u8]>>,
    pub artwork_dark: Option<Arc<[u8]>>,
    pub launch_light: Option<Arc<[u8]>>,
    pub launch_dark: Option<Arc<[u8]>>,
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
    wallpaper_light: Option<Arc<[u8]>>,
    wallpaper_dark: Option<Arc<[u8]>>,
    artwork_light: Option<Arc<[u8]>>,
    artwork_dark: Option<Arc<[u8]>>,
    launch_light: Option<Arc<[u8]>>,
    launch_dark: Option<Arc<[u8]>>,
}

fn current() -> &'static RwLock<Option<Current>> {
    static CURRENT: OnceLock<RwLock<Option<Current>>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(None))
}

fn pick(dark: bool, a: &Option<Arc<[u8]>>, b: &Option<Arc<[u8]>>) -> Option<Arc<[u8]>> {
    if dark {
        a.clone().or_else(|| b.clone())
    } else {
        b.clone().or_else(|| a.clone())
    }
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

/// How the theme wants a slot's image shown (`home.wallpaper`,
/// `home.artwork`, `launch.artwork`): the manifest's fit, opacity and
/// focal point, or the defaults when the slot is unset.
pub fn slot_look(slot: &str, scheme: ColorScheme) -> super::pack::SlotLook {
    let guard = current().read().unwrap();
    guard
        .as_ref()
        .and_then(|c| c.manifest.slot_look(slot, scheme == ColorScheme::Dark))
        .unwrap_or_default()
}

/// Soft Home backdrop (`home.wallpaper`).
pub fn current_wallpaper(scheme: ColorScheme) -> Option<Arc<[u8]>> {
    let guard = current().read().unwrap();
    let c = guard.as_ref()?;
    pick(
        scheme == ColorScheme::Dark,
        &c.wallpaper_dark,
        &c.wallpaper_light,
    )
}

/// Home figure (`home.artwork`).
pub fn current_artwork(scheme: ColorScheme) -> Option<Arc<[u8]>> {
    let guard = current().read().unwrap();
    let c = guard.as_ref()?;
    pick(
        scheme == ColorScheme::Dark,
        &c.artwork_dark,
        &c.artwork_light,
    )
}

/// Startup vignette (`launch.artwork`), else the Home figure/cover.
pub fn current_launch(scheme: ColorScheme) -> Option<Arc<[u8]>> {
    let guard = current().read().unwrap();
    let c = guard.as_ref()?;
    pick(scheme == ColorScheme::Dark, &c.launch_dark, &c.launch_light)
        .or_else(|| {
            pick(
                scheme == ColorScheme::Dark,
                &c.artwork_dark,
                &c.artwork_light,
            )
        })
        .or_else(|| current_cover_from(c, scheme))
}

/// The Home hero image for the scheme on screen (artwork, else wallpaper).
pub fn current_cover(scheme: ColorScheme) -> Option<Arc<[u8]>> {
    let guard = current().read().unwrap();
    current_cover_from(guard.as_ref()?, scheme)
}

fn current_cover_from(c: &Current, scheme: ColorScheme) -> Option<Arc<[u8]>> {
    pick(
        scheme == ColorScheme::Dark,
        &c.artwork_dark,
        &c.artwork_light,
    )
    .or_else(|| {
        pick(
            scheme == ColorScheme::Dark,
            &c.wallpaper_dark,
            &c.wallpaper_light,
        )
    })
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
        wallpaper_light: applied.wallpaper_light,
        wallpaper_dark: applied.wallpaper_dark,
        artwork_light: applied.artwork_light,
        artwork_dark: applied.artwork_dark,
        launch_light: applied.launch_light,
        launch_dark: applied.launch_dark,
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
            wallpaper_light: None,
            wallpaper_dark: None,
            artwork_light: None,
            artwork_dark: None,
            launch_light: None,
            launch_dark: None,
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
