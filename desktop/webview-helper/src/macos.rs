//! macOS offscreen WKWebView backend bridging `macos.m` to the shared
//! stdin/stdout JSON + RGBA wire protocol.
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::{self, BufRead},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::SyncSender,
    },
};

use crate::navigation;

#[repr(C)]
struct MuqunMacCallbacks {
    ctx: *mut c_void,
    allow_navigation: extern "C" fn(*mut c_void, *const c_char) -> c_int,
    on_frame: extern "C" fn(*mut c_void, u32, u32, *const u8, usize),
    on_nav: extern "C" fn(*mut c_void, *const c_char, c_int, c_int, *const c_char, c_int),
    on_browser_icon: extern "C" fn(*mut c_void, *const c_char, *const c_char),
    on_draft: extern "C" fn(*mut c_void, *const c_char, *const c_char),
    on_cursor: extern "C" fn(*mut c_void, *const c_char),
    get_idle: extern "C" fn(*mut c_void) -> u8,
    on_terminated: extern "C" fn(*mut c_void),
}

unsafe extern "C" {
    fn muqun_macos_run(
        url: *const c_char,
        browser_mode: c_int,
        cursor_enabled: c_int,
        logical_w: i32,
        logical_h: i32,
        raster_w: i32,
        raster_h: i32,
        context_js: *const c_char,
        draft_js: *const c_char,
        favicon_js: *const c_char,
        cursor_js: *const c_char,
        cb: MuqunMacCallbacks,
    );
    fn muqun_macos_resize(lw: i32, lh: i32, rw: i32, rh: i32);
    fn muqun_macos_focus(focused: c_int);
    fn muqun_macos_visible(show: c_int);
    fn muqun_macos_navigate(url: *const c_char);
    fn muqun_macos_history(action: c_int);
    fn muqun_macos_eval_js(script: *const c_char);
    fn muqun_macos_set_context(script: *const c_char);
    fn muqun_macos_pointer(phase: c_int, x: f64, y: f64, button: c_int, mods: u32);
    fn muqun_macos_wheel(x: f64, y: f64, dx: f64, dy: f64, mods: u32);
    fn muqun_macos_key(name: *const c_char, down: c_int, mods: u32);
    fn muqun_macos_close();
}

const NS_MOD_SHIFT: u32 = 1 << 17;
const NS_MOD_CTRL: u32 = 1 << 18;
const NS_MOD_OPTION: u32 = 1 << 19;
const NS_MOD_COMMAND: u32 = 1 << 20;

fn modifier_flags(e: &Value) -> u32 {
    let mut mods = 0u32;
    if e["shift"] == true {
        mods |= NS_MOD_SHIFT;
    }
    if e["ctrl"] == true {
        mods |= NS_MOD_CTRL;
    }
    if e["alt"] == true {
        mods |= NS_MOD_OPTION;
    }
    if e["logo"] == true {
        mods |= NS_MOD_COMMAND;
    }
    mods
}

fn cstr_or_empty(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap())
}

fn valid_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

fn context_script_source(origin: &url::Origin, value: &Value) -> Option<String> {
    if !value.is_object() {
        return None;
    }
    Some(format!(
        "({})({},{})",
        include_str!("context.js"),
        json!(origin.ascii_serialization()),
        value
    ))
}

fn draft_script_source(origin: &url::Origin, nonce: &str) -> String {
    format!(
        "({})({},{})",
        include_str!("draft.js"),
        json!(origin.ascii_serialization()),
        json!(nonce)
    )
}

fn metadata_packet(payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    packet.extend_from_slice(&[0u8; 4]);
    packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    packet.extend_from_slice(payload);
    packet
}

#[derive(Default)]
struct NavCache {
    last_sent: Vec<u8>,
    last_url: String,
    last_back: bool,
    last_forward: bool,
    last_title: String,
    committed: bool,
    favicon: Option<Vec<u8>>,
}

struct SharedState {
    browser: bool,
    origin: url::Origin,
    draft_nonce: Option<String>,
    draft_in_flight: AtomicBool,
    idle: Arc<AtomicU8>,
    pixels: SyncSender<Vec<u8>>,
    nav: Mutex<NavCache>,
    last_cursor: Mutex<String>,
}

impl SharedState {
    fn flush_nav(&self, cache: &mut NavCache) {
        if cache.last_url.is_empty() {
            return;
        }
        let title = if cache.committed {
            navigation::clean_title(&cache.last_title)
        } else {
            String::new()
        };
        let favicon = if cache.committed {
            cache.favicon.clone()
        } else {
            None
        };
        let payload = serde_json::to_vec(&json!({
            "url": cache.last_url,
            "back": cache.last_back,
            "forward": cache.last_forward,
            "title": title,
            "favicon": favicon,
        }))
        .expect("navigation contains only JSON primitives");
        if payload.len() >= 65536 || cache.last_sent == payload {
            return;
        }
        if self.pixels.try_send(metadata_packet(&payload)).is_ok() {
            cache.last_sent = payload;
        }
    }
}

extern "C" fn cb_allow_navigation(ctx: *mut c_void, url: *const c_char) -> c_int {
    let state = unsafe { &*(ctx as *const SharedState) };
    let Ok(addr) = unsafe { CStr::from_ptr(url) }.to_str() else {
        return 0;
    };
    let ok = url::Url::parse(addr).ok().is_some_and(|candidate| {
        navigation::allowed(&candidate, (!state.browser).then_some(&state.origin))
    });
    c_int::from(ok)
}

extern "C" fn cb_on_frame(ctx: *mut c_void, width: u32, height: u32, rgba: *const u8, len: usize) {
    let state = unsafe { &*(ctx as *const SharedState) };
    if rgba.is_null() || len != width as usize * height as usize * 4 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(rgba, len) };
    let mut frame = Vec::with_capacity(8 + len);
    frame.extend_from_slice(&width.to_le_bytes());
    frame.extend_from_slice(&height.to_le_bytes());
    frame.extend_from_slice(slice);
    let _ = state.pixels.try_send(frame);
}

extern "C" fn cb_on_nav(
    ctx: *mut c_void,
    url: *const c_char,
    back: c_int,
    forward: c_int,
    title: *const c_char,
    committed: c_int,
) {
    let state = unsafe { &*(ctx as *const SharedState) };
    let Ok(url_str) = unsafe { CStr::from_ptr(url) }.to_str() else {
        return;
    };
    let title_str = unsafe { CStr::from_ptr(title) }
        .to_str()
        .unwrap_or_default();
    let mut cache = state.nav.lock().unwrap();
    if cache.last_url != url_str {
        cache.favicon = None;
    }
    cache.last_url = url_str.to_owned();
    cache.last_back = back != 0;
    cache.last_forward = forward != 0;
    if !title_str.is_empty() {
        cache.last_title = title_str.to_owned();
    }
    cache.committed = committed != 0;
    state.flush_nav(&mut cache);
}

extern "C" fn cb_on_browser_icon(
    ctx: *mut c_void,
    page_url: *const c_char,
    msg_json: *const c_char,
) {
    let state = unsafe { &*(ctx as *const SharedState) };
    let Ok(page) = unsafe { CStr::from_ptr(page_url) }.to_str() else {
        return;
    };
    let Ok(raw) = unsafe { CStr::from_ptr(msg_json) }.to_str() else {
        return;
    };
    if raw.len() >= 32768 {
        return;
    }
    let Ok(data) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    if data["url"].as_str() != Some(page) {
        return;
    }
    let mut cache = state.nav.lock().unwrap();
    if !cache.committed {
        return;
    }
    if let Some(title) = data["title"].as_str() {
        cache.last_title = title.to_owned();
        state.flush_nav(&mut cache);
        return;
    }
    if data["pixels"].is_null() {
        cache.favicon = Some(Vec::new());
    } else if let Ok(bytes) = serde_json::from_value::<Vec<u8>>(data["pixels"].clone()) {
        if bytes.len() != 4096 {
            return;
        }
        cache.favicon = Some(bytes);
    } else {
        return;
    }
    state.flush_nav(&mut cache);
}

extern "C" fn cb_on_draft(ctx: *mut c_void, page_url: *const c_char, msg_json: *const c_char) {
    let state = unsafe { &*(ctx as *const SharedState) };
    let Some(expected_nonce) = state.draft_nonce.as_deref() else {
        return;
    };
    let Ok(page) = unsafe { CStr::from_ptr(page_url) }.to_str() else {
        return;
    };
    if url::Url::parse(page)
        .ok()
        .is_none_or(|u| u.origin() != state.origin)
    {
        return;
    }
    let Ok(raw) = unsafe { CStr::from_ptr(msg_json) }.to_str() else {
        return;
    };
    if raw.len() > 65536 {
        return;
    }
    let Ok(data) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    if data["nonce"].as_str() != Some(expected_nonce) {
        return;
    }
    let Some(id) = data["id"].as_str().filter(|id| valid_uuid(id)) else {
        return;
    };
    let Some(text) = data["text"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 8192)
    else {
        return;
    };
    let payload = json!({"draft":{"text":text,"id":id}}).to_string();
    if state.draft_in_flight.swap(true, Ordering::AcqRel)
        || state
            .pixels
            .try_send(metadata_packet(payload.as_bytes()))
            .is_err()
    {
        state.draft_in_flight.store(false, Ordering::Release);
        let reply = format!(
            "window.__muqunDraftReply?.({},{})",
            data["id"],
            json!("Desktop is busy. Try again.")
        );
        let c = cstr_or_empty(&reply);
        unsafe { muqun_macos_eval_js(c.as_ptr()) };
    }
}

extern "C" fn cb_on_cursor(ctx: *mut c_void, cursor_name: *const c_char) {
    let state = unsafe { &*(ctx as *const SharedState) };
    let Ok(name) = unsafe { CStr::from_ptr(cursor_name) }.to_str() else {
        return;
    };
    let mut last = state.last_cursor.lock().unwrap();
    if last.as_str() == name {
        return;
    }
    *last = name.to_owned();
    let payload = json!({"cursor": name}).to_string();
    let _ = state.pixels.try_send(metadata_packet(payload.as_bytes()));
}

extern "C" fn cb_get_idle(ctx: *mut c_void) -> u8 {
    let state = unsafe { &*(ctx as *const SharedState) };
    state.idle.load(Ordering::Relaxed)
}

extern "C" fn cb_on_terminated(_ctx: *mut c_void) {
    eprintln!("Web process terminated");
}

pub fn run(
    event: Value,
    url: String,
    uri: url::Url,
    browser: bool,
    pixels: SyncSender<Vec<u8>>,
    idle: Arc<AtomicU8>,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial = crate::viewport(&event, 1);
    let logical_w = event["width"].as_i64().unwrap_or(800).clamp(64, 8192) as i32;
    let logical_h = event["height"].as_i64().unwrap_or(600).clamp(64, 8192) as i32;
    let cursor_enabled = event["capabilities"]
        .as_array()
        .is_some_and(|values| values.iter().any(|v| v == "cursor"));
    let draft_nonce = (!browser)
        .then(|| event["draftNonce"].as_str().map(str::to_owned))
        .flatten();
    let context_js = (!browser)
        .then(|| context_script_source(&uri.origin(), &event["context"]))
        .flatten()
        .unwrap_or_default();
    let draft_js = draft_nonce
        .as_deref()
        .map(|nonce| draft_script_source(&uri.origin(), nonce))
        .unwrap_or_default();
    let favicon_js = if browser {
        include_str!("favicon.js")
    } else {
        ""
    };
    let cursor_js = if cursor_enabled {
        include_str!("cursor.js")
    } else {
        ""
    };

    let shared = Arc::new(SharedState {
        browser,
        origin: uri.origin(),
        draft_nonce,
        draft_in_flight: AtomicBool::new(false),
        idle: Arc::clone(&idle),
        pixels,
        nav: Mutex::new(NavCache::default()),
        last_cursor: Mutex::new("default".into()),
    });

    let reader_state = Arc::clone(&shared);
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Ok(e) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            reader_state.idle.store(0, Ordering::Relaxed);
            match e["type"].as_str() {
                Some("resize") => {
                    let vp = crate::viewport(&e, 1);
                    let lw = e["width"].as_i64().unwrap_or(800).clamp(64, 8192) as i32;
                    let lh = e["height"].as_i64().unwrap_or(600).clamp(64, 8192) as i32;
                    unsafe { muqun_macos_resize(lw, lh, vp.raster.0, vp.raster.1) };
                }
                Some("focus") => {
                    unsafe { muqun_macos_focus(c_int::from(e["focused"] == true)) };
                }
                Some("visible") => {
                    let show = e["show"].as_bool().unwrap_or(true);
                    unsafe { muqun_macos_visible(c_int::from(show)) };
                }
                Some("draftReply")
                    if reader_state.draft_in_flight.swap(false, Ordering::AcqRel) =>
                {
                    let js = format!("window.__muqunDraftReply?.({},{})", e["id"], e["error"]);
                    let c = cstr_or_empty(&js);
                    unsafe { muqun_macos_eval_js(c.as_ptr()) };
                }
                Some("context") if !reader_state.browser => {
                    if let Some(js) = context_script_source(&reader_state.origin, &e["context"]) {
                        let c = cstr_or_empty(&js);
                        unsafe { muqun_macos_set_context(c.as_ptr()) };
                    }
                }
                Some("navigate") if reader_state.browser => {
                    if let Some(target) = e["url"].as_str().filter(|s| {
                        url::Url::parse(s)
                            .ok()
                            .is_some_and(|u| navigation::allowed(&u, None))
                    }) {
                        let c = cstr_or_empty(target);
                        unsafe { muqun_macos_navigate(c.as_ptr()) };
                    }
                }
                Some("back") if reader_state.browser => unsafe { muqun_macos_history(0) },
                Some("forward") if reader_state.browser => unsafe { muqun_macos_history(1) },
                Some("reload") if reader_state.browser => unsafe { muqun_macos_history(2) },
                Some("pointer") => {
                    let phase = match e["phase"].as_str() {
                        Some("down") => 0,
                        Some("up") => 1,
                        Some("move") => 2,
                        _ => continue,
                    };
                    let x = e["x"].as_f64().unwrap_or(0.0);
                    let y = e["y"].as_f64().unwrap_or(0.0);
                    let button = e["button"].as_i64().unwrap_or(1).clamp(0, 5) as c_int;
                    unsafe { muqun_macos_pointer(phase, x, y, button, modifier_flags(&e)) };
                }
                Some("wheel") => {
                    let x = e["x"].as_f64().unwrap_or(0.0);
                    let y = e["y"].as_f64().unwrap_or(0.0);
                    let dx = e["dx"].as_f64().unwrap_or(0.0);
                    let dy = e["dy"].as_f64().unwrap_or(0.0);
                    unsafe { muqun_macos_wheel(x, y, dx, dy, modifier_flags(&e)) };
                }
                Some("key") => {
                    if let Some(name) = e["key"].as_str() {
                        let c = cstr_or_empty(name);
                        let down = c_int::from(e["down"] == true);
                        unsafe { muqun_macos_key(c.as_ptr(), down, modifier_flags(&e)) };
                    }
                }
                Some("close") => break,
                _ => {}
            }
        }
        unsafe { muqun_macos_close() };
    });

    let c_url = cstr_or_empty(&url);
    let c_context = cstr_or_empty(&context_js);
    let c_draft = cstr_or_empty(&draft_js);
    let c_favicon = cstr_or_empty(favicon_js);
    let c_cursor = cstr_or_empty(cursor_js);

    let cb = MuqunMacCallbacks {
        ctx: Arc::as_ptr(&shared) as *mut c_void,
        allow_navigation: cb_allow_navigation,
        on_frame: cb_on_frame,
        on_nav: cb_on_nav,
        on_browser_icon: cb_on_browser_icon,
        on_draft: cb_on_draft,
        on_cursor: cb_on_cursor,
        get_idle: cb_get_idle,
        on_terminated: cb_on_terminated,
    };

    unsafe {
        muqun_macos_run(
            c_url.as_ptr(),
            c_int::from(browser),
            c_int::from(cursor_enabled),
            logical_w,
            logical_h,
            initial.raster.0,
            initial.raster.1,
            c_context.as_ptr(),
            c_draft.as_ptr(),
            c_favicon.as_ptr(),
            c_cursor.as_ptr(),
            cb,
        );
    }

    drop(shared);
    Ok(())
}
