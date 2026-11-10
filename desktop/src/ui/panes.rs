//! The right-hand panes (terminal, a row's details, the inspector): as wide
//! as the owner drags them while the session column keeps room to read, and
//! how every side pane moves.

use rune::Spring;
use rune_kit::shell::{PaneMotion, PaneState};

/// What the session column keeps beside a right-hand pane, in points.
pub const MAIN_MIN: f32 = 360.0;
/// The window's edge around the main card (`SplitView::frame_inset`), and
/// around the floating sidebar.
pub const FRAME: f32 = 8.0;
/// A pane's ceiling before the window is known; [`Fit`] brings it down.
pub const UNBOUNDED: f32 = 4096.0;

/// Side panes open and close quickly and stop dead: no overshoot, no long
/// tail for the eye to follow. One spring moves both sides (`SplitView`), so
/// the sidebar gets the same brisk slide.
pub fn motion() -> PaneMotion {
    PaneMotion::new().spring(SPRING)
}

/// Instant pane motion without slide animation (e.g. for opening code editor in terminal).
pub fn instant() -> PaneMotion {
    PaneMotion::new().spring(Spring::NONE)
}

/// The spring [`motion`] moves the panes on, for what moves with them.
pub const SPRING: Spring = Spring::new(0.15, 0.0);

/// The widest a right-hand pane may be in a window `window` points wide
/// beside a sidebar `sidebar` points wide (0 when closed).
pub fn right_max(window: f32, sidebar: f32) -> f32 {
    window - sidebar - MAIN_MIN - FRAME
}

/// A right-hand pane's width as the owner chose it, kept while a narrower
/// window (or an opened sidebar) holds the pane narrower, and given back
/// when there is room again.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fit {
    /// Unset until the first fit: then the pane's width is the choice.
    want: Option<f32>,
    fitted: f32,
    max: f32,
}

/// The three right-hand panes' fits, and the window they fit in.
#[derive(Debug, Default)]
pub struct Fits {
    /// The window's width in points, once laid out.
    pub window: f32,
    pub term: Fit,
    pub inspect: Fit,
    pub right: Fit,
}

impl Fit {
    /// The width the owner chose: what to save.
    pub fn chosen(&self, p: &PaneState) -> f32 {
        match self.want {
            Some(want) if p.is_resizing() || p.width() == self.fitted => want,
            _ => p.width(),
        }
    }

    /// Keeps `p` within `max` (never below its minimum). Waits for a drag
    /// in progress to end. Whether its width changed.
    pub fn apply(&mut self, p: &mut PaneState, max: f32) -> bool {
        if p.is_resizing() || !max.is_finite() {
            return false;
        }
        // The owner dragged its edge or reset it since the last fit.
        let want = self.chosen(p);
        self.want = Some(want);
        let max = max.round().max(p.min_width());
        if max == self.max && p.width() == self.fitted {
            return false;
        }
        let before = p.width();
        *p = p.range(p.min_width(), max).with_width(want);
        self.max = max;
        self.fitted = p.width();
        p.width() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_gives_way_to_a_narrow_window_and_comes_back() {
        let mut p = PaneState::new(560.0)
            .range(360.0, UNBOUNDED)
            .with_width(1200.0);
        let mut fit = Fit::default();
        // 1440 wide beside a 260 sidebar: the session column keeps 360.
        assert!(fit.apply(&mut p, right_max(1440.0, 260.0)));
        assert_eq!(p.width(), 812.0);
        assert_eq!(p.max_width(), 812.0);
        // Wider window: back to what the owner chose, not further.
        assert!(fit.apply(&mut p, right_max(2560.0, 260.0)));
        assert_eq!(p.width(), 1200.0);
        // A drag is the owner's new choice, kept through a squeeze.
        p.set_width(900.0);
        fit.apply(&mut p, right_max(1440.0, 0.0));
        assert_eq!((p.width(), fit.chosen(&p)), (900.0, 900.0));
        fit.apply(&mut p, right_max(1000.0, 0.0));
        assert_eq!((p.width(), fit.chosen(&p)), (632.0, 900.0));
        // Never below the pane's minimum, however small the window.
        fit.apply(&mut p, right_max(500.0, 0.0));
        assert_eq!(p.width(), 360.0);
        fit.apply(&mut p, right_max(1440.0, 0.0));
        assert_eq!(p.width(), 900.0);
    }
}
