//! Making a theme the current one: `ThemeState::init` with its bundle, on the
//! main thread, then a redraw (architecture §5 step 4).
//!
//! Blinc's `init` swaps every token and fires its redraw callback, which
//! re-parses the stylesheets against the new variables and rebuilds the
//! tree (`blinc_app/src/windowed.rs` `init_theme`); nothing here rebuilds
//! anything itself. The bundle carries no CSS (see `muqun_theme.rs`).
//!
//! Agent brand colours are not theme tokens (`ui/styles.css` `--agent-*`),
//! so no theme can change their hue (architecture §6.1b).
//!
//! Timing: with `BLINC_TRACE_INPUT=1` this writes `mq.theme.*` lines in
//! Blinc's own trace format (`[blinc-input] <unix-µs> <step> <detail>`), so
//! the switch lines up with the runner's `present` lines on one timeline.

use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use blinc_theme::{ColorScheme, SystemSchemeWatcher, ThemeState};

use super::pack::Manifest;
use crate::config::Scheme;

/// Everything the main thread needs to switch: read and checked on the
/// tokio side.
#[derive(Clone)]
pub struct AppliedTheme {
    pub manifest: Arc<Manifest>,
    pub scheme: Scheme,
    pub cover_light: Option<Arc<[u8]>>,
    pub cover_dark: Option<Arc<[u8]>>,
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
    scheme: Scheme,
    cover_light: Option<Arc<[u8]>>,
    cover_dark: Option<Arc<[u8]>>,
}

fn current() -> &'static RwLock<Option<Current>> {
    static CURRENT: OnceLock<RwLock<Option<Current>>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(None))
}

/// The one system-scheme follower. Shared with the top bar's toggle, which
/// stops it on an explicit choice.
pub fn scheme_watcher() -> Arc<Mutex<Option<SystemSchemeWatcher>>> {
    static WATCHER: OnceLock<Arc<Mutex<Option<SystemSchemeWatcher>>>> = OnceLock::new();
    WATCHER.get_or_init(Default::default).clone()
}

/// The current theme's id.
pub fn current_theme_id() -> Option<String> {
    current().read().unwrap().as_ref().map(|c| c.id.clone())
}

/// The current theme and the scheme the owner chose for it.
pub fn current_choice() -> Option<(String, Scheme)> {
    current()
        .read()
        .unwrap()
        .as_ref()
        .map(|c| (c.id.clone(), c.scheme))
}

/// The current theme's Home hero image for the scheme on screen
/// ([`Manifest::cover_asset`]: artwork, else wallpaper, else preview).
/// Encoded image bytes (WebP/PNG/JPEG), for `image()` as a data URI.
// The Home hero's source; Home mounts it at merge (task 1.10 hand-off).
#[allow(dead_code)]
pub fn current_cover() -> Option<Arc<[u8]>> {
    let dark = ThemeState::try_get().is_some_and(|t| t.scheme() == ColorScheme::Dark);
    let guard = current().read().unwrap();
    let c = guard.as_ref()?;
    if dark {
        c.cover_dark.clone().or_else(|| c.cover_light.clone())
    } else {
        c.cover_light.clone().or_else(|| c.cover_dark.clone())
    }
}

/// `scheme` as Blinc's, resolving System to what the OS says now.
pub fn resolve(scheme: Scheme) -> ColorScheme {
    match scheme {
        Scheme::Light => ColorScheme::Light,
        Scheme::Dark => ColorScheme::Dark,
        Scheme::System => blinc_theme::detect_system_color_scheme(),
    }
}

fn unix_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            std::env::var("BLINC_TRACE_INPUT").as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        )
    })
}

/// One `[blinc-input]` line, when tracing is on.
pub fn trace(step: &str, detail: std::fmt::Arguments<'_>) {
    if trace_enabled() {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr().lock(),
            "[blinc-input] {} {step} {detail}",
            unix_us()
        );
    }
}

/// Install `applied` now. Main thread (Blinc's runner owns the theme's
/// readers there). Returns how long the swap itself took.
pub fn activate(applied: AppliedTheme) -> anyhow::Result<std::time::Duration> {
    let started = Instant::now();
    let bundle = applied.manifest.bundle()?;
    let initial = resolve(applied.scheme);
    // Follow the OS only when asked to; an explicit choice stops following.
    {
        let watcher = scheme_watcher();
        let mut watcher = watcher.lock().unwrap();
        match applied.scheme {
            Scheme::System => {
                if watcher.is_none() {
                    *watcher = Some(SystemSchemeWatcher::start());
                }
            }
            Scheme::Light | Scheme::Dark => {
                if let Some(mut w) = watcher.take() {
                    w.stop();
                }
            }
        }
    }
    trace(
        "mq.theme.init",
        format_args!("id={} scheme={initial:?}", applied.manifest.id),
    );
    ThemeState::init(bundle, initial);
    *current().write().unwrap() = Some(Current {
        id: applied.manifest.id.clone(),
        scheme: applied.scheme,
        cover_light: applied.cover_light,
        cover_dark: applied.cover_dark,
    });
    if let Some(scheduler) = blinc_animation::try_get_scheduler() {
        scheduler.request_redraw();
    }
    let took = started.elapsed();
    trace(
        "mq.theme.init.done",
        format_args!(
            "id={} swap_us={} since_request_us={}",
            applied.manifest.id,
            took.as_micros(),
            applied.requested.elapsed().as_micros()
        ),
    );
    tracing::info!(
        id = %applied.manifest.id,
        scheme = ?applied.scheme,
        swap_us = took.as_micros() as u64,
        since_request_ms = applied.requested.elapsed().as_millis() as u64,
        "theme applied"
    );
    Ok(took)
}
