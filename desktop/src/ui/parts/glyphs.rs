//! Glyphs for timeline cards, from `desktop/assets/glyphs` (synced from the App).
//! Root stroke attributes are copied onto each element for Rune's rasterizer.

use rune::Icon;

pub static DOCUMENT: Icon = Icon {
    name: "tl-document",
    view: 24.0,
    body: r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5M9 13h6m-6 4h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static EDIT_FILE: Icon = Icon {
    name: "tl-edit-file",
    view: 24.0,
    body: r#"<path d="M12 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h4M12 3v5h6l-6-5Zm1 13 5-5 3 3-5 5-4 1 1-4Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static TERMINAL: Icon = Icon {
    name: "tl-terminal",
    view: 24.0,
    body: r#"<path d="m5 7 5 5-5 5m9 0h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static SEARCH: Icon = Icon {
    name: "tl-search",
    view: 24.0,
    body: r#"<circle cx="10.5" cy="10.5" r="6.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="m16 16 5 5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static NETWORK: Icon = Icon {
    name: "tl-network",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M3 12h18M9 4c-3 5 9 11 6 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static AGENT: Icon = Icon {
    name: "tl-agent",
    view: 24.0,
    body: r#"<rect x="4" y="7" width="16" height="13" rx="4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M12 7V3m-2 0h4M8 12v2m8-2v2m-6 3h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static REASONING: Icon = Icon {
    name: "tl-reasoning",
    view: 24.0,
    body: r#"<path d="M8 17H6a3 3 0 0 1-3-3 3 3 0 0 1 1-2 4 4 0 0 1 3-7 3 3 0 0 1 5-2 3 3 0 0 1 5 2 4 4 0 0 1 3 7 3 3 0 0 1 1 2 3 3 0 0 1-3 3h-2M12 3v18m-4-9 4 3 4-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static TASK_LIST: Icon = Icon {
    name: "tl-task-list",
    view: 24.0,
    body: r#"<path d="m4 6 1.5 1.5L8 5m3 1h9M4 12h3m4 0h9M4 18h3m4 0h9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static CHOICE: Icon = Icon {
    name: "tl-choice",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="12" cy="12" r="2" fill="currentColor" stroke="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static COMPUTER: Icon = Icon {
    name: "tl-computer",
    view: 24.0,
    body: r#"<rect x="3" y="4" width="18" height="13" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M9 21h6m-3-4v4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static TOOL: Icon = Icon {
    name: "tl-tool",
    view: 24.0,
    body: r#"<path d="m15 4-3 3 5 5 3-3a6 6 0 0 1-8 6l-6 6-3-3 6-6a6 6 0 0 1 6-8Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static TIME: Icon = Icon {
    name: "tl-time",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M12 6v6l4 2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static DONE: Icon = Icon {
    name: "tl-done",
    view: 24.0,
    body: r#"<path d="m5 12 4.5 5L19 7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static DISMISS: Icon = Icon {
    name: "tl-dismiss",
    view: 24.0,
    body: r#"<rect x="3" y="3" width="18" height="18" rx="6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="m8 8 8 8m0-8-8 8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static CHEVRON: Icon = Icon {
    name: "tl-disclosure-down",
    view: 24.0,
    body: r#"<path d="m5 9 7 6 7-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static TRUST_WARNING: Icon = Icon {
    name: "tl-trust-warning",
    view: 24.0,
    body: r#"<path d="m12 3 8 3v7c0 4-8 8-8 8s-8-4-8-8V6l8-3Zm0 5v5m0 3h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static CODE: Icon = Icon {
    name: "tl-code",
    view: 24.0,
    body: r#"<path d="m7 7-5 5 5 5m10-10 5 5-5 5M14 4l-4 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static OPEN: Icon = Icon {
    name: "tl-open-external",
    view: 24.0,
    body: r#"<path d="M13 3h8v8m0-8L10 14M8 5H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static FOLDER: Icon = Icon {
    name: "tl-folder",
    view: 24.0,
    body: r#"<path d="M3 8V6a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static WORKFLOW: Icon = Icon {
    name: "tl-workflow",
    view: 24.0,
    body: r#"<rect x="3" y="3" width="7" height="6" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><rect x="14" y="15" width="7" height="6" rx="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M10 6h5a3 3 0 0 1 3 3v6M6 9v9h8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static CLOSE: Icon = Icon {
    name: "tl-close",
    view: 24.0,
    body: r#"<path d="m6 6 12 12M18 6 6 18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static COMPLETED: Icon = Icon {
    name: "tl-completed",
    view: 24.0,
    body: r#"<circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="m8 12 3 3 5-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static WARNING: Icon = Icon {
    name: "tl-warning",
    view: 24.0,
    body: r#"<path d="M10 4a2.3 2.3 0 0 1 4 0l8 14a2 2 0 0 1-2 3H4a2 2 0 0 1-2-3l8-14Zm2 5v4m0 4h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static ATTACH: Icon = Icon {
    name: "tl-attach",
    view: 24.0,
    body: r#"<path d="m9 13 6-6a2.5 2.5 0 0 1 3.5 3.5l-8 8a4 4 0 0 1-5.7-5.7l8.5-8.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
