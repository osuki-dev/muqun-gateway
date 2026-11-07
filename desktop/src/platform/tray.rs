//! The status icon: Rune's tray (StatusNotifierItem over D-Bus on Linux, the
//! notification area on Windows; none on macOS yet). Its tooltip says how
//! many sessions need the owner and its menu lists them, with Open Muqun and
//! Quit. A primary click brings the window up. When the window is closed
//! with "keep running in the tray" on, this is how the owner gets back.

use std::sync::Mutex;

use rune::{Tray, TrayEvent, TrayIcon, TrayItem, TraySpec};

/// What a tray click asks of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayRequest {
    /// Show the window (with the host's activation token on Wayland).
    Open(Option<String>),
    /// Show the window on this session.
    Session(String),
    Quit,
}

/// The menu's words, in the owner's language.
pub struct Words {
    pub open: String,
    pub quit: String,
    /// "2 sessions need you", or `None` when none does.
    pub waiting: Option<String>,
    pub tooltip: String,
}

static TRAY: Mutex<Option<Tray>> = Mutex::new(None);

/// Whether the icon is up (a tray host may still not be showing it).
pub fn active() -> bool {
    TRAY.lock().map(|t| t.is_some()).unwrap_or(false)
}

fn icon() -> Option<TrayIcon> {
    let image = image::load_from_memory(crate::branding::APP_MARK_PNG).ok()?;
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    // Named through a binding: the colour-literal lint would read the call as CSS.
    let from_pixels = TrayIcon::rgba;
    Some(from_pixels(w, h, rgba.into_raw()))
}

fn menu(words: &Words, sessions: &[(String, String)]) -> Vec<TrayItem> {
    let mut items = vec![TrayItem::action("open", words.open.clone())];
    if let Some(waiting) = &words.waiting {
        items.push(TrayItem::Separator);
        items.push(TrayItem::action("waiting", waiting.clone()).enabled(false));
        for (asid, title) in sessions.iter().take(8) {
            items.push(TrayItem::action(format!("session:{asid}"), title.clone()));
        }
    }
    items.push(TrayItem::Separator);
    items.push(TrayItem::action("quit", words.quit.clone()));
    items
}

/// Put the icon up. `on_request` runs on the tray's thread.
pub fn start(words: &Words, on_request: impl Fn(TrayRequest) + Send + Sync + 'static) {
    let Some(icon) = icon() else {
        return;
    };
    let spec = TraySpec::new(crate::branding::APP_ID, crate::branding::APP_NAME, icon)
        .tooltip(words.tooltip.clone())
        .menu(menu(words, &[]));
    match Tray::new(spec, move |event| match event {
        TrayEvent::Activate { token } => on_request(TrayRequest::Open(token)),
        TrayEvent::Menu(id) => match id.as_str() {
            "open" => on_request(TrayRequest::Open(None)),
            "quit" => on_request(TrayRequest::Quit),
            other => {
                if let Some(asid) = other.strip_prefix("session:") {
                    on_request(TrayRequest::Session(asid.to_string()));
                }
            }
        },
        _ => {}
    }) {
        Ok(tray) => {
            if let Ok(mut slot) = TRAY.lock() {
                *slot = Some(tray);
            }
        }
        Err(err) => tracing::info!("no status icon: {err}"),
    }
}

/// Show what needs the owner: `sessions` are (asid, title).
pub fn update(words: &Words, sessions: &[(String, String)]) {
    if let Ok(slot) = TRAY.lock() {
        if let Some(tray) = slot.as_ref() {
            tray.set_tooltip(words.tooltip.clone());
            tray.set_menu(menu(words, sessions));
        }
    }
}
