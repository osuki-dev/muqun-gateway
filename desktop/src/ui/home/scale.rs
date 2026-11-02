//! The §6.1d scale, for the values Blinc takes from the builder rather than
//! from CSS: text sizes (a text's layout is measured at its builder size)
//! and fixed box sizes. Each constant mirrors a variable of the `/* scale */`
//! block in `styles.css`; spacing, radii and colours stay in CSS.

/// `--mq-fs-11` … `--mq-fs-56`.
pub const FONT_12: f32 = 12.0;
pub const FONT_13: f32 = 13.0;
pub const FONT_15: f32 = 15.0;
pub const FONT_18: f32 = 18.0;
pub const FONT_24: f32 = 24.0;
pub const FONT_32: f32 = 32.0;
pub const FONT_56: f32 = 56.0;

/// Section titles: 13 px uppercase, tracked 0.08 em.
pub const TITLE_TRACKING: f32 = FONT_13 * 0.08;

/// `--mq-space-10`: the page margin and the gap between sections.
pub const SPACE_40: f32 = 40.0;

/// Status dots and agent dots.
pub const DOT: f32 = 8.0;
pub const DOT_RADIUS: f32 = DOT / 2.0;
/// The agent brand bar on cards (`--mq-bar-3`).
pub const BAR: f32 = 3.0;
