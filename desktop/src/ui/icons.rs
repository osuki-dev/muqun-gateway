//! App glyph Icon bodies used by the desktop chrome.
//!
//! Source SVGs live in `desktop/assets/glyphs` (synced from the App). Stroke
//! attributes that sit on the `<svg>` root are copied onto each element so
//! Rune's icon rasterizer can see them.

use rune::Icon;

pub static INBOX: Icon = Icon {
    name: "inbox",
    view: 24.0,
    body: concat!(
        r#"<path d="M6 4h12l4 11v5H2v-5L6 4ZM2 15h6l2 3h4l2-3h6""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

pub static TASK: Icon = Icon {
    name: "task",
    view: 24.0,
    body: concat!(
        r#"<rect x="4" y="4" width="16" height="16" rx="3""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="m8 12 3 3 5-6""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

pub static SETTINGS: Icon = Icon {
    name: "settings",
    view: 24.0,
    body: concat!(
        r#"<path d="M4 7h7m4 0h5M4 17h3m4 0h9""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="13" cy="7" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="9" cy="17" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `undo.svg`: roll back to a message.
pub static UNDO: Icon = Icon {
    name: "undo",
    view: 24.0,
    body: concat!(
        r#"<path d="m8 3-5 5 5 5M3 8h10a7 7 0 0 1 0 14""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `fork.svg`: fork a session from a message.
pub static FORK: Icon = Icon {
    name: "fork",
    view: 24.0,
    body: concat!(
        r#"<circle cx="6" cy="5" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="18" cy="5" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="12" cy="19" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M6 7v2a6 6 0 0 0 12 0V7m-6 8v2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `working-spinner.svg`: the sidebar's Running row, still (reduce motion).
pub static RUNNING: Icon = Icon {
    name: "working-spinner",
    view: 24.0,
    body: concat!(
        r#"<path d="M12 3.4a8.6 8.6 0 1 1-8.6 8.6""#,
        r#" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"/>"#,
    ),
};

/// `branch.svg`: a git branch (start card worktree and folder rows).
pub static BRANCH: Icon = Icon {
    name: "branch",
    view: 24.0,
    body: concat!(
        r#"<circle cx="7" cy="5" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="7" cy="19" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<circle cx="18" cy="7" r="2""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M7 7v10m0-5h5a6 6 0 0 0 6-3""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `move-down.svg`: the timeline's "jump to latest".
pub static MOVE_DOWN: Icon = Icon {
    name: "move-down",
    view: 24.0,
    body: concat!(
        r#"<path d="M12 4v16m-6-6 6 6 6-6""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `keyboard.svg`: the shortcuts sheet.
pub static KEYBOARD: Icon = Icon {
    name: "keyboard",
    view: 24.0,
    body: concat!(
        r#"<rect x="2" y="5" width="20" height="14" rx="3""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 12h.01M10 12h.01M14 12h.01M18 12h.01M7 16h10""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// A plain plus in the kit's close-button weight: "Add to split view" beside
/// a pane's ×.
pub static PLUS: Icon = Icon {
    name: "plus",
    view: 16.0,
    body: r#"<path d="M8 3.75V12.25M3.75 8H12.25" stroke="currentColor"/>"#,
};
