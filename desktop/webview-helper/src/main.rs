use serde_json::Value;
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
};

/// GTK allocates in its monitor's integer scale. WebKit zoom reconciles it
/// with the owning Rune window (which can also have a fractional scale).
pub(crate) struct Viewport {
    #[allow(dead_code)]
    pub(crate) widget: (i32, i32),
    #[allow(dead_code)]
    pub(crate) zoom: f64,
    pub(crate) raster: (i32, i32),
}

pub(crate) fn viewport(event: &Value, native_scale: i32) -> Viewport {
    let dimension =
        |key: &str, default: i64| event[key].as_i64().unwrap_or(default).clamp(64, 8192) as f64;
    let w = dimension("width", 800);
    let h = dimension("height", 600);
    let native = native_scale.max(1);
    // Bound the raw frame to 64 MiB. Choose one scale for both dimensions so
    // hitting the budget doesn't change the page's aspect ratio or CSS layout.
    let max_scale = (8192.0 / w.max(h)).min((16_777_216.0 / (w * h)).sqrt());
    let requested = event["scale"].as_f64().unwrap_or(1.0).clamp(0.25, 8.0);
    let zoom = requested.min(max_scale) / f64::from(native);
    let widget = (
        (w * zoom).round().max(1.0) as i32,
        (h * zoom).round().max(1.0) as i32,
    );
    let raster = (widget.0 * native, widget.1 * native);
    // Rounding an almost-full frame can also cross the budget.
    let widget = if i64::from(raster.0) * i64::from(raster.1) > 16_777_216
        || raster.0.max(raster.1) > 8192
    {
        (
            (w * zoom).floor().max(1.0) as i32,
            (h * zoom).floor().max(1.0) as i32,
        )
    } else {
        widget
    };
    Viewport {
        widget,
        zoom,
        raster: (widget.0 * native, widget.1 * native),
    }
}

#[cfg(target_os = "linux")]
mod context;
#[cfg(target_os = "linux")]
mod cursor;
#[cfg(target_os = "linux")]
mod draft;
#[cfg(target_os = "linux")]
mod frames;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "macos")]
mod macos;
mod navigation;

fn spawn_writer(idle: Arc<AtomicU8>) -> mpsc::SyncSender<Vec<u8>> {
    // Only the writer owns the previous frame. The capture loop backs off
    // after repeated identical images, and resumes immediately on host input.
    let (pixels, writer) = mpsc::sync_channel::<Vec<u8>>(1);
    std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        let mut previous = Vec::new();
        for frame in writer {
            let metadata = frame.starts_with(&[0; 4]);
            if !metadata && frame == previous {
                let old = idle.load(Ordering::Relaxed);
                let _ = idle.compare_exchange(
                    old,
                    old.saturating_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                continue;
            }
            if out.write_all(&frame).and_then(|_| out.flush()).is_err() {
                return;
            }
            if !metadata {
                idle.store(0, Ordering::Relaxed);
                previous = frame;
            }
        }
    });
    pixels
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let read = io::stdin();
    let mut start = String::new();
    read.read_line(&mut start)?;
    let event: Value = serde_json::from_str(&start)?;
    let url = event["url"].as_str().ok_or("Expected URL")?.to_owned();
    let uri = url::Url::parse(&url)?;
    let browser = event["mode"] == "browser";
    if event["type"] != "open"
        || !navigation::allowed(&uri, None)
        || (!browser && (uri.scheme() != "http" || uri.host_str() != Some("127.0.0.1")))
    {
        return Err("Expected a local panel".into());
    }
    let idle = Arc::new(AtomicU8::new(0));
    let pixels = spawn_writer(Arc::clone(&idle));

    #[cfg(target_os = "linux")]
    {
        run_linux(read, event, &url, uri, browser, pixels, idle)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = read;
        macos::run(event, url, uri, browser, pixels, idle)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (read, event, url, uri, browser, pixels, idle);
        Err("Embedded WebView helper is only supported on Linux and macOS".into())
    }
}

#[cfg(target_os = "linux")]
fn run_linux(
    read: io::Stdin,
    event: Value,
    url: &str,
    uri: url::Url,
    browser: bool,
    pixels: mpsc::SyncSender<Vec<u8>>,
    idle: Arc<AtomicU8>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt;
    use gtk::glib;
    use gtk::prelude::*;
    use std::cell::Cell;
    use std::io::BufRead;
    use webkit2gtk::WebViewExt;
    use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

    configure_linux_renderer();
    gtk::init()?;
    let window = gtk::OffscreenWindow::new();
    // Once realized, GtkOffscreenWindow reports scale 1 even though WebKit
    // retains the display scale it used to create its backing store.
    let backing_scale = window.scale_factor().max(1);
    let initial = viewport(&event, backing_scale);
    let (w, h) = initial.widget;
    let area = gtk::Box::new(gtk::Orientation::Vertical, 0);
    area.set_size_request(w, h);
    window.add(&area);
    let origin = uri.origin();
    let mut context = (!browser).then(|| context::Bridge::new(&origin));
    let view = WebViewBuilder::new()
        .with_incognito(true)
        .with_navigation_handler(move |address| {
            if matches!(address.as_str(), "about:srcdoc" | "about:blank") {
                return true;
            }
            url::Url::parse(&address).ok().is_some_and(|candidate| {
                navigation::allowed(&candidate, (!browser).then_some(&origin))
            })
        })
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .build_gtk(&area)?;
    let web = view.webview();
    web.set_zoom_level(initial.zoom);
    if let Some(bridge) = &mut context {
        bridge.set(&web, &event["context"], false);
    }

    web.connect_web_process_terminated(|_, why| {
        eprintln!("Web process terminated {why:?}");
        gtk::main_quit();
    });
    window.show_all();
    let (tx, mut rx) = futures_channel::mpsc::unbounded();
    std::thread::spawn(move || {
        for line in read.lock().lines().map_while(Result::ok) {
            if let Ok(command) = serde_json::from_str::<Value>(&line)
                && tx.unbounded_send(command).is_err()
            {
                break;
            }
        }
    });
    // Earlier Desktop versions only understand navigation records on this stream.
    if event["capabilities"]
        .as_array()
        .is_some_and(|values| values.iter().any(|v| v == "cursor"))
    {
        cursor::observe(&web, pixels.clone());
    }
    let draft_in_flight = std::rc::Rc::new(Cell::new(false));
    if let Some(nonce) = event["draftNonce"].as_str().filter(|_| !browser) {
        draft::install(
            &web,
            uri.origin(),
            nonce.to_owned(),
            pixels.clone(),
            draft_in_flight.clone(),
        );
    }
    if browser {
        navigation::observe(&web, pixels.clone());
    }
    view.load_url(url)?;
    let frames = frames::Frames::new(web.clone(), pixels, initial.raster, idle);
    let inner = web.clone();
    let buttons = Cell::new(0u32);
    glib::MainContext::default().spawn_local(async move {
        while let Some(e) = rx.next().await {
            frames.input();
            match e["type"].as_str() {
                Some("resize") => {
                    let viewport = viewport(&e, backing_scale);
                    let (w, h) = viewport.widget;
                    inner.set_zoom_level(viewport.zoom);
                    frames.size.set(viewport.raster);
                    area.set_size_request(w, h);
                    window.resize(w, h);
                }
                Some("focus") => input::set_focus(&inner, e["focused"] == true),
                Some("draftReply") if draft_in_flight.replace(false) => {
                    draft::reply(&inner, &e["id"], &e["error"])
                }
                Some("context") => {
                    if let Some(bridge) = &mut context {
                        bridge.set(&inner, &e["context"], true);
                    }
                }
                Some("navigate") if browser => {
                    if let Some(url) = e["url"].as_str().filter(|s| {
                        url::Url::parse(s)
                            .ok()
                            .is_some_and(|u| navigation::allowed(&u, None))
                    }) {
                        inner.load_uri(url);
                    }
                }
                Some("back") if browser => inner.go_back(),
                Some("forward") if browser => inner.go_forward(),
                Some("reload") if browser => inner.reload(),
                Some("visible") => {
                    let visible = e["show"].as_bool().unwrap_or(true);
                    if visible {
                        window.show();
                    } else {
                        input::set_focus(&inner, false);
                        window.hide();
                    }
                    frames.set_visible(visible);
                }
                Some("close") => {
                    break;
                }
                _ => input::forward(&inner, &e, &buttons),
            }
        }
        gtk::main_quit();
    });
    gtk::main();
    drop(view);
    Ok(())
}

/// WebKitGTK drawing into an offscreen GDK window needs a usable EGL context.
/// NVIDIA GLVND can abort the process during GTK initialization on Wayland.
/// Select Mesa software rendering for this helper on affected systems only.
#[cfg(target_os = "linux")]
fn configure_linux_renderer() {
    use std::{env, path::Path};
    let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    let wayland = env::var_os("WAYLAND_DISPLAY").is_some()
        && env::var("GDK_BACKEND")
            .map(|v| v.split(',').next() == Some("wayland"))
            .unwrap_or(true);
    if wayland
        && Path::new("/sys/module/nvidia").exists()
        && Path::new(mesa).is_file()
        && env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none()
        && env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none()
    {
        // SAFETY: this standalone helper is still single-threaded before gtk::init.
        unsafe {
            env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
            env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        }
    }
}

#[cfg(test)]
mod dpi_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn viewport_preserves_css_layout_across_mixed_dpi_and_bounded_frames() {
        for (w, h, display, gtk) in [
            (640, 360, 1.0, 2),
            (640, 360, 1.25, 2),
            (2560, 1440, 2.0, 2),
            (5101, 2941, 2.0, 3),
            (8191, 8192, 4.0, 2),
        ] {
            let view = viewport(&json!({"width":w,"height":h,"scale":display}), gtk);
            let (x, y) = view.raster;
            assert!(x.max(y) <= 8192 && i64::from(x) * i64::from(y) <= 16_777_216);
            assert!((f64::from(view.widget.0) / view.zoom - f64::from(w)).abs() <= 1.0 / view.zoom);
            assert!((f64::from(view.widget.1) / view.zoom - f64::from(h)).abs() <= 1.0 / view.zoom);
            if w <= 2560 {
                assert_eq!(
                    view.raster,
                    (
                        (f64::from(w) * display) as i32,
                        (f64::from(h) * display) as i32
                    )
                );
            }
        }
    }
}
