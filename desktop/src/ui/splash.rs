//! Launch splash: the theme's launch art lit from behind, with the brand,
//! while the core warms up. Also the short beat after a theme switch.
//!
//! Timing is the app's (`tick`): a minimum hold so the entrance finishes, a
//! maximum so discovery never keeps the window dark, and a dissolve out. The
//! pictures move on Rune springs and loops, so reduce motion holds them still
//! (and `arm_splash` skips the splash altogether under reduce motion or
//! reduce transparency).

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use rune::{
    col, el, row, text, El, ImageCache, LoopSpec, MotionIntent, Spring, TextStyle, Transition,
};
use rune_kit::Kit;

use super::art;
use super::copy::tr_ref;
use crate::branding::APP_NAME;
use crate::config::Language;
use crate::theme::color_scheme::ColorScheme;
use crate::theme::look::Look;

/// Minimum time the boot splash stays up so the entrance can finish.
pub const BOOT_MIN: Duration = Duration::from_millis(1400);
/// Never block the UI longer than this waiting for discovery.
pub const BOOT_MAX: Duration = Duration::from_millis(4200);
/// Theme-switch splash is shorter.
pub const THEME_MIN: Duration = Duration::from_millis(900);
pub const THEME_MAX: Duration = Duration::from_millis(1800);
/// Exit dissolve length.
pub const EXIT: Duration = Duration::from_millis(620);
/// Entrance settle before we treat "hold" as started.
const ENTER: Duration = Duration::from_millis(560);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashKind {
    /// App open: wait for hub + agents (or timeout).
    Boot,
    /// Theme applied: short brand beat, no discovery wait.
    Theme,
}

#[derive(Debug, Clone)]
pub struct Splash {
    pub kind: SplashKind,
    pub started: Instant,
    pub exit_at: Option<Instant>,
}

impl Splash {
    pub fn start(kind: SplashKind) -> Self {
        Self {
            kind,
            started: Instant::now(),
            exit_at: None,
        }
    }

    fn min_max(&self) -> (Duration, Duration) {
        match self.kind {
            SplashKind::Boot => (BOOT_MIN, BOOT_MAX),
            SplashKind::Theme => (THEME_MIN, THEME_MAX),
        }
    }

    /// Whether discovery has gone far enough to reveal the shell.
    pub fn data_ready(kind: SplashKind, agents_seen: bool, hub_ready: bool) -> bool {
        match kind {
            SplashKind::Theme => true,
            SplashKind::Boot => agents_seen && hub_ready,
        }
    }

    /// Advance phases; returns whether the splash is still active.
    pub fn tick(&mut self, agents_seen: bool, hub_ready: bool) -> bool {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.started);
        let (min, max) = self.min_max();
        if self.exit_at.is_none() {
            let ready = Self::data_ready(self.kind, agents_seen, hub_ready);
            if (ready && elapsed >= min) || elapsed >= max {
                self.exit_at = Some(now);
            }
        }
        if let Some(exit) = self.exit_at {
            if now.saturating_duration_since(exit) >= EXIT {
                return false;
            }
        }
        true
    }

    pub fn view(
        &self,
        images: &ImageCache,
        k: &Kit,
        look: &Look,
        scheme: ColorScheme,
        language: Language,
        loading: bool,
    ) -> Option<El> {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let pose = if let Some(exit) = self.exit_at {
            let t = (now.saturating_duration_since(exit).as_secs_f32() / EXIT.as_secs_f32())
                .clamp(0.0, 1.0);
            let ease = t * t * (3.0 - 2.0 * t);
            Pose {
                opacity: 1.0 - ease,
                scale: 1.0 + 0.10 * ease,
                lift: -36.0 * ease,
                blur: 5.0 * ease,
                settled: 1.0,
                loading,
            }
        } else {
            let enter = (elapsed / ENTER.as_secs_f32()).clamp(0.0, 1.0);
            let ease = 1.0 - (1.0 - enter).powi(3);
            // Soft breathing after the kick settles.
            let scale = if enter < 1.0 {
                0.86 + 0.14 * ease
            } else {
                1.0 + 0.016 * (elapsed * TAU * 0.28).sin()
            };
            Pose {
                opacity: ease,
                scale,
                lift: 28.0 * (1.0 - ease),
                blur: 0.0,
                settled: enter,
                loading,
            }
        };
        if pose.opacity < 0.02 {
            return None;
        }
        Some(stage(images, k, look, scheme, language, pose))
    }
}

struct Pose {
    opacity: f32,
    scale: f32,
    lift: f32,
    blur: f32,
    /// 0..1 through the entrance, 1 after.
    settled: f32,
    loading: bool,
}

/// One layer of the hero stack, centred over the whole stage.
fn layer(key: &str, e: El) -> El {
    el().key(key.to_string())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .center()
        .child(e)
        .a11y_hidden()
}

/// A ring of light that turns slowly while the core loads.
fn orbit(k: &Kit, look: &Look, size: f32, alpha: f32, period: f64, dash: (f32, f32)) -> El {
    let ring = el()
        .size(size, size)
        .radius(size / 2.0)
        .border(1.2, k.c.bg_accent.alpha(alpha))
        .border_dash(dash.0, dash.1)
        .a11y_hidden();
    if look.motion {
        ring.looping(LoopSpec::rotation(period))
    } else {
        ring
    }
}

/// The launch art on a bloom of accent light, the brand under it.
fn stage(
    images: &ImageCache,
    k: &Kit,
    look: &Look,
    scheme: ColorScheme,
    language: Language,
    pose: Pose,
) -> El {
    let subtitle = if pose.loading {
        tr_ref(language, "Warming up agents…")
    } else {
        tr_ref(language, "Ready")
    };
    let title_font = TextStyle {
        size: 30.0,
        line_height: 36.0,
        ..k.ty.ui_medium
    };
    let accent = k.c.bg_accent;

    // The brand badge: a liquid-metal slab in the accent with the mark on it.
    let fx_on = look.glass && look.motion;
    let badge = |key: &str| {
        let mut slab = el()
            .key(key.to_string())
            .size(92.0, 92.0)
            .radius(26.0)
            .center()
            .fill(k.c.layer_02);
        if fx_on {
            slab = slab.child(
                super::fx::metal(&format!("{key}-metal"), 92.0, 26.0, accent, 0.5)
                    .absolute(0.0, 0.0),
            );
        }
        look.glow(
            slab.child(
                art::picture(
                    images,
                    std::sync::Arc::<[u8]>::from(crate::branding::APP_MARK_PNG),
                    rune::SurfaceFit::Fill,
                )
                .size(52.0, 52.0)
                .radius(14.0)
                .clip_rounded(14.0),
            ),
            accent,
            0.9,
        )
    };
    // The picture: the pack's launch art, else the badge alone.
    let launch = crate::theme::apply::launch(scheme);
    let has_art = launch.is_some();
    let hero: El = match launch {
        // A cover fills a rounded card; a contained picture stands whole,
        // its edges dissolving (a cut-out's own transparency is untouched,
        // a painting never ends on a line).
        Some(launch) if launch.look.cover => {
            art::cover(images, &launch, "splash-art", 300.0, 360.0)
                .radius(24.0)
                .clip_rounded(24.0)
                .opacity(pose.opacity * launch.look.opacity)
        }
        Some(launch) => {
            let (pic, [w, h]) = art::contained(images, &launch, 420.0, 360.0);
            let fade = w.min(h) * 0.12;
            el().key("splash-art")
                .size(420.0, 360.0)
                .center()
                .child(pic.clip_fade(fade, fade, fade, fade))
                .opacity(pose.opacity * launch.look.opacity)
        }
        None => el()
            .key("splash-art")
            .size(300.0, 360.0)
            .center()
            .child(badge("splash-badge-solo").scale(1.3))
            .opacity(pose.opacity),
    };
    let hero = hero
        .blur(pose.blur)
        .scale(pose.scale)
        .scale_motion(Spring::new(0.46, 0.26))
        .motion_intent(MotionIntent::KICK)
        .offset(0.0, pose.lift)
        .offset_motion(Spring::new(0.38, 0.16))
        .raise()
        .transition(
            Transition::scale(0.9)
                .combined(Transition::offset(0.0, 30.0))
                .combined(Transition::opacity())
                .combined(Transition::blur(6.0)),
        );

    // Light from behind: a wide bloom and a tighter core, breathing.
    let bloom = el()
        .key("splash-bloom")
        .size(520.0, 520.0)
        .radius(260.0)
        .radial_gradient(
            [0.5, 0.5],
            260.0,
            [
                (0.0, accent.alpha(0.42 * pose.opacity)),
                (0.45, accent.alpha(0.16 * pose.opacity)),
                (1.0, accent.alpha(0.0)),
            ],
        )
        .scale(0.92 + 0.08 * pose.scale)
        .scale_motion(Spring::new(0.6, 0.2))
        .a11y_hidden();
    let bloom = if look.motion {
        bloom.looping(LoopSpec::opacity(3.2, 0.0, 0.7, 1.0))
    } else {
        bloom
    };

    let rings = el()
        .key("splash-rings")
        .size(440.0, 440.0)
        .center()
        .opacity(pose.opacity * (0.35 + 0.65 * pose.settled))
        .child(orbit(k, look, 440.0, 0.22, 26.0, (46.0, 22.0)))
        .child(orbit(k, look, 380.0, 0.30, 19.0, (18.0, 14.0)).absolute(30.0, 30.0))
        .a11y_hidden();

    let mut copy = col()
        .key("splash-copy")
        .gap(8.0)
        .items_center()
        .opacity(pose.opacity * pose.settled.max(0.2))
        .offset(0.0, pose.lift * 0.4)
        .offset_motion(Spring::new(0.4, 0.12))
        .transition(Transition::offset(0.0, 16.0).combined(Transition::opacity()));
    if has_art {
        copy = copy.child(
            el().size(50.0, 50.0)
                .center()
                .child(badge("splash-badge").scale(0.5)),
        );
    }
    let copy = copy
        .child(
            text(APP_NAME.to_string())
                .font(title_font)
                .color(k.c.text_base.alpha(pose.opacity)),
        )
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(if pose.loading {
                    rune_kit::Spinner::new()
                        .size(12.0)
                        .color(k.c.text_muted)
                        .build(k)
                } else {
                    el().size(7.0, 7.0).radius(3.5).fill(k.c.success.icon)
                })
                .child(
                    text(subtitle.to_string())
                        .font(k.ty.small)
                        .color(k.c.text_muted.alpha(pose.opacity * 0.95)),
                ),
        );

    let backdrop = el()
        .key("splash-backdrop")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(k.c.bg_deep.alpha(0.94 * pose.opacity.max(0.3)))
        .radial_gradient(
            [0.5, 0.42],
            [760.0, 620.0],
            [
                (0.0, accent.alpha(0.10 * pose.opacity)),
                (1.0, accent.alpha(0.0)),
            ],
        )
        .a11y_hidden();

    el().key("theme-splash")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .center()
        .child(backdrop)
        .child(
            col()
                .key("splash-stack")
                .gap(22.0)
                .items_center()
                .child(
                    el().key("splash-hero")
                        .size(520.0, 420.0)
                        .child(layer("splash-bloom-layer", bloom))
                        .child(layer("splash-rings-layer", rings))
                        .child(layer("splash-art-layer", hero)),
                )
                .child(copy),
        )
        .a11y_hidden()
}
