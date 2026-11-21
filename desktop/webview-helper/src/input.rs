use gtk::glib::translate::{ToGlibPtr, ToGlibPtrMut};
use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;
use std::cell::Cell;

/// Deliver input directly to the widget so WebKit dispatches trusted DOM events
/// (including inside cross-origin iframes) and performs native hit testing.
pub fn forward(web: &webkit2gtk::WebView, e: &Value, buttons: &Cell<u32>) {
    let Some(window) = web.window() else {
        return;
    };
    let Some(seat) = gtk::prelude::WidgetExt::display(web).default_seat() else {
        return;
    };
    let button = e["button"].as_u64().unwrap_or(1).clamp(1, 5) as u32;
    let mask = 1u32 << (button + 7);
    // Host pointer coordinates follow CSS pixels; GDK events use widget units.
    let zoom = webkit2gtk::WebViewExt::zoom_level(web);
    let (x, y) = (
        e["x"].as_f64().unwrap_or(0.) * zoom,
        e["y"].as_f64().unwrap_or(0.) * zoom,
    );
    let typ = match (e["type"].as_str(), e["phase"].as_str()) {
        (Some("pointer"), Some("down")) => gdk::EventType::ButtonPress,
        (Some("pointer"), Some("up")) => gdk::EventType::ButtonRelease,
        (Some("pointer"), Some("move")) => gdk::EventType::MotionNotify,
        (Some("wheel"), _) => gdk::EventType::Scroll,
        (Some("key"), _) if e["down"] == true => gdk::EventType::KeyPress,
        (Some("key"), _) => gdk::EventType::KeyRelease,
        _ => return,
    };
    let key = matches!(typ, gdk::EventType::KeyPress | gdk::EventType::KeyRelease);
    let mut event = gdk::Event::new(typ);
    let device = if key { seat.keyboard() } else { seat.pointer() };
    event.set_device(device.as_ref());
    let time = (glib::monotonic_time() / 1000) as u32;
    let mut state = buttons.get() | e["modifiers"].as_u64().unwrap_or(0) as u32;
    for (name, flag) in [
        ("ctrl", gdk::ffi::GDK_CONTROL_MASK),
        ("shift", gdk::ffi::GDK_SHIFT_MASK),
        ("alt", gdk::ffi::GDK_MOD1_MASK),
        ("logo", gdk::ffi::GDK_SUPER_MASK),
    ] {
        if e[name] == true {
            state |= flag;
        }
    }
    // gtk-rs exposes event creation and dispatch, but no writable coordinates.
    // The GDK event holds its own window reference until Event::drop.
    unsafe {
        let raw: *mut gdk::ffi::GdkEvent = event.to_glib_none_mut().0;
        let win: *mut gdk::ffi::GdkWindow = window.to_glib_none().0;
        glib::gobject_ffi::g_object_ref(win.cast());
        (*raw).any.window = win;
        (*raw).any.send_event = 1;
        match typ {
            gdk::EventType::ButtonPress | gdk::EventType::ButtonRelease => {
                let b = &mut (*raw).button;
                b.time = time;
                b.state = state;
                b.button = button;
                b.x = x;
                b.y = y;
                b.x_root = x;
                b.y_root = y;
                if typ == gdk::EventType::ButtonPress {
                    set_focus(web, true);
                    buttons.set(buttons.get() | mask);
                } else {
                    buttons.set(buttons.get() & !mask);
                }
            }
            gdk::EventType::MotionNotify => {
                let b = &mut (*raw).motion;
                b.time = time;
                b.state = state;
                b.x = x;
                b.y = y;
                b.x_root = x;
                b.y_root = y;
            }
            gdk::EventType::Scroll => {
                let b = &mut (*raw).scroll;
                b.time = time;
                b.state = state;
                b.x = x;
                b.y = y;
                b.x_root = x;
                b.y_root = y;
                b.direction = gdk::ffi::GDK_SCROLL_SMOOTH;
                b.delta_x = e["dx"].as_f64().unwrap_or(0.) / 40.;
                b.delta_y = e["dy"].as_f64().unwrap_or(0.) / 40.;
            }
            _ => {
                let k = &mut (*raw).key;
                let Some(name) = e["key"].as_str() else {
                    return;
                };
                let kv = if name.chars().count() == 1 {
                    gdk::keys::Key::from_unicode(name.chars().next().unwrap())
                } else {
                    gdk::keys::Key::from_name(name)
                };
                k.time = time;
                k.state = state;
                k.keyval = *kv;
                if let Some(code) = gdk::Keymap::for_display(&gtk::prelude::WidgetExt::display(web))
                    .and_then(|m| m.entries_for_keyval(*kv).into_iter().next())
                {
                    k.hardware_keycode = code.keycode() as u16;
                    k.group = code.group() as u8;
                }
            }
        }
    }
    web.event(&event);
}

/// An offscreen window never receives a compositor focus event. Route the
/// owning tab's focus through GTK so WebKit updates its active-page state.
pub fn set_focus(web: &webkit2gtk::WebView, focused: bool) {
    let Some(top) = web.toplevel() else {
        return;
    };
    let Some(window) = top.window() else {
        return;
    };
    if focused {
        web.grab_focus();
    }
    let mut event = gdk::Event::new(gdk::EventType::FocusChange);
    unsafe {
        let raw: *mut gdk::ffi::GdkEvent = event.to_glib_none_mut().0;
        let win: *mut gdk::ffi::GdkWindow = window.to_glib_none().0;
        glib::gobject_ffi::g_object_ref(win.cast());
        (*raw).focus_change.window = win;
        (*raw).focus_change.in_ = i16::from(focused);
        (*raw).focus_change.send_event = 1;
    }
    top.event(&event);
}
