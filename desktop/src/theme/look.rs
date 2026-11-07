//! The desktop's material language on top of a rune-kit [`Kit`]: glass
//! panels, light from behind, glow on what is lit, springy entrances.
//!
//! Every colour here is a kit token at some alpha, so a theme pack drives the
//! whole look. [`Look::of`] reads the owner's accessibility choices: with
//! "reduce transparency" panels turn solid, with "reduce motion" entrances and
//! pulses hold still (Rune's loops already honour the system setting; this
//! honours the app's own switch too).

use rune::{el, El, Shadow, Spring, Transition};
use rune_kit::Kit;

use crate::config::Appearance;

/// What the materials may do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub glass: bool,
    /// Backdrop blur is allowed: glass, and the window is small enough that
    /// Rune's blur textures stay within [`BLUR_PIXEL_BUDGET`].
    pub blur: bool,
    pub motion: bool,
    pub dark: bool,
}

/// The most device pixels a window may have before frosted panels fall back
/// to a translucent fill. While any element blurs its backdrop Rune keeps a
/// window-sized copy of the frame per target format (4 or 8 bytes a pixel)
/// plus a half-resolution float pyramid, never freed for the rest of the run,
/// and each new resource class commits another allocator block (64 MiB on
/// NVIDIA). A large window therefore pays ~130 MiB for the frost; a small one
/// shares blocks the frame already holds. 3 Mpx is a 1600×1200 window at 1×
/// or a 1200×800 one at 2×; the owner's full-width 2542×1396 stays above it.
pub const BLUR_PIXEL_BUDGET: f32 = 3_000_000.0;

/// Corner radius of panels and tiles.
pub const PANEL_RADIUS: f32 = 14.0;
/// Corner radius of cards inside a panel.
pub const CARD_RADIUS: f32 = 10.0;

/// A soft, slightly bouncy spring for things that arrive.
pub const ARRIVE: Spring = Spring::new(0.42, 0.18);
/// A tight spring for things that move between places.
pub const MOVE: Spring = Spring::new(0.36, 0.1);

/// Apply the owner's text size, density and corner profile to a kit: every
/// type token scales, compact density tightens line heights, and the radii
/// follow the shape. Shape and size are the owner's choices, never a theme's.
pub fn tune_kit(k: &mut Kit, appearance: &Appearance) {
    let scale = appearance.text_scale();
    let lines = if appearance.density == crate::config::Density::Compact {
        0.92
    } else {
        1.0
    };
    let style = |t: &mut rune::TextStyle| {
        t.size *= scale;
        if t.line_height > 0.0 {
            t.line_height *= scale * lines;
        }
    };
    let ty = &mut k.ty;
    for t in [
        &mut ty.ui,
        &mut ty.ui_medium,
        &mut ty.ui_compact,
        &mut ty.small,
        &mut ty.small_medium,
        &mut ty.body,
        &mut ty.body_medium,
        &mut ty.text_small,
        &mut ty.text_small_medium,
        &mut ty.text_xsmall,
        &mut ty.text_xsmall_medium,
        &mut ty.mono,
    ] {
        style(t);
    }
    let f = appearance.shape.factor();
    let r = &mut k.radius;
    r.xs *= f;
    r.sm *= f;
    r.md *= f;
    r.lg *= f;
    r.xl *= f;
}

impl Look {
    /// Decides whether backdrop blur fits the budget from the window's device
    /// pixels (`ui::fx::viewport_pixels`, 0 before the first frame). Glass goes
    /// solid once the GPU refused an allocation, so the window keeps drawing.
    pub fn of(appearance: &Appearance, dark: bool) -> Self {
        let lost = crate::ui::fx::gpu_lost();
        let pixels = crate::ui::fx::viewport_pixels();
        let glass = !appearance.reduced_transparency && !lost;
        Self {
            glass,
            blur: glass && pixels > 0.0 && pixels <= BLUR_PIXEL_BUDGET,
            motion: !appearance.reduced_motion,
            dark,
        }
    }

    /// The rim light on a panel's top edge.
    fn rim(&self, k: &Kit) -> (rune::Color, rune::Color) {
        if self.dark {
            (k.c.white.alpha(0.16), k.c.white.alpha(0.04))
        } else {
            (k.c.white.alpha(0.9), k.c.black.alpha(0.08))
        }
    }

    /// A translucent panel over the page (a frosted sheet over the theme's
    /// artwork when the window is within the blur budget, a translucent fill
    /// otherwise), with a top-lit rim. Solid under reduce transparency. At
    /// most four blurred panels per frame read their backdrop; use
    /// [`Look::card`] for the rest.
    pub fn panel(&self, k: &Kit, e: El, radius: f32) -> El {
        let (top, bottom) = self.rim(k);
        let e = e.radius(radius).clip_rounded(radius);
        if self.glass {
            // Without the blur the fill carries the frost, so it is denser.
            let tint = match (self.blur, self.dark) {
                (true, true) => 0.62,
                (true, false) => 0.70,
                (false, true) => 0.86,
                (false, false) => 0.90,
            };
            let e = if self.blur { e.backdrop_blur(18.0) } else { e };
            e.fill(k.c.surface_raised_base.alpha(tint))
                .border_gradient(1.0, top, bottom)
        } else {
            e.fill(k.c.surface_raised_base).border(1.0, k.c.border_base)
        }
    }

    /// A card: a translucent surface with a rim light, no blur.
    pub fn card(&self, k: &Kit, e: El, radius: f32) -> El {
        let (top, bottom) = self.rim(k);
        let e = e.radius(radius);
        if self.glass {
            e.fill(
                k.c.surface_raised_base
                    .alpha(if self.dark { 0.72 } else { 0.78 }),
            )
            .border_gradient(1.0, top, bottom)
        } else {
            e.fill(k.c.surface_raised_base).border(1.0, k.c.border_base)
        }
    }

    /// A quieter card for list items: the layer colour, hairline border.
    pub fn row_card(&self, k: &Kit, e: El, radius: f32) -> El {
        let fill = if self.glass {
            k.c.layer_01.alpha(if self.dark { 0.55 } else { 0.65 })
        } else {
            k.c.layer_01
        };
        e.radius(radius).fill(fill).border(1.0, k.c.border_muted)
    }

    /// Light coming through from behind `e`: two soft shadow layers in
    /// `color`, `strength` 0..=1.
    pub fn glow(&self, e: El, color: rune::Color, strength: f32) -> El {
        let s = strength.clamp(0.0, 1.0);
        if s <= 0.0 {
            return e;
        }
        // Light themes show a coloured shadow more readily: keep it softer.
        let s = if self.dark { s } else { s * 0.7 };
        e.box_shadow(Shadow {
            x: 0.0,
            y: 0.0,
            blur: 36.0,
            spread: 2.0,
            color: color.alpha(0.30 * s),
            inset: false,
        })
        .box_shadow(Shadow {
            x: 0.0,
            y: 0.0,
            blur: 10.0,
            spread: 0.0,
            color: color.alpha(0.35 * s),
            inset: false,
        })
    }

    /// The focused element's edge: a lit rim in `color` over its border.
    pub fn lit_edge(&self, e: El, color: rune::Color) -> El {
        e.border_gradient(1.5, color.alpha(0.95), color.alpha(0.35))
    }

    /// A panel's rim with a hint of `color` along its top edge: lit, not
    /// outlined (an outline in the accent reads as a warning in light themes).
    pub fn accent_rim(&self, k: &Kit, e: El, color: rune::Color) -> El {
        let (light_top, bottom) = self.rim(k);
        if self.dark {
            e.border_gradient(1.0, color.alpha(0.55), bottom)
        } else {
            // A hint of the accent mixed into the light rim, not a coloured outline.
            e.border_gradient(1.0, light_top.mix(color, 0.18).alpha(0.8), bottom)
        }
    }

    /// A small lit mark (a status dot): a tight glow sized for it.
    pub fn spark(&self, e: El, color: rune::Color) -> El {
        e.box_shadow(Shadow {
            x: 0.0,
            y: 0.0,
            blur: 12.0,
            spread: 1.0,
            color: color.alpha(if self.dark { 0.55 } else { 0.35 }),
            inset: false,
        })
    }

    /// A soft ambient light pinned behind a stage, centred at `center`
    /// (0..=1 of the stage) in `color`.
    pub fn light(&self, key: &str, center: [f32; 2], radius: [f32; 2], color: rune::Color) -> El {
        let peak = if self.dark { 0.26 } else { 0.20 };
        el().key(key.to_string())
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .radial_gradient(
                center,
                radius,
                [
                    (0.0, color.alpha(peak)),
                    (0.55, color.alpha(peak * 0.35)),
                    (1.0, color.alpha(0.0)),
                ],
            )
            .a11y_hidden()
    }

    /// An element arriving: slides up a little and fades, `i` steps late.
    pub fn arrive(&self, e: El, i: usize) -> El {
        if !self.motion {
            return e.transition(Transition::opacity());
        }
        e.transition(
            Transition::offset(0.0, 14.0)
                .combined(Transition::opacity())
                .combined(Transition::scale(0.985)),
        )
        .motion(ARRIVE)
        .enter_delay(0.045 * i as f32)
    }

    /// A gentle breathing pulse on `e` (opacity 0.55..1 over 1.8 s); still
    /// under reduce motion.
    pub fn pulse(&self, e: El) -> El {
        if !self.motion {
            return e;
        }
        e.looping(rune::LoopSpec::opacity(1.8, 0.0, 0.55, 1.0))
    }

    /// The kit's hover and press overlays on a card, with a slight press
    /// shrink.
    pub fn pressable(&self, k: &Kit, e: El) -> El {
        let e = k.interactive(e);
        if self.motion {
            e.press_scale(0.985)
        } else {
            e
        }
    }
}
