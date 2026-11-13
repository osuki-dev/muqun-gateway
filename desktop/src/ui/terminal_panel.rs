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
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, MenuItem, TextInput,
};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

impl TermTab {
    pub fn send_bytes(&self, bytes: &[u8]) {
        if let Some(session) = &self.session {
            if let Ok(text) = std::str::from_utf8(bytes) {
                session.send(Msg::Text(text.to_string()));
            }
        }
    }
}

/// The grid's type: the mono slot (JetBrains Mono where installed).
const FONT_SIZE: f32 = 13.0;
const LINE: f32 = 18.0;
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

/// One search match in terminal scrollback/grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermSearchMatch {
    pub row: usize,
    pub col: u16,
    pub len: u16,
}

/// State for the in-terminal scrollback search bar.
#[derive(Debug, Clone, Default)]
pub struct TermSearchState {
    pub open: bool,
    pub query: TextEdit,
    pub matches: Vec<TermSearchMatch>,
    pub active_idx: usize,
}

/// A hovered clickable link (URL or file path) in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermHoverLink {
    pub link: crate::term::link::TerminalLink,
    pub row: usize,
    pub col_start: u16,
    pub col_end: u16,
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
    /// Search state for scrollback and grid.
    pub search: TermSearchState,
    /// Hovered link under pointer when Ctrl/Cmd is held.
    pub hover_link: Option<TermHoverLink>,
    /// Screenshot scenes: what a program would print at a size, replayed
    /// into a fresh emulator instead of a live shell.
    pub fixture: Option<fn(u16, u16) -> Vec<u8>>,
    /// Screenshot scenes: how the fixture's shell ended.
    pub fixture_ended: Option<Ended>,
    pub error: Option<String>,
    /// A command queued to run as soon as the shell session starts.
    pub pending_command: Option<String>,
    /// The grid's size the shell and emulator have, in cells.
    size: Option<Size>,
    /// Per-tab grid sizes when multiple terminal tiles sit side by side.
    pub tab_sizes: std::collections::HashMap<u64, Size>,
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
            search: TermSearchState::default(),
            hover_link: None,
            fixture: None,
            fixture_ended: None,
            error: None,
            pending_command: None,
            size: None,
            tab_sizes: std::collections::HashMap::new(),
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

    pub fn active_mut(&mut self) -> Option<&mut TermTab> {
        self.tabs.get_mut(self.active_tab)
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

/// The grid's size in cells inside a split tile laid out at `rect`.
fn tile_grid_size(rect: [f32; 4], m: Metrics, scale: f32) -> Size {
    Size {
        cols: (((rect[2] - 2.0 * INSET) / m.cw).floor() as u16).max(2),
        rows: ((rect[3] / m.ch).floor() as u16).max(1),
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

/// Relative luminance for linear light [`Color`] (WCAG 2.1).
pub fn relative_luminance(c: Color) -> f32 {
    0.2126 * c.r.clamp(0.0, 1.0) + 0.7152 * c.g.clamp(0.0, 1.0) + 0.0722 * c.b.clamp(0.0, 1.0)
}

/// Contrast ratio between two colors in linear light (1.0 to 21.0).
pub fn contrast_ratio(c1: Color, c2: Color) -> f32 {
    let (l1, l2) = (relative_luminance(c1), relative_luminance(c2));
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Ensure color `c` has at least `min_ratio` contrast against `bg`.
/// Darkens towards black on light backgrounds, or lightens towards white on dark backgrounds,
/// preserving the color's original hue.
pub fn ensure_contrast(c: Color, bg: Color, min_ratio: f32) -> Color {
    if contrast_ratio(c, bg) >= min_ratio {
        return c;
    }
    let target = if relative_luminance(bg) >= 0.45 {
        Color::BLACK
    } else {
        Color::WHITE
    };
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        let test = c.mix(target, mid);
        if contrast_ratio(test, bg) >= min_ratio {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    c.mix(target, hi)
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
            let mut adjusted_ansi = ansi;
            for slot in &mut adjusted_ansi {
                *slot = to_rgb(ensure_contrast(color(*slot), bg, 4.65));
            }
            return Colors {
                fg: to_rgb(ensure_contrast(fg, bg, 4.65)),
                bg: to_rgb(bg),
                cursor: to_rgb(cursor),
                selection: to_rgb(bg.mix(sel.alpha(1.0), sel.a.max(0.25))),
                ansi: adjusted_ansi,
            };
        }
    }
    let c = &k.c;
    let (bg, fg) = (c.bg_base, c.text_base);
    let is_light = relative_luminance(bg) >= 0.45;
    let (red, green, yellow, blue) = (c.danger.fg, c.success.fg, c.warning.fg, c.info.fg);
    let magenta = red.mix(blue, 0.5);
    let cyan = green.mix(blue, 0.5);

    let (black, white, grey, bright_white) = if !is_light {
        let tone = |col: Color, t: f32| to_rgb(bg.mix(col, t));
        (tone(fg, 0.25), tone(fg, 0.75), tone(fg, 0.45), to_rgb(fg))
    } else {
        (
            to_rgb(fg),
            to_rgb(fg.mix(bg, 0.35)),
            to_rgb(fg.mix(bg, 0.50)),
            to_rgb(fg),
        )
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
        let lift = if !is_light {
            Color::WHITE
        } else {
            Color::BLACK
        };
        ansi[i + 8] = to_rgb(color(base[i]).mix(lift, 0.2));
    }
    ansi[8] = grey;
    ansi[15] = bright_white;

    for slot in &mut ansi {
        *slot = to_rgb(ensure_contrast(color(*slot), bg, 4.65));
    }

    Colors {
        fg: to_rgb(ensure_contrast(fg, bg, 4.65)),
        bg: to_rgb(bg),
        cursor: to_rgb(c.text_accent),
        selection: to_rgb(bg.mix(c.bg_accent, 0.3)),
        ansi,
    }
}

impl UiState {
    /// Give the terminal sink exclusive keyboard focus and sync split-tile focus.
    pub fn focus_terminal(&mut self) {
        if self.in_split() {
            if let Some(i) = self.tile_of_terminal() {
                if self.focus != i {
                    self.focus_tile(i);
                }
            }
        }
        self.term.sink.focused = true;
        self.draft.focused = false;
        self.start_draft.focused = false;
        self.timeline_search.query.focused = false;
        self.term.search.query.focused = false;
        self.search.focused = false;
    }

    pub fn focus_terminal_tx(&mut self, tx: &mut Tx) {
        self.focus_terminal();
        tx.focus("terminal-keys", false);
    }

    /// Ctrl+`: open and focus; focused, close and hand the keyboard back;
    /// open but elsewhere, take the keyboard.
    pub fn toggle_terminal(&mut self) {
        if let Some(i) = self.tile_of_terminal() {
            if self.focus == i && self.term.sink.focused {
                let next = if i > 0 {
                    i - 1
                } else if self.tiles.len() > 1 {
                    1
                } else {
                    0
                };
                self.focus_tile(next);
            } else {
                self.focus_tile(i);
                self.focus_terminal();
            }
            return;
        }
        if !self.term.open {
            self.open_terminal();
        } else if self.term.sink.focused {
            self.close_terminal();
        } else {
            self.focus_terminal();
        }
    }

    pub fn open_terminal(&mut self) {
        self.term.open = true;
        self.term.pane.set_open(true);
        self.focus_terminal();
        self.right_open = false;
        self.right.set_open(false);
    }

    /// Hide the panel; the shell keeps running.
    pub fn close_terminal(&mut self) {
        self.term.open = false;
        self.term.pane.set_open(false);
        self.term.sink.focused = false;
        self.term.search.open = false;
        self.term.hover_link = None;
        if self.term.tabs.is_empty() {
            self.term.session = None;
            self.term.frame = None;
        }
        if self.mode == super::Mode::Session {
            self.draft.focused = true;
        }
    }

    /// Open a file in the embedded terminal using a terminal editor (e.g. nvim).
    pub fn open_file_in_terminal(&mut self, path: &std::path::Path, editor: &str) {
        self.close_viewer();

        let path_str = path.to_string_lossy();
        let escaped_shell = format!("'{}'", path_str.replace('\'', "'\\''"));
        let cmd = format!("{editor} {escaped_shell}\n");
        let cwd = path
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.terminal_cwd());

        self.open_terminal();

        if self
            .term
            .session
            .as_ref()
            .is_some_and(|s| s.ended().is_some())
        {
            self.term.pending_command = Some(cmd);
            self.restart_terminal();
            return;
        }

        if self.term.session.is_none() {
            self.term.pending_command = Some(cmd);
            self.start_terminal_in(cwd);
            return;
        }

        self.term_new_tab_in(cwd, Some(cmd));
    }

    /// Open a new terminal tab in `cwd`, optionally executing `command`.
    pub fn term_new_tab(&mut self) {
        let cwd = self.terminal_cwd();
        self.term_new_tab_in(cwd, None);
    }

    /// Record the lone shell as tab 0 before a second tab joins it. `force`
    /// seeds even before that shell has started (a second terminal tile).
    pub(crate) fn ensure_term_tabs_seeded(&mut self, force: bool) {
        if self.term.tabs.is_empty()
            && (force || self.term.session.is_some() || self.term.frame.is_some())
        {
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
            self.term.active_tab = 0;
        }
    }

    pub fn term_new_tab_in(&mut self, cwd: PathBuf, command: Option<String>) {
        if !self.has_terminal_tile() {
            self.open_terminal();
        }
        if self.term.tabs.is_empty() {
            self.ensure_term_tabs_seeded(false);
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
        if self.in_split() && self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) {
            self.tiles[self.focus].term_tab = Some(self.term.active_tab);
        }
        self.term.session = None;
        self.term.frame = None;
        self.term.pending_command = command;
        self.start_terminal_in(cwd);
        self.focus_terminal();
    }

    pub fn term_switch_tab(&mut self, index: usize) {
        if index >= self.term.tabs.len() {
            self.focus_terminal();
            return;
        }
        if self.in_split() && self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) {
            // A tab already shown by another tile trades places with ours, so
            // two tiles never drive the same shell.
            let old = self.term.active_tab;
            let focus = self.focus;
            for (j, t) in self.tiles.iter_mut().enumerate() {
                if j != focus && t.is_terminal() && t.term_tab == Some(index) {
                    t.term_tab = Some(old);
                }
            }
            self.tiles[focus].term_tab = Some(index);
        }
        if index == self.term.active_tab {
            self.focus_terminal();
            return;
        }
        if let Some(cur) = self.term.tabs.get_mut(self.term.active_tab) {
            cur.session = self.term.session.clone();
            cur.frame = self.term.frame.clone();
        }
        self.term.active_tab = index;
        self.term.session = self.term.tabs[index].session.clone();
        self.term.frame = self.term.tabs[index].frame.clone();
        if let Some(want) = self.term.want.get() {
            self.term.send(Msg::Colors(want));
        }
        if let Some(size) = self.term.size {
            self.term.send(Msg::Resize(size));
        }
        self.focus_terminal();
    }

    pub fn term_switch_tab_for_tile(&mut self, tile_idx: Option<usize>, index: usize) {
        if let Some(ti) = tile_idx {
            if ti < self.tiles.len() && self.focus != ti {
                self.focus_tile(ti);
            }
        }
        self.term_switch_tab(index);
    }

    pub fn term_next_tab(&mut self) {
        if self.term.tabs.len() <= 1 {
            return;
        }
        let next = (self.term.active_tab + 1) % self.term.tabs.len();
        self.term_switch_tab(next);
    }

    pub fn term_prev_tab(&mut self) {
        if self.term.tabs.len() <= 1 {
            return;
        }
        let prev = if self.term.active_tab == 0 {
            self.term.tabs.len() - 1
        } else {
            self.term.active_tab - 1
        };
        self.term_switch_tab(prev);
    }

    pub fn term_close_tab(&mut self, index: usize) {
        if index >= self.term.tabs.len() {
            if self.term.tabs.is_empty() {
                if let Some(session) = self.term.session.take() {
                    session.close_remote();
                    session.send(Msg::Shutdown);
                }
                self.term.frame = None;
                self.close_terminal();
            }
            return;
        }
        if self.term.tabs.len() <= 1 {
            if let Some(session) = self.term.session.take() {
                session.close_remote();
                session.send(Msg::Shutdown);
            }
            for tab in self.term.tabs.drain(..) {
                if let Some(s) = tab.session {
                    s.close_remote();
                    s.send(Msg::Shutdown);
                }
            }
            self.term.frame = None;
            self.close_terminal();
            return;
        }
        if index == self.term.active_tab {
            if let Some(session) = &self.term.session {
                session.close_remote();
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
                s.close_remote();
                s.send(Msg::Shutdown);
            }
            if index < self.term.active_tab {
                self.term.active_tab -= 1;
            }
        }
        self.rebind_tiles_after_close(index);
        self.focus_terminal();
    }

    /// Tab `index` is gone: shift the tabs tiles show, and give a tile that
    /// showed it another unshown tab, or close that tile when none is left.
    fn rebind_tiles_after_close(&mut self, index: usize) {
        let mut orphans = Vec::new();
        for (i, t) in self.tiles.iter_mut().enumerate() {
            if !t.is_terminal() {
                continue;
            }
            match t.term_tab {
                Some(tab) if tab == index => {
                    t.term_tab = None;
                    orphans.push(i);
                }
                Some(tab) if tab > index => t.term_tab = Some(tab - 1),
                _ => {}
            }
        }
        if orphans.is_empty() {
            return;
        }
        // The focused tile first: it takes the newly active tab when free.
        orphans.sort_by_key(|i| *i != self.focus);
        let mut doomed = Vec::new();
        for i in orphans {
            let used: Vec<usize> = self
                .tiles
                .iter()
                .enumerate()
                .filter(|(j, t)| *j != i && t.is_terminal())
                .filter_map(|(_, t)| t.term_tab)
                .collect();
            let free = if used.contains(&self.term.active_tab) {
                (0..self.term.tabs.len()).find(|tab| !used.contains(tab))
            } else {
                Some(self.term.active_tab)
            };
            match free {
                Some(tab) => self.tiles[i].term_tab = Some(tab),
                None => doomed.push(i),
            }
        }
        doomed.sort_unstable();
        for i in doomed.into_iter().rev() {
            if self.tiles.iter().filter(|t| t.is_terminal()).count() > 1 {
                self.close_tile(i);
            }
        }
        if self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) {
            self.sync_tile_terminal_tab(self.focus);
        }
        self.persist_tiles();
    }

    /// Switch the active terminal to the focused session's workspace directory,
    /// or open a new terminal tab in that directory.
    pub fn term_sync_cwd(&mut self) {
        let cwd = self.terminal_cwd();
        let escaped = cwd.display().to_string().replace('"', "\\\"");
        let cd_cmd = format!("cd \"{escaped}\"\n");
        let active_running = self
            .term
            .session
            .as_ref()
            .is_some_and(|s| s.ended().is_none());

        if active_running && !self.term.tabs.is_empty() {
            if let Some(tab) = self.term.active_mut() {
                tab.cwd = cwd.clone();
                tab.send_bytes(cd_cmd.as_bytes());
            } else {
                self.term.send(Msg::Text(cd_cmd));
            }
        } else {
            self.term_new_tab_in(cwd, None);
        }
        self.open_terminal();

        self.toast(
            super::copy::t(
                self.cfg.general.language,
                "Terminal switched to session folder",
            ),
            None,
        );
    }

    /// End the shell and start a new one in the focused session's folder.
    pub fn restart_terminal(&mut self) {
        if let Some(session) = self.term.session.take() {
            session.close_remote();
            session.send(Msg::Shutdown);
        }
        if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
            if let Some(s) = active.session.take() {
                s.close_remote();
                s.send(Msg::Shutdown);
            }
            active.frame = None;
        }
        self.term.frame = None;
        self.term.error = None;
        let cwd = self
            .term
            .tabs
            .get(self.term.active_tab)
            .map(|t| t.cwd.clone())
            .unwrap_or_else(|| self.terminal_cwd());
        self.start_terminal_in(cwd);
        self.focus_terminal();
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

    pub fn term_active_title(&self) -> String {
        let frame = self.term.frame.as_ref();
        if let Some(t) = frame.map(|f| f.title.clone()).filter(|t| !t.is_empty()) {
            t
        } else {
            let cwd = self
                .term
                .session
                .as_ref()
                .map(|x| x.cwd.clone())
                .unwrap_or_else(|| self.terminal_cwd());
            short_path(&cwd)
        }
    }

    /// The title a terminal tile's header shows: its own shell's.
    pub fn term_tile_title(&self, tile_idx: usize) -> String {
        if self.tile_of_terminal() == Some(tile_idx) {
            return self.term_active_title();
        }
        match self
            .tile_terminal_tab(tile_idx)
            .and_then(|tab| self.term.tabs.get(tab))
        {
            Some(tab) => tab
                .frame
                .as_ref()
                .map(|f| f.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| short_path(&tab.cwd)),
            None => short_path(&self.terminal_cwd()),
        }
    }

    pub(crate) fn start_terminal(&mut self) {
        #[cfg(unix)]
        if self.term.fixture.is_none()
            && self.term.session.is_none()
            && self.term.tabs.is_empty()
            && self.term.waker.is_some()
        {
            if let Ok(sessions) = crate::term::daemon::SyncDaemonClient::new(None).list_sessions() {
                let alive: Vec<_> = sessions.into_iter().filter(|s| s.alive).collect();
                if !alive.is_empty() {
                    let waker = self.term.waker.clone().unwrap();
                    let size = self.term.size.unwrap_or(Size {
                        cols: 80,
                        rows: 24,
                        cell_w: 8,
                        cell_h: 17,
                    });
                    let colors = self.term.want.get().unwrap_or_else(|| {
                        colors(
                            &Kit::neutral(self.dark),
                            if self.dark {
                                ColorScheme::Dark
                            } else {
                                ColorScheme::Light
                            },
                        )
                    });
                    for s_info in alive {
                        if let Ok(session) = Session::start_with_id(
                            Some(s_info.id),
                            s_info.cwd.clone(),
                            size,
                            colors,
                            waker.clone(),
                        ) {
                            let id = self.term.next_tab_id;
                            self.term.next_tab_id += 1;
                            self.term.tabs.push(TermTab {
                                id,
                                session: Some(session),
                                frame: None,
                                cwd: s_info.cwd,
                                title: None,
                                pending_command: None,
                            });
                        }
                    }
                    if !self.term.tabs.is_empty() {
                        self.term.active_tab = 0;
                        self.term.session = self.term.tabs[0].session.clone();
                        self.term.sent = Some(colors);
                        self.term.error = None;
                        return;
                    }
                }
            }
        }
        let cwd = self.terminal_cwd();
        self.start_terminal_in(cwd);
    }

    fn start_terminal_in(&mut self, cwd: PathBuf) {
        if self.term.fixture.is_some() || self.term.session.is_some() {
            return;
        }
        let Some(waker) = self.term.waker.clone() else {
            return;
        };
        let size = self.term.size.unwrap_or(Size {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        });
        let colors = self.term.want.get().unwrap_or_else(|| {
            colors(
                &Kit::neutral(self.dark),
                if self.dark {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                },
            )
        });
        match Session::start(cwd.clone(), size, colors, waker) {
            Ok(session) => {
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
                        pending_command: self.term.pending_command.clone(),
                    });
                    self.term.active_tab = 0;
                } else if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
                    active.session = Some(session);
                    active.cwd = cwd;
                    active.pending_command = self.term.pending_command.clone();
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
        if (!self.term.open && !self.has_terminal_tile()) || rect[2] < 40.0 || rect[3] < 20.0 {
            return;
        }
        // Each tab keeps the size it was last given: after a switch, the grid
        // compares against the new tab's, not the old one's.
        if let Some(id) = self.term.tabs.get(self.term.active_tab).map(|t| t.id) {
            let had = self.term.tab_sizes.get(&id).copied();
            if had.is_some() && had != self.term.size {
                self.term.size = had;
            }
        }
        let m = metrics();
        let size = if self.has_terminal_tile() {
            tile_grid_size(rect, m, scale)
        } else {
            grid_size(self.term.pane.width(), rect[3], m, scale)
        };
        if self.term.size == Some(size)
            && (self.term.session.is_some() || self.term.frame.is_some())
        {
            self.term.resize.pending = None;
            return;
        }
        // A size held back waits for `poll_terminal`, which wakes for it.
        let dragging = self.term.pane.is_resizing() || self.tile_drag.is_some();
        if let Some(size) = self.term.resize.want(size, self.term.now, dragging) {
            self.resize_terminal(size);
        }
    }

    /// A terminal tile's grid was laid out. The live tile drives the active
    /// shell; any other tile sizes the background tab it shows, starting a
    /// shell for it first when it has none yet (a restored layout).
    fn terminal_tile_layout(&mut self, tile_idx: Option<usize>, rect: [f32; 4], scale: f32) {
        let Some(ti) = tile_idx.filter(|ti| self.tile_of_terminal() != Some(*ti)) else {
            self.terminal_layout(rect, scale);
            return;
        };
        if rect[2] < 40.0 || rect[3] < 20.0 {
            return;
        }
        let size = tile_grid_size(rect, metrics(), scale);
        // A scene's fixture paints every tile's shell (no real processes).
        if let Some(fixture) = self.term.fixture {
            let (Some(tab), Some(colors)) = (self.tile_terminal_tab(ti), self.term.want.get())
            else {
                return;
            };
            if let Some(t) = self.term.tabs.get_mut(tab) {
                if t.frame
                    .as_ref()
                    .is_none_or(|f| f.cols != size.cols || f.rows != size.rows)
                {
                    let bytes = fixture(size.cols, size.rows);
                    t.frame = crate::term::engine::render_fixture(&bytes, size, colors);
                }
            }
            return;
        }
        let tab = match self.tile_terminal_tab(ti) {
            Some(tab) => tab,
            None => match self.spawn_background_tab(size) {
                Some(tab) => {
                    self.tiles[ti].term_tab = Some(tab);
                    return;
                }
                None => return,
            },
        };
        if tab == self.term.active_tab {
            return;
        }
        let Some(t) = self.term.tabs.get(tab) else {
            return;
        };
        if self.term.tab_sizes.get(&t.id) == Some(&size) {
            return;
        }
        if let Some(session) = &t.session {
            session.send(Msg::Resize(size));
        }
        self.term.tab_sizes.insert(t.id, size);
    }

    /// Start a shell in a new tab without making it the active one.
    fn spawn_background_tab(&mut self, size: Size) -> Option<usize> {
        let waker = self.term.waker.clone()?;
        let colors = self.term.want.get()?;
        let cwd = self.terminal_cwd();
        let session = Session::start(cwd.clone(), size, colors, waker).ok()?;
        self.ensure_term_tabs_seeded(true);
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
        self.term.tab_sizes.insert(id, size);
        Some(self.term.tabs.len() - 1)
    }

    /// Give the shell and emulator (or a scene's fixture) a new grid size.
    fn resize_terminal(&mut self, size: Size) {
        self.term.size = Some(size);
        if let Some(id) = self.term.tabs.get(self.term.active_tab).map(|t| t.id) {
            self.term.tab_sizes.insert(id, size);
        }
        tracing::debug!(target: "muqun_desktop::term_resize", cols = size.cols, rows = size.rows, "grid resize");
        if let Some(fixture) = self.term.fixture {
            if let Some(colors) = self.term.want.get() {
                let bytes = fixture(size.cols, size.rows);
                let f = crate::term::engine::render_fixture(&bytes, size, colors);
                self.term.frame = f.clone();
                if let Some(t) = self.term.tabs.get_mut(self.term.active_tab) {
                    t.frame = f;
                }
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
        let dragging = self.term.pane.is_resizing() || self.tile_drag.is_some();
        if let Some(size) = self.term.resize.due(now, dragging) {
            if self.term.size != Some(size) {
                self.resize_terminal(size);
                changed = true;
            }
        } else if let Some(left) = self.term.resize.wait(now) {
            tx.wake_after(std::time::Duration::from_secs_f64(left));
        }
        if let Some(want) = self.term.want.get() {
            if self.term.sent.is_none_or(|sent| sent != want) {
                self.term.sent = Some(want);
                if let (Some(fixture), Some(size)) = (self.term.fixture, self.term.size) {
                    let bytes = fixture(size.cols, size.rows);
                    let f = crate::term::engine::render_fixture(&bytes, size, want);
                    self.term.frame = f.clone();
                    if let Some(t) = self.term.tabs.get_mut(self.term.active_tab) {
                        t.frame = f;
                    }
                    for (tab_idx, tab) in self.term.tabs.iter_mut().enumerate() {
                        if tab_idx != self.term.active_tab {
                            let f_tab = crate::term::engine::render_fixture(&bytes, size, want);
                            tab.frame = f_tab;
                        }
                    }
                    changed = true;
                } else {
                    for tab in &self.term.tabs {
                        if let Some(sess) = &tab.session {
                            sess.send(Msg::Colors(want));
                        }
                    }
                    if let Some(session) = &self.term.session {
                        session.send(Msg::Colors(want));
                    }
                }
            }
        }
        if let Some(frame) = self.term.session.as_ref().and_then(Session::take_frame) {
            if let Some(cmd) = self.term.pending_command.take() {
                if let Some(sess) = &self.term.session {
                    sess.send(Msg::Text(cmd));
                }
            }
            self.term.frame = Some(frame.clone());
            if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
                active.pending_command = None;
                active.frame = Some(frame);
            }
            changed = true;
        }
        let shown: Vec<usize> = (0..self.tiles.len())
            .filter_map(|i| self.tile_terminal_tab(i))
            .collect();
        for (i, tab) in self.term.tabs.iter_mut().enumerate() {
            if i != self.term.active_tab {
                if let Some(frame) = tab.session.as_ref().and_then(Session::take_frame) {
                    if let Some(cmd) = tab.pending_command.take() {
                        if let Some(sess) = &tab.session {
                            sess.send(Msg::Text(cmd));
                        }
                    }
                    tab.frame = Some(frame);
                    // Another tile shows this tab: redraw it.
                    changed |= shown.contains(&i);
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
                if self.term.tabs.len() > 1 {
                    self.term_close_tab(self.term.active_tab);
                    true
                } else {
                    if let Some(s) = self.term.session.take() {
                        s.close_remote();
                        s.send(Msg::Shutdown);
                    }
                    for tab in self.term.tabs.drain(..) {
                        if let Some(s) = tab.session {
                            s.close_remote();
                            s.send(Msg::Shutdown);
                        }
                    }
                    self.term.frame = None;
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
    pub(crate) fn terminal_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        // Split view pane navigation from terminal:
        if self.in_split() && super::tiles::arrow_key(self, ev) {
            return true;
        }

        let mac = crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;
        let letter =
            |c: char| matches!(&ev.key, rune::Key::Command(k) if k.eq_ignore_ascii_case(&c));

        if self.term.search.open && ev.key == rune::Key::Escape {
            self.close_terminal_search(tx);
            return true;
        }

        // Tab, search & split shortcuts: Ctrl+Shift+T (new tab), Ctrl+Shift+W (close tab),
        // Ctrl+Shift+F (find in terminal), Ctrl+Shift+D / Ctrl+Shift+\ (dock/split terminal pane)
        if (ev.mods.ctrl && ev.mods.shift && !ev.mods.alt) || (mac && ev.mods.logo && ev.mods.shift)
        {
            if letter('f') {
                self.open_terminal_search();
                return true;
            }
            if letter('t') {
                self.term_new_tab();
                return true;
            }
            if letter('w') {
                self.term_close_tab(self.term.active_tab);
                return true;
            }
            if letter('d')
                || matches!(&ev.key, rune::Key::Command('\\'))
                || matches!(&ev.key, rune::Key::Text(t) if t == "\\" || t == "|")
                || tx.physical_key() == Some(rune::PhysicalKeyCode::Backslash)
            {
                if self.in_split() {
                    self.split_terminal_tile();
                } else {
                    self.open_terminal_tile();
                }
                return true;
            }
            if matches!(&ev.key, rune::Key::Command(']'))
                || matches!(&ev.key, rune::Key::Text(t) if t == "]" || t == "}")
                || tx.physical_key() == Some(rune::PhysicalKeyCode::BracketRight)
            {
                self.term_next_tab();
                return true;
            }
            if matches!(&ev.key, rune::Key::Command('['))
                || matches!(&ev.key, rune::Key::Text(t) if t == "[" || t == "{")
                || tx.physical_key() == Some(rune::PhysicalKeyCode::BracketLeft)
            {
                self.term_prev_tab();
                return true;
            }
        }
        // Herdr / global split shortcut inside terminal: Ctrl+\ (or Cmd+D / Cmd+\ on macOS)
        if (ev.mods.ctrl
            && !ev.mods.shift
            && !ev.mods.alt
            && (matches!(&ev.key, rune::Key::Command('\\'))
                || tx.physical_key() == Some(rune::PhysicalKeyCode::Backslash)))
            || (mac
                && ev.mods.logo
                && !ev.mods.shift
                && !ev.mods.alt
                && (letter('d') || matches!(&ev.key, rune::Key::Command('\\'))))
        {
            if self.in_split() {
                self.split();
            } else {
                self.open_terminal_tile();
            }
            return true;
        }
        // Terminal search shortcut on macOS: Cmd+F
        if mac && ev.mods.logo && !ev.mods.shift && !ev.mods.alt && letter('f') {
            self.open_terminal_search();
            return true;
        }
        // Tab switching: Ctrl+Tab (next), Ctrl+Shift+Tab (prev)
        if ev.key == rune::Key::Tab && ev.mods.ctrl && !ev.mods.alt {
            if ev.mods.shift {
                self.term_prev_tab();
            } else {
                self.term_next_tab();
            }
            return true;
        }
        // PageDown / PageUp with Ctrl: Next / Prev tab
        if ev.mods.ctrl && !ev.mods.alt && !ev.mods.shift {
            if ev.key == rune::Key::PageDown {
                self.term_next_tab();
                return true;
            }
            if ev.key == rune::Key::PageUp {
                self.term_prev_tab();
                return true;
            }
        }
        // Direct jump: Alt+1..Alt+9 (or Cmd+1..Cmd+9 on macOS)
        let is_jump_mod = (ev.mods.alt && !ev.mods.ctrl && !ev.mods.shift && !ev.mods.cmd)
            || (mac && ev.mods.logo && !ev.mods.ctrl && !ev.mods.shift);
        if is_jump_mod {
            let digit = match &ev.key {
                rune::Key::Command(c) if c.is_ascii_digit() => c.to_digit(10),
                rune::Key::Text(t)
                    if t.len() == 1 && t.chars().next().unwrap().is_ascii_digit() =>
                {
                    t.chars().next().unwrap().to_digit(10)
                }
                _ => None,
            };
            if let Some(d) = digit {
                if (1..=8).contains(&d) {
                    let target = (d - 1) as usize;
                    if target < self.term.tabs.len() {
                        self.term_switch_tab(target);
                        return true;
                    }
                } else if d == 9 && !self.term.tabs.is_empty() {
                    self.term_switch_tab(self.term.tabs.len() - 1);
                    return true;
                }
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
                let letter = |c: char| matches!(&ev.key, rune::Key::Command(k) if k.eq_ignore_ascii_case(&c));
                if self
                    .term
                    .session
                    .as_ref()
                    .is_some_and(|x| x.ended().is_some())
                {
                    if ev.key == rune::Key::Enter {
                        self.restart_terminal();
                    } else if (ev.mods.ctrl && (letter('d') || letter('w')))
                        || ev.key == rune::Key::Escape
                    {
                        self.term_close_tab(self.term.active_tab);
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

    pub fn open_terminal_search(&mut self) {
        self.term.search.open = true;
        self.term.search.query.focused = true;
        if !self.term.search.query.text.trim().is_empty() {
            self.update_term_search_matches();
            self.term_search_jump_to_current();
        }
    }

    pub fn close_terminal_search(&mut self, tx: &mut Tx) {
        self.close_terminal_search_no_tx();
        self.focus_terminal_tx(tx);
    }

    pub fn close_terminal_search_no_tx(&mut self) {
        self.term.search.open = false;
        self.term.search.matches.clear();
        self.term.search.active_idx = 0;
        self.term.send(Msg::ScrollToBottom);
    }

    pub fn toggle_terminal_search(&mut self) {
        if self.term.search.open {
            self.close_terminal_search_no_tx();
        } else {
            self.open_terminal_search();
        }
    }

    pub fn update_term_search_matches(&mut self) {
        let q = self.term.search.query.text.trim().to_lowercase();
        self.term.search.matches.clear();
        self.term.search.active_idx = 0;
        if q.is_empty() {
            return;
        }

        let text = self
            .term
            .active_mut()
            .and_then(|t| t.session.as_ref())
            .and_then(|s| s.dump_plain_text())
            .or_else(|| self.term.session.as_ref().and_then(|s| s.dump_plain_text()))
            .unwrap_or_else(|| {
                self.term
                    .frame
                    .as_ref()
                    .map(|f| {
                        f.grid
                            .iter()
                            .map(|r| r.plain_text())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            });

        for (row_idx, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            let mut start = 0;
            while let Some(pos) = lower[start..].find(&q) {
                let byte_pos = start + pos;
                let col = line[..byte_pos].chars().count() as u16;
                let len = q.chars().count() as u16;
                self.term.search.matches.push(TermSearchMatch {
                    row: row_idx,
                    col,
                    len,
                });
                start = byte_pos + q.len().max(1);
            }
        }
    }

    pub fn term_search_jump_to_current(&mut self) {
        if self.term.search.matches.is_empty() {
            return;
        }
        let idx = self.term.search.active_idx % self.term.search.matches.len();
        let target_row = self.term.search.matches[idx].row;
        let msg = Msg::ScrollToRow(target_row);
        if let Some(session) = self.term.active_mut().and_then(|t| t.session.as_ref()) {
            session.send(msg);
        } else {
            self.term.send(msg);
        }
    }

    pub fn term_search_next(&mut self) {
        if self.term.search.matches.is_empty() {
            return;
        }
        self.term.search.active_idx =
            (self.term.search.active_idx + 1) % self.term.search.matches.len();
        self.term_search_jump_to_current();
    }

    pub fn term_search_prev(&mut self) {
        if self.term.search.matches.is_empty() {
            return;
        }
        if self.term.search.active_idx == 0 {
            self.term.search.active_idx = self.term.search.matches.len() - 1;
        } else {
            self.term.search.active_idx -= 1;
        }
        self.term_search_jump_to_current();
    }

    pub fn open_terminal_link(&mut self, link: crate::term::link::TerminalLink) {
        match link {
            crate::term::link::TerminalLink::Url(url) => {
                crate::platform::shell::open_url(&url);
            }
            crate::term::link::TerminalLink::FilePath {
                path,
                line: _,
                col: _,
            } => {
                let custom = self.cfg.general.editor_command.clone();
                let choice = Some(self.cfg.general.editor);
                if let Some(editor) =
                    crate::platform::editors::resolve_terminal_editor(custom.as_deref(), choice)
                {
                    self.open_file_in_terminal(&path, &editor);
                } else if let Some(cmd) = custom.as_deref().filter(|c| !c.trim().is_empty()) {
                    let _ = crate::platform::editors::try_custom_command(cmd, &path);
                } else {
                    self.open_file(path);
                }
            }
        }
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
            (
                k.c.surface_raised,
                k.c.text_base,
                k.alpha(k.c.border_weak, 0.45),
            )
        } else {
            (
                k.c.bg_base.alpha(0.0),
                k.c.text_muted,
                k.c.border_base.alpha(0.0),
            )
        };

        let close_tab = if tab_count > 1 {
            Some(
                IconButton::new(k.glyph.close)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .label(tr_ref(language, "Close tab"))
                    .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                        s.term_close_tab(i);
                        if s.term.open || s.has_terminal_tile() {
                            s.focus_terminal_tx(tx);
                        }
                    })
                    .build(k)
                    .key(format!("terminal-tab-close-{i}")),
            )
        } else {
            None
        };

        let mut tab_pill = row()
            .key(format!("terminal-tab-{i}"))
            .h(28.0)
            .pl(10.0)
            .pr(if close_tab.is_some() { 4.0 } else { 10.0 })
            .gap(6.0)
            .center_y()
            .fill(bg_color)
            .border(1.0, border_color)
            .clip_rounded(6.0)
            .cursor(rune::Cursor::Pointer)
            .preserve_focus_on_press(true)
            .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                s.term_switch_tab(i);
                s.focus_terminal_tx(tx);
            })
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
                    .font(if is_active {
                        k.ty.small_medium
                    } else {
                        k.ty.small
                    })
                    .color(fg_color)
                    .truncate()
                    .shrink()
                    .max_w(140.0),
            );

        if let Some(btn) = close_tab {
            tab_pill = tab_pill.child(btn);
        }

        tabs_strip = tabs_strip.child(tab_pill);
    }

    let add_tab_btn = IconButton::new(&PLUS)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(language, "New tab (Ctrl+Shift+T)"))
        .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
            s.term_new_tab();
            s.focus_terminal_tx(tx);
        })
        .build(k)
        .key("terminal-new-tab");
    tabs_strip = tabs_strip.child(add_tab_btn);

    let sync_cwd_btn = IconButton::new(&rune_kit::glyphs::FOLDER)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(super::copy::t_ref(language, "Sync directory with session"))
        .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
            s.term_sync_cwd();
            s.focus_terminal_tx(tx);
        })
        .build(k)
        .key("terminal-sync-cwd");
    tabs_strip = tabs_strip.child(sync_cwd_btn);

    let header = row()
        .key("terminal-head")
        .w_full()
        .h(HEAD)
        .pl(8.0)
        .pr(8.0)
        .gap(6.0)
        .center_y()
        .fill(k.alpha(k.c.layer_01, 0.65))
        .preserve_focus_on_press(true)
        .child(tabs_strip)
        .child(
            IconButton::new(&RESTART)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Restart shell"))
                .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.restart_terminal();
                    s.focus_terminal_tx(tx);
                })
                .build(k)
                .key("terminal-restart"),
        )
        .child(
            IconButton::new(&super::icons::PLUS)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Add to split view"))
                .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.open_terminal_tile();
                    s.focus_terminal_tx(tx);
                })
                .build(k)
                .key("terminal-dock-split"),
        )
        .child(
            IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Hide terminal"))
                .on_press(|s: &mut UiState| s.close_terminal())
                .build(k)
                .key("terminal-close"),
        );

    let ended_now = s.term.fixture_ended.is_some()
        || s.term.session.as_ref().is_some_and(|x| x.ended().is_some());
    let view = GridView::panel(ended_now);
    let grid = context_menu(s, k, grid(s, k, frame, m, bg, scale, view), layer);
    let mut body = col()
        .key("terminal-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(6.0)
        .px(INSET)
        .pb(INSET)
        .gap(8.0)
        .preserve_focus_on_press(true)
        .child(grid);
    if s.term.search.open {
        body = body.child(terminal_search_bar(s, k));
    }
    if let Some(err) = &s.term.error {
        body = body.child(
            text(err.clone())
                .key("terminal-error")
                .font(k.ty.body)
                .color(k.c.danger.fg)
                .wrap(),
        );
    } else if let Some(ended) = ended {
        body = body.child(ended_notice(k, language, ended, "", None));
    }
    col()
        .key("terminal-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .preserve_focus_on_press(true)
        .child(header)
        .child(el().w_full().h(1.0).fill(k.alpha(k.c.border_weak, 0.35)))
        .child(body)
}

/// The embedded terminal content inside a split tile.
pub fn tile_body(s: &UiState, k: &Kit, scheme: ColorScheme, scale: f32, layer: &mut Vec<El>) -> El {
    tile_body_for(s, k, scheme, scale, layer, None)
}

/// How one grid is shown: in which tile, whether it is the live shell (the
/// one with the keyboard sink), and the suffix that keeps its keys unique.
#[derive(Clone, Copy)]
struct GridView<'a> {
    tile_idx: Option<usize>,
    live: bool,
    sfx: &'a str,
    ended: bool,
}

impl GridView<'static> {
    /// The bottom panel's single grid (or a lone terminal tile): live.
    fn panel(ended: bool) -> Self {
        Self {
            tile_idx: None,
            live: true,
            sfx: "",
            ended,
        }
    }
}

impl UiState {
    /// Take the keyboard into terminal tile `tile_idx` (its shell becomes active).
    fn focus_terminal_tile_tx(&mut self, tile_idx: Option<usize>, tx: &mut Tx) {
        if let Some(i) = tile_idx {
            if i < self.tiles.len() && self.focus != i {
                self.focus_tile(i);
            }
        }
        self.focus_terminal_tx(tx);
    }
}

/// The terminal inside split tile `tile_idx`: each terminal tile shows its
/// own tab, so several shells sit side by side.
pub fn tile_body_for(
    s: &UiState,
    k: &Kit,
    scheme: ColorScheme,
    scale: f32,
    layer: &mut Vec<El>,
    tile_idx: Option<usize>,
) -> El {
    let language = s.cfg.general.language;
    let colors = colors(k, scheme);
    s.term.want.set(Some(colors));
    let m = metrics();
    let live = tile_idx.is_none_or(|i| s.tile_of_terminal() == Some(i));
    // The first terminal tile keeps the plain keys; others get a suffix.
    let first = tile_idx.is_none_or(|i| !s.tiles.iter().take(i).any(|t| t.is_terminal()));
    let sfx = match tile_idx {
        Some(i) if !first => format!("-{i}"),
        _ => String::new(),
    };
    let shown_tab = if live {
        Some(s.term.active_tab)
    } else {
        tile_idx.and_then(|i| s.tile_terminal_tab(i))
    };
    let bg_tab = if live {
        None
    } else {
        shown_tab.and_then(|t| s.term.tabs.get(t))
    };
    let frame = if live {
        s.term.frame.as_ref()
    } else {
        bg_tab.and_then(|t| t.frame.as_ref())
    };
    let bg = color(frame.map_or(colors.bg, |f| f.bg));
    let ended = if live {
        s.term
            .session
            .as_ref()
            .and_then(Session::ended)
            .or(s.term.fixture_ended)
    } else {
        bg_tab
            .and_then(|t| t.session.as_ref())
            .and_then(Session::ended)
    };

    let tab_count = s.term.tabs.len().max(1);
    let mut tabs_strip = row()
        .key(format!("tile-terminal-tabs-strip{sfx}"))
        .gap(4.0)
        .center_y()
        .shrink()
        .min_w(0.0)
        .grow(1.0);

    for i in 0..tab_count {
        let is_active = Some(i) == shown_tab;
        let tab_title = if i == s.term.active_tab {
            s.term
                .frame
                .as_ref()
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
            (
                k.c.surface_raised,
                k.c.text_base,
                k.alpha(k.c.border_weak, 0.45),
            )
        } else {
            (
                k.c.bg_base.alpha(0.0),
                k.c.text_muted,
                k.c.border_base.alpha(0.0),
            )
        };

        let close_tab = if tab_count > 1 {
            Some(
                IconButton::new(k.glyph.close)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .label(tr_ref(language, "Close tab"))
                    .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                        s.focus_terminal_tile_tx(tile_idx, tx);
                        s.term_close_tab(i);
                        if s.term.open || s.has_terminal_tile() {
                            s.focus_terminal_tx(tx);
                        }
                    })
                    .build(k)
                    .key(format!("tile-terminal-tab-close{sfx}-{i}")),
            )
        } else {
            None
        };

        let mut tab_pill = row()
            .key(format!("tile-terminal-tab{sfx}-{i}"))
            .h(26.0)
            .pl(8.0)
            .pr(if close_tab.is_some() { 4.0 } else { 8.0 })
            .gap(5.0)
            .center_y()
            .fill(bg_color)
            .border(1.0, border_color)
            .clip_rounded(6.0)
            .cursor(rune::Cursor::Pointer)
            .preserve_focus_on_press(true)
            .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                s.term_switch_tab_for_tile(tile_idx, i);
                s.focus_terminal_tile_tx(tile_idx, tx);
            })
            .child(
                rune::icon(&TERMINAL_WINDOW, 11.0)
                    .color(if is_active {
                        k.c.text_accent
                    } else {
                        k.c.icon_muted
                    })
                    .a11y_hidden(),
            )
            .child(
                text(tab_title)
                    .font(if is_active {
                        k.ty.small_medium
                    } else {
                        k.ty.small
                    })
                    .color(fg_color)
                    .truncate()
                    .shrink()
                    .max_w(130.0),
            );

        if let Some(btn) = close_tab {
            tab_pill = tab_pill.child(btn);
        }

        tabs_strip = tabs_strip.child(tab_pill);
    }

    let add_tab_btn = IconButton::new(&PLUS)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(language, "New tab (Ctrl+Shift+T)"))
        .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
            s.focus_terminal_tile_tx(tile_idx, tx);
            s.term_new_tab();
            s.focus_terminal_tx(tx);
        })
        .build(k)
        .key(format!("tile-terminal-new-tab{sfx}"));
    tabs_strip = tabs_strip.child(add_tab_btn);

    let sync_cwd_btn = IconButton::new(&rune_kit::glyphs::FOLDER)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(super::copy::t_ref(language, "Sync directory with session"))
        .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
            s.focus_terminal_tile_tx(tile_idx, tx);
            s.term_sync_cwd();
            s.focus_terminal_tx(tx);
        })
        .build(k)
        .key(format!("tile-terminal-sync-cwd{sfx}"));
    tabs_strip = tabs_strip.child(sync_cwd_btn);

    let subheader = row()
        .key(format!("tile-terminal-subhead{sfx}"))
        .w_full()
        .h(30.0)
        .pl(8.0)
        .pr(8.0)
        .gap(6.0)
        .center_y()
        .fill(k.alpha(k.c.layer_01, 0.45))
        .preserve_focus_on_press(true)
        .child(tabs_strip)
        .child(
            IconButton::new(&RESTART)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Restart shell"))
                .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.focus_terminal_tile_tx(tile_idx, tx);
                    s.restart_terminal();
                    s.focus_terminal_tx(tx);
                })
                .build(k)
                .key(format!("tile-terminal-restart{sfx}")),
        );

    let view = GridView {
        tile_idx,
        live,
        sfx: &sfx,
        ended: ended.is_some(),
    };
    let g = grid(s, k, frame, m, bg, scale, view);
    let grid = if live {
        context_menu(s, k, g, layer)
    } else {
        g.on_secondary_press(move |s: &mut UiState, _at, tx: &mut Tx| {
            s.focus_terminal_tile_tx(tile_idx, tx);
        })
    };
    let mut body = col()
        .key(format!("tile-terminal-body{sfx}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(6.0)
        .px(INSET)
        .pb(INSET)
        .gap(8.0)
        .preserve_focus_on_press(true)
        .child(grid);
    if live && s.term.search.open {
        body = body.child(terminal_search_bar(s, k));
    }

    match (&s.term.error, ended) {
        (Some(err), _) if live => {
            body = body.child(
                text(err.clone())
                    .key("tile-terminal-error")
                    .font(k.ty.body)
                    .color(k.c.danger.fg)
                    .wrap(),
            );
        }
        (_, Some(ended)) => {
            body = body.child(ended_notice(k, language, ended, &sfx, tile_idx));
        }
        _ => {}
    }

    col()
        .key(format!("tile-terminal-content{sfx}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .preserve_focus_on_press(true)
        .child(subheader)
        .child(el().w_full().h(1.0).fill(k.alpha(k.c.border_weak, 0.25)))
        .child(body)
}

fn terminal_search_bar(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let match_count = format!(
        "{}/{}",
        if s.term.search.matches.is_empty() {
            0
        } else {
            s.term.search.active_idx + 1
        },
        s.term.search.matches.len(),
    );
    let field = TextInput::new(&s.term.search.query)
        .placeholder(tr_ref(language, "Find in terminal…"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(Some(160.0))
        .plain()
        .on_edit(|s: &mut UiState| &mut s.term.search.query)
        .on_change(|s: &mut UiState, _: &mut rune::Tx| {
            s.update_term_search_matches();
            s.term_search_jump_to_current();
        })
        .on_key(|s: &mut UiState, ev: &rune::KeyEvent, tx: &mut rune::Tx| {
            if ev.key == rune::Key::Enter {
                if ev.mods.shift {
                    s.term_search_prev();
                } else {
                    s.term_search_next();
                }
                return true;
            }
            if ev.key == rune::Key::Escape {
                s.close_terminal_search(tx);
                return true;
            }
            false
        })
        .build(k)
        .key("term-search-input");

    let counter = text(match_count).font(k.ty.small).color(k.c.text_faint);

    let prev_btn = IconButton::new(&rune_kit::glyphs::CHEVRON_UP)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(language, "Previous match"))
        .on_press(|s: &mut UiState| s.term_search_prev())
        .build(k)
        .key("term-search-prev");

    let next_btn = IconButton::new(&rune_kit::glyphs::CHEVRON_DOWN)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(language, "Next match"))
        .on_press(|s: &mut UiState| s.term_search_next())
        .build(k)
        .key("term-search-next");

    let close_btn = IconButton::new(&rune_kit::glyphs::CLOSE)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(tr_ref(language, "Close search"))
        .on_press_tx(|s: &mut UiState, tx: &mut rune::Tx| s.close_terminal_search(tx))
        .build(k)
        .key("term-search-close");

    let mut bar = row()
        .key("terminal-search-bar")
        .gap(6.0)
        .center_y()
        .px(10.0)
        .py(4.0)
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_muted)
        .child(field)
        .child(counter)
        .child(prev_btn)
        .child(next_btn)
        .child(close_btn);

    for shadow in k.shadow_layers(&k.elevation.overlay) {
        bar = bar.box_shadow(shadow);
    }

    row()
        .key("term-search-band")
        .pin(None, Some(8.0), Some(16.0), None)
        .child(bar)
}

/// What a shell that ended badly says under its last screen: how it ended,
/// and a new one a press (or Return) away.
fn ended_notice(
    k: &Kit,
    language: crate::config::Language,
    ended: Ended,
    sfx: &str,
    tile_idx: Option<usize>,
) -> El {
    use super::copy::tr;
    let what = match ended {
        Ended::Shell(Exit::Code(n)) => tr(language, &format!("Shell exited (code {n})")),
        Ended::Shell(Exit::Signal(n)) => tr(language, &format!("Shell exited (signal {n})")),
        Ended::Broken => tr(language, "The terminal stopped"),
    };
    row()
        .key(format!("terminal-ended{sfx}"))
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(what)
                .key(format!("terminal-ended-text{sfx}"))
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .truncate()
                .shrink()
                .min_w(0.0)
                .grow(1.0),
        )
        .child(
            Button::new(tr_ref(language, "Restart"))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.focus_terminal_tile_tx(tile_idx, tx);
                    s.restart_terminal();
                })
                .build(k)
                .key(format!("terminal-ended-restart{sfx}")),
        )
}

/// Right-click: what a desktop terminal offers there.
fn context_menu(s: &UiState, k: &Kit, grid: El, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let has_selection = s.term.frame.as_ref().is_some_and(|f| f.selection.is_some());
    use super::keymap::{label, Action};
    let grid = grid.on_secondary_press(|s: &mut UiState, at, tx: &mut Tx| {
        s.term.menu_at = Some(at);
        s.focus_terminal_tx(tx);
    });
    rune_kit::context_menu("terminal-menu", grid, s.term.menu_at)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.term.menu_at = None)
        .item(
            MenuItem::new(tr_ref(language, "Copy"))
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
            MenuItem::new(tr_ref(language, "Paste"))
                .shortcut(label(Action::TermPaste))
                .on_select_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.term.menu_at = None;
                    tx.request_paste();
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Find in terminal…"))
                .shortcut(label(Action::SearchMessages))
                .on_select(|s: &mut UiState| {
                    s.term.menu_at = None;
                    s.open_terminal_search();
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Clear")).on_select(|s: &mut UiState| {
                s.term.menu_at = None;
                s.term.send(Msg::Clear);
            }),
        )
        .separator()
        .item(
            MenuItem::new(tr_ref(language, "New shell"))
                .shortcut(label(Action::NewShell))
                .on_select(|s: &mut UiState| {
                    s.term.menu_at = None;
                    s.restart_terminal();
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Hide terminal"))
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

fn grid(
    s: &UiState,
    k: &Kit,
    frame: Option<&Frame>,
    m: Metrics,
    bg: Color,
    scale: f32,
    view: GridView<'_>,
) -> El {
    let GridView {
        tile_idx,
        live,
        sfx,
        ended,
    } = view;
    let focused = live && s.term.sink.focused;
    let mut grid = el()
        .key(format!("terminal-grid{sfx}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .preserve_focus_on_press(true)
        .cursor(if s.term.hover_link.is_some() {
            rune::Cursor::Pointer
        } else if frame.is_some_and(|f| f.mouse_tracking) {
            rune::Cursor::Default
        } else {
            rune::Cursor::Text
        })
        .on_layout(move |s: &mut UiState, r| s.terminal_tile_layout(tile_idx, r, scale))
        .on_drop_files(move |s: &mut UiState, files, tx: &mut Tx| {
            s.focus_terminal_tile_tx(tile_idx, tx);
            paste_terminal_files(s, files);
        })
        .on_pointer_move_local(move |s: &mut UiState, ev, tx: &mut Tx| {
            let mac =
                crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;
            let ctrl = tx.mods().ctrl || (mac && tx.mods().logo);
            if !ctrl {
                if s.term.hover_link.is_some() {
                    s.term.hover_link = None;
                }
                return;
            }
            let ch = metrics().ch;
            let cw = metrics().cw;
            let col = (ev.x / cw).floor() as u16;
            let row = (ev.y / ch).floor() as usize;
            let frame = if live {
                s.term.frame.as_ref()
            } else {
                tile_idx
                    .and_then(|i| s.tile_terminal_tab(i))
                    .and_then(|t| s.term.tabs.get(t))
                    .and_then(|t| t.frame.as_ref())
            };
            if let Some(row_data) = frame.and_then(|f| f.grid.get(row)) {
                let text = row_data.plain_text();
                let cwd = s.terminal_cwd();
                if let Some(detected) = crate::term::link::find_link_at(&text, col as usize, &cwd) {
                    s.term.hover_link = Some(TermHoverLink {
                        link: detected.link,
                        row,
                        col_start: detected.col_start,
                        col_end: detected.col_end,
                    });
                    return;
                }
            }
            s.term.hover_link = None;
        })
        .on_drag(move |s: &mut UiState, d: &mut rune::Drag, tx: &mut Tx| {
            let mac =
                crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;
            let ctrl = tx.mods().ctrl || (mac && tx.mods().logo);
            if ctrl && d.phase == rune::Phase::Down {
                let ch = metrics().ch;
                let cw = metrics().cw;
                let col = (d.x / cw).floor() as u16;
                let row = (d.y / ch).floor() as usize;
                let frame = if live {
                    s.term.frame.as_ref()
                } else {
                    tile_idx
                        .and_then(|i| s.tile_terminal_tab(i))
                        .and_then(|t| s.term.tabs.get(t))
                        .and_then(|t| t.frame.as_ref())
                };
                if let Some(row_data) = frame.and_then(|f| f.grid.get(row)) {
                    let text = row_data.plain_text();
                    let cwd = s.terminal_cwd();
                    if let Some(detected) =
                        crate::term::link::find_link_at(&text, col as usize, &cwd)
                    {
                        s.open_terminal_link(detected.link);
                        return;
                    }
                }
            }
            let phase = match d.phase {
                rune::Phase::Down => {
                    s.focus_terminal_tile_tx(tile_idx, tx);
                    MousePhase::Down
                }
                rune::Phase::Move => MousePhase::Move,
                rune::Phase::Up => MousePhase::Up,
            };
            s.terminal_mouse(phase, d.x, d.y, tx.mods());
        })
        .on_wheel(move |s: &mut UiState, e: rune::WheelEvent, _: &mut Tx| {
            let ch = metrics().ch;
            s.term.wheel += e.dy;
            let lines = -(s.term.wheel / ch).trunc() as i32;
            if lines != 0 {
                s.term.wheel += lines as f32 * ch;
                // A tile beside the live one scrolls its own shell.
                let other = tile_idx
                    .filter(|i| s.tile_of_terminal() != Some(*i))
                    .and_then(|i| s.tile_terminal_tab(i))
                    .and_then(|t| s.term.tabs.get(t));
                let size = match other {
                    Some(tab) => s.term.tab_sizes.get(&tab.id).copied(),
                    None => s.term.size,
                };
                let (x, y) = size.map_or((0.0, 0.0), |sz| {
                    (
                        e.x / metrics().cw * sz.cell_w as f32,
                        e.y / ch * sz.cell_h as f32,
                    )
                });
                let msg = Msg::Wheel {
                    lines,
                    x,
                    y,
                    mods: Mods {
                        shift: e.mods.shift,
                        ctrl: e.mods.ctrl,
                        alt: e.mods.alt,
                        logo: e.mods.logo,
                    },
                };
                match other.and_then(|tab| tab.session.as_ref()) {
                    Some(session) => session.send(msg),
                    None if other.is_none() => s.term.send(msg),
                    None => {}
                }
            }
            true
        });
    let Some(frame) = frame else {
        return if live {
            grid.child(sink(s, k, m, 0, 0, bg))
        } else {
            grid
        };
    };
    for (y, row) in frame.grid.iter().enumerate() {
        let row = row.clone();
        let key = format!("terminal-row{sfx}-{y}");
        let version = (
            row.hash,
            m.cw.to_bits(),
            y,
            bg.r.to_bits(),
            bg.g.to_bits(),
            bg.b.to_bits(),
        );
        grid = grid.child(memo(key.clone(), version, move || {
            draw_row(&key, &row, y, m, bg)
        }));
    }

    // Search match highlights (when search overlay is active)
    if s.term.search.open && !s.term.search.matches.is_empty() {
        let offset = frame.scroll.offset as usize;
        let len = frame.scroll.len as usize;
        for (idx, m_item) in s.term.search.matches.iter().enumerate() {
            if m_item.row >= offset && m_item.row < offset + len {
                let visible_y = (m_item.row - offset) as f32;
                let is_active = idx == s.term.search.active_idx;
                let x = m_item.col as f32 * m.cw;
                let y = visible_y * m.ch;
                let w = m_item.len as f32 * m.cw;
                let h = m.ch;
                let highlight_fill = if is_active {
                    k.c.bg_accent.alpha(0.65)
                } else {
                    color(Rgb(250, 204, 21)).alpha(0.35)
                };
                grid = grid.child(
                    el().key(format!("term-match{sfx}-{idx}"))
                        .absolute(x, y)
                        .size(w.max(2.0), h)
                        .radius(2.0)
                        .fill(highlight_fill)
                        .a11y_hidden(),
                );
            }
        }
    }

    // Hovered link underline (Ctrl/Cmd + hover)
    if let Some(hover) = &s.term.hover_link {
        if hover.row < frame.grid.len() {
            let x = hover.col_start as f32 * m.cw;
            let y = hover.row as f32 * m.ch;
            let w = (hover.col_end.saturating_sub(hover.col_start)) as f32 * m.cw;
            grid = grid.child(
                el().key(format!("terminal-hover-link{sfx}"))
                    .absolute(x, y + m.ch - 1.5)
                    .size(w.max(2.0), 1.5)
                    .fill(k.c.text_accent)
                    .a11y_hidden(),
            );
        }
    }

    // A shell that ended takes no more keys: no cursor.
    if let Some(cursor) = frame.cursor.filter(|_| !ended) {
        let (_ch, is_wide, head_col) = cursor_char_at(frame, cursor.col, cursor.row);
        let wide = cursor.wide || is_wide;
        let cells = if wide { 2.0 } else { 1.0 };
        let col = if wide { head_col } else { cursor.col };
        let (x, y) = (f32::from(col) * m.cw, f32::from(cursor.row) * m.ch);
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
        grid = grid.child(
            mark.key(format!("terminal-cursor{sfx}"))
                .instant_changes()
                .motion(rune::Spring::NONE)
                .transition(rune::Transition::none())
                .a11y_hidden(),
        );
    }
    if frame.scroll.in_history() && frame.scroll.total > 0 {
        // Where the viewport is in the history, along the right edge.
        let h = f32::from(frame.rows) * m.ch;
        let total = frame.scroll.total as f32;
        let thumb = (frame.scroll.len as f32 / total * h).clamp(16.0, h);
        let range = (frame.scroll.total - frame.scroll.len).max(1) as f32;
        let top = frame.scroll.offset as f32 / range * (h - thumb);
        grid = grid.child(
            el().key(format!("terminal-scroll{sfx}"))
                .absolute(f32::from(frame.cols) * m.cw - 3.0, top.min(h - thumb))
                .size(3.0, thumb)
                .radius(1.5)
                .fill(k.c.text_faint.alpha(0.6))
                .a11y_hidden(),
        );
    }
    if !live {
        return grid;
    }
    let (cx, cy) = frame.cursor.map_or((0, 0), |c| (c.col, c.row));
    grid.child(sink(s, k, m, cx, cy, bg))
}

/// Extract the character, wide status, and start column for the cell at (col, row).
pub(crate) fn cursor_char_at(frame: &Frame, col: u16, row: u16) -> (String, bool, u16) {
    let Some(row_data) = frame.grid.get(usize::from(row)) else {
        return (String::new(), false, col);
    };

    for run in &row_data.runs {
        if col >= run.col && col < run.col + run.cells {
            let offset = usize::from(col - run.col);
            let total_cells = usize::from(run.cells);

            if run.text.is_empty() || total_cells == 0 {
                return (String::new(), false, col);
            }

            // Tab stops or whitespace runs draw a space and never span wide cells.
            if run.text == "\t" || (run.text.trim().is_empty() && !run.text.is_empty()) {
                return (" ".to_string(), false, col);
            }

            // Single grapheme cluster (e.g. CJK wide character, Nerd font icon,
            // or emoji with variation selector).
            let grapheme_count = run.text.graphemes(true).count();
            if grapheme_count <= 1 {
                let is_wide = total_cells >= 2;
                return (run.text.clone(), is_wide, run.col);
            }

            // Fast path: simple 1-cell run where every grapheme is 1 char and 1 cell.
            if run.text.chars().count() == total_cells && grapheme_count == total_cells {
                let ch = run
                    .text
                    .chars()
                    .nth(offset)
                    .map(String::from)
                    .unwrap_or_default();
                return (ch, false, col);
            }

            // Multiple graphemes where char count != cell count (e.g. mixed CJK/ASCII,
            // multi-byte sequences, combining marks, or tab stops).
            let mut current_cell = 0usize;
            for g in run.text.graphemes(true) {
                let w = if g == "\t" {
                    (8 - (current_cell % 8))
                        .min(total_cells.saturating_sub(current_cell))
                        .max(1)
                } else {
                    UnicodeWidthStr::width(g).max(1)
                };

                if w > 0 {
                    let next_cell = current_cell + w;
                    if offset >= current_cell && offset < next_cell {
                        let is_wide = w >= 2;
                        let head_col = run.col + current_cell as u16;
                        let text = if g == "\t" {
                            " ".to_string()
                        } else {
                            g.to_string()
                        };
                        return (text, is_wide, head_col);
                    }
                    current_cell = next_cell;
                    if current_cell > offset {
                        break;
                    }
                }
            }

            return (String::new(), false, col);
        }
    }

    (String::new(), false, col)
}

/// The character under a block cursor, drawn in the background colour.
pub(crate) fn cursor_glyph(frame: &Frame, col: u16, row: u16, bg: Color) -> El {
    let (ch, _wide, _head) = cursor_char_at(frame, col, row);
    text(ch).font(style()).color(bg)
}

fn draw_row(key: &str, row: &Row, y: usize, m: Metrics, default_bg: Color) -> El {
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
        let cell_bg = row
            .bgs
            .iter()
            .find(|b| run.col >= b.col && run.col < b.col + b.len)
            .map_or(default_bg, |b| color(b.color));
        let mut fg = color(run.fg);
        if run.attrs.faint {
            fg = ensure_contrast(fg.mix(cell_bg, 0.35), cell_bg, 3.2);
        } else {
            fg = ensure_contrast(fg, cell_bg, 4.65);
        }
        let mut font = style();
        if run.attrs.bold {
            font.weight = 700.0;
        }
        if run.attrs.italic {
            font = font.italic();
        }
        out = out.child(text(run.text.clone()).font(font).color(fg).absolute(x, 0.0));
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
            paste_terminal_clipboard(s, content);
            true
        });
    if composing {
        field = field.fill(bg);
    }
    field
}

fn shell_quote_path(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '~'))
    {
        raw.into_owned()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

fn paste_terminal_files(s: &mut UiState, files: &[PathBuf]) {
    if files.is_empty() {
        return;
    }
    let joined: Vec<String> = files.iter().map(|p| shell_quote_path(p)).collect();
    s.term.send(Msg::Paste(format!("{} ", joined.join(" "))));
}

fn paste_terminal_clipboard(s: &mut UiState, content: &rune::ClipboardContent) {
    let files = content.files();
    if !files.is_empty() {
        paste_terminal_files(s, files);
        return;
    }
    if let Some(image) = content.image() {
        let dir = std::env::temp_dir().join("muqun-clipboard");
        let _ = std::fs::create_dir_all(&dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("paste-{stamp}.png"));
        if std::fs::write(&path, image.png()).is_ok() {
            let quoted = shell_quote_path(&path);
            s.term.send(Msg::Paste(format!("{quoted} ")));
            return;
        }
    }
    let text = content.text();
    if !text.is_empty() {
        s.term.send(Msg::Paste(text.to_string()));
    } else {
        // If ClipboardContent held neither files, image, nor text when Ctrl+V
        // triggered a paste, forward raw Ctrl+V (\x16) to the PTY so TUI agents
        // or editors listening for literal ^V still receive it.
        s.term.send(Msg::Text("\x16".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;
    use rune::{Env, Ime, Key, Mods, Runtime};
    use std::sync::Arc;

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
        rt.ime(Ime::Commit("test".into()));
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
                "text test",
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
    fn open_file_in_terminal_opens_terminal_and_queues_command() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let test_path = std::path::Path::new("/tmp/test_file.rs");
        state.open_file_in_terminal(test_path, "nvim");

        assert!(state.term.open);
        assert!(state.term.sink.focused);
        assert!(state.viewer.is_none());
        assert_eq!(
            state.term.pending_command.as_deref(),
            Some("nvim '/tmp/test_file.rs'\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_file_in_terminal_with_active_session_opens_new_tab() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let size = grid_size(560.0, 340.0, Metrics { cw: 8.0, ch: 17.0 }, 1.0);
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 10"]);
        let s0 = Session::start_command(
            cmd,
            size,
            colors(&Kit::neutral(false), ColorScheme::Light),
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        state.term.session = Some(s0.clone());
        state.term.tabs.push(TermTab {
            id: 1,
            session: Some(s0),
            frame: None,
            cwd: PathBuf::from("/tmp"),
            title: None,
            pending_command: None,
        });
        state.term.active_tab = 0;

        let test_path = std::path::Path::new("/tmp/another_file.rs");
        state.open_file_in_terminal(test_path, "nvim");

        assert_eq!(state.term.tabs.len(), 2);
        assert_eq!(state.term.active_tab, 1);
        assert_eq!(
            state.term.pending_command.as_deref(),
            Some("nvim '/tmp/another_file.rs'\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_multiple_tabs_closed_to_last_then_ctrl_d() {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.open_terminal();
        let size = grid_size(560.0, 340.0, Metrics { cw: 8.0, ch: 17.0 }, 1.0);
        let make_session = |script: &str| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", script]);
            Session::start_command(
                cmd,
                size,
                colors(&Kit::neutral(false), ColorScheme::Light),
                std::sync::Arc::new(|| {}),
            )
            .unwrap()
        };
        // Tab 0
        let s0 = make_session("exit 0");
        s.term.session = Some(s0.clone());
        s.term.tabs.push(TermTab {
            id: 1,
            session: Some(s0.clone()),
            frame: None,
            cwd: PathBuf::from("/tmp"),
            title: None,
            pending_command: None,
        });
        s.term.active_tab = 0;

        // Tab 1
        let s1 = make_session("exit 0");
        s.term.tabs.push(TermTab {
            id: 2,
            session: Some(s1.clone()),
            frame: None,
            cwd: PathBuf::from("/tmp"),
            title: None,
            pending_command: None,
        });
        s.term.active_tab = 1;
        s.term.session = Some(s1);

        // Wait for Tab 1 to exit
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while s.term.session.as_ref().is_some_and(|x| x.ended().is_none()) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Settle Tab 1
        assert!(s.settle_terminal());
        assert_eq!(s.term.tabs.len(), 1);
        assert_eq!(s.term.active_tab, 0);

        // Wait for Tab 0 to exit
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while s.term.session.as_ref().is_some_and(|x| x.ended().is_none()) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(s.settle_terminal());
        assert_eq!(s.term.tabs.len(), 0);
        assert!(!s.term.open);
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

    #[test]
    fn terminal_sync_cwd_opens_or_switches_tab() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.term_sync_cwd();
        assert!(state.term.open);
        assert!(!state.toasts.is_empty());
        assert_eq!(
            state.toasts.last().unwrap().title,
            "Terminal switched to session folder"
        );
    }

    #[test]
    fn terminal_tab_switch_shortcuts() {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.term.tabs.push(TermTab {
            id: 1,
            session: None,
            frame: None,
            cwd: PathBuf::from("/tmp/1"),
            title: Some("tab1".into()),
            pending_command: None,
        });
        s.term.tabs.push(TermTab {
            id: 2,
            session: None,
            frame: None,
            cwd: PathBuf::from("/tmp/2"),
            title: Some("tab2".into()),
            pending_command: None,
        });
        s.term.tabs.push(TermTab {
            id: 3,
            session: None,
            frame: None,
            cwd: PathBuf::from("/tmp/3"),
            title: Some("tab3".into()),
            pending_command: None,
        });
        s.term.active_tab = 0;

        // Next tab
        s.term_next_tab();
        assert_eq!(s.term.active_tab, 1);
        s.term_next_tab();
        assert_eq!(s.term.active_tab, 2);
        // Wraps around
        s.term_next_tab();
        assert_eq!(s.term.active_tab, 0);

        // Prev tab
        s.term_prev_tab();
        assert_eq!(s.term.active_tab, 2);
        s.term_prev_tab();
        assert_eq!(s.term.active_tab, 1);

        // Test keyboard switching via runtime
        s.open_terminal();
        s.term.sink.focused = true;
        let mut rt = Runtime::new(s, super::super::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);

        // Alt+1 jumps to tab 0
        rt.key(KeyEvent {
            key: Key::Text("1".into()),
            mods: Mods {
                shift: false,
                cmd: false,
                alt: true,
                ctrl: false,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 0);

        // Alt+3 jumps to tab 2
        rt.key(KeyEvent {
            key: Key::Text("3".into()),
            mods: Mods {
                shift: false,
                cmd: false,
                alt: true,
                ctrl: false,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 2);

        // Ctrl+Tab cycles next (wraps to 0)
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Mods {
                shift: false,
                cmd: true,
                alt: false,
                ctrl: true,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 0);

        // Ctrl+Shift+Tab cycles prev (wraps to 2)
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Mods {
                shift: true,
                cmd: true,
                alt: false,
                ctrl: true,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 2);

        // Ctrl+PageUp cycles prev (to 1)
        rt.key(KeyEvent {
            key: Key::PageUp,
            mods: Mods {
                shift: false,
                cmd: true,
                alt: false,
                ctrl: true,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 1);

        // Ctrl+PageDown cycles next (to 2)
        rt.key(KeyEvent {
            key: Key::PageDown,
            mods: Mods {
                shift: false,
                cmd: true,
                alt: false,
                ctrl: true,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 2);
    }

    #[test]
    fn terminal_colors_maintain_high_contrast_in_light_and_dark_modes() {
        for dark in [false, true] {
            let scheme = if dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            };
            let kit = Kit::neutral(dark);
            let c = colors(&kit, scheme);
            let bg = color(c.bg);
            let fg = color(c.fg);

            let fg_ratio = contrast_ratio(fg, bg);
            assert!(
                fg_ratio >= 4.5,
                "fg contrast ratio too low in dark={dark}: {fg_ratio}"
            );

            for (i, ansi_rgb) in c.ansi.iter().enumerate() {
                let ansi_col = color(*ansi_rgb);
                let ratio = contrast_ratio(ansi_col, bg);
                assert!(
                    ratio >= 4.5,
                    "ansi[{i}] contrast ratio too low in dark={dark}: {ratio} (ansi: {ansi_rgb:?}, bg: {:?})",
                    c.bg
                );
            }
        }
    }

    #[test]
    fn cursor_glyph_handles_ascii_cjk_and_icons() {
        use crate::term::grid::{Attrs, TextRun};
        use std::sync::Arc;

        let mut frame = Frame {
            cols: 80,
            rows: 24,
            ..Default::default()
        };

        let row0 = Row {
            runs: vec![
                TextRun {
                    col: 0,
                    cells: 5,
                    text: "hello".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // CJK wide character (U+4E16)
                TextRun {
                    col: 5,
                    cells: 2,
                    text: "\u{4e16}".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // CJK wide character (U+754C)
                TextRun {
                    col: 7,
                    cells: 2,
                    text: "\u{754c}".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // Nerd Font icon
                TextRun {
                    col: 9,
                    cells: 1,
                    text: "\u{e0a0}".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // Mixed run with CJK and ASCII
                TextRun {
                    col: 10,
                    cells: 4,
                    text: "a\u{4e16}b".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
            ],
            ..Row::default()
        };

        let row1 = Row {
            runs: vec![
                // Combining mark (e + acute)
                TextRun {
                    col: 0,
                    cells: 1,
                    text: "e\u{0301}".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // Tab stop
                TextRun {
                    col: 1,
                    cells: 4,
                    text: "\t".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                // Emoji with variation selector
                TextRun {
                    col: 5,
                    cells: 2,
                    text: "\u{2764}\u{fe0f}".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
            ],
            ..Row::default()
        };

        frame.grid = vec![Arc::new(row0), Arc::new(row1)];

        // 1. ASCII tests
        assert_eq!(cursor_char_at(&frame, 0, 0), ("h".to_string(), false, 0));
        assert_eq!(cursor_char_at(&frame, 1, 0), ("e".to_string(), false, 1));
        assert_eq!(cursor_char_at(&frame, 4, 0), ("o".to_string(), false, 4));

        // 2. CJK wide character at col 5 (2 cells wide)
        // Head of wide character
        assert_eq!(
            cursor_char_at(&frame, 5, 0),
            ("\u{4e16}".to_string(), true, 5)
        );
        // Tail of wide character (should return the same glyph and point to head_col 5)
        assert_eq!(
            cursor_char_at(&frame, 6, 0),
            ("\u{4e16}".to_string(), true, 5)
        );

        // Next CJK wide character at col 7
        assert_eq!(
            cursor_char_at(&frame, 7, 0),
            ("\u{754c}".to_string(), true, 7)
        );
        assert_eq!(
            cursor_char_at(&frame, 8, 0),
            ("\u{754c}".to_string(), true, 7)
        );

        // 3. Nerd font icon at col 9
        assert_eq!(
            cursor_char_at(&frame, 9, 0),
            ("\u{e0a0}".to_string(), false, 9)
        );

        // 4. Mixed run "a\u{4e16}b" at col 10..14
        assert_eq!(cursor_char_at(&frame, 10, 0), ("a".to_string(), false, 10));
        assert_eq!(
            cursor_char_at(&frame, 11, 0),
            ("\u{4e16}".to_string(), true, 11)
        );
        assert_eq!(
            cursor_char_at(&frame, 12, 0),
            ("\u{4e16}".to_string(), true, 11)
        );
        assert_eq!(cursor_char_at(&frame, 13, 0), ("b".to_string(), false, 13));

        // 5. Combining mark on row 1
        assert_eq!(
            cursor_char_at(&frame, 0, 1),
            ("e\u{0301}".to_string(), false, 0)
        );

        // 6. Tab stop on row 1 (renders spaces, not raw tab character)
        assert_eq!(cursor_char_at(&frame, 1, 1), (" ".to_string(), false, 1));
        assert_eq!(cursor_char_at(&frame, 4, 1), (" ".to_string(), false, 4));

        // 7. Emoji on row 1
        assert_eq!(
            cursor_char_at(&frame, 5, 1),
            ("\u{2764}\u{fe0f}".to_string(), true, 5)
        );
        assert_eq!(
            cursor_char_at(&frame, 6, 1),
            ("\u{2764}\u{fe0f}".to_string(), true, 5)
        );

        // Verify cursor_glyph builds elements for all these positions without panic
        let bg = Color::WHITE;
        let _ = cursor_glyph(&frame, 0, 0, bg);
        let _ = cursor_glyph(&frame, 5, 0, bg);
        let _ = cursor_glyph(&frame, 6, 0, bg);
        let _ = cursor_glyph(&frame, 9, 0, bg);
        let _ = cursor_glyph(&frame, 11, 0, bg);
        let _ = cursor_glyph(&frame, 0, 1, bg);
        let _ = cursor_glyph(&frame, 1, 1, bg);
    }

    #[test]
    fn cursor_glyph_bounds_checking() {
        use crate::term::grid::{Attrs, TextRun};
        use std::sync::Arc;

        let mut frame = Frame {
            cols: 80,
            rows: 24,
            ..Default::default()
        };

        // Gap between runs: run 0 at 0..5, run 1 at 10..15
        let row0 = Row {
            runs: vec![
                TextRun {
                    col: 0,
                    cells: 5,
                    text: "start".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
                TextRun {
                    col: 10,
                    cells: 5,
                    text: "after".to_string(),
                    fg: Rgb::default(),
                    attrs: Attrs::default(),
                },
            ],
            ..Row::default()
        };

        // Row with an empty text run
        let row1 = Row {
            runs: vec![TextRun {
                col: 0,
                cells: 0,
                text: String::new(),
                fg: Rgb::default(),
                attrs: Attrs::default(),
            }],
            ..Row::default()
        };

        frame.grid = vec![Arc::new(row0), Arc::new(row1)];

        // Cell in the gap (cols 5..9)
        assert_eq!(cursor_char_at(&frame, 5, 0), (String::new(), false, 5));
        assert_eq!(cursor_char_at(&frame, 7, 0), (String::new(), false, 7));
        assert_eq!(cursor_char_at(&frame, 9, 0), (String::new(), false, 9));

        // Column beyond all runs on row 0
        assert_eq!(cursor_char_at(&frame, 15, 0), (String::new(), false, 15));
        assert_eq!(cursor_char_at(&frame, 79, 0), (String::new(), false, 79));
        assert_eq!(cursor_char_at(&frame, 100, 0), (String::new(), false, 100));
        assert_eq!(
            cursor_char_at(&frame, u16::MAX, 0),
            (String::new(), false, u16::MAX)
        );

        // Empty run on row 1
        assert_eq!(cursor_char_at(&frame, 0, 1), (String::new(), false, 0));

        // Row out of bounds
        assert_eq!(cursor_char_at(&frame, 0, 2), (String::new(), false, 0));
        assert_eq!(cursor_char_at(&frame, 0, 99), (String::new(), false, 0));
        assert_eq!(
            cursor_char_at(&frame, 0, u16::MAX),
            (String::new(), false, 0)
        );

        // Completely empty frame
        let empty_frame = Frame::default();
        assert_eq!(
            cursor_char_at(&empty_frame, 0, 0),
            (String::new(), false, 0)
        );

        // Ensure cursor_glyph handles all boundary cases without panic
        let bg = Color::WHITE;
        let _ = cursor_glyph(&frame, 7, 0, bg);
        let _ = cursor_glyph(&frame, 100, 0, bg);
        let _ = cursor_glyph(&frame, 0, 99, bg);
        let _ = cursor_glyph(&empty_frame, 0, 0, bg);
    }

    #[test]
    fn cursor_element_in_grid_handles_wide_character_and_instant_layout() {
        use crate::term::grid::{Cursor, CursorShape, TextRun};
        use std::sync::Arc;

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.term.sink.focused = true;
        let k = Kit::neutral(false);
        let m = Metrics { cw: 8.0, ch: 16.0 };
        let bg = Color::BLACK;

        let mut frame = Frame {
            cols: 80,
            rows: 24,
            cursor_color: Rgb(255, 255, 255),
            ..Default::default()
        };

        // Put a CJK wide character at col 4..6
        let row0 = Row {
            runs: vec![
                TextRun {
                    col: 0,
                    cells: 4,
                    text: "test".to_string(),
                    fg: Rgb::default(),
                    attrs: Default::default(),
                },
                TextRun {
                    col: 4,
                    cells: 2,
                    text: "\u{4e16}".to_string(),
                    fg: Rgb::default(),
                    attrs: Default::default(),
                },
            ],
            ..Row::default()
        };
        frame.grid = vec![Arc::new(row0)];

        // Cursor positioned at head of wide character (col 4)
        frame.cursor = Some(Cursor {
            col: 4,
            row: 0,
            shape: CursorShape::Block,
            wide: false,
        });

        let grid_el = grid(&s, &k, Some(&frame), m, bg, 1.0, GridView::panel(false));
        // Verify grid builds cleanly
        let _ = grid_el;

        // Cursor positioned at tail of wide character (col 5)
        frame.cursor = Some(Cursor {
            col: 5,
            row: 0,
            shape: CursorShape::Block,
            wide: true,
        });
        let grid_el_tail = grid(&s, &k, Some(&frame), m, bg, 1.0, GridView::panel(false));
        let _ = grid_el_tail;
    }

    #[test]
    fn clicking_terminal_in_session_mode_keeps_keyboard_on_terminal_not_composer() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.mode = super::super::Mode::Session;
        state.draft = TextEdit::new("hello world");
        state.draft.set_caret(2);
        state.draft.focused = true;

        state.open_terminal();
        assert!(state.term.sink.focused);
        assert!(!state.draft.focused);

        let mut rt = Runtime::new(
            state,
            super::super::view,
            (1440.0, 900.0),
            Env::new(true, 1.0),
        );
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);

        // Click inside the terminal grid on the right side of the window.
        rt.pointer_down(1200.0, 300.0);
        rt.pointer_up(1200.0, 300.0);
        rt.frame(2.0 / 60.0);

        assert!(rt.state.term.sink.focused);
        assert!(!rt.state.draft.focused);

        // Pressing Left, Right, Ctrl+E, and typing characters must go to the terminal,
        // never moving the composer caret or inserting into `state.draft`.
        for ev in [
            key(Key::Left, false, false),
            key(Key::Right, false, false),
            key(Key::Command('e'), true, false),
            key(Key::Text("x".into()), false, false),
        ] {
            rt.key(ev);
            rt.frame(3.0 / 60.0);
        }
        assert_eq!(rt.state.draft.text, "hello world");
        assert_eq!(rt.state.draft.focus, 2);
        assert!(rt.state.term.sink.focused);
        assert!(!rt.state.draft.focused);
    }

    #[test]
    fn terminal_search_finds_matches_and_navigates() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let frame = Frame {
            rows: 4,
            cols: 40,
            grid: vec![
                Arc::new(Row {
                    runs: vec![crate::term::grid::TextRun {
                        col: 0,
                        cells: 14,
                        text: "Error in main.rs".into(),
                        fg: Rgb(200, 200, 200),
                        attrs: Default::default(),
                    }],
                    ..Default::default()
                }),
                Arc::new(Row {
                    runs: vec![crate::term::grid::TextRun {
                        col: 0,
                        cells: 18,
                        text: "Checking main.rs line".into(),
                        fg: Rgb(200, 200, 200),
                        attrs: Default::default(),
                    }],
                    ..Default::default()
                }),
            ],
            ..Default::default()
        };
        state.term.frame = Some(frame);

        // Open terminal search
        state.open_terminal_search();
        assert!(state.term.search.open);

        // Search for "main.rs"
        state.term.search.query = TextEdit::new("main.rs");
        state.update_term_search_matches();
        assert_eq!(state.term.search.matches.len(), 2);
        assert_eq!(state.term.search.matches[0].row, 0);
        assert_eq!(state.term.search.matches[0].col, 9);
        assert_eq!(state.term.search.matches[1].row, 1);
        assert_eq!(state.term.search.matches[1].col, 9);

        // Next / Prev navigation
        state.term_search_next();
        assert_eq!(state.term.search.active_idx, 1);
        state.term_search_next();
        assert_eq!(state.term.search.active_idx, 0);
        state.term_search_prev();
        assert_eq!(state.term.search.active_idx, 1);

        // Close search
        state.close_terminal_search_no_tx();
        assert!(!state.term.search.open);
        assert!(state.term.search.matches.is_empty());
    }

    #[test]
    fn terminal_link_detection_and_open() {
        let cwd = std::env::current_dir().unwrap();
        let line =
            "error: file not found at desktop/src/main.rs:10:5 (see https://github.com/rust-lang)";
        let url_link = crate::term::link::find_link_at(line, 60, &cwd).unwrap();
        assert_eq!(
            url_link.link,
            crate::term::link::TerminalLink::Url("https://github.com/rust-lang".into())
        );

        let file_link = crate::term::link::find_link_at(line, 30, &cwd).unwrap();
        match file_link.link {
            crate::term::link::TerminalLink::FilePath { path, line, col } => {
                assert!(path.ends_with("desktop/src/main.rs"));
                assert_eq!(line, Some(10));
                assert_eq!(col, Some(5));
            }
            _ => panic!("expected FilePath"),
        }
    }
}
