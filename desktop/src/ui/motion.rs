//! The motion system (architecture §6.1): three duration tiers, the stagger
//! steps and caps, and the "reduce motion" downgrade -- in one place, so
//! every view refers to these and never writes its own numbers.
//!
//! Only `opacity` and `transform` are animated here (red line 2), and every
//! animation is a one-shot entrance: nothing loops, so a settled screen
//! draws no frames (red line 3).

// The shared motion system. Its only caller was the old Home dashboard; the
// start page enters without animation, so it waits here for the views that
// do animate (architecture §6.1).
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashSet;

use blinc_animation::{AnimationPreset, Easing, MultiKeyframeAnimation};
use blinc_layout::motion::{motion_derived, Motion, StaggerConfig};
use blinc_layout::prelude::*;

// Tier 1, micro (120–160 ms, ease-out: hover, press, focus, badges) is
// CSS-only: `--mq-motion-micro` in the scale block of `styles.css`.
/// Tier 2, medium (220–280 ms): cards entering, panels, tabs, popovers.
pub const MEDIUM_MS: u32 = 240;
/// Tier 3, large (320–400 ms): sidebar, sheets, theme cross-fade, covers.
pub const LARGE_MS: u32 = 320;
/// What tiers 2 and 3 become with "reduce motion": a short fade, no travel.
pub const REDUCED_MS: u32 = 120;

/// Home: the greeting rises 12 px over 280 ms (§6.0).
pub const GREETING_MS: u32 = 280;
pub const GREETING_RISE_PX: f32 = 12.0;
/// Home: cards and rows rise this far as they enter.
pub const ITEM_RISE_PX: f32 = 8.0;
/// Home: the cover fades in (tier 3).
pub const COVER_FADE_MS: u32 = LARGE_MS;

/// Launch cards cascade 30 ms apart, the first 8 only (§6.0).
pub const CARD_STAGGER: Stagger = Stagger {
    step_ms: 30,
    cap: 8,
};
/// List rows cascade 20 ms apart, the first 12 only (red line 5).
pub const ROW_STAGGER: Stagger = Stagger {
    step_ms: 20,
    cap: 12,
};

// The tiers' ranges (§6.1), checked when the crate compiles.
const _: () = assert!(MEDIUM_MS >= 220 && MEDIUM_MS <= 280);
const _: () = assert!(GREETING_MS >= 220 && GREETING_MS <= 280);
const _: () = assert!(LARGE_MS >= 320 && LARGE_MS <= 400);
const _: () = assert!(REDUCED_MS <= 120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stagger {
    pub step_ms: u32,
    /// Items at or past this index appear at once, unanimated.
    pub cap: usize,
}

impl Stagger {
    /// The delay for item `index`, or `None` when it is past the cap and
    /// should simply be shown.
    pub fn delay(self, index: usize) -> Option<u32> {
        (index < self.cap).then(|| self.step_ms * index as u32)
    }
}

/// The owner's motion preference, read from the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionPrefs {
    pub reduced: bool,
}

impl MotionPrefs {
    /// Live preference installed from Settings → Appearance.
    pub fn current() -> Self {
        Self {
            reduced: super::parts::scale::reduced_motion(),
        }
    }

    /// An entrance that rises `rise_px` while fading in over `duration_ms`
    /// (a tier-2 or tier-3 constant above). Reduced: a [`REDUCED_MS`] fade
    /// with no travel.
    pub fn rise(self, duration_ms: u32, rise_px: f32) -> MultiKeyframeAnimation {
        if self.reduced {
            AnimationPreset::fade_in(REDUCED_MS)
        } else {
            AnimationPreset::slide_in_with(duration_ms, 0.0, rise_px, Easing::EaseOutCubic)
        }
    }

    /// A plain fade over `duration_ms` (reduced: [`REDUCED_MS`]).
    pub fn fade(self, duration_ms: u32) -> MultiKeyframeAnimation {
        AnimationPreset::fade_in(if self.reduced {
            REDUCED_MS
        } else {
            duration_ms
        })
    }

    /// The delay item `index` of a cascade waits, `None` past the cap.
    /// Reduced motion has no cascade: everything fades together.
    pub fn stagger(self, stagger: Stagger, index: usize) -> Option<u32> {
        let delay = stagger.delay(index)?;
        Some(if self.reduced { 0 } else { delay })
    }
}

/// `child` entering with `animation` after `delay_ms`, keyed by `key` so a
/// rebuild of the surrounding view does not replay it; it replays only when
/// the view is mounted again.
///
/// Blinc's `Motion` takes a start delay only from its stagger, by child
/// index, so a delayed single item is a two-child stagger whose first child
/// is an empty, zero-size placeholder: child 1 then starts `delay_ms` late.
pub fn enter(
    key: &str,
    animation: MultiKeyframeAnimation,
    delay_ms: u32,
    child: impl ElementBuilder + 'static,
) -> Motion {
    let m = motion_derived(key);
    if delay_ms == 0 {
        return m.enter_animation(animation).child(child);
    }
    m.stagger(StaggerConfig::new(delay_ms, animation))
        .child_box(Box::new(div().w(0.0).h(0.0)))
        .child_box(Box::new(child))
}

/// Which entrances a view has already played during one visit, so a
/// rebuild for new data does not replay them.
///
/// Blinc keeps a keyed motion's state across rebuilds, except when a
/// rebuild patches an unchanged structure in place: that path keeps the
/// motion but drops its key (`renderer/build/subtree.rs`
/// `update_subtree_layout_from_builder`, `diff.rs`
/// `update_render_props_in_place`), and the motion then replays as a new
/// node-keyed one -- on every session update. So a view emits an entrance
/// only the first time an item appears in a visit, and nothing afterwards.
#[derive(Debug, Default)]
pub struct Visit {
    token: Option<(u64, u64)>,
    shown: HashSet<String>,
}

impl Visit {
    /// Start a visit when `token` -- whatever changes exactly when the
    /// view is left and entered again -- differs from the last one. True
    /// when a new visit began.
    pub fn begin(&mut self, token: (u64, u64)) -> bool {
        if self.token == Some(token) {
            return false;
        }
        self.token = Some(token);
        self.shown.clear();
        true
    }

    /// Whether `key` was already shown in this visit.
    pub fn has_shown(&self, key: &str) -> bool {
        self.shown.contains(key)
    }

    /// True the first time `key` is seen in this visit.
    pub fn first_time(&mut self, key: &str) -> bool {
        self.shown.insert(key.to_string())
    }
}

thread_local! {
    /// Home's visit, on the UI thread.
    pub static HOME_VISIT: RefCell<Visit> = RefCell::new(Visit::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visit_plays_each_entrance_once() {
        let mut visit = Visit::default();
        assert!(visit.begin((1, 1)));
        assert!(visit.first_time("card:codex"));
        assert!(
            !visit.first_time("card:codex"),
            "a data rebuild does not replay"
        );
        assert!(!visit.begin((1, 1)), "a resize keeps the visit");
        assert!(!visit.first_time("card:codex"));
        assert!(
            visit.first_time("card:claude"),
            "a newly arrived item enters"
        );
        assert!(visit.begin((2, 1)), "leaving and coming back");
        assert!(visit.first_time("card:codex"));
    }

    #[test]
    fn cascades_stop_at_their_caps() {
        assert_eq!(CARD_STAGGER.delay(0), Some(0));
        assert_eq!(CARD_STAGGER.delay(7), Some(210));
        assert_eq!(CARD_STAGGER.delay(8), None, "the ninth card just appears");
        assert_eq!(ROW_STAGGER.delay(11), Some(220));
        assert_eq!(ROW_STAGGER.delay(12), None);
    }

    #[test]
    fn reduced_motion_drops_travel_and_cascades() {
        let reduced = MotionPrefs { reduced: true };
        assert_eq!(reduced.stagger(CARD_STAGGER, 5), Some(0));
        assert_eq!(reduced.stagger(CARD_STAGGER, 9), None);
        let full = MotionPrefs { reduced: false };
        assert_eq!(full.stagger(ROW_STAGGER, 3), Some(60));
        assert_eq!(reduced.rise(GREETING_MS, 12.0).duration_ms(), REDUCED_MS);
        assert_eq!(full.rise(GREETING_MS, 12.0).duration_ms(), GREETING_MS);
        assert_eq!(reduced.fade(COVER_FADE_MS).duration_ms(), REDUCED_MS);
    }
}
