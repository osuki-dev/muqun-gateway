//! Forward the cursor WebKit chooses during native hit testing, including
//! controls inside frames. GDK3 exposes named cursors as opaque pixmaps.
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::Cell,
    rc::Rc,
    sync::mpsc::{SyncSender, TrySendError},
    time::Duration,
};
use webkit2gtk::{HitTestResultExt, WebViewExt};

struct CursorState {
    sink: SyncSender<Vec<u8>>,
    wanted: Cell<&'static str>,
    sent: Cell<&'static str>,
    retrying: Cell<bool>,
}

impl CursorState {
    fn send(self: &Rc<Self>, name: &'static str) {
        self.wanted.set(name);
        if self.sent.get() == name || self.retrying.get() {
            return;
        }
        let payload = serde_json::json!({"cursor":name}).to_string();
        let mut packet = vec![0; 4];
        packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        packet.extend_from_slice(payload.as_bytes());
        match self.sink.try_send(packet) {
            Ok(()) => self.sent.set(name),
            Err(TrySendError::Full(_)) => {
                self.retrying.set(true);
                let state = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(32), move || {
                    state.retrying.set(false);
                    state.send(state.wanted.get());
                });
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

pub fn observe(web: &webkit2gtk::WebView, sink: SyncSender<Vec<u8>>) {
    let Some(window) = web.window() else {
        return;
    };
    let display = window.display();
    // GTK caches named cursors on Wayland. X11 can supply a fresh opaque
    // pixmap instead; use WebKit's native target signal in that case.
    let named: Vec<_> = [
        "default",
        "pointer",
        "text",
        "crosshair",
        "move",
        "wait",
        "progress",
        "help",
        "grab",
        "grabbing",
        "not-allowed",
        "ew-resize",
        "ns-resize",
        "nwse-resize",
        "nesw-resize",
        "col-resize",
        "row-resize",
        "zoom-in",
        "zoom-out",
        "copy",
        "alias",
        "vertical-text",
        "cell",
        "context-menu",
    ]
    .into_iter()
    .filter_map(|name| Some((gdk::Cursor::from_name(&display, name)?, name)))
    .collect();
    let state = Rc::new(CursorState {
        sink,
        wanted: Cell::new("default"),
        sent: Cell::new("default"),
        retrying: Cell::new(false),
    });
    let native = Rc::new(Cell::new(None));
    let target = Rc::new(Cell::new("default"));
    let cursor = state.clone();
    let shape = native.clone();
    let fallback = target.clone();
    window.connect_cursor_notify(move |window| {
        let name = window.cursor().and_then(|current| {
            named
                .iter()
                .find(|(candidate, _)| candidate == &current)
                .map(|(_, name)| *name)
        });
        shape.set(name);
        cursor.send(name.unwrap_or(fallback.get()));
    });
    web.connect_mouse_target_changed(move |_, hit, _| {
        target.set(if hit.context_is_editable() {
            "text"
        } else if hit.context_is_link() {
            "pointer"
        } else {
            "default"
        });
        state.send(native.get().unwrap_or(target.get()));
    });
}
