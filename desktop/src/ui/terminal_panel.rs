//! The terminal panel: the owner's own shell, in a panel on the right.
//!
//! Ctrl+` (or the terminal button in the session header) slides it in and
//! gives it the keyboard; it starts in the focused session's folder. While
//! it has the keyboard every key goes to the shell, Esc included; the
//! shortcut plan is in `term/input.rs`. Closing hides it, the shell keeps
//! running until the owner ends it.
//!
//! Drawing: each grid row is one memoised element of absolutely placed
//! background rects and style runs (`term/grid.rs`), so only rows whose
//! content changed are built again. The keyboard lands in a transparent text
//! field sitting on the cursor cell: that is how the panel takes focus from
//! the model, and how an input method composes (its preedit draws there, and
//! its candidate window opens there).

use std::cell::Cell;
use std::path::PathBuf;

use rune::{col, el, memo, row, text, text_input, Color, El, KeyEvent, TextEdit, TextStyle, Tx};
use rune_kit::shell::PaneState;
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, MenuItem};

use super::copy::tr_ref;
use super::UiState;
use crate::platform::pty::Exit;
use crate::term::engine::{Colors, MouseInput, MousePhase, Size};
use crate::term::grid::{CursorShape, Frame, Rgb, Row};
use crate::term::input::{self, Mods, Route};
use crate::term::session::{Ended, Msg, Session};
use crate::theme::color_scheme::ColorScheme;

pub static TERMINAL_WINDOW: rune::Icon = rune::Icon {
    name: "terminal-window",
    view: 24.0,
    body: concat!(
        r#"<rect x="3" y="4" width="18" height="16" rx="2.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="m7 9 3 3-3 3m6 0h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

static RESTART: rune::Icon = rune::Icon {
    name: "refresh",
    view: 24.0,
    body: r#"<path d="M4 9a8 8 0 0 1 14-3l3 3M3 15l3 3a8 8 0 0 0 14-3M21 4v5h-5M3 20v-5h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static PLUS: rune::Icon = rune::Icon {
    name: "plus",
    view: 24.0,
    body: r#"<path d="M12 5v14m-7-7h14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// One tab within the terminal panel.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TermTab {
    pub id: u64,
    pub session: Option<Session>,
    pub frame: Option<Frame>,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub pending_command: Option<String>,
}

/// The grid's type: the mono slot (JetBrains Mono where installed).
const FONT_SIZE: f32 = 13.0;
const LINE: f32 = 17.0;
/// Inside the panel, beside and below the cells.
const INSET: f32 = 10.0;
/// The title strip's height: the main card's toolbar row, so the two line up.
const HEAD: f32 = rune_kit::shell::TOOLBAR_HEIGHT;
/// A shell that fails this soon after it started did not start right: the
/// panel stays to say so. Later, any exit is the owner's `exit` or Ctrl+D
/// (whose status is only the last command's).
const EARLY: std::time::Duration = std::time::Duration::from_secs(3);

fn style() -> TextStyle {
    TextStyle::new(FONT_SIZE, 400.0).mono().lh(LINE)
}

/// One cell's size in points: the mono face's advance, and the line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub cw: f32,
    pub ch: f32,
}

pub fn metrics() -> Metrics {
    let cw = rune::text::advance("0123456789", style()) / 10.0;
    Metrics {
        cw: if cw > 1.0 { cw } else { FONT_SIZE * 0.6 },
        ch: LINE,
    }
}

/// What the panel keeps between frames.
pub struct TermPanel {
    pub open: bool,
    pub pane: PaneState,
    pub tabs: Vec<TermTab>,
    pub active_tab: usize,
    pub next_tab_id: u64,
    pub session: Option<Session>,
    pub frame: Option<Frame>,
    /// The keyboard sink on the cursor cell (see the module docs).
    pub sink: TextEdit,
    pub waker: Option<rune::Waker>,
    /// Screenshot scenes: what a program would print at a size, replayed
    /// into a fresh emulator instead of a live shell.
    pub fixture: Option<fn(u16, u16) -> Vec<u8>>,
    /// Screenshot scenes: how the fixture's shell ended.
    pub fixture_ended: Option<Ended>,
    pub error: Option<String>,
    /// Instant layout without slide motion (used when launching code editors).
    pub instant: bool,
    /// A command queued to run as soon as the shell session starts.
    pub pending_command: Option<String>,
    /// The grid's size the shell and emulator have, in cells.
    size: Option<Size>,
    /// New sizes on their way to the shell (paced while the divider drags).
    resize: Resizer,
    /// The frame clock's seconds, as the last poll saw them.
    now: f64,
    /// The theme's colours as the last view resolved them; [`poll`] hands
    /// them to the emulator when they changed.
    pub want: Cell<Option<Colors>>,
    sent: Option<Colors>,
    wheel: f32,
    /// Where the context menu opened, while it is open.
    pub menu_at: Option<rune::PointerPress>,
    /// What the panel sent the shell, for the routing test.
    #[cfg(test)]
    pub sent_log: std::cell::RefCell<Vec<String>>,
}

impl TermPanel {
    pub fn new(width: f32) -> Self {
        Self {
            open: false,
            pane: PaneState::new(560.0)
                .range(360.0, super::panes::UNBOUNDED)
                .with_width(width)
                .collapsed(),
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            session: None,
            frame: None,
            sink: TextEdit::new(""),
            waker: None,
            fixture: None,
            fixture_ended: None,
            error: None,
            instant: false,
            pending_command: None,
            size: None,
            resize: Resizer::default(),
            now: 0.0,
            want: Cell::new(None),
            sent: None,
            wheel: 0.0,
            menu_at: None,
            #[cfg(test)]
            sent_log: Default::default(),
        }
    }

    /// Whether the panel is in the window (open, or sliding shut over a
    /// shell that keeps running).
    pub fn mounted(&self) -> bool {
        self.open || self.session.is_some() || self.fixture.is_some()
    }

    fn send(&self, msg: Msg) {
        #[cfg(test)]
        self.sent_log.borrow_mut().push(match &msg {
            Msg::Key(k) => format!("key {:?} ctrl={}", k.key, k.mods.ctrl),
            Msg::Text(t) => format!("text {t}"),
            Msg::Paste(t) => format!("paste {t}"),
            _ => "other".into(),
        });
        if let Some(session) = &self.session {
            session.send(msg);
        }
    }
}

/// Seconds between the sizes a dragged divider sends the shell. Every size
/// makes a full-screen program (nvim, lazygit) redraw the whole screen, so a
/// drag sends a few and the last as it ends, never one a frame.
const DRAG_EVERY: f64 = 0.12;

/// When a new grid size reaches the shell: at once, except while the owner
/// drags the divider, when at most every [`DRAG_EVERY`] seconds.
#[derive(Debug, Default)]
struct Resizer {
    pending: Option<Size>,
    /// When the last size went out.
    at: Option<f64>,
}

impl Resizer {
    /// `size` is wanted at `now`: it, if it goes out now.
    fn want(&mut self, size: Size, now: f64, dragging: bool) -> Option<Size> {
        self.pending = Some(size);
        self.due(now, dragging)
    }

    /// The waiting size, once it may go out.
    fn due(&mut self, now: f64, dragging: bool) -> Option<Size> {
        let size = self.pending?;
        if dragging && self.wait(now).is_some() {
            return None;
        }
        self.pending = None;
        self.at = Some(now);
        Some(size)
    }

    /// Seconds until the waiting size may go out, while it must wait.
    fn wait(&self, now: f64) -> Option<f64> {
        let left = DRAG_EVERY - (now - self.at?);
        (self.pending.is_some() && left > 0.0).then_some(left)
    }
}

/// The grid's size in cells: across from the pane's settled width (the
/// grid fills its panel, inside the panel's frame and the body's inset),
/// never its width mid-slide; down from the laid-out height.
fn grid_size(pane_width: f32, height: f32, m: Metrics, scale: f32) -> Size {
    let across = pane_width - 2.0 * super::panes::FRAME - 2.0 * INSET;
    Size {
        cols: ((across / m.cw).floor() as u16).max(2),
        rows: ((height / m.ch).floor() as u16).max(1),
        cell_w: (m.cw * scale).round().max(1.0) as u32,
        cell_h: (m.ch * scale).round().max(1.0) as u32,
    }
}

/// sRGB bytes to a Rune colour (linear light).
fn color(c: Rgb) -> Color {
    let d = |v: u8| rune::color::srgb_decode(f32::from(v) / 255.0);
    Color {
        r: d(c.0),
        g: d(c.1),
        b: d(c.2),
        a: 1.0,
    }
}

fn to_rgb(c: Color) -> Rgb {
    let e = |v: f32| (rune::color::srgb_encode(v.clamp(0.0, 1.0)) * 255.0).round() as u8;
    Rgb(e(c.r), e(c.g), e(c.b))
}

/// The panel's floor: the terminal's background (a program may set its own).
pub fn surface(s: &UiState, k: &Kit, scheme: ColorScheme) -> Color {
    color(s.term.frame.as_ref().map_or(colors(k, scheme).bg, |f| f.bg))
}

/// Whether a session that ended so, `lived` after it started, ended as the
/// owner meant it to: then the panel closes. Otherwise it stays to say how.
fn ended_well(ended: Ended, lived: std::time::Duration) -> bool {
    match ended {
        Ended::Shell(Exit::Code(0)) => true,
        Ended::Shell(Exit::Code(_)) => lived >= EARLY,
        Ended::Shell(Exit::Signal(_)) | Ended::Broken => false,
    }
}

/// The terminal's colours for this theme and scheme: the pack's own terminal
/// palette, or one derived from the kit's tokens when no pack is active.
pub fn colors(k: &Kit, scheme: ColorScheme) -> Colors {
    if let Some(t) = crate::theme::apply::terminal(scheme) {
        let hex = |s: &str| rune::color::hex(s);
        let mut ansi = [Rgb::default(); 16];
        let mut ok = true;
        for (slot, value) in ansi.iter_mut().zip(&t.ansi) {
            match hex(value) {
                Some(c) => *slot = to_rgb(c),
                None => ok = false,
            }
        }
        if let (true, Some(fg), Some(bg), Some(cursor), Some(sel)) = (
            ok,
            hex(&t.foreground),
            hex(&t.background),
            hex(&t.cursor),
            hex(&t.selection),
        ) {
            return Colors {
                fg: to_rgb(fg),
                bg: to_rgb(bg),
                cursor: to_rgb(cursor),
                selection: to_rgb(bg.mix(sel.alpha(1.0), sel.a.max(0.25))),
                ansi,
            };
        }
    }
    let c = &k.c;
    let (bg, fg) = (c.bg_base, c.text_base);
    let tone = |col: Color, t: f32| to_rgb(bg.mix(col, t));
    let (red, green, yellow, blue) = (c.danger.fg, c.success.fg, c.warning.fg, c.info.fg);
    let magenta = red.mix(blue, 0.5);
    let cyan = green.mix(blue, 0.5);
    // Black stays the dark end and white the light end in either scheme,
    // as programs assume.
    let (black, white, grey, bright_white) = if k.dark {
        (tone(fg, 0.25), tone(fg, 0.75), tone(fg, 0.45), to_rgb(fg))
    } else {
        (to_rgb(fg), tone(fg, 0.3), tone(fg, 0.55), tone(fg, 0.12))
    };
    let base = [
        black,
        to_rgb(red),
        to_rgb(green),
        to_rgb(yellow),
        to_rgb(blue),
        to_rgb(magenta),
        to_rgb(cyan),
        white,
    ];
    let mut ansi = [Rgb::default(); 16];
    for i in 0..8 {
        ansi[i] = base[i];
        let lift = if k.dark { Color::WHITE } else { Color::BLACK };
        ansi[i + 8] = to_rgb(color(base[i]).mix(lift, 0.2));
    }
    ansi[8] = grey;
    ansi[15] = bright_white;
    Colors {
        fg: to_rgb(fg),
        bg: to_rgb(bg),
        cursor: to_rgb(c.text_accent),
        selection: to_rgb(bg.mix(c.bg_accent, 0.3)),
        ansi,
    }
}

impl UiState {
    /// Ctrl+`: open and focus; focused, close and hand the keyboard back;
    /// open but elsewhere, take the keyboard.
    pub fn toggle_terminal(&mut self) {
        if !self.term.open {
            self.open_terminal();
        } else if self.term.sink.focused {
            self.close_terminal();
        } else {
            self.term.sink.focused = true;
        }
    }

    pub fn open_terminal(&mut self) {
        self.term.open = true;
        self.term.pane.set_open(true);
        self.term.sink.focused = true;
        self.right_open = false;
        self.right.set_open(false);
    }

    /// Hide the panel; the shell keeps running.
    pub fn close_terminal(&mut self) {
        self.term.instant = false;
        self.term.open = false;
        self.term.pane.set_open(false);
        self.term.sink.focused = false;
        if self.mode == super::Mode::Session {
            self.draft.focused = true;
        }
    }

    /// Open a file in the embedded terminal using a terminal editor (e.g. nvim).
    /// Opens the panel instantly without slide animation so it does not distract from editing.
    pub fn open_file_in_terminal(&mut self, path: &std::path::Path, editor: &str) {
        self.close_viewer();
        self.term.instant = true;

        let path_str = path.to_string_lossy();
        let escaped_shell = format!("'{}'", path_str.replace('\'', "'\\''"));
        let cmd = format!("{editor} {escaped_shell}\n");

        self.open_terminal();

        if let Some(session) = &self.term.session {
            if session.ended().is_some() {
                self.term.pending_command = Some(cmd);
                self.restart_terminal();
            } else if self.term.frame.as_ref().is_some_and(|f| f.alternate_screen) {
                // If an editor (like nvim) is already running in alternate screen,
                // send Esc to exit to normal mode and `:e <path>` to edit buffer.
                let escaped_vim = path_str.replace(' ', "\\ ");
                self.term.send(Msg::Text(format!("\x1b:e {escaped_vim}\r")));
            } else {
                // Shell is active: clear line with Ctrl+C, Ctrl+U and send command.
                self.term.send(Msg::Text(format!("\x03\x15{cmd}")));
            }
        } else {
            self.term.pending_command = Some(cmd);
        }
    }

    /// Open a new terminal tab in `cwd`, optionally executing `command`.
    pub fn term_new_tab(&mut self) {
        let cwd = self.terminal_cwd();
        self.term_new_tab_in(cwd, None);
    }

    pub fn term_new_tab_in(&mut self, cwd: PathBuf, command: Option<String>) {
        self.open_terminal();
        if self.term.tabs.is_empty() {
            if self.term.session.is_some() || self.term.frame.is_some() {
                let id = self.term.next_tab_id;
                self.term.next_tab_id += 1;
                let cur_cwd = self
                    .term
                    .session
                    .as_ref()
                    .map(|s| s.cwd.clone())
                    .unwrap_or_else(|| self.terminal_cwd());
                self.term.tabs.push(TermTab {
                    id,
                    session: self.term.session.clone(),
                    frame: self.term.frame.clone(),
                    cwd: cur_cwd,
                    title: None,
                    pending_command: None,
                });
            }
        } else if let Some(cur) = self.term.tabs.get_mut(self.term.active_tab) {
            cur.session = self.term.session.clone();
            cur.frame = self.term.frame.clone();
        }

        let id = self.term.next_tab_id;
        self.term.next_tab_id += 1;
        self.term.tabs.push(TermTab {
            id,
            session: None,
            frame: None,
            cwd: cwd.clone(),
            title: None,
            pending_command: command.clone(),
        });
        self.term.active_tab = self.term.tabs.len() - 1;
        self.term.session = None;
        self.term.frame = None;
        self.term.pending_command = command;
        self.start_terminal_in(cwd);
        self.term.sink.focused = true;
    }

    pub fn term_switch_tab(&mut self, index: usize) {
        if index >= self.term.tabs.len() || index == self.term.active_tab {
            return;
        }
        if let Some(cur) = self.term.tabs.get_mut(self.term.active_tab) {
            cur.session = self.term.session.clone();
            cur.frame = self.term.frame.clone();
        }
        self.term.active_tab = index;
        self.term.session = self.term.tabs[index].session.clone();
        self.term.frame = self.term.tabs[index].frame.clone();
        if let Some(size) = self.term.size {
            self.term.send(Msg::Resize(size));
        }
        self.term.sink.focused = true;
    }

    pub fn term_close_tab(&mut self, index: usize) {
        if index >= self.term.tabs.len() {
            if self.term.tabs.is_empty() {
                self.close_terminal();
            }
            return;
        }
        if self.term.tabs.len() <= 1 {
            self.term.tabs.clear();
            self.close_terminal();
            return;
        }
        if index == self.term.active_tab {
            if let Some(session) = &self.term.session {
                session.send(Msg::Shutdown);
            }
            self.term.tabs.remove(index);
            self.term.active_tab = self.term.active_tab.min(self.term.tabs.len() - 1);
            self.term.session = self.term.tabs[self.term.active_tab].session.clone();
            self.term.frame = self.term.tabs[self.term.active_tab].frame.clone();
            if let Some(size) = self.term.size {
                self.term.send(Msg::Resize(size));
            }
        } else {
            let tab = self.term.tabs.remove(index);
            if let Some(s) = tab.session {
                s.send(Msg::Shutdown);
            }
            if index < self.term.active_tab {
                self.term.active_tab -= 1;
            }
        }
        self.term.sink.focused = true;
    }

    /// End the shell and start a new one in the focused session's folder.
    pub fn restart_terminal(&mut self) {
        self.term.session = None;
        self.term.frame = None;
        self.term.error = None;
        let cwd = self
            .term
            .tabs
            .get(self.term.active_tab)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.terminal_cwd());
        self.start_terminal_in(cwd);
    }

    /// Where a new shell starts: the focused session's folder, else home.
    pub fn terminal_cwd(&self) -> PathBuf {
        self.current()
            .and_then(|info| info.directory.clone())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn start_terminal(&mut self) {
        let cwd = self.terminal_cwd();
        self.start_terminal_in(cwd);
    }

    fn start_terminal_in(&mut self, cwd: PathBuf) {
        if self.term.fixture.is_some() || self.term.session.is_some() {
            return;
        }
        let (Some(size), Some(colors), Some(waker)) = (
            self.term.size,
            self.term.want.get(),
            self.term.waker.clone(),
        ) else {
            return;
        };
        match Session::start(cwd.clone(), size, colors, waker) {
            Ok(session) => {
                if let Some(cmd) = self.term.pending_command.take() {
                    session.send(Msg::Text(cmd));
                }
                self.term.session = Some(session.clone());
                if self.term.tabs.is_empty() {
                    let id = self.term.next_tab_id;
                    self.term.next_tab_id += 1;
                    self.term.tabs.push(TermTab {
                        id,
                        session: Some(session),
                        frame: None,
                        cwd,
                        title: None,
                        pending_command: None,
                    });
                    self.term.active_tab = 0;
                } else if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
                    active.session = Some(session);
                    active.cwd = cwd;
                }
                self.term.sent = Some(colors);
                self.term.error = None;
            }
            Err(err) => self.term.error = Some(err.to_string()),
        }
    }

    /// The grid was laid out at `[x, y, w, h]`: the shell's size follows.
    /// Hidden, the shell keeps the size it had.
    fn terminal_layout(&mut self, rect: [f32; 4], scale: f32) {
        if !self.term.open || rect[2] < 40.0 || rect[3] < 20.0 {
            return;
        }
        let size = grid_size(self.term.pane.width(), rect[3], metrics(), scale);
        if self.term.size == Some(size)
            && (self.term.session.is_some() || self.term.frame.is_some())
        {
            self.term.resize.pending = None;
            return;
        }
        // A size held back waits for `poll_terminal`, which wakes for it.
        let dragging = self.term.pane.is_resizing();
        if let Some(size) = self.term.resize.want(size, self.term.now, dragging) {
            self.resize_terminal(size);
        }
    }

    /// Give the shell and emulator (or a scene's fixture) a new grid size.
    fn resize_terminal(&mut self, size: Size) {
        self.term.size = Some(size);
        tracing::debug!(target: "muqun_desktop::term_resize", cols = size.cols, rows = size.rows, "grid resize");
        if let Some(fixture) = self.term.fixture {
            if let Some(colors) = self.term.want.get() {
                let bytes = fixture(size.cols, size.rows);
                self.term.frame = crate::term::engine::render_fixture(&bytes, size, colors);
                self.term.sent = Some(colors);
            }
        } else if self.term.session.is_some() {
            self.term.send(Msg::Resize(size));
        } else {
            self.start_terminal();
        }
    }

    /// Before each frame: send a size a drag held back, take the emulator's
    /// newest picture, and pass on a theme change.
    pub fn poll_terminal(&mut self, now: f64, tx: &mut Tx) -> bool {
        self.term.now = now;
        let mut changed = false;
        let dragging = self.term.pane.is_resizing();
        if let Some(size) = self.term.resize.due(now, dragging) {
            if self.term.size != Some(size) {
                self.resize_terminal(size);
                changed = true;
            }
        } else if let Some(left) = self.term.resize.wait(now) {
            tx.wake_after(std::time::Duration::from_secs_f64(left));
        }
        if let Some(want) = self.term.want.get() {
            if self.term.sent.is_some_and(|sent| sent != want) {
                self.term.sent = Some(want);
                if let (Some(fixture), Some(size)) = (self.term.fixture, self.term.size) {
                    let bytes = fixture(size.cols, size.rows);
                    self.term.frame = crate::term::engine::render_fixture(&bytes, size, want);
                    changed = true;
                } else {
                    self.term.send(Msg::Colors(want));
                }
            }
        }
        if let Some(frame) = self.term.session.as_ref().and_then(Session::take_frame) {
            self.term.frame = Some(frame.clone());
            if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
                active.frame = Some(frame);
            }
            changed = true;
        }
        for (i, tab) in self.term.tabs.iter_mut().enumerate() {
            if i != self.term.active_tab {
                if let Some(frame) = tab.session.as_ref().and_then(Session::take_frame) {
                    tab.frame = Some(frame);
                }
            }
        }
        self.settle_terminal() | changed
    }

    /// The shell ended as the owner meant it to (`exit`, Ctrl+D): close the
    /// panel and drop the session, so the next Ctrl+` starts a fresh shell.
    /// Ended otherwise, it stays with a notice and a Restart.
    fn settle_terminal(&mut self) -> bool {
        let Some(session) = &self.term.session else {
            return false;
        };
        match session.ended() {
            Some(ended) if ended_well(ended, session.started.elapsed()) => {
                self.term.session = None;
                self.term.frame = None;
                if self.term.tabs.len() > 1 {
                    self.term_close_tab(self.term.active_tab);
                    true
                } else {
                    self.term.tabs.clear();
                    if self.term.open {
                        self.close_terminal();
                    }
                    true
                }
            }
            _ => false,
        }
    }

    /// A key while the terminal has the keyboard (the sink's key handler).
    fn terminal_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let mac = crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;
        let letter =
            |c: char| matches!(&ev.key, rune::Key::Command(k) if k.eq_ignore_ascii_case(&c));
        // Tab shortcuts: Ctrl+Shift+T (new tab) and Ctrl+Shift+W (close tab)
        if (ev.mods.ctrl && ev.mods.shift && !ev.mods.alt) || (mac && ev.mods.logo && ev.mods.shift)
        {
            if letter('t') {
                self.term_new_tab();
                return true;
            }
            if letter('w') {
                self.term_close_tab(self.term.active_tab);
                return true;
            }
        }
        match input::route(ev, tx.physical_key(), mac) {
            Route::Toggle => self.toggle_terminal(),
            Route::Copy => {
                if let Some(text) = self.term.frame.as_ref().and_then(|f| f.selection.clone()) {
                    tx.copy(text);
                }
            }
            Route::Paste => tx.request_paste(),
            Route::App => {
                // Ctrl+Shift+K reaches Ctrl+K's shortcut when the app has
                // no Ctrl+Shift+K of its own.
                if !super::on_key(self, ev, tx) {
                    let mut plain = ev.clone();
                    plain.mods.shift = false;
                    super::on_key(self, &plain, tx);
                }
            }
            Route::Terminal => {
                if self
                    .term
                    .session
                    .as_ref()
                    .is_some_and(|x| x.ended().is_some())
                {
                    if ev.key == rune::Key::Enter {
                        self.restart_terminal();
                    }
                    return true;
                }
                if let Some(key) =
                    input::key_input(ev, tx.physical_key(), tx.key_base(), tx.key_repeat())
                {
                    self.term.send(Msg::Key(key));
                }
            }
        }
        true
    }

    fn terminal_mouse(&mut self, phase: MousePhase, x: f32, y: f32, mods: rune::Mods) {
        let (Some(size), m) = (self.term.size, metrics()) else {
            return;
        };
        self.term.send(Msg::Mouse(MouseInput {
            phase,
            x: x / m.cw * size.cell_w as f32,
            y: y / m.ch * size.cell_h as f32,
            mods: Mods {
                shift: mods.shift,
                ctrl: mods.ctrl,
                alt: mods.alt,
                logo: mods.logo,
            },
        }));
    }
}

/// The panel.
pub fn view(s: &UiState, k: &Kit, scheme: ColorScheme, scale: f32, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let colors = colors(k, scheme);
    s.term.want.set(Some(colors));
    let m = metrics();
    let frame = s.term.frame.as_ref();
    let bg = color(frame.map_or(colors.bg, |f| f.bg));
    let ended = s
        .term
        .session
        .as_ref()
        .and_then(Session::ended)
        .or(s.term.fixture_ended);

    let tab_count = s.term.tabs.len().max(1);
    let mut tabs_strip = row()
        .key("terminal-tabs-strip")
        .gap(4.0)
        .center_y()
        .shrink()
        .min_w(0.0)
        .grow(1.0);

    for i in 0..tab_count {
        let is_active = i == s.term.active_tab;
        let tab_title = if is_active {
            frame
                .map(|f| f.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    let cwd = s
                        .term
                        .session
                        .as_ref()
                        .map(|x| x.cwd.clone())
                        .unwrap_or_else(|| s.terminal_cwd());
                    short_path(&cwd)
                })
        } else if let Some(tab) = s.term.tabs.get(i) {
            tab.frame
                .as_ref()
                .map(|f| f.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| short_path(&tab.cwd))
        } else {
            format!("Tab {}", i + 1)
        };

        let (bg_color, fg_color, border_color) = if is_active {
            (k.c.layer_01, k.c.text_base, k.c.border_base)
        } else {
            (
                k.c.bg_base.alpha(0.0),
                k.c.text_muted,
                k.c.border_base.alpha(0.0),
            )
        };

        let mut tab_pill = row()
            .key(format!("terminal-tab-{i}"))
            .h(26.0)
            .pl(8.0)
            .pr(if tab_count > 1 { 2.0 } else { 8.0 })
            .gap(5.0)
            .center_y()
            .fill(bg_color)
            .border(1.0, border_color)
            .clip_rounded(6.0)
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut UiState| s.term_switch_tab(i))
            .child(
                rune::icon(&TERMINAL_WINDOW, 12.0)
                    .color(if is_active {
                        k.c.text_accent
                    } else {
                        k.c.icon_muted
                    })
                    .a11y_hidden(),
            )
            .child(
                text(tab_title)
                    .font(k.ty.caption())
                    .color(fg_color)
                    .truncate()
                    .shrink()
                    .max_w(130.0),
            );

        if tab_count > 1 {
            let close_tab = IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Close tab", "关闭标签页"))
                .on_press(move |s: &mut UiState| s.term_close_tab(i))
                .build(k)
                .key(format!("terminal-tab-close-{i}"));
            tab_pill = tab_pill.child(close_tab);
        }

        tabs_strip = tabs_strip.child(tab_pill);
    }

    let add_tab_btn = IconButton::new(&PLUS)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(
            language,
            "New tab (Ctrl+Shift+T)",
            "新建标签页 (Ctrl+Shift+T)",
        ))
        .on_press(|s: &mut UiState| s.term_new_tab())
        .build(k)
        .key("terminal-new-tab");
    tabs_strip = tabs_strip.child(add_tab_btn);

    let header = row()
        .key("terminal-head")
        .w_full()
        .h(HEAD)
        .pl(INSET)
        .pr(INSET)
        .gap(6.0)
        .center_y()
        .child(tabs_strip)
        .child(
            IconButton::new(&RESTART)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Restart shell", "重启 Shell"))
                .on_press(|s: &mut UiState| s.restart_terminal())
                .build(k)
                .key("terminal-restart"),
        )
        .child(
            IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Hide terminal", "隐藏终端"))
                .on_press(|s: &mut UiState| s.close_terminal())
                .build(k)
                .key("terminal-close"),
        );

    let grid = context_menu(s, k, grid(s, k, frame, m, bg, scale), layer);
    let mut body = col()
        .key("terminal-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(INSET)
        .pb(INSET)
        .gap(8.0)
        .child(grid);
    if let Some(err) = &s.term.error {
        body = body.child(
            text(err.clone())
                .key("terminal-error")
                .font(k.ty.body)
                .color(k.c.danger.fg)
                .wrap(),
        );
    } else if let Some(ended) = ended {
        body = body.child(ended_notice(k, language, ended));
    }
    col()
        .key("terminal-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(header)
        .child(body)
}

/// What a shell that ended badly says under its last screen: how it ended,
/// and a new one a press (or Return) away.
fn ended_notice(k: &Kit, language: crate::config::Language, ended: Ended) -> El {
    use super::copy::tr;
    let what = match ended {
        Ended::Shell(Exit::Code(n)) => tr(
            language,
            &format!("Shell exited (code {n})"),
            &format!("Shell 已退出（代码 {n}）"),
        ),
        Ended::Shell(Exit::Signal(n)) => tr(
            language,
            &format!("Shell exited (signal {n})"),
            &format!("Shell 已退出（信号 {n}）"),
        ),
        Ended::Broken => tr(language, "The terminal stopped", "终端已停止"),
    };
    row()
        .key("terminal-ended")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(what)
                .key("terminal-ended-text")
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .truncate()
                .shrink()
                .min_w(0.0)
                .grow(1.0),
        )
        .child(
            Button::new(tr_ref(language, "Restart", "重新启动"))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.restart_terminal())
                .build(k)
                .key("terminal-ended-restart"),
        )
}

/// Right-click: what a desktop terminal offers there.
fn context_menu(s: &UiState, k: &Kit, grid: El, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let has_selection = s.term.frame.as_ref().is_some_and(|f| f.selection.is_some());
    use super::keymap::{label, Action};
    let grid = grid.on_secondary_press(|s: &mut UiState, at, _: &mut Tx| {
        s.term.menu_at = Some(at);
        s.term.sink.focused = true;
    });
    rune_kit::context_menu("terminal-menu", grid, s.term.menu_at)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.term.menu_at = None)
        .item(
            MenuItem::new(tr_ref(language, "Copy", "复制"))
                .shortcut(label(Action::TermCopy))
                .disabled(!has_selection)
                .on_select_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.term.menu_at = None;
                    if let Some(text) = s.term.frame.as_ref().and_then(|f| f.selection.clone()) {
                        tx.copy(text);
                    }
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Paste", "粘贴"))
                .shortcut(label(Action::TermPaste))
                .on_select_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.term.menu_at = None;
                    tx.request_paste();
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Clear", "清屏")).on_select(|s: &mut UiState| {
                s.term.menu_at = None;
                s.term.send(Msg::Clear);
            }),
        )
        .separator()
        .item(
            MenuItem::new(tr_ref(language, "New shell", "新的 Shell"))
                .shortcut(label(Action::NewShell))
                .on_select(|s: &mut UiState| {
                    s.term.menu_at = None;
                    s.restart_terminal();
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Hide terminal", "隐藏终端"))
                .shortcut(label(Action::ToggleTerminal))
                .on_select(|s: &mut UiState| {
                    s.term.menu_at = None;
                    s.close_terminal();
                }),
        )
        .build(k, layer)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
}

/// `~/…` for paths under home.
fn short_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

fn grid(s: &UiState, k: &Kit, frame: Option<&Frame>, m: Metrics, bg: Color, scale: f32) -> El {
    let focused = s.term.sink.focused;
    let mut grid = el()
        .key("terminal-grid")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .cursor(if frame.is_some_and(|f| f.mouse_tracking) {
            rune::Cursor::Default
        } else {
            rune::Cursor::Text
        })
        .on_layout(move |s: &mut UiState, r| s.terminal_layout(r, scale))
        .on_drag(|s: &mut UiState, d: &mut rune::Drag, tx: &mut Tx| {
            let phase = match d.phase {
                rune::Phase::Down => {
                    s.term.sink.focused = true;
                    MousePhase::Down
                }
                rune::Phase::Move => MousePhase::Move,
                rune::Phase::Up => MousePhase::Up,
            };
            s.terminal_mouse(phase, d.x, d.y, tx.mods());
        })
        .on_wheel(|s: &mut UiState, e: rune::WheelEvent, _: &mut Tx| {
            let ch = metrics().ch;
            s.term.wheel += e.dy;
            let lines = -(s.term.wheel / ch).trunc() as i32;
            if lines != 0 {
                s.term.wheel += lines as f32 * ch;
                let size = s.term.size;
                let (x, y) = size.map_or((0.0, 0.0), |sz| {
                    (
                        e.x / metrics().cw * sz.cell_w as f32,
                        e.y / ch * sz.cell_h as f32,
                    )
                });
                s.term.send(Msg::Wheel {
                    lines,
                    x,
                    y,
                    mods: Mods {
                        shift: e.mods.shift,
                        ctrl: e.mods.ctrl,
                        alt: e.mods.alt,
                        logo: e.mods.logo,
                    },
                });
            }
            true
        });
    let Some(frame) = frame else {
        return grid.child(sink(s, k, m, 0, 0, bg));
    };
    for (y, row) in frame.grid.iter().enumerate() {
        let row = row.clone();
        let key = format!("terminal-row-{y}");
        let version = (row.hash, m.cw.to_bits(), y);
        grid = grid.child(memo(key.clone(), version, move || {
            draw_row(&key, &row, y, m)
        }));
    }
    // A shell that ended takes no more keys: no cursor.
    let ended = s.term.fixture_ended.is_some()
        || s.term.session.as_ref().is_some_and(|x| x.ended().is_some());
    if let Some(cursor) = frame.cursor.filter(|_| !ended) {
        let cells = if cursor.wide { 2.0 } else { 1.0 };
        let (x, y) = (f32::from(cursor.col) * m.cw, f32::from(cursor.row) * m.ch);
        let c = color(frame.cursor_color);
        let mark = match (cursor.shape, focused) {
            (CursorShape::Block, true) => el()
                .absolute(x, y)
                .size(m.cw * cells, m.ch)
                .fill(c)
                .child(cursor_glyph(frame, cursor.col, cursor.row, bg)),
            (CursorShape::Block, false) => {
                el().absolute(x, y).size(m.cw * cells, m.ch).border(1.0, c)
            }
            (CursorShape::Bar, _) => el().absolute(x, y).size(2.0, m.ch).fill(c),
            (CursorShape::Underline, _) => el()
                .absolute(x, y + m.ch - 2.0)
                .size(m.cw * cells, 2.0)
                .fill(c),
        };
        grid = grid.child(mark.key("terminal-cursor").a11y_hidden());
    }
    if frame.scroll.in_history() && frame.scroll.total > 0 {
        // Where the viewport is in the history, along the right edge.
        let h = f32::from(frame.rows) * m.ch;
        let total = frame.scroll.total as f32;
        let thumb = (frame.scroll.len as f32 / total * h).clamp(16.0, h);
        let range = (frame.scroll.total - frame.scroll.len).max(1) as f32;
        let top = frame.scroll.offset as f32 / range * (h - thumb);
        grid = grid.child(
            el().key("terminal-scroll")
                .absolute(f32::from(frame.cols) * m.cw - 3.0, top.min(h - thumb))
                .size(3.0, thumb)
                .radius(1.5)
                .fill(k.c.text_faint.alpha(0.6))
                .a11y_hidden(),
        );
    }
    let (cx, cy) = frame.cursor.map_or((0, 0), |c| (c.col, c.row));
    grid.child(sink(s, k, m, cx, cy, bg))
}

/// The character under a block cursor, drawn in the background colour.
fn cursor_glyph(frame: &Frame, col: u16, row: u16, bg: Color) -> El {
    let ch = frame
        .grid
        .get(usize::from(row))
        .and_then(|r| {
            r.runs.iter().find_map(|run| {
                (col >= run.col && col < run.col + run.cells).then(|| {
                    if run.text.chars().count() == usize::from(run.cells) {
                        run.text
                            .chars()
                            .nth(usize::from(col - run.col))
                            .map(String::from)
                            .unwrap_or_default()
                    } else {
                        run.text.clone()
                    }
                })
            })
        })
        .unwrap_or_default();
    text(ch).font(style()).color(bg)
}

fn draw_row(key: &str, row: &Row, y: usize, m: Metrics) -> El {
    let mut out = el()
        .key(key.to_string())
        .absolute(0.0, y as f32 * m.ch)
        .h(m.ch)
        .w_full()
        .a11y_hidden();
    for bg in &row.bgs {
        out = out.child(
            el().absolute(f32::from(bg.col) * m.cw, 0.0)
                .size(f32::from(bg.len) * m.cw, m.ch)
                .fill(color(bg.color)),
        );
    }
    for run in &row.runs {
        let x = f32::from(run.col) * m.cw;
        let mut fg = color(run.fg);
        if run.attrs.faint {
            fg = fg.alpha(0.6);
        }
        let mut font = style();
        if run.attrs.bold {
            font.weight = 700.0;
        }
        if run.attrs.italic {
            font = font.italic();
        }
        out = out.child(text(run.text.clone()).font(font).color(fg).absolute(x, 0.0));
        if run.attrs.bold {
            // Rune resolves the mono slot's family to its regular face only
            // (the Nerd Font's Bold is installed but not picked), so bold is
            // drawn twice, a fraction of a point apart.
            out = out.child(
                text(run.text.clone())
                    .font(font)
                    .color(fg)
                    .absolute(x + 0.6, 0.0),
            );
        }
        let w = f32::from(run.cells) * m.cw;
        if run.attrs.underline {
            out = out.child(el().absolute(x, m.ch - 2.0).size(w, 1.0).fill(fg));
        }
        if run.attrs.strike {
            out = out.child(el().absolute(x, (m.ch / 2.0).round()).size(w, 1.0).fill(fg));
        }
    }
    out
}

/// The transparent field on the cursor cell that holds the keyboard.
fn sink(s: &UiState, k: &Kit, m: Metrics, col: u16, row: u16, bg: Color) -> El {
    let composing = s.term.sink.is_composing();
    let mut field = text_input(&s.term.sink, style())
        .key("terminal-keys")
        .single_line()
        .absolute(f32::from(col) * m.cw, f32::from(row) * m.ch)
        .min_w(m.cw)
        .h(m.ch)
        .caret(Color::TRANSPARENT, k.c.bg_accent.alpha(0.3))
        .color(k.c.text_base)
        .label("Terminal")
        .on_edit(|s: &mut UiState| &mut s.term.sink)
        .on_input_key(|s: &mut UiState, ev: &KeyEvent, tx: &mut Tx| s.terminal_key(ev, tx))
        .on_input_change(|s: &mut UiState, _| {
            // Only an input method's commits land in the field (typed keys
            // are taken above): send them on as typed.
            let typed = std::mem::take(&mut s.term.sink.text);
            if !typed.is_empty() {
                s.term.send(Msg::Text(typed));
                s.term.sink.clear();
            }
        })
        .on_input_paste(|s: &mut UiState, content: &rune::ClipboardContent, _| {
            s.term.send(Msg::Paste(content.text().to_string()));
            true
        });
    if composing {
        field = field.fill(bg);
    }
    field
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;
    use rune::{Env, Ime, Key, Mods, Runtime};

    #[test]
    fn the_grid_takes_the_settled_pane_width_and_a_drag_paces_its_resizes() {
        let m = Metrics { cw: 8.0, ch: 17.0 };
        // 560 wide less the panel's frame and the inset on each side: 65
        // columns, whatever the slide.
        assert_eq!(grid_size(560.0, 850.0, m, 2.0).cols, 65);
        assert_eq!(grid_size(560.0, 850.0, m, 2.0).rows, 50);

        let size = |cols| Size {
            cols,
            rows: 50,
            cell_w: 16,
            cell_h: 34,
        };
        let mut r = Resizer::default();
        // Not dragging: every size goes out at once.
        assert_eq!(r.want(size(60), 0.0, false), Some(size(60)));
        assert_eq!(r.want(size(61), 0.01, false), Some(size(61)));
        // A drag a frame at a time for half a second: a few sizes, not 30.
        let mut sent = 0;
        for f in 1..=30 {
            let now = 1.0 + f as f64 / 60.0;
            if r.want(size(61 + f), now, true).is_some() {
                sent += 1;
            }
        }
        assert!(
            (3..=5).contains(&sent),
            "{sent} sizes in half a second of dragging"
        );
        // Held still, the last size waits its turn; released, it goes at once.
        assert!(r.wait(1.5).is_some());
        assert_eq!(r.due(1.5, false), Some(size(91)));
        assert_eq!(r.due(1.6, false), None);
    }

    /// The panel over a real pty running `/bin/sh -c script`, polled until
    /// the shell has ended and the panel has settled.
    #[cfg(unix)]
    fn panel_after(script: &str) -> UiState {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.open_terminal();
        let size = grid_size(560.0, 340.0, Metrics { cw: 8.0, ch: 17.0 }, 1.0);
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", script]);
        let session = Session::start_command(
            cmd,
            size,
            colors(&Kit::neutral(false), ColorScheme::Light),
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        s.term.session = Some(session);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while s.term.session.as_ref().is_some_and(|x| x.ended().is_none()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never ended"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        s.settle_terminal();
        s
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_that_exits_closes_the_panel_and_one_that_fails_stays_to_say_so() {
        // `exit` (status 0): the panel closes and the next open starts fresh.
        let s = panel_after("echo bye; exit 0");
        assert!(!s.term.open && !s.term.pane.is_open());
        assert!(s.term.session.is_none() && s.term.frame.is_none());
        assert!(!s.term.mounted());

        // Failing at once: the panel stays, with the shell's last screen
        // and how it ended.
        let s = panel_after("echo 'no such rc file'; exit 3");
        assert!(s.term.open);
        let session = s.term.session.as_ref().unwrap();
        assert_eq!(session.ended(), Some(Ended::Shell(Exit::Code(3))));
        let frame = session.take_frame().expect("the last screen");
        let screen: String = frame
            .grid
            .iter()
            .flat_map(|r| &r.runs)
            .map(|r| r.text.as_str())
            .collect();
        assert!(screen.contains("no such rc file"), "{screen:?}");
    }

    #[test]
    fn a_late_nonzero_exit_is_the_owners_own() {
        use std::time::Duration;
        let late = EARLY + Duration::from_secs(1);
        assert!(ended_well(Ended::Shell(Exit::Code(0)), Duration::ZERO));
        assert!(!ended_well(Ended::Shell(Exit::Code(3)), Duration::ZERO));
        // Ctrl+D after a failed command: the shell exits with its status.
        assert!(ended_well(Ended::Shell(Exit::Code(1)), late));
        assert!(!ended_well(Ended::Shell(Exit::Signal(9)), late));
        assert!(!ended_well(Ended::Broken, late));
    }

    fn key(key: Key, ctrl: bool, shift: bool) -> KeyEvent {
        KeyEvent {
            key,
            mods: Mods {
                shift,
                cmd: ctrl,
                alt: false,
                ctrl,
                logo: false,
            },
        }
    }

    #[test]
    fn a_focused_terminal_takes_every_key_but_lends_ctrl_shift_to_the_app() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.mode = super::super::Mode::Inbox;
        state.open_terminal();
        let mut rt = Runtime::new(
            state,
            super::super::view,
            (1440.0, 900.0),
            Env::new(true, 1.0),
        );
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        for ev in [
            key(Key::Escape, false, false),
            key(Key::Tab, false, false),
            key(Key::Command('c'), true, false),
            key(Key::Text("x".into()), false, false),
            // Readline's back-a-character, not the sidebar.
            key(Key::Command('b'), true, false),
        ] {
            rt.key(ev);
            rt.frame(1.0 / 60.0);
        }
        // An input method's commit goes to the shell as typed.
        rt.ime(Ime::Commit("中文".into()));
        rt.frame(1.0 / 60.0);
        if crate::platform::is_macos() {
            let mut cmd_k = key(Key::Command('k'), false, false);
            cmd_k.mods.logo = true;
            cmd_k.mods.cmd = true;
            rt.key(cmd_k);
        } else {
            rt.key(key(Key::Command('k'), true, true));
        }
        rt.frame(1.0 / 60.0);
        let s = &rt.state;
        assert_eq!(
            *s.term.sent_log.borrow(),
            [
                "key Escape ctrl=false",
                "key Tab ctrl=false",
                "key C ctrl=true",
                "key X ctrl=false",
                "key B ctrl=true",
                "text 中文",
            ]
        );
        // Ctrl+Shift+K on Linux (Cmd+K on macOS) reached the app: the palette.
        assert!(s.palette.is_some());
        // The window's own Ctrl+Shift+H (Home) arrives as it is.
        for _ in 0..3 {
            rt.frame(1.0 / 60.0);
        }
        rt.key(key(Key::Command('h'), true, true));
        rt.frame(1.0 / 60.0);
        let s = &rt.state;
        assert_eq!(s.mode, super::super::Mode::Home);
        assert!(s.palette.is_none(), "a command leaves the palette");
    }

    #[test]
    fn open_file_in_terminal_sets_instant_and_queues_command() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let test_path = std::path::Path::new("/tmp/test_file.rs");
        state.open_file_in_terminal(test_path, "nvim");

        assert!(state.term.open);
        assert!(state.term.instant);
        assert!(state.term.sink.focused);
        assert!(state.viewer.is_none());
        assert_eq!(
            state.term.pending_command.as_deref(),
            Some("nvim '/tmp/test_file.rs'\n")
        );
    }

    #[test]
    fn terminal_tabs_lifecycle_new_switch_close() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.open_terminal();
        assert!(state.term.open);

        // Initially 0 or 1 tab
        state.term_new_tab_in(PathBuf::from("/tmp"), None);
        assert_eq!(state.term.tabs.len(), 1);
        assert_eq!(state.term.active_tab, 0);

        // Open second tab
        state.term_new_tab_in(PathBuf::from("/home"), Some("echo tab2\n".into()));
        assert_eq!(state.term.tabs.len(), 2);
        assert_eq!(state.term.active_tab, 1);
        assert_eq!(state.term.tabs[1].cwd, PathBuf::from("/home"));

        // Switch back to tab 0
        state.term_switch_tab(0);
        assert_eq!(state.term.active_tab, 0);
        assert_eq!(state.term.tabs[0].cwd, PathBuf::from("/tmp"));

        // Close tab 1
        state.term_close_tab(1);
        assert_eq!(state.term.tabs.len(), 1);
        assert_eq!(state.term.active_tab, 0);

        // Close last tab closes the terminal panel
        state.term_close_tab(0);
        assert!(!state.term.open);
    }
}
