//! The windowed list's arithmetic (architecture §8.2 R2), free of Blinc so
//! it can be tested and timed on its own.
//!
//! Content coordinates: `0` is the top of the first row. Every row's height
//! is its measured height once it has been laid out, else an estimate from
//! its [`Shape`]. Rows outside the window are not built; the spacers above
//! and below them take exactly the heights this model gives those rows, so
//! after a layout the model and the laid-out list agree row for row -- which
//! is what scroll anchoring relies on.

use std::collections::HashMap;
use std::ops::Range;

use crate::model::view::{Shape, ShapeKind};

/// Pixels of the gap below every row (part of its measured height).
pub const ROW_GAP: f32 = 16.0;
/// Average advance of a body character at 15 px, CJK and Latin mixed.
const CHAR_WIDTH: f32 = 8.0;
/// A body line, comfortable density (`parts::scale`).
const BODY_LINE: f32 = 24.0;
/// Rows of a diff a collapsed card shows.
pub const DIFF_PREVIEW_LINES: u32 = 12;

/// A row's height before it has been laid out, at content width `width`.
pub fn estimate(shape: &Shape, width: f32) -> f32 {
    let per_line = ((width - 40.0).max(160.0) / CHAR_WIDTH).max(16.0);
    let wrapped = |chars: u32, lines: u32| -> f32 {
        let soft = (chars as f32 / per_line).ceil();
        soft.max(1.0) + (lines.saturating_sub(1) as f32) * 0.6
    };
    let body = match shape.kind {
        ShapeKind::Prose => wrapped(shape.chars, shape.lines) * BODY_LINE + 8.0,
        ShapeKind::User => wrapped(shape.chars, shape.lines) * BODY_LINE + 42.0,
        ShapeKind::Reasoning => 28.0,
        ShapeKind::Tool => 36.0,
        ShapeKind::Diff => shape.lines.min(DIFF_PREVIEW_LINES) as f32 * 18.0 + 64.0,
        ShapeKind::Card => shape.lines as f32 * 24.0 + 36.0,
        ShapeKind::Notice => 24.0,
    };
    body + ROW_GAP
}

/// Measured row heights of one session, by item id.
#[derive(Debug, Default, Clone)]
pub struct Heights {
    measured: HashMap<String, f32>,
}

impl Heights {
    pub fn get(&self, id: &str) -> Option<f32> {
        self.measured.get(id).copied()
    }

    /// Record a laid-out height. True when it differs from what was known.
    pub fn record(&mut self, id: &str, height: f32) -> bool {
        match self.measured.get_mut(id) {
            Some(known) if (*known - height).abs() < 0.5 => false,
            Some(known) => {
                *known = height;
                true
            }
            None => {
                self.measured.insert(id.to_string(), height);
                true
            }
        }
    }
}

/// Every row's height and the running sums: `offsets[i]` is the top of row
/// `i`, `offsets[n]` the height of all rows.
#[derive(Debug, Default, Clone)]
pub struct Model {
    pub heights: Vec<f32>,
    pub offsets: Vec<f32>,
}

impl Model {
    pub fn build(ids: &[String], shapes: &[Shape], heights: &Heights, width: f32) -> Self {
        let heights: Vec<f32> = ids
            .iter()
            .zip(shapes)
            .map(|(id, shape)| heights.get(id).unwrap_or_else(|| estimate(shape, width)))
            .collect();
        let mut model = Self {
            heights,
            offsets: Vec::new(),
        };
        model.sum();
        model
    }

    fn sum(&mut self) {
        self.offsets.clear();
        self.offsets.reserve(self.heights.len() + 1);
        let mut y = 0.0;
        self.offsets.push(0.0);
        for h in &self.heights {
            y += h;
            self.offsets.push(y);
        }
    }

    /// Change row `i`'s height. Running sums are recomputed by the caller
    /// with [`Self::resum`] once all changes of a layout are in.
    pub fn set(&mut self, i: usize, height: f32) {
        if let Some(h) = self.heights.get_mut(i) {
            *h = height;
        }
    }

    pub fn resum(&mut self) {
        self.sum();
    }

    pub fn total(&self) -> f32 {
        *self.offsets.last().unwrap_or(&0.0)
    }

    pub fn top(&self, i: usize) -> f32 {
        self.offsets.get(i).copied().unwrap_or_else(|| self.total())
    }

    /// The row containing content y `y` (clamped to the rows).
    pub fn row_at(&self, y: f32) -> usize {
        if self.heights.is_empty() {
            return 0;
        }
        // The last offset <= y.
        let i = self.offsets.partition_point(|&top| top <= y);
        i.saturating_sub(1).min(self.heights.len() - 1)
    }

    /// Rows intersecting `[top, bottom)`.
    pub fn rows_between(&self, top: f32, bottom: f32) -> Range<usize> {
        if self.heights.is_empty() {
            return 0..0;
        }
        let start = self.row_at(top.max(0.0));
        let end = (self.row_at(bottom.max(0.0)) + 1).min(self.heights.len());
        start..end.max(start)
    }

    /// The window to build for a viewport at `top` of height `height`:
    /// every row within `buffer` above or below it.
    pub fn window(&self, top: f32, height: f32, buffer: f32) -> Range<usize> {
        self.rows_between(top - buffer, top + height + buffer)
    }

    /// Whether `window` still holds the viewport with at least `margin` to
    /// spare on both sides (or reaches the end of the list on that side).
    pub fn covers(&self, window: &Range<usize>, top: f32, height: f32, margin: f32) -> bool {
        if self.heights.is_empty() {
            return window.is_empty();
        }
        if window.is_empty() || window.end > self.heights.len() {
            return false;
        }
        let above = window.start == 0 || self.top(window.start) <= (top - margin).max(0.0);
        let below = window.end == self.heights.len()
            || self.top(window.end) >= (top + height + margin).min(self.total());
        above && below
    }

    /// Heights of the spacers standing in for the rows before and after
    /// `window`.
    pub fn spacers(&self, window: &Range<usize>) -> (f32, f32) {
        let top = self.top(window.start);
        let bottom = self.total() - self.top(window.end);
        (top, bottom.max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn shapes(n: usize) -> (Vec<String>, Vec<Shape>) {
        let ids = (0..n).map(|i| format!("item-{i:05}")).collect();
        let kinds = [
            ShapeKind::User,
            ShapeKind::Prose,
            ShapeKind::Tool,
            ShapeKind::Reasoning,
            ShapeKind::Diff,
            ShapeKind::Notice,
        ];
        let shapes = (0..n)
            .map(|i| Shape {
                kind: kinds[i % kinds.len()],
                chars: (i as u32 * 37) % 1200,
                lines: (i as u32 % 9) + 1,
            })
            .collect();
        (ids, shapes)
    }

    #[test]
    fn estimates_grow_with_text_and_shrink_with_width() {
        let short = Shape {
            kind: ShapeKind::Prose,
            chars: 40,
            lines: 1,
        };
        let long = Shape {
            chars: 4000,
            ..short
        };
        assert!(estimate(&long, 800.0) > estimate(&short, 800.0));
        assert!(estimate(&long, 400.0) > estimate(&long, 1200.0));
        assert!(estimate(&short, 800.0) >= BODY_LINE + ROW_GAP);
    }

    #[test]
    fn measured_heights_replace_estimates_and_sums_follow() {
        let (ids, shapes) = shapes(4);
        let mut heights = Heights::default();
        assert!(heights.record(&ids[1], 100.0));
        assert!(
            !heights.record(&ids[1], 100.2),
            "sub-pixel noise is not a change"
        );
        let model = Model::build(&ids, &shapes, &heights, 800.0);
        assert_eq!(model.heights[1], 100.0);
        assert_eq!(model.top(2), model.heights[0] + 100.0);
        assert_eq!(model.total(), model.heights.iter().sum::<f32>());
    }

    #[test]
    fn the_window_covers_the_viewport_plus_its_buffer() {
        let ids: Vec<String> = (0..100).map(|i| i.to_string()).collect();
        let mut heights = Heights::default();
        for id in &ids {
            heights.record(id, 50.0);
        }
        let shapes = vec![
            Shape {
                kind: ShapeKind::Notice,
                chars: 0,
                lines: 1
            };
            100
        ];
        let model = Model::build(&ids, &shapes, &heights, 800.0);
        // Viewport 1000..1500 → rows 20..30; buffer 200 → rows 16..34.
        assert_eq!(model.rows_between(1000.0, 1500.0), 20..31);
        let window = model.window(1000.0, 500.0, 200.0);
        assert_eq!(window, 16..35);
        assert!(model.covers(&window, 1000.0, 500.0, 100.0));
        assert!(
            !model.covers(&window, 1200.0, 500.0, 100.0),
            "scrolled past the margin"
        );
        let (above, below) = model.spacers(&window);
        assert_eq!(above, 16.0 * 50.0);
        assert_eq!(below, (100.0 - 35.0) * 50.0);
        assert_eq!(above + below + 19.0 * 50.0, model.total());
    }

    #[test]
    fn the_ends_of_the_list_always_count_as_covered() {
        let (ids, shapes) = shapes(5);
        let model = Model::build(&ids, &shapes, &Heights::default(), 800.0);
        let all = 0..5;
        assert!(model.covers(&all, 0.0, 5000.0, 1000.0));
        assert_eq!(model.window(0.0, 5000.0, 1000.0), all);
        let empty = Model::build(&[], &[], &Heights::default(), 800.0);
        assert_eq!(empty.window(0.0, 500.0, 100.0), 0..0);
        assert!(empty.covers(&(0..0), 0.0, 500.0, 100.0));
    }

    #[test]
    fn prepending_rows_moves_an_anchor_by_exactly_their_height() {
        let (ids, shapes) = shapes(50);
        let heights = Heights::default();
        let before = Model::build(&ids, &shapes, &heights, 800.0);
        let (old, old_shapes) = shapes_prefixed(10);
        let all_ids: Vec<String> = old.iter().chain(ids.iter()).cloned().collect();
        let all_shapes: Vec<Shape> = old_shapes.iter().chain(shapes.iter()).copied().collect();
        let after = Model::build(&all_ids, &all_shapes, &heights, 800.0);
        let anchor = 20;
        let moved = after.top(anchor + 10) - before.top(anchor);
        assert!((moved - after.top(10)).abs() < 0.01);
    }

    fn shapes_prefixed(n: usize) -> (Vec<String>, Vec<Shape>) {
        let (ids, shapes) = shapes(n);
        (
            ids.into_iter().map(|i| format!("old-{i}")).collect(),
            shapes,
        )
    }

    /// 10 000 rows: building the model and answering a scroll frame's
    /// questions stays far inside a frame budget.
    #[test]
    fn ten_thousand_rows_cost_microseconds_per_scroll_frame() {
        let (ids, shapes) = shapes(10_000);
        let mut heights = Heights::default();
        for id in ids.iter().step_by(3) {
            heights.record(id, 64.0);
        }
        let started = Instant::now();
        let mut model = Model::build(&ids, &shapes, &heights, 900.0);
        let build = started.elapsed();
        let total = model.total();
        let started = Instant::now();
        let mut windows = 0usize;
        let frames = 2_000;
        let mut window = 0..0;
        for frame in 0..frames {
            let top = (frame as f32 / frames as f32) * (total - 900.0);
            if !model.covers(&window, top, 900.0, 450.0) {
                window = model.window(top, 900.0, 900.0);
                windows += 1;
            }
            let _ = model.spacers(&window);
            // A layout changed a few rows: resum.
            if frame % 10 == 0 {
                model.set(window.start, 70.0);
                model.resum();
            }
        }
        let per_frame = started.elapsed() / frames;
        eprintln!(
            "10k rows: model build {build:?}, per scroll frame {per_frame:?}, {windows} window changes over {frames} frames, total height {total:.0}px"
        );
        assert!(windows > 10 && windows < frames as usize);
        // Generous for a debug build on a loaded machine; release is ~100x less.
        assert!(per_frame.as_micros() < 2_000, "per frame {per_frame:?}");
        assert!(build.as_millis() < 200, "build {build:?}");
    }
}
