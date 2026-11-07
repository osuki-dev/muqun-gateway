//! Launch splash: cool Rune motion while the core loads behind it.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use rune::{
    col, el, text, El, ImageCache, MotionIntent, Spring, SurfaceFit, TextStyle, Transition,
};
use rune_kit::Kit;

use super::art;
use crate::branding::APP_NAME;
use crate::config::Language;
use crate::theme::color_scheme::ColorScheme;

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
const ENTER: Duration = Duration::from_millis(520);

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
        scheme: ColorScheme,
        language: Language,
        loading: bool,
    ) -> Option<El> {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let (opacity, scale, lift, blur, pulse) = if let Some(exit) = self.exit_at {
            let t = (now.saturating_duration_since(exit).as_secs_f32() / EXIT.as_secs_f32())
                .clamp(0.0, 1.0);
            let ease = t * t * (3.0 - 2.0 * t);
            (1.0 - ease, 1.0 + 0.12 * ease, -42.0 * ease, 4.0 * ease, 1.0)
        } else {
            let enter = (elapsed / ENTER.as_secs_f32()).clamp(0.0, 1.0);
            let ease = 1.0 - (1.0 - enter).powi(3);
            // Soft breathing after the kick settles.
            let scale = if enter < 1.0 {
                0.84 + 0.24 * ease
            } else {
                1.0 + 0.022 * (elapsed * TAU * 0.32).sin()
            };
            (ease.clamp(0.0, 1.0), scale, 32.0 * (1.0 - ease), 0.0, enter)
        };
        if opacity < 0.02 {
            return None;
        }
        stage(
            images,
            k,
            scheme,
            language,
            SplashLook {
                opacity,
                scale,
                lift,
                blur,
                pulse,
                loading,
            },
        )
    }
}

struct SplashLook {
    opacity: f32,
    scale: f32,
    lift: f32,
    blur: f32,
    pulse: f32,
    loading: bool,
}

/// Elevated launch art with spring kick, brand, and a soft accent bloom.
fn stage(
    images: &ImageCache,
    k: &Kit,
    scheme: ColorScheme,
    language: Language,
    look: SplashLook,
) -> Option<El> {
    let bytes = crate::theme::apply::current_launch(scheme)?;
    let subtitle = if look.loading {
        match language {
            Language::En => "Warming up agents…",
            Language::ZhCn => "正在准备 Agents…",
        }
    } else {
        match language {
            Language::En => "Ready",
            Language::ZhCn => "已就绪",
        }
    };
    let title_font = TextStyle {
        size: 28.0,
        line_height: 34.0,
        ..k.ty.ui_medium
    };
    let art = art::picture(images, bytes, SurfaceFit::Fit)
        .key("splash-art")
        .w(320.0)
        .h(380.0)
        .opacity(look.opacity)
        .blur(look.blur)
        .scale(look.scale)
        .scale_motion(Spring::new(0.42, 0.28))
        .motion_intent(MotionIntent::KICK)
        .offset(0.0, look.lift)
        .offset_motion(Spring::new(0.36, 0.18))
        .raise()
        .transition(
            Transition::scale(0.88)
                .combined(Transition::offset(0.0, 36.0))
                .combined(Transition::opacity())
                .combined(Transition::blur(6.0)),
        );

    let glow = el()
        .key("splash-glow")
        .size(360.0, 360.0)
        .radius(180.0)
        .fill(
            k.c.bg_accent
                .alpha(0.12 * look.opacity * (0.55 + 0.45 * look.pulse)),
        )
        .blur(6.0)
        .scale(0.90 + 0.10 * look.scale)
        .scale_motion(Spring::new(0.55, 0.22))
        .motion_intent(MotionIntent::PULSE | MotionIntent::OVERSHOOT)
        .a11y_hidden();

    let ring = el()
        .key("splash-ring")
        .size(300.0, 300.0)
        .radius(150.0)
        .border(
            1.2,
            k.c.bg_accent
                .alpha(0.28 * look.opacity * look.pulse.max(0.2)),
        )
        .scale(look.scale)
        .scale_motion(Spring::new(0.48, 0.20))
        .opacity(look.opacity * 0.85)
        .a11y_hidden();

    let copy = col()
        .key("splash-copy")
        .gap(6.0)
        .items_center()
        .opacity(look.opacity * look.pulse.max(0.25))
        .offset(0.0, look.lift * 0.35)
        .offset_motion(Spring::new(0.40, 0.12))
        .transition(Transition::offset(0.0, 16.0).combined(Transition::opacity()))
        .child(
            text(APP_NAME.to_string())
                .font(title_font)
                .color(k.c.text_base.alpha(look.opacity)),
        )
        .child(
            text(subtitle.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted.alpha(look.opacity * 0.95)),
        );

    Some(
        el().key("theme-splash")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .fill(k.c.bg_deep.alpha(0.90 * look.opacity.max(0.30)))
            .center()
            .child(
                col()
                    .key("splash-stack")
                    .gap(20.0)
                    .items_center()
                    .child(
                        el().key("splash-hero")
                            .size(360.0, 400.0)
                            .center()
                            .child(glow)
                            .child(ring)
                            .child(art),
                    )
                    .child(copy),
            )
            .a11y_hidden(),
    )
}
