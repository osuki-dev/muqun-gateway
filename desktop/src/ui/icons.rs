//! App glyph Icon bodies used by the desktop chrome.
//!
//! Source SVGs live in `desktop/assets/glyphs` (synced from the App). Stroke
//! attributes that sit on the `<svg>` root are copied onto each element so
//! Rune's icon rasterizer can see them.

use rune::Icon;

pub static HOME: Icon = Icon {
    name: "home",
    view: 24.0,
    body: concat!(
        r#"<path d="m3 10 9-7 9 7M5 9v12h5v-7h4v7h5V9""#,
        r#" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

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
