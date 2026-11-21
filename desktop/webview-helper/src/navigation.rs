//! Emit location and history state after WebKit navigation. An idle browser
//! has no navigation timer. Backpressure coalesces updates to the latest state.
use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::{SyncSender, TrySendError},
    time::Duration,
};
use webkit2gtk::{LoadEvent, WebContextExt, WebViewExt};

fn favicon(web: &webkit2gtk::WebView) -> Option<Vec<u8>> {
    use webkit2gtk::FaviconDatabaseExt;
    web.context()?
        .favicon_database()?
        .favicon_uri(&web.uri()?)?;
    // Render the engine's decoded favicon into a small RGBA buffer. No URL
    // from the document is fetched by the host or passed to its image loader.
    let source = gtk::cairo::ImageSurface::try_from(web.favicon()?).ok()?;
    let canvas = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, 32, 32).ok()?;
    let painter = gtk::cairo::Context::new(&canvas).ok()?;
    let (w, h) = (source.width() as f64, source.height() as f64);
    if w == 0.0 || h == 0.0 {
        return None;
    }
    let scale = 32.0 / w.max(h);
    painter.translate((32.0 - w * scale) / 2.0, (32.0 - h * scale) / 2.0);
    painter.scale(scale, scale);
    painter.set_source_surface(&source, 0.0, 0.0).ok()?;
    painter.paint().ok()?;
    let image = gtk::gdk::pixbuf_get_from_surface(&canvas, 0, 0, 32, 32)?;
    Some(image.read_pixel_bytes().to_vec())
}

fn title(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(256)
        .collect::<String>()
        .trim()
        .into()
}

struct Updates {
    frames: SyncSender<Vec<u8>>,
    web: glib::WeakRef<webkit2gtk::WebView>,
    sent: RefCell<Vec<u8>>,
    queued: Cell<bool>,
    provisional: Cell<bool>,
    dynamic_icon: RefCell<Option<Vec<u8>>>,
    document: RefCell<Option<(String, String)>>,
}

impl Updates {
    fn queue(self: &Rc<Self>, delay: Duration) {
        if self.queued.replace(true) {
            return;
        }
        let signal = self.clone();
        glib::timeout_add_local_once(delay, move || {
            signal.queued.set(false);
            let Some(web) = signal.web.upgrade() else {
                return;
            };
            let Some(uri) = web.uri() else {
                return;
            };
            let document = signal.document.borrow();
            let title = document
                .as_ref()
                .filter(|(url, _)| url == uri.as_str() && !signal.provisional.get())
                .map(|(_, title)| title.as_str());
            let committed = title.is_some();
            let payload = serde_json::to_vec(&serde_json::json!({
                "url":uri.as_str(),
                "back":web.can_go_back(),
                "forward":web.can_go_forward(),
                "title":title.unwrap_or_default(),
                "favicon":if committed { signal.dynamic_icon.borrow().clone().or_else(|| favicon(&web)) } else { None }
            }))
            .expect("navigation contains only JSON primitives");
            if payload.len() >= 65536 || *signal.sent.borrow() == payload {
                return;
            }
            let mut packet = vec![0; 4];
            packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            packet.extend_from_slice(&payload);
            match signal.frames.try_send(packet) {
                Ok(()) => *signal.sent.borrow_mut() = payload,
                Err(TrySendError::Full(_)) => signal.queue(Duration::from_millis(80)),
                Err(TrySendError::Disconnected(_)) => {}
            }
        });
    }
}

pub fn observe(web: &webkit2gtk::WebView, frames: SyncSender<Vec<u8>>) {
    let state = Rc::new(Updates {
        frames,
        web: web.downgrade(),
        sent: RefCell::default(),
        queued: Cell::new(false),
        provisional: Cell::new(true),
        dynamic_icon: RefCell::default(),
        document: RefCell::default(),
    });
    if let Some(context) = web.context() {
        context.set_favicon_database_directory(None);
    }
    observe_icon_changes(web, &state);
    let icon = state.clone();
    web.connect_favicon_notify(move |_| icon.queue(Duration::ZERO));
    let location = state.clone();
    web.connect_uri_notify(move |web| {
        location.queue(Duration::ZERO);
        // pushState/replaceState may keep the same document and title.
        web.run_javascript_in_world(
            "window.webkit.messageHandlers.browserIcon.postMessage(JSON.stringify({url:location.href,title:document.title.slice(0,512)}))",
            "muqun-browser-icon", None::<&gtk::gio::Cancellable>, |_| {},
        );
    });
    let load = state.clone();
    web.connect_load_changed(move |_, event| {
        match event {
            LoadEvent::Started | LoadEvent::Redirected => load.provisional.set(true),
            LoadEvent::Committed => {
                load.document.take();
                load.dynamic_icon.take();
                load.provisional.set(false);
            }
            LoadEvent::Finished => load.provisional.set(false),
            _ => {}
        }
        load.queue(Duration::ZERO);
    });
    if let Some(list) = web.back_forward_list() {
        // webkit2gtk-rs omits the typed hook for this signal's GList argument.
        // We only read the current state from the associated WebView.
        let history = state.clone();
        list.connect_local("changed", false, move |_| {
            history.queue(Duration::ZERO);
            None
        });
    }
    state.queue(Duration::ZERO);
}

fn observe_icon_changes(web: &webkit2gtk::WebView, state: &Rc<Updates>) {
    use webkit2gtk::{
        UserContentInjectedFrames, UserContentManagerExt, UserScript, UserScriptInjectionTime,
    };
    const WORLD: &str = "muqun-browser-icon";
    let Some(manager) = web.user_content_manager() else {
        return;
    };
    let state = state.clone();
    manager.connect_script_message_received(Some("browserIcon"), move |_, msg| {
        let Some(web) = state.web.upgrade() else {
            return;
        };
        let Some(raw) = msg
            .js_value()
            .map(|v| v.to_string())
            .filter(|v| v.len() < 32768)
        else {
            return;
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        if state.provisional.get() || data["url"].as_str() != web.uri().as_deref() {
            return;
        }
        if let Some(value) = data["title"].as_str() {
            state.document.replace(Some((
                data["url"].as_str().unwrap_or_default().into(),
                title(value),
            )));
            state.queue(Duration::ZERO);
            return;
        }
        let pixels = if data["pixels"].is_null() {
            Vec::new()
        } else {
            let Ok(bytes) = serde_json::from_value::<Vec<u8>>(data["pixels"].clone()) else {
                return;
            };
            if bytes.len() != 4096 {
                return;
            }
            bytes
        };
        state.dynamic_icon.replace(Some(pixels));
        state.queue(Duration::ZERO);
    });
    if manager.register_script_message_handler_in_world("browserIcon", WORLD) {
        manager.add_script(&UserScript::for_world(
            include_str!("favicon.js"),
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::End,
            WORLD,
            &[],
            &[],
        ));
    }
}

/// A plugin panel may navigate on its original loopback origin. Browser tabs
/// additionally support ordinary HTTP(S) sites without privileged host APIs.
pub fn allowed(address: &url::Url, fixed_origin: Option<&url::Origin>) -> bool {
    matches!(address.scheme(), "http" | "https")
        && address.host().is_some()
        && address.username().is_empty()
        && address.password().is_none()
        && fixed_origin.is_none_or(|origin| &address.origin() == origin)
}

#[cfg(test)]
mod tests {
    use super::allowed;
    use url::Url;

    #[test]
    fn links_stay_in_the_plugin_origin_but_a_browser_can_follow_other_http_sites() {
        let origin = Url::parse("http://127.0.0.1:3456/panel").unwrap().origin();
        let link = |s| Url::parse(s).unwrap();
        assert!(allowed(
            &link("http://127.0.0.1:3456/subpage"),
            Some(&origin)
        ));
        assert!(!allowed(
            &link("http://127.0.0.1:3457/subpage"),
            Some(&origin)
        ));
        assert!(!allowed(&link("https://example.org/"), Some(&origin)));
        assert!(allowed(&link("https://example.org/"), None));
        assert!(!allowed(&link("file:///home/local"), None));
        assert!(!allowed(&link("https://user:password@example.org/"), None));
    }
}
