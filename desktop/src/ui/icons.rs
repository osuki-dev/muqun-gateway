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

// ---- Settings ----------------------------------------------------------------------

/// `controls.svg`: Settings → General.
pub static CONTROLS: Icon = Icon {
    name: "controls",
    view: 24.0,
    body: r#"<path d="M4 6h8m4 0h4M4 12h3m4 0h9M4 18h9m4 0h3M14 4v4m-5 2v4m6 2v4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// `appearance.svg`: Settings → Appearance.
pub static APPEARANCE: Icon = Icon {
    name: "appearance",
    view: 24.0,
    body: concat!(
        r#"<path d="M12 3a9 9 0 0 0 0 18h2a2 2 0 0 0 2-2c0-1-2-2-1-3s3 1 5-1c3-3 0-12-8-12Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M7 10h.01M10 6.5h.01M16 7h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `grid.svg`: Settings → Themes.
pub static GRID: Icon = Icon {
    name: "grid",
    view: 24.0,
    body: concat!(
        r#"<rect x="3" y="3" width="7" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<rect x="14" y="3" width="7" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<rect x="3" y="14" width="7" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<rect x="14" y="14" width="7" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `agent.svg`: Settings → Agents.
pub static AGENT: Icon = Icon {
    name: "agent",
    view: 24.0,
    body: concat!(
        r#"<rect x="4" y="7" width="16" height="13" rx="4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M12 7V3m-2 0h4M8 12v2m8-2v2m-6 3h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `storage.svg`: Settings → Storage.
pub static STORAGE: Icon = Icon {
    name: "storage",
    view: 24.0,
    body: r#"<path d="m5 4-3 11v4a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-4L19 4H5ZM2 15h20M6 18h.01M10 18h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// `information.svg`: Settings → About.
pub static INFORMATION: Icon = Icon {
    name: "information",
    view: 24.0,
    body: concat!(
        r#"<rect x="3" y="3" width="18" height="18" rx="6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M12 11v6m0-10h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

/// `open-external.svg`: a link that opens in the browser.
pub static OPEN_EXTERNAL: Icon = Icon {
    name: "open-external",
    view: 24.0,
    body: r#"<path d="M13 3h8v8m0-8L10 14M8 5H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// `refresh.svg`: look again (agents, themes).
pub static REFRESH: Icon = Icon {
    name: "refresh",
    view: 24.0,
    body: r#"<path d="M4 9a8 8 0 0 1 14-3l3 3M3 15l3 3a8 8 0 0 0 14-3M21 4v5h-5M3 20v-5h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// `delete.svg`: remove a theme, a saved key.
pub static DELETE: Icon = Icon {
    name: "delete",
    view: 24.0,
    body: r#"<path d="M4 6h16M9 6V3h6v3M6 6l1 15h10l1-15M10 10v7m4-7v7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
