//! Draw a bounded stream of offscreen browser frames in a normal Rune element.
//! Browser I/O runs off the UI thread. The latest frame replaces the previous one.
use std::{
    fmt,
    fs::OpenOptions,
    io::{BufReader, BufWriter, Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use rune::{el, text, Cursor, El, Key, KeyEvent, Phase, Pixels, PointerButton, Tx};
use rune_kit::Kit;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ui::UiState;

type SurfaceShortcut = fn(&mut UiState, &KeyEvent, &mut Tx) -> bool;

const MAX_EDGE: u32 = 8192;
const MAX_FRAME_PIXELS: u64 = 16_777_216;
const MIN_EDGE: u32 = 64;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default, Deserialize)]
pub(crate) struct Navigation {
    pub url: String,
    pub back: bool,
    pub forward: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default, deserialize_with = "read_favicon")]
    pub favicon: Option<Pixels>,
}

fn read_favicon<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<Option<Pixels>, D::Error> {
    Ok(Option::<Vec<u8>>::deserialize(decoder)?
        .filter(|rgba| rgba.len() == 32 * 32 * 4)
        .map(|rgba| {
            let image = Pixels::new();
            image.set(32, 32, rgba);
            image
        }))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Metadata {
    Draft { draft: DraftRequest },
    Navigation(Navigation),
    Cursor { cursor: String },
}

#[derive(Deserialize)]
struct DraftRequest {
    id: uuid::Uuid,
    text: String,
}

// CSS cursor names available in Rune. Unsupported and custom images use the
// platform arrow so web content cannot hide the pointer outside its bounds.
fn page_cursor(shape: &str) -> Cursor {
    match shape {
        "pointer" | "hand2" => Cursor::Pointer,
        "text" | "vertical-text" | "xterm" => Cursor::Text,
        "grab" => Cursor::Grab,
        "grabbing" => Cursor::Grabbing,
        "not-allowed" | "no-drop" => Cursor::NotAllowed,
        "move" | "all-scroll" => Cursor::Move,
        "col-resize" | "ew-resize" | "e-resize" | "w-resize" => Cursor::ResizeColumn,
        "row-resize" | "ns-resize" | "n-resize" | "s-resize" => Cursor::ResizeRow,
        "crosshair" => Cursor::Crosshair,
        "cell" => Cursor::Cell,
        "zoom-in" => Cursor::ZoomIn,
        "zoom-out" => Cursor::ZoomOut,
        "none" => Cursor::Hidden,
        _ => Cursor::Default,
    }
}

struct State {
    id: u64,
    pixels: Pixels,
    pending_draft: Mutex<Option<DraftRequest>>,
    navigation: Mutex<Navigation>,
    cursor: Mutex<Cursor>,
    focused: AtomicBool,
    error: Mutex<Option<String>>,
    dirty: AtomicBool,
    visible: AtomicBool,
    metrics: Option<std::path::PathBuf>,
}

impl State {
    fn fail(&self, message: &str) {
        *self.error.lock().unwrap() = Some(message.to_owned());
        self.dirty.store(true, Ordering::Release);
    }

    fn metric(&self, event: Value) {
        let Some(path) = &self.metrics else { return };
        if let Ok(mut file) = OpenOptions::new().append(true).create(true).open(path) {
            let ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let _ = writeln!(
                file,
                "{}",
                json!({"timeMs":ms,"surface":self.id,"sample":event})
            );
        }
    }
}

struct Inner {
    state: Arc<State>,
    tx: SyncSender<Value>,
    child: Mutex<Option<Child>>,
    layout: Mutex<[f32; 3]>,
    plugin_context: Mutex<Option<super::context::WebContext>>,
    draft_target: Option<super::draft::Target>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(json!({"type":"close"}));
        if let Some(mut child) = self.child.get_mut().unwrap().take() {
            thread::spawn(move || {
                for _ in 0..25 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

#[derive(Clone)]
pub(crate) struct Surface(Arc<Inner>);

impl fmt::Debug for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WebViewSurface")
            .field(&self.0.state.id)
            .finish()
    }
}

impl Surface {
    pub(crate) fn new(
        helper: Command,
        url: String,
        context: super::context::WebContext,
        draft_access: Option<Option<super::draft::Target>>,
    ) -> Result<Self, String> {
        Self::start(helper, url, Some(context), draft_access)
    }
    pub(crate) fn browser(helper: Command, url: String) -> Result<Self, String> {
        Self::start(helper, url, None, None)
    }
    fn start(
        mut helper: Command,
        url: String,
        context: Option<super::context::WebContext>,
        draft_access: Option<Option<super::draft::Target>>,
    ) -> Result<Self, String> {
        let mut child = helper
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not start the WebView helper: {e}"))?;
        let mut input = BufWriter::new(child.stdin.take().unwrap());
        let output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel::<Value>(192);
        let state = Arc::new(State {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            pixels: Pixels::new(),
            pending_draft: Mutex::default(),
            navigation: Mutex::new(Navigation {
                url: url.clone(),
                ..Navigation::default()
            }),
            cursor: Mutex::default(),
            focused: AtomicBool::new(false),
            error: Mutex::default(),
            dirty: AtomicBool::new(false),
            visible: AtomicBool::new(true),
            metrics: std::env::var_os("MUQUN_WEBVIEW_METRICS_FILE").map(Into::into),
        });
        thread::spawn(move || {
            for command in rx {
                if serde_json::to_writer(&mut input, &command).is_err()
                    || input.write_all(b"\n").and_then(|_| input.flush()).is_err()
                {
                    break;
                }
                if command["type"] == "close" {
                    break;
                }
            }
        });
        let frames = state.clone();
        thread::spawn(move || {
            if read_frames(BufReader::new(output), &frames).is_err() {
                frames.fail("The WebView connection closed.");
            }
        });
        let mut open = json!({
            "type": "open",
            "mode": if context.is_some() { "plugin" } else { "browser" },
            "url": url,
            "width": 800,
            "height": 600,
            "capabilities": ["cursor"],
        });
        if draft_access.is_some() {
            open["draftNonce"] = json!(uuid::Uuid::new_v4());
        }
        if let Some(ctx) = &context {
            open["context"] = json!(ctx);
        }
        let surface = Self(Arc::new(Inner {
            state,
            tx,
            child: Mutex::new(Some(child)),
            layout: Mutex::new([800., 600., 1.]),
            plugin_context: Mutex::new(context),
            draft_target: draft_access.flatten(),
        }));
        surface.send(open);
        Ok(surface)
    }

    pub(super) fn apply_pending_draft(&self, s: &mut UiState, visible: bool) -> bool {
        let Some(request) = self.0.state.pending_draft.lock().unwrap().take() else {
            return false;
        };
        let result = if visible {
            self.0
                .draft_target
                .as_ref()
                .ok_or("Open this plugin from a conversation to add to its draft.")
                .and_then(|target| target.append(s, &request.text))
        } else {
            Err("This plugin view is no longer visible.")
        };
        self.send(json!({"type":"draftReply","id":request.id,"error":result.err()}));
        true
    }

    pub(super) fn refresh_context(&self, s: &UiState) {
        let mut saved = self.0.plugin_context.lock().unwrap();
        let Some(previous) = saved.as_mut() else {
            return;
        };
        let next = super::context::web_context(s, previous.workspace.is_some());
        if *previous != next {
            self.send(json!({"type":"context","context":next}));
            *previous = next;
        }
    }

    fn send(&self, event: Value) {
        match self.0.tx.try_send(event) {
            Ok(()) => (),
            Err(TrySendError::Full(v)) if v["phase"] == "move" => (),
            Err(_) => self.0.state.fail("The WebView helper is not responding."),
        }
    }

    pub(crate) fn navigate(&self, url: &str) {
        self.send(json!({"type":"navigate","url":url}));
    }
    pub(crate) fn back(&self) {
        self.send(json!({"type":"back"}));
    }
    pub(crate) fn forward(&self) {
        self.send(json!({"type":"forward"}));
    }
    pub(crate) fn reload(&self) {
        self.send(json!({"type":"reload"}));
    }
    pub(crate) fn navigation(&self) -> Navigation {
        self.0.state.navigation.lock().unwrap().clone()
    }

    pub(crate) fn set_visible(&self, show: bool) {
        if self.0.state.visible.swap(show, Ordering::AcqRel) != show {
            if !show {
                self.set_focused(false);
            }
            self.send(json!({"type":"visible","show":show}));
            self.0.state.metric(json!({"visible":show}));
        }
    }

    pub(crate) fn sync(&self) -> bool {
        self.0.state.dirty.swap(false, Ordering::AcqRel)
    }

    fn resize(&self, w: f32, h: f32, scale: f32) {
        if !(w > 0. && h > 0.) {
            return;
        }
        let mut size = self.0.layout.lock().unwrap();
        if *size == [w, h, scale] {
            return;
        }
        *size = [w, h, scale];
        self.send(json!({"type":"resize","width":edge(w),"height":edge(h),"scale":scale}));
    }

    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        let [w, h, _] = *self.0.layout.lock().unwrap();
        (x * edge(w) as f32 / w, y * edge(h) as f32 / h)
    }

    fn pointer(&self, phase: &str, x: f32, y: f32, button: u8) {
        let (x, y) = self.point(x, y);
        self.send(json!({"type":"pointer","phase":phase,"x":x,"y":y,"button":button}));
    }

    fn key(&self, event: &KeyEvent, down: bool) -> bool {
        let name = match &event.key {
            Key::Text(s) => s.clone(),
            Key::Command(c) => c.to_string(),
            Key::Function(number) => format!("F{number}"),
            Key::Enter => "Return".into(),
            Key::Backspace => "BackSpace".into(),
            Key::PageUp => "Page_Up".into(),
            Key::PageDown => "Page_Down".into(),
            Key::Tab
            | Key::Escape
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::Delete
            | Key::End
            | Key::Home
            | Key::Insert => format!("{:?}", event.key),
            _ => return false,
        };
        let mods = event.mods;
        self.send(json!({"type":"key","key":name,"down":down,"shift":mods.shift,"ctrl":mods.ctrl,"alt":mods.alt,"logo":mods.logo}));
        true
    }

    fn focus_key(&self) -> String {
        format!("embedded-webview-{}", self.0.state.id)
    }

    pub(crate) fn set_focused(&self, focused: bool) {
        if self.0.state.focused.swap(focused, Ordering::AcqRel) != focused {
            self.send(json!({"type":"focus","focused":focused}));
        }
    }

    pub(crate) fn focus(&self, tx: &mut Tx) {
        self.set_focused(true);
        tx.focus_when_shown(self.focus_key(), false);
    }

    pub(crate) fn view(&self, k: &Kit, scale: f32) -> El {
        self.view_with_keys(k, scale, None)
    }

    pub(crate) fn view_with_keys(
        &self,
        k: &Kit,
        scale: f32,
        shortcut: Option<SurfaceShortcut>,
    ) -> El {
        if let Some(error) = self.0.state.error.lock().unwrap().as_ref() {
            return text(error.clone()).color(k.c.icon_muted);
        }
        // A window may move to another monitor without changing its logical layout.
        let [w, h, previous] = *self.0.layout.lock().unwrap();
        if previous != scale {
            self.resize(w, h, scale);
        }
        let key = self.focus_key();
        let (layout, drag, hover, scroll, keydown, keyup, context) = (
            self.clone(),
            self.clone(),
            self.clone(),
            self.clone(),
            self.clone(),
            self.clone(),
            self.clone(),
        );
        el().key(key.clone())
            .w_full()
            .h_full()
            .grow(1.)
            .min_w(0.)
            .min_h(0.)
            .label("Web content")
            .focusable(true)
            .cursor(*self.0.state.cursor.lock().unwrap())
            .pixels(&self.0.state.pixels)
            .on_layout(move |_: &mut UiState, [_, _, w, h]| layout.resize(w, h, scale))
            .pointer_buttons([PointerButton::Middle])
            .on_drag(move |_: &mut UiState, event, tx: &mut Tx| {
                let phase = match event.phase {
                    Phase::Down => {
                        drag.set_focused(true);
                        tx.focus(key.clone(), false);
                        "down"
                    }
                    Phase::Up => "up",
                    Phase::Move => "move",
                };
                drag.pointer(
                    phase,
                    event.x,
                    event.y,
                    if tx.button() == PointerButton::Middle {
                        2
                    } else {
                        1
                    },
                );
            })
            .on_pointer_move_local(move |_: &mut UiState, event, tx| {
                hover.pointer("move", event.x, event.y, 0);
                tx.live_only();
            })
            .on_secondary_press_local(move |_: &mut UiState, event, _| {
                context.pointer("down", event.x, event.y, 3);
                context.pointer("up", event.x, event.y, 3);
            })
            .on_wheel(move |_: &mut UiState, event, _| {
                let (x, y) = scroll.point(event.x, event.y);
                scroll.send(json!({"type":"wheel","x":x,"y":y,"dx":-event.dx,"dy":-event.dy}));
                true
            })
            .on_key(move |s: &mut UiState, event, tx| {
                if s.leader_armed.is_some()
                    || s.leader_group.is_some()
                    || crate::ui::keymap::is_leader_event(s, event, tx)
                {
                    return false;
                }
                shortcut.is_some_and(|handle| handle(s, event, tx)) || keydown.key(event, true)
            })
            .on_key_up(move |_: &mut UiState, event, _| keyup.key(event, false))
    }
}

fn edge(value: f32) -> u32 {
    (value.round() as u32).clamp(MIN_EDGE, MAX_EDGE)
}

fn read_frames(mut stream: impl Read, state: &State) -> std::io::Result<()> {
    let mut count = 0u64;
    let mut bytes = 0u64;
    let mut last = Instant::now();
    loop {
        let mut header = [0; 8];
        stream.read_exact(&mut header)?;
        let width = u32::from_le_bytes(header[..4].try_into().unwrap());
        let height = u32::from_le_bytes(header[4..].try_into().unwrap());
        if width == 0 && (1..=65536).contains(&height) {
            let mut metadata = vec![0; height as usize];
            stream.read_exact(&mut metadata)?;
            match serde_json::from_slice(&metadata)? {
                Metadata::Draft { draft } => {
                    *state.pending_draft.lock().unwrap() = Some(draft);
                }
                Metadata::Navigation(location) => *state.navigation.lock().unwrap() = location,
                Metadata::Cursor { cursor } => {
                    let shape = page_cursor(&cursor);
                    let mut current = state.cursor.lock().unwrap();
                    if *current == shape {
                        continue;
                    }
                    *current = shape;
                    state.metric(json!({"cursor": format!("{shape:?}")}));
                }
            }
            state.dirty.store(true, Ordering::Release);
            continue;
        }
        if !(1..=MAX_EDGE).contains(&width)
            || !(1..=MAX_EDGE).contains(&height)
            || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
        {
            return Err(std::io::Error::other("Invalid WebView frame size"));
        }
        let len = (width * height * 4) as usize;
        let mut rgba = vec![0; len];
        stream.read_exact(&mut rgba)?;
        count += 1;
        bytes += len as u64;
        if state.metrics.is_some() && (count == 1 || last.elapsed() >= Duration::from_secs(1)) {
            state.metric(json!({"frames":count,"bytes":bytes,"width":width,"height":height,"visible":state.visible.load(Ordering::Acquire)}));
            last = Instant::now();
        }
        state.pixels.set(width, height, rgba);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_update_and_invalid_frame_preserve_navigation_and_last_complete_pixels() {
        let state = State {
            id: 1,
            pixels: Pixels::new(),
            pending_draft: Mutex::default(),
            navigation: Mutex::default(),
            cursor: Mutex::default(),
            focused: AtomicBool::new(false),
            error: Mutex::default(),
            dirty: AtomicBool::new(false),
            visible: AtomicBool::new(true),
            metrics: None,
        };
        let nav = br#"{"url":"https://example.com/","back":true,"forward":false}"#;
        let mut bytes = Vec::new();
        bytes.extend(0u32.to_le_bytes());
        bytes.extend((nav.len() as u32).to_le_bytes());
        bytes.extend(nav);
        let cursor = br#"{"cursor":"text"}"#;
        bytes.extend(0u32.to_le_bytes());
        bytes.extend((cursor.len() as u32).to_le_bytes());
        bytes.extend(cursor);
        bytes.extend(2u32.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        bytes.extend([255; 8]);
        bytes.extend(u32::MAX.to_le_bytes());
        bytes.extend(u32::MAX.to_le_bytes());

        assert!(read_frames(bytes.as_slice(), &state).is_err());
        assert_eq!(state.pixels.size(), (2, 1));
        assert_eq!(*state.cursor.lock().unwrap(), Cursor::Text);
        assert!(state.navigation.lock().unwrap().back);
        assert!(state.dirty.load(Ordering::Acquire));
    }
}
