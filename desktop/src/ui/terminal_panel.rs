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
    fn accept_frame(&mut self, frame: Frame) {
        if let (Some(session), Some(command)) = (&self.session, self.pending_command.take()) {
            session.send(Msg::Text(command));
        }
        self.frame = Some(frame);
    }

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

/// Tracks an embedded terminal tab opened as a per-workspace editor so opening
/// additional files in the same working directory reuses one editor instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTabMeta {
    pub workspace: PathBuf,
    pub editor_kind: String,
    pub ipc: Option<String>,
}

/// What the panel keeps between frames.
pub struct TermPanel {
    pub open: bool,
    pub pane: PaneState,
    pub tabs: Vec<TermTab>,
    pub active_tab: usize,
    pub next_tab_id: u64,
    pub editor_tabs: std::collections::HashMap<u64, EditorTabMeta>,
    /// Maps terminal tab ID → remote computer ID when that tab runs a remote PTY.
    pub remote_tabs: std::collections::HashMap<u64, String>,
    pub session: Option<Session>,
    pub frame: Option<Frame>,
    /// The keyboard sink on the cursor cell (see the module docs).
    pub sink: TextEdit,
    pub waker: Option<rune::Waker>,
    /// Search state for scrollback and grid.
    pub search: TermSearchState,
    /// Hovered link under pointer when Ctrl/Cmd is held.
    pub hover_link: Option<TermHoverLink>,
    /// When the Herdr `Ctrl+Space` prefix was armed.
    pub prefix_armed: Option<std::time::Instant>,
    /// Screenshot scenes: what a program would print at a size, replayed
    /// into a fresh emulator instead of a live shell.
    pub fixture: Option<fn(u16, u16) -> Vec<u8>>,
    /// Screenshot scenes: how the fixture's shell ended.
    pub fixture_ended: Option<Ended>,
    pub error: Option<String>,
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
            editor_tabs: std::collections::HashMap::new(),
            remote_tabs: std::collections::HashMap::new(),
            session: None,
            frame: None,
            sink: TextEdit::new(""),
            waker: None,
            search: TermSearchState::default(),
            hover_link: None,
            prefix_armed: None,
            fixture: None,
            fixture_ended: None,
            error: None,
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

    /// Whether the Herdr / Leader prefix is currently armed.
    /// Matches global Which-Key: stays until Esc or a completed chord.
    pub fn is_prefix_armed(&self) -> bool {
        self.prefix_armed.is_some()
    }

    /// Whether the panel is in the window (open, or sliding shut over a
    /// shell that keeps running).
    pub fn mounted(&self) -> bool {
        self.open || self.session.is_some() || self.fixture.is_some()
    }

    pub fn active_mut(&mut self) -> Option<&mut TermTab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub(crate) fn send(&self, msg: Msg) {
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
            // Explicit terminal palettes carry ANSI semantics (including black
            // and dim colours). Do not recolour them as generic UI text.
            return Colors {
                fg: to_rgb(fg),
                bg: to_rgb(bg),
                cursor: to_rgb(cursor),
                selection: to_rgb(bg.mix(sel.alpha(1.0), sel.a.max(0.25))),
                ansi,
            };
        }
    }
    fallback_colors(k)
}

fn fallback_colors(k: &Kit) -> Colors {
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
                let next = (0..self.tiles.len())
                    .find(|&j| j != i && !self.tiles[j].is_terminal())
                    .unwrap_or(if i > 0 {
                        i - 1
                    } else if self.tiles.len() > 1 {
                        1
                    } else {
                        0
                    });
                if next != i {
                    self.focus_tile(next);
                } else {
                    self.term.sink.focused = false;
                    self.draft.focused = true;
                }
            } else {
                self.focus_tile(i);
                self.focus_terminal();
            }
            return;
        }
        if !self.term.open {
            self.open_terminal_default();
        } else if self.term.sink.focused {
            self.close_terminal();
        } else {
            self.focus_terminal();
        }
    }

    pub fn toggle_terminal_tx(&mut self, tx: &mut Tx) {
        self.toggle_terminal();
        if self.term.sink.focused {
            tx.focus("terminal-keys", false);
        } else if self.draft.focused {
            tx.focus("draft", false);
        }
    }

    /// Switch keyboard focus between the session composer and the terminal
    /// without closing the terminal panel. Opens the terminal if closed.
    pub fn toggle_focus(&mut self) {
        if self.in_split() {
            if self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) && self.term.sink.focused
            {
                if let Some(j) = (0..self.tiles.len()).find(|&j| !self.tiles[j].is_terminal()) {
                    self.focus_tile(j);
                } else {
                    self.term.sink.focused = false;
                    self.draft.focused = true;
                }
            } else if let Some(i) = self.tile_of_terminal() {
                self.focus_tile(i);
                self.focus_terminal();
            } else if self.term.open {
                self.focus_terminal();
            } else {
                self.open_terminal_default();
            }
            return;
        }
        if self.term.open && self.term.sink.focused {
            self.term.sink.focused = false;
            if self.mode == super::Mode::Home {
                self.start_draft.focused = true;
            } else {
                self.draft.focused = true;
            }
        } else if self.term.open {
            self.focus_terminal();
        } else {
            self.open_terminal_default();
        }
    }

    pub fn toggle_focus_tx(&mut self, tx: &mut Tx) {
        self.toggle_focus();
        if self.term.sink.focused {
            tx.focus("terminal-keys", false);
        } else if self.draft.focused {
            tx.focus("draft", false);
        }
    }

    /// Split a terminal pane with explicit orientation (`across`: true for
    /// vertical side-by-side split, false for horizontal top/bottom split).
    pub fn split_terminal_with_axis(&mut self, across: bool) {
        if self.in_split() {
            self.split_terminal_tile();
        } else {
            self.open_terminal_tile();
        }
        let target = self.focus;
        let mut tree = self.tile_layout();
        set_leaf_parent_across(&mut tree, target, across);
        self.tile_tree = tree;
        self.persist_tiles();
    }

    /// Toggle terminal zoom/maximize (expands panel width or split ratio).
    pub fn toggle_terminal_zoom(&mut self) {
        if self.in_split() {
            let target = self.focus;
            let mut tree = self.tile_layout();
            toggle_leaf_parent_zoom(&mut tree, target);
            self.tile_tree = tree;
            self.persist_tiles();
        } else if self.term.open {
            let cur = self.term.pane.width();
            let next = if cur < 800.0 { 960.0 } else { 560.0 };
            self.term.pane = self.term.pane.with_width(next);
            self.term.pane.set_open(true);
        }
    }

    /// Ordinary entry points honour the preferred placement only on open.
    /// Explicit docking/splitting and restoring a layout use their own paths.
    pub fn open_terminal_default(&mut self) {
        if let Some(i) = self.tile_of_terminal() {
            self.focus_tile(i);
            self.focus_terminal();
            return;
        }
        if self.term.open {
            self.focus_terminal();
            return;
        }
        if self.dock_tabs.iter().any(|tab| tab == "terminal") {
            self.open_terminal();
            return;
        }
        match self.cfg.ui.terminal_open_mode {
            crate::config::TerminalOpenMode::Docked => self.open_terminal(),
            crate::config::TerminalOpenMode::Split | crate::config::TerminalOpenMode::Floating => {
                // Keep the home surface beside/behind the new terminal.
                // open_terminal_tile normally reuses an empty tile instead.
                if self.tiles.is_empty() && self.selected.is_none() {
                    self.tiles.push(super::Tile::empty());
                    self.tiles.push(super::Tile::empty());
                    self.tiles[1].set_terminal_tab(self.term.active_tab);
                    self.tile_tree = crate::config::TileNode::Split {
                        across: true,
                        ratio: 0.5,
                        a: Box::new(crate::config::TileNode::Leaf(0)),
                        b: Box::new(crate::config::TileNode::Leaf(1)),
                    };
                    self.mode = super::Mode::Session;
                    self.focus_tile(1);
                    self.start_terminal();
                } else {
                    self.open_terminal_tile();
                }
                if let Some(i) = self.tile_of_terminal() {
                    if self.cfg.ui.terminal_open_mode == crate::config::TerminalOpenMode::Floating {
                        self.tiles[i].popped = true;
                        self.tiles[i].floating_rect =
                            Some(Self::default_floating_rect(i, self.tile_grid));
                    }
                    self.right_open = false;
                    self.right.set_open(false);
                    self.focus_terminal();
                    self.persist_tiles();
                }
            }
        }
    }

    pub fn open_terminal(&mut self) {
        self.hidden_dock_tab = None;
        if let Some(previous) = self.selected_dock_tab().map(str::to_owned) {
            self.carry_dock_width(&previous, "terminal");
            self.remember_dock_tab(&previous);
        }
        self.remember_dock_tab("terminal");
        self.active_plugin_drawer = None;
        self.quick_notes_edit.focused = false;
        self.term.open = true;
        self.term.pane.set_open(true);
        self.focus_terminal();
        self.right_open = false;
        self.right.set_open(false);
    }

    /// Hide the panel; the shell keeps running.
    pub fn close_terminal(&mut self) {
        let was_open = self.term.open;
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

        if was_open {
            self.remove_dock_tab("terminal", true);
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn normalized_editor_kind(editor: &str) -> String {
    let first = editor.split_whitespace().next().unwrap_or(editor);
    std::path::Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase()
}

fn canonical_or_self(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn find_git_root(path: &std::path::Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(canonical_or_self)
}

fn nvim_socket_path(tab_id: u64) -> String {
    let path =
        std::env::temp_dir().join(format!("muqun-nvim-{}-{tab_id}.sock", std::process::id()));
    path.to_string_lossy().into_owned()
}

fn spawn_detached(program: &str, args: &[&str], cwd: &std::path::Path) -> bool {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

fn append_file_to_pending_command(pending: &mut String, escaped_path: &str) {
    let trimmed = pending.trim_end_matches('\n');
    let mut updated = String::with_capacity(trimmed.len() + escaped_path.len() + 2);
    updated.push_str(trimmed);
    updated.push(' ');
    updated.push_str(escaped_path);
    updated.push('\n');
    *pending = updated;
}

impl UiState {
    fn editor_workspace_for(&self, path: &std::path::Path) -> PathBuf {
        let abs = canonical_or_self(path);
        let session_dir = self
            .current()
            .and_then(|s| s.directory.as_deref())
            .map(PathBuf::from);
        for candidate in [session_dir, Some(self.terminal_cwd())]
            .into_iter()
            .flatten()
        {
            if candidate.is_dir() {
                let root = canonical_or_self(&candidate);
                if abs.starts_with(&root) {
                    return root;
                }
            }
        }
        if let Some(git_root) = find_git_root(&abs) {
            return git_root;
        }
        path.parent()
            .filter(|p| p.is_dir())
            .map(canonical_or_self)
            .unwrap_or_else(|| canonical_or_self(&self.terminal_cwd()))
    }

    fn cleanup_editor_tab_meta(&mut self, tab_id: u64) {
        self.term.remote_tabs.remove(&tab_id);
        if let Some(meta) = self.term.editor_tabs.remove(&tab_id) {
            if meta.editor_kind == "nvim" {
                if let Some(sock) = meta.ipc {
                    let _ = std::fs::remove_file(sock);
                }
            }
        }
    }

    /// Open a remote file in an embedded terminal running a remote PTY on
    /// `computer` (via `GET /api/pty/ws`), launching `editor` (e.g. `nvim`).
    pub fn open_remote_file_in_terminal(
        &mut self,
        computer: &str,
        remote_path: &str,
        editor: &str,
    ) {
        self.close_viewer();
        let workspace = self
            .current()
            .and_then(|info| info.directory.clone())
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::path::Path::new(remote_path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("~"));
        let escaped_path = shell_quote(remote_path);
        let cmd = format!("{editor} {escaped_path}\n");
        self.term_new_tab_on(Some(computer.to_owned()), workspace, Some(cmd));
    }

    /// Open a file in the embedded terminal using a terminal editor (e.g. nvim).
    /// Reuses one editor tab/instance per working directory instead of spawning
    /// multiple editor tabs for files inside the same workspace.
    pub fn open_file_in_terminal(&mut self, path: &std::path::Path, editor: &str) {
        self.close_viewer();

        let workspace = self.editor_workspace_for(path);
        let editor_kind = normalized_editor_kind(editor);
        let path_str = path.to_string_lossy();
        let escaped_path = shell_quote(&path_str);

        // Reuse an existing live editor tab for the same workspace & editor.
        let existing_idx = self.term.tabs.iter().enumerate().find_map(|(idx, tab)| {
            let meta = self.term.editor_tabs.get(&tab.id)?;
            if meta.workspace != workspace || meta.editor_kind != editor_kind {
                return None;
            }
            let alive = tab.session.as_ref().is_none_or(|s| s.ended().is_none());
            alive.then_some(idx)
        });

        if let Some(idx) = existing_idx {
            let tab_id = self.term.tabs[idx].id;
            let meta = self.term.editor_tabs.get(&tab_id).cloned();
            if let Some(pending) = self.term.tabs[idx].pending_command.as_mut() {
                append_file_to_pending_command(pending, &escaped_path);
                self.reveal_terminal_tab(idx);
                return;
            }
            self.reveal_terminal_tab(idx);
            let session = self.term.tabs[idx]
                .session
                .clone()
                .or_else(|| self.term.session.clone());
            match editor_kind.as_str() {
                "nvim" => {
                    let sock = meta
                        .and_then(|m| m.ipc)
                        .unwrap_or_else(|| nvim_socket_path(tab_id));
                    if std::path::Path::new(&sock).exists()
                        && spawn_detached(
                            "nvim",
                            &["--server", &sock, "--remote-silent", path_str.as_ref()],
                            &workspace,
                        )
                    {
                        return;
                    }
                    // Neovim exited back to the shell prompt in this tab: relaunch inside the same tab.
                    let _ = std::fs::remove_file(&sock);
                    let cmd = format!("{editor} --listen {} {escaped_path}\n", shell_quote(&sock));
                    if let Some(s) = session {
                        s.send(Msg::Text(cmd));
                    }
                    return;
                }
                "vim" => {
                    if let Some(server) = meta.and_then(|m| m.ipc) {
                        if spawn_detached(
                            "vim",
                            &[
                                "--servername",
                                &server,
                                "--remote-silent",
                                path_str.as_ref(),
                            ],
                            &workspace,
                        ) {
                            return;
                        }
                    }
                }
                "emacs" => {
                    if spawn_detached("emacsclient", &["-n", path_str.as_ref()], &workspace) {
                        return;
                    }
                }
                "hx" | "helix" => {
                    if let Some(s) = session {
                        s.send(Msg::Text(format!("\x1b:open {escaped_path}\r")));
                        return;
                    }
                }
                _ => {}
            }
            if let Some(s) = session {
                s.send(Msg::Text(format!("{editor} {escaped_path}\n")));
            }
            return;
        }

        let next_id = self.term.next_tab_id;
        let (cmd, ipc) = match editor_kind.as_str() {
            "nvim" => {
                let sock = nvim_socket_path(next_id);
                let _ = std::fs::remove_file(&sock);
                (
                    format!("{editor} --listen {} {escaped_path}\n", shell_quote(&sock)),
                    Some(sock),
                )
            }
            "vim" => {
                let server = format!("MUQUN_{}_{next_id}", std::process::id());
                (
                    format!("{editor} --servername {server} {escaped_path}\n"),
                    Some(server),
                )
            }
            _ => (format!("{editor} {escaped_path}\n"), None),
        };

        self.term_new_tab_in(workspace.clone(), Some(cmd));
        if let Some(tab) = self.term.tabs.get(self.term.active_tab) {
            self.term.editor_tabs.insert(
                tab.id,
                EditorTabMeta {
                    workspace,
                    editor_kind,
                    ipc,
                },
            );
        }
    }
}

impl UiState {
    /// Open a new terminal tab in `cwd`, optionally executing `command`.
    pub fn term_new_tab(&mut self) {
        let cwd = self.terminal_cwd();
        self.term_new_tab_in(cwd, None);
    }

    /// Open a new local terminal tab on this machine, even when viewing a
    /// remote computer session.
    pub fn term_new_local_tab(&mut self) {
        let cwd = self.local_terminal_cwd();
        self.term_new_tab_on(None, cwd, None);
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
            if let Some(comp) = self
                .term
                .session
                .as_ref()
                .and_then(|s| s.remote_computer.clone())
            {
                self.term.remote_tabs.insert(id, comp);
            }
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
        let comp = self.active_session_computer();
        let remote = Self::is_remote_computer_id(&comp).then_some(comp);
        self.term_new_tab_on(remote, cwd, command);
    }

    pub fn term_new_tab_on(
        &mut self,
        remote_computer: Option<String>,
        cwd: PathBuf,
        command: Option<String>,
    ) {
        // Select the existing surface before changing the active tab: focusing
        // it afterwards could restore its old tab over the newly created shell.
        if let Some(i) = self.tile_of_terminal() {
            self.focus_tile(i);
        }
        if self.term.tabs.is_empty() {
            self.ensure_term_tabs_seeded(false);
        } else if let Some(cur) = self.term.tabs.get_mut(self.term.active_tab) {
            cur.session = self.term.session.clone();
            cur.frame = self.term.frame.clone();
        }

        let id = self.term.next_tab_id;
        self.term.next_tab_id += 1;
        if let Some(comp) = &remote_computer {
            self.term.remote_tabs.insert(id, comp.clone());
        }
        self.term.tabs.push(TermTab {
            id,
            session: None,
            frame: None,
            cwd: cwd.clone(),
            title: None,
            pending_command: command,
        });
        self.term.active_tab = self.term.tabs.len() - 1;
        if self.in_split() && self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) {
            self.tiles[self.focus].term_tab = Some(self.term.active_tab);
        }
        self.term.session = None;
        self.term.frame = None;
        self.start_terminal_on(remote_computer, cwd);
        self.open_terminal_default();
    }

    /// Reveal a task shell in its existing surface without trading tabs between
    /// tiles or opening a second dock beside an already visible terminal.
    pub(crate) fn reveal_terminal_tab(&mut self, index: usize) {
        let owner = self
            .tiles
            .iter()
            .position(|tile| tile.is_terminal() && tile.term_tab == Some(index));
        if let Some(i) = owner.or_else(|| self.tile_of_terminal()) {
            self.focus_tile(i);
        }
        self.term_switch_tab(index);
        self.open_terminal_default();
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

    /// Remove empty terminal surfaces through the normal tile lifecycle so
    /// neighbouring sessions retain their state and the exit motion still runs.
    fn close_empty_terminal_surfaces(&mut self) {
        if !self.term.open {
            self.remove_dock_tab("terminal", false);
        }
        let terminal_tiles: Vec<_> = self
            .tiles
            .iter()
            .enumerate()
            .filter_map(|(i, tile)| tile.is_terminal().then_some(i))
            .collect();
        // close_tile hands focus to a neighbour before removing a pane. Mark
        // every dead terminal first so that handoff cannot start a fresh PTY
        // in another stale terminal tile while the last shell is being removed.
        for &i in &terminal_tiles {
            self.tiles[i].content = super::TileContent::Empty;
            self.tiles[i].term_tab = None;
        }
        for i in terminal_tiles.into_iter().rev() {
            self.close_tile(i);
        }
        self.close_terminal();
    }

    pub fn term_close_tab(&mut self, index: usize) {
        if index >= self.term.tabs.len() {
            if self.term.tabs.is_empty() {
                if let Some(session) = self.term.session.take() {
                    session.close_remote();
                    session.send(Msg::Shutdown);
                }
                self.term.frame = None;
                self.close_empty_terminal_surfaces();
            }
            return;
        }
        if self.term.tabs.len() <= 1 {
            if let Some(session) = self.term.session.take() {
                session.close_remote();
                session.send(Msg::Shutdown);
            }
            let drained = std::mem::take(&mut self.term.tabs);
            for tab in drained {
                self.cleanup_editor_tab_meta(tab.id);
                if let Some(s) = tab.session {
                    s.close_remote();
                    s.send(Msg::Shutdown);
                }
            }
            self.term.frame = None;
            self.close_empty_terminal_surfaces();
            return;
        }
        if index == self.term.active_tab {
            let tab = self.term.tabs.remove(index);
            self.cleanup_editor_tab_meta(tab.id);
            if let Some(s) = tab.session {
                s.close_remote();
                s.send(Msg::Shutdown);
            }
            if let Some(session) = &self.term.session {
                session.close_remote();
                session.send(Msg::Shutdown);
            }
            self.term.active_tab = self
                .term
                .active_tab
                .min(self.term.tabs.len().saturating_sub(1));
            if !self.term.tabs.is_empty() {
                self.term.session = self.term.tabs[self.term.active_tab].session.clone();
                self.term.frame = self.term.tabs[self.term.active_tab].frame.clone();
                if let Some(size) = self.term.size {
                    self.term.send(Msg::Resize(size));
                }
            } else {
                self.term.session = None;
                self.term.frame = None;
                self.close_empty_terminal_surfaces();
            }
        } else {
            let tab = self.term.tabs.remove(index);
            self.cleanup_editor_tab_meta(tab.id);
            if let Some(s) = tab.session {
                s.close_remote();
                s.send(Msg::Shutdown);
            }
            if index < self.term.active_tab {
                self.term.active_tab -= 1;
            }
        }
        self.rebind_tiles_after_close(index);
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
            self.close_tile(i);
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
        self.open_terminal_default();

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
        let (cwd, remote) = self
            .term
            .tabs
            .get(self.term.active_tab)
            .map(|t| (t.cwd.clone(), self.term.remote_tabs.get(&t.id).cloned()))
            .unwrap_or_else(|| {
                let comp = self.active_session_computer();
                (
                    self.terminal_cwd(),
                    Self::is_remote_computer_id(&comp).then_some(comp),
                )
            });
        self.start_terminal_on(remote, cwd);
        self.focus_terminal();
    }

    /// Where a new shell starts: the focused session's folder, else home.
    /// On a remote computer, preserve the remote session's folder verbatim
    /// without testing against the local filesystem.
    pub fn terminal_cwd(&self) -> PathBuf {
        let comp = self.active_session_computer();
        if Self::is_remote_computer_id(&comp) {
            return self
                .current()
                .and_then(|info| info.directory.clone())
                .filter(|d| !d.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"));
        }
        self.current()
            .and_then(|info| info.directory.clone())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// A guaranteed-local working directory for spawning a shell on this
    /// machine, never returning a remote session path.
    pub fn local_terminal_cwd(&self) -> PathBuf {
        if let Some(agent) = self.cfg.general.last_agent.as_deref() {
            if let Some(path) = self.cfg.general.last_cwd.get(agent).filter(|p| p.is_dir()) {
                return path.clone();
            }
        }
        let mut last_cwds: Vec<&PathBuf> = self
            .cfg
            .general
            .last_cwd
            .values()
            .filter(|p| p.is_dir())
            .collect();
        last_cwds.sort();
        if let Some(path) = last_cwds.first() {
            return (*path).clone();
        }
        if let Some(path) = self.cfg.general.default_cwd.as_ref().filter(|p| p.is_dir()) {
            return path.clone();
        }
        dirs::home_dir()
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok().filter(|p| p.is_dir()))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub fn term_active_title(&self) -> String {
        let base = if let Some(t) = self
            .term
            .frame
            .as_ref()
            .map(|f| f.title.clone())
            .filter(|t| !t.is_empty())
        {
            t
        } else {
            let cwd = self
                .term
                .session
                .as_ref()
                .map(|x| x.cwd.clone())
                .unwrap_or_else(|| self.terminal_cwd());
            short_path(&cwd)
        };
        let remote = self
            .term
            .session
            .as_ref()
            .and_then(|s| s.remote_computer.as_deref())
            .or_else(|| {
                self.term
                    .tabs
                    .get(self.term.active_tab)
                    .and_then(|t| self.term.remote_tabs.get(&t.id).map(String::as_str))
            });
        match remote {
            Some(comp) => format!("[{}] {base}", self.computer_name(comp)),
            None => base,
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
            Some(tab) => {
                let base = tab
                    .frame
                    .as_ref()
                    .map(|f| f.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| short_path(&tab.cwd));
                let remote = tab
                    .session
                    .as_ref()
                    .and_then(|s| s.remote_computer.as_deref())
                    .or_else(|| self.term.remote_tabs.get(&tab.id).map(String::as_str));
                match remote {
                    Some(comp) => format!("[{}] {base}", self.computer_name(comp)),
                    None => base,
                }
            }
            None => short_path(&self.terminal_cwd()),
        }
    }

    pub(crate) fn start_terminal(&mut self) {
        let comp = self.active_session_computer();
        if Self::is_remote_computer_id(&comp) {
            let cwd = self.terminal_cwd();
            self.start_terminal_on(Some(comp), cwd);
            return;
        }
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
                    let fallback_size = self.term.size.unwrap_or(Size {
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
                        // Replay daemon scrollback at the session's recorded dimensions
                        // (not a hardcoded 80x24 fallback) so wide TUI statuslines do not
                        // wrap into garbage before the viewport's real Resize arrives.
                        let size = Size {
                            cols: s_info.cols.max(2),
                            rows: s_info.rows.max(2),
                            cell_w: fallback_size.cell_w,
                            cell_h: fallback_size.cell_h,
                        };
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
        let comp = self.active_session_computer();
        let remote = Self::is_remote_computer_id(&comp).then_some(comp);
        self.start_terminal_on(remote, cwd);
    }

    fn start_terminal_on(&mut self, remote_computer: Option<String>, cwd: PathBuf) {
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
        let started = if let Some(comp) = remote_computer.clone() {
            match self.remote_gateway_for(&comp) {
                Some(gateway) => {
                    Session::start_remote(comp, gateway, cwd.clone(), size, colors, waker)
                }
                None => Err(std::io::Error::other(
                    "This remote computer is not paired or has no saved URL.",
                )),
            }
        } else {
            Session::start(cwd.clone(), size, colors, waker)
        };
        match started {
            Ok(session) => {
                self.term.session = Some(session.clone());
                if self.term.tabs.is_empty() {
                    let id = self.term.next_tab_id;
                    self.term.next_tab_id += 1;
                    if let Some(comp) = remote_computer {
                        self.term.remote_tabs.insert(id, comp);
                    }
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
                    if let Some(comp) = remote_computer {
                        self.term.remote_tabs.insert(active.id, comp);
                    }
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
        // Global Leader / Which-Key stays open until Esc or a chord; it does
        // not share the short Herdr prefix timeout.
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
            self.term.frame = Some(frame.clone());
            if let Some(active) = self.term.tabs.get_mut(self.term.active_tab) {
                active.accept_frame(frame);
            }
            changed = true;
        }
        let shown: Vec<usize> = (0..self.tiles.len())
            .filter_map(|i| self.tile_terminal_tab(i))
            .collect();
        for (i, tab) in self.term.tabs.iter_mut().enumerate() {
            if i != self.term.active_tab {
                if let Some(frame) = tab.session.as_ref().and_then(Session::take_frame) {
                    tab.accept_frame(frame);
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
        let completed = |session: &Session| {
            session
                .ended()
                .is_some_and(|ended| ended_well(ended, session.started.elapsed()))
        };
        let ended = if self.term.session.as_ref().is_some_and(completed) {
            Some(self.term.active_tab)
        } else {
            self.term.tabs.iter().enumerate().find_map(|(i, tab)| {
                (i != self.term.active_tab && tab.session.as_ref().is_some_and(completed))
                    .then_some(i)
            })
        };
        if let Some(index) = ended {
            self.term_close_tab(index);
            true
        } else {
            false
        }
    }

    /// A key while the terminal has the keyboard (the sink's key handler).
    pub(crate) fn terminal_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let mac = crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;

        // 1. Global Leader key / Which-Key & shortcut recording interception:
        if super::keymap::intercept_global_key(self, ev, tx) {
            return true;
        }

        use super::keymap::{bind_matches_cfg, entry, run, Action};
        let matches = |action| bind_matches_cfg(entry(action), &self.cfg.keymap, ev, tx);
        let action = [
            Action::ToggleFocus,
            Action::ToggleTerminal,
            Action::NewShell,
            Action::TermNewTab,
            Action::TermCloseTab,
            Action::TermSearch,
            Action::TermSplit,
            Action::TermCycleTab,
            Action::TermSwitchTab,
            Action::TermCopy,
            Action::TermPaste,
        ]
        .into_iter()
        .find(|action| matches(*action));
        if let Some(action) = action {
            match action {
                Action::ToggleFocus => self.toggle_focus_tx(tx),
                Action::ToggleTerminal => self.toggle_terminal_tx(tx),
                Action::NewShell => {
                    self.open_terminal_default();
                    self.restart_terminal();
                    self.focus_terminal_tx(tx);
                }
                Action::TermNewTab => self.term_new_tab(),
                Action::TermCloseTab => self.term_close_tab(self.term.active_tab),
                Action::TermSearch => self.open_terminal_search(),
                Action::TermSplit => {
                    if self.in_split() {
                        self.split_terminal_tile();
                    } else {
                        self.open_terminal_tile();
                    }
                    self.focus_terminal_tx(tx);
                }
                Action::TermCycleTab => {
                    let previous = ev.mods.shift
                        || matches!(
                            ev.key,
                            rune::Key::Left | rune::Key::PageUp | rune::Key::Command('[')
                        )
                        || tx.physical_key() == Some(rune::PhysicalKeyCode::BracketLeft);
                    if previous {
                        self.term_prev_tab();
                    } else {
                        self.term_next_tab();
                    }
                }
                Action::TermSwitchTab => {
                    let digit = match &ev.key {
                        rune::Key::Command(c) => c.to_digit(10),
                        rune::Key::Text(t) => t.chars().next().and_then(|c| c.to_digit(10)),
                        _ => None,
                    };
                    if let Some(d @ 1..=9) = digit {
                        let target = if d == 9 {
                            self.term.tabs.len().saturating_sub(1)
                        } else {
                            d as usize - 1
                        };
                        if target < self.term.tabs.len() {
                            self.term_switch_tab(target);
                        }
                    }
                }
                Action::TermCopy => {
                    if let Some(text) = self.term.frame.as_ref().and_then(|f| f.selection.clone()) {
                        tx.copy(text);
                    }
                }
                Action::TermPaste => tx.request_paste(),
                _ => unreachable!(),
            }
            return true;
        }
        if self.in_split() {
            for action in [
                Action::CycleTileLayout,
                Action::ToggleTileZoom,
                Action::ToggleTilePop,
                Action::EqualizeTiles,
                Action::SplitHorizontal,
                Action::SplitVertical,
                Action::FocusPaneToward,
                Action::SwapPaneToward,
                Action::FocusPane,
            ] {
                if bind_matches_cfg(entry(action), &self.cfg.keymap, ev, tx) {
                    return run(self, action, ev, tx);
                }
            }
        }

        if self.term.search.open && ev.key == rune::Key::Escape {
            self.close_terminal_search(tx);
            return true;
        }

        // Legacy routing remains for shell/application ownership, but customized
        // actions must not also fire through their old clipboard/toggle aliases.
        let route = match input::route(ev, tx.physical_key(), mac) {
            Route::Copy if entry(Action::TermCopy).is_overridden(&self.cfg.keymap) => {
                Route::Terminal
            }
            Route::Paste if entry(Action::TermPaste).is_overridden(&self.cfg.keymap) => {
                Route::Terminal
            }
            Route::Toggle if entry(Action::ToggleTerminal).is_overridden(&self.cfg.keymap) => {
                Route::Terminal
            }
            other => other,
        };
        match route {
            Route::Toggle => self.toggle_terminal_tx(tx),
            Route::Copy => {
                if let Some(text) = self.term.frame.as_ref().and_then(|f| f.selection.clone()) {
                    tx.copy(text);
                }
            }
            Route::Paste => tx.request_paste(),
            Route::App => {
                // Ctrl+Shift+K reaches Ctrl+K's shortcut when the app has
                // no Ctrl+Shift+K of its own.
                if !super::keymap::dispatch_window(self, ev, tx) {
                    let mut plain = ev.clone();
                    plain.mods.shift = false;
                    super::keymap::dispatch_window(self, &plain, tx);
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
        self.term.sink.focused = false;
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
        self.term.search.query.focused = false;
        self.term.search.matches.clear();
        self.term.search.active_idx = 0;
        if self.term.open || self.has_terminal_tile() {
            self.focus_terminal();
        }
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

fn tab_remote_computer(s: &UiState, index: usize) -> Option<&str> {
    let from_tab = s.term.tabs.get(index).and_then(|tab| {
        tab.session
            .as_ref()
            .and_then(|sess| sess.remote_computer.as_deref())
            .or_else(|| s.term.remote_tabs.get(&tab.id).map(String::as_str))
    });
    from_tab.or_else(|| {
        (index == s.term.active_tab)
            .then(|| {
                s.term
                    .session
                    .as_ref()
                    .and_then(|sess| sess.remote_computer.as_deref())
            })
            .flatten()
    })
}

/// The panel.
fn shell_tab(
    k: &Kit,
    key: String,
    title: String,
    remote: Option<(String, Color)>,
    active: bool,
    number: usize,
) -> El {
    let base_caption = if title.starts_with('/') || title.starts_with("~/") {
        std::path::Path::new(&title)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or(&title)
            .to_string()
    } else {
        title.clone()
    };
    let (caption, tooltip, dot) = match remote {
        Some((host, dot_color)) => (
            format!("{host} · {base_caption}"),
            format!("[{host}] {title}"),
            Some(dot_color),
        ),
        None => (base_caption, title, None),
    };
    let ink = if active {
        k.c.text_base
    } else {
        k.c.text_muted
    };
    let mut pill = row()
        .key(key)
        .h(32.0)
        .min_w(76.0)
        .max_w(if dot.is_some() { 210.0 } else { 190.0 })
        .shrink()
        .pl(10.0)
        .pr(4.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.lg)
        .fill(if active {
            k.c.surface_raised.mix(k.c.bg_accent, 0.14)
        } else {
            k.c.bg_base.alpha(0.0)
        })
        .motion(crate::theme::look::MOVE)
        .role("tab")
        .label(tooltip.clone())
        .help(tooltip)
        .cursor(rune::Cursor::Pointer)
        .preserve_focus_on_press(true)
        .child(
            text(format!("{:02}", number + 1))
                .font(k.ty.small)
                .color(if active {
                    k.c.text_accent
                } else {
                    k.c.text_faint
                }),
        );
    if let Some(dot_color) = dot {
        pill = pill.child(el().w(6.0).h(6.0).radius(3.0).fill(dot_color));
    }
    k.interactive(
        pill.child(
            text(caption)
                .font(k.ty.small_medium)
                .color(ink)
                .truncate()
                .shrink()
                .min_w(0.0),
        ),
    )
}

pub(crate) fn shell_tabs(s: &UiState, k: &Kit, in_dock: bool) -> El {
    let language = s.cfg.general.language;
    let frame = s.term.frame.as_ref();
    let tab_count = s.term.tabs.len().max(1);
    let mut tabs_strip = row().key("terminal-tabs-strip").gap(4.0).center_y();

    for i in 0..tab_count {
        let is_active = i == s.term.active_tab && (!in_dock || s.term.open);
        let tab_title = if i == s.term.active_tab {
            frame
                .map(|f| f.title.clone())
                .filter(|t| !t.is_empty())
                .or_else(|| s.term.tabs.get(i).and_then(|tab| tab.title.clone()))
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
                .or_else(|| tab.title.clone())
                .unwrap_or_else(|| short_path(&tab.cwd))
        } else {
            format!("Tab {}", i + 1)
        };

        let remote =
            tab_remote_computer(s, i).map(|comp| (s.computer_name(comp), s.computer_color(comp)));

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

        let mut tab_pill = shell_tab(
            k,
            format!("terminal-tab-{i}"),
            tab_title,
            remote,
            is_active,
            i,
        )
        .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
            if in_dock && !s.term.open {
                s.open_terminal();
            }
            s.term_switch_tab(i);
            s.focus_terminal_tx(tx);
        });

        if let Some(btn) = close_tab {
            tab_pill = tab_pill.child(btn);
        }

        tabs_strip = tabs_strip.child(tab_pill);
    }

    let new_tab_label = tr_ref(language, "New terminal tab");
    tabs_strip = tabs_strip.child(
        IconButton::new(&super::icons::PLUS)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(new_tab_label)
            .help(super::keymap::tip(
                new_tab_label,
                super::keymap::Action::TermNewTab,
            ))
            .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                if in_dock && !s.term.open {
                    s.open_terminal();
                }
                s.term_new_tab();
                s.focus_terminal_tx(tx);
            })
            .build(k)
            .key("terminal-tab-new")
            .preserve_focus_on_press(true),
    );

    if UiState::is_remote_computer_id(&s.active_session_computer()) {
        tabs_strip = tabs_strip.child(
            Button::new(tr_ref(language, "+ Local"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .help(tr_ref(language, "New local terminal"))
                .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                    if in_dock && !s.term.open {
                        s.open_terminal();
                    }
                    s.term_new_local_tab();
                    s.focus_terminal_tx(tx);
                })
                .build(k)
                .key("terminal-tab-new-local")
                .preserve_focus_on_press(true),
        );
    }

    tabs_strip
}

pub fn view(
    s: &UiState,
    k: &Kit,
    scheme: ColorScheme,
    scale: f32,
    layer: &mut Vec<El>,
    controls: El,
) -> El {
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

    let mut header = row()
        .key("terminal-head")
        .w_full()
        .h(HEAD)
        .pl(8.0)
        .pr(8.0)
        .gap(6.0)
        .center_y()
        .fill(k.alpha(k.c.layer_01, 0.65))
        .preserve_focus_on_press(true)
        .child(crate::plugin::window::dock_tabs(s, k).unwrap_or_else(|| {
            el().key("shell-tabs-viewport")
                .scroll_x()
                .h(34.0)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .child(shell_tabs(s, k, false))
        }))
        .child(crate::plugin::window::dock_actions(s, k, true, layer));
    if s.term.is_prefix_armed() {
        header = header.child(prefix_badge(k, ""));
    }
    let header = header.child(controls);

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
    pub(super) fn focus_terminal_tile_tx(&mut self, tile_idx: Option<usize>, tx: &mut Tx) {
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
    tile_body_with_controls(s, k, scheme, scale, layer, tile_idx, None)
}

/// Compact shell tab pills rendered directly inside the unified `tiles::header()`
/// leading drag area so Terminal tiles/floating windows have a single full-width
/// draggable header with identical right-side window chrome.
pub fn tile_header_tabs(s: &UiState, k: &Kit, tile_idx: Option<usize>) -> El {
    let language = s.cfg.general.language;
    let live = tile_idx.is_none_or(|i| s.tile_of_terminal() == Some(i));
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
    let tab_count = s.term.tabs.len().max(1);
    let mut tabs_strip = row()
        .key(format!("tile-terminal-tabs-strip{sfx}"))
        .gap(4.0)
        .center_y()
        .shrink()
        .min_w(0.0);

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

        let remote =
            tab_remote_computer(s, i).map(|comp| (s.computer_name(comp), s.computer_color(comp)));

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

        let mut tab_pill = shell_tab(
            k,
            format!("tile-terminal-tab{sfx}-{i}"),
            tab_title,
            remote,
            is_active,
            i,
        )
        .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
            s.term_switch_tab_for_tile(tile_idx, i);
            s.focus_terminal_tile_tx(tile_idx, tx);
        });

        if let Some(btn) = close_tab {
            tab_pill = tab_pill.child(btn);
        }

        tabs_strip = tabs_strip.child(tab_pill);
    }

    let new_tab_label = tr_ref(language, "New terminal tab");
    tabs_strip = tabs_strip.child(
        IconButton::new(&super::icons::PLUS)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(new_tab_label)
            .help(super::keymap::tip(
                new_tab_label,
                super::keymap::Action::TermNewTab,
            ))
            .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                s.focus_terminal_tile_tx(tile_idx, tx);
                s.term_new_tab();
                s.focus_terminal_tx(tx);
            })
            .build(k)
            .key(format!("tile-terminal-tab-new{sfx}"))
            .preserve_focus_on_press(true),
    );

    if UiState::is_remote_computer_id(&s.active_session_computer()) {
        tabs_strip = tabs_strip.child(
            Button::new(tr_ref(language, "+ Local"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .help(tr_ref(language, "New local terminal"))
                .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.focus_terminal_tile_tx(tile_idx, tx);
                    s.term_new_local_tab();
                    s.focus_terminal_tx(tx);
                })
                .build(k)
                .key(format!("tile-terminal-tab-new-local{sfx}"))
                .preserve_focus_on_press(true),
        );
    }

    if live && s.term.is_prefix_armed() {
        tabs_strip = tabs_strip.child(prefix_badge(k, &sfx));
    }

    tabs_strip
}

/// Terminal body without its own header bar (used inside `tiles::tile`, where the
/// unified `tiles::header()` owns dragging, tabs, actions, surface switcher, and close).
pub fn tile_body_only(
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
        .child(body)
}

pub fn tile_body_with_controls(
    s: &UiState,
    k: &Kit,
    scheme: ColorScheme,
    scale: f32,
    layer: &mut Vec<El>,
    tile_idx: Option<usize>,
    controls: Option<El>,
) -> El {
    let first = tile_idx.is_none_or(|i| !s.tiles.iter().take(i).any(|t| t.is_terminal()));
    let sfx = match tile_idx {
        Some(i) if !first => format!("-{i}"),
        _ => String::new(),
    };
    let tabs_strip = tile_header_tabs(s, k, tile_idx);
    let subheader = row()
        .key(format!("tile-terminal-subhead{sfx}"))
        .w_full()
        .h(HEAD)
        .pl(8.0)
        .pr(8.0)
        .gap(6.0)
        .center_y()
        .fill(k.alpha(k.c.layer_01, 0.45))
        .preserve_focus_on_press(true)
        .child(
            el().key("shell-tabs-viewport")
                .scroll_x()
                .h(34.0)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .child(tabs_strip),
        )
        .children(controls);

    col()
        .key(format!("tile-terminal-page{sfx}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .preserve_focus_on_press(true)
        .child(subheader)
        .child(el().w_full().h(1.0).fill(k.alpha(k.c.border_weak, 0.25)))
        .child(tile_body_only(s, k, scheme, scale, layer, tile_idx))
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

    let prev_btn = super::art::direction_button(
        s,
        k,
        tr_ref(language, "Previous match"),
        -std::f32::consts::FRAC_PI_2,
    )
    .variant(rune_kit::ButtonVariant::GhostMuted)
    .control_size(ControlSize::Small)
    .on_press(|s: &mut UiState| s.term_search_prev())
    .build(k)
    .key("term-search-prev");

    let next_btn = super::art::direction_button(
        s,
        k,
        tr_ref(language, "Next match"),
        std::f32::consts::FRAC_PI_2,
    )
    .variant(rune_kit::ButtonVariant::GhostMuted)
    .control_size(ControlSize::Small)
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
        .radius(k.radius.lg)
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
    let is_remote = UiState::is_remote_computer_id(&s.active_session_computer());
    let mut menu = rune_kit::context_menu("terminal-menu", grid, s.term.menu_at)
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
                .shortcut(label(Action::TermSearch))
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
            MenuItem::new(tr_ref(language, "New terminal tab"))
                .shortcut(label(Action::TermNewTab))
                .on_select_tx(|s: &mut UiState, tx: &mut Tx| {
                    s.term.menu_at = None;
                    s.term_new_tab();
                    s.focus_terminal_tx(tx);
                }),
        );
    if is_remote {
        menu = menu.item(
            MenuItem::new(tr_ref(language, "New local terminal")).on_select_tx(
                |s: &mut UiState, tx: &mut Tx| {
                    s.term.menu_at = None;
                    s.term_new_local_tab();
                    s.focus_terminal_tx(tx);
                },
            ),
        );
    }
    menu.item(
        MenuItem::new(tr_ref(language, "New shell"))
            .shortcut(label(Action::NewShell))
            .on_select(|s: &mut UiState| {
                s.term.menu_at = None;
                s.restart_terminal();
            }),
    )
    .item(
        MenuItem::new(tr_ref(language, "Close tab"))
            .shortcut(label(Action::TermCloseTab))
            .on_select(|s: &mut UiState| {
                s.term.menu_at = None;
                s.term_close_tab(s.term.active_tab);
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

fn prefix_badge(k: &Kit, sfx: &str) -> El {
    let leader = super::keymap::leader_label(None);
    row()
        .key(format!("terminal-prefix-badge{sfx}"))
        .h(20.0)
        .px(7.0)
        .center()
        .radius(k.radius.xs)
        .fill(k.c.bg_accent.alpha(0.18))
        .border(1.0, k.c.bg_accent.alpha(0.45))
        .child(
            text(format!("LEADER ({leader})"))
                .font(k.ty.small_medium)
                .color(k.c.text_accent),
        )
}

fn set_leaf_parent_across(node: &mut crate::config::TileNode, target: usize, across: bool) {
    if let crate::config::TileNode::Split {
        across: a_dir,
        a,
        b,
        ..
    } = node
    {
        let is_direct_child = matches!(**a, crate::config::TileNode::Leaf(i) if i == target)
            || matches!(**b, crate::config::TileNode::Leaf(i) if i == target);
        if is_direct_child {
            *a_dir = across;
            return;
        }
        set_leaf_parent_across(a, target, across);
        set_leaf_parent_across(b, target, across);
    }
}

fn toggle_leaf_parent_zoom(node: &mut crate::config::TileNode, target: usize) {
    if let crate::config::TileNode::Split { ratio, a, b, .. } = node {
        let in_a = matches!(**a, crate::config::TileNode::Leaf(i) if i == target);
        let in_b = matches!(**b, crate::config::TileNode::Leaf(i) if i == target);
        if in_a || in_b {
            let zoomed = if in_a { 0.82 } else { 0.18 };
            *ratio = if (*ratio - 0.5).abs() > 0.05 {
                0.5
            } else {
                zoomed
            };
            return;
        }
        toggle_leaf_parent_zoom(a, target);
        toggle_leaf_parent_zoom(b, target);
    }
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
                        .radius((k.radius.xs * 0.5).min(3.0))
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
                .radius((k.radius.xs * 0.5).min(1.5))
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

pub(crate) fn shell_quote_path(path: &std::path::Path) -> String {
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
                cmd: ctrl && !crate::platform::is_macos(),
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
        let mut home = key(Key::Command('h'), !crate::platform::is_macos(), true);
        home.mods.cmd = true;
        home.mods.logo = crate::platform::is_macos();
        rt.key(home);
        rt.frame(1.0 / 60.0);
        let s = &rt.state;
        assert_eq!(s.mode, super::super::Mode::Home);
        assert!(s.palette.is_none(), "a command leaves the palette");
    }

    #[cfg(unix)]
    #[test]
    fn switching_before_the_first_frame_delivers_a_command_only_to_its_new_tab() {
        let tmp = tempfile::tempdir().unwrap();
        let make_shell = |name: &str| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args([
                "-c",
                r#"printf ready; while IFS= read -r line; do printf '%s\n' "$line" >> "$1"; done"#,
                "shell",
            ])
            .arg(tmp.path().join(name));
            Session::start_command(
                cmd,
                grid_size(560.0, 340.0, Metrics { cw: 8.0, ch: 17.0 }, 1.0),
                colors(&Kit::neutral(false), ColorScheme::Light),
                std::sync::Arc::new(|| {}),
            )
            .unwrap()
        };
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let old = make_shell("old");
        state.term.session = Some(old.clone());
        state.term_new_tab_in(tmp.path().into(), Some("manage-fixture\n".into()));
        let new = make_shell("new");
        state.term.session = Some(new.clone());
        state.term_switch_tab(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.term.frame.is_none() || !tmp.path().join("new").exists() {
            state.poll_terminal(0.1, &mut rune::Tx::default());
            assert!(
                std::time::Instant::now() < deadline,
                "new tab never received its command"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        old.send(Msg::Text("sentinel\n".into()));
        while !std::fs::read_to_string(tmp.path().join("old"))
            .unwrap_or_default()
            .contains("sentinel")
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("old")).unwrap(),
            "sentinel\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new")).unwrap(),
            "manage-fixture\n"
        );
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
        let pending = state.term.tabs[state.term.active_tab]
            .pending_command
            .as_deref()
            .unwrap();
        assert!(
            pending.starts_with("nvim --listen '") && pending.ends_with(" '/tmp/test_file.rs'\n"),
            "{pending}"
        );
    }

    #[test]
    fn open_file_in_terminal_reuses_single_editor_tab_for_same_workspace() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file_a = repo.join("src/config.rs");
        let file_b = repo.join("src/ui/tiles.rs");

        state.open_file_in_terminal(&file_a, "nvim");
        assert_eq!(state.term.tabs.len(), 1);
        state.open_file_in_terminal(&file_b, "nvim");
        assert_eq!(
            state.term.tabs.len(),
            1,
            "second file in the same workspace must reuse the single editor tab"
        );
        let pending = state.term.tabs[0].pending_command.as_deref().unwrap();
        assert!(pending.contains(&file_a.to_string_lossy().to_string()));
        assert!(pending.contains(&file_b.to_string_lossy().to_string()));
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
        let pending = state.term.tabs[state.term.active_tab]
            .pending_command
            .as_deref()
            .unwrap();
        assert!(
            pending.starts_with("nvim --listen '")
                && pending.ends_with(" '/tmp/another_file.rs'\n"),
            "{pending}"
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

    #[cfg(unix)]
    #[test]
    fn background_shell_exit_removes_its_tile_without_stealing_focus() {
        let (tx, _) = commands::channel();
        let mut state = UiState::new(tx);
        state.mode = super::super::Mode::Session;
        state.selected = Some("conversation".into());
        state.draft = rune::TextEdit::new("keep this draft");
        state.draft.focused = true;
        state.tiles = vec![
            super::super::Tile::session("conversation"),
            super::super::Tile::empty(),
            super::super::Tile::empty(),
        ];
        state.tiles[1].set_terminal_tab(0);
        state.tiles[2].set_terminal_tab(1);
        state.focus = 0;
        let make = |script: &str| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", script]);
            Session::start_command(
                cmd,
                grid_size(560.0, 340.0, Metrics { cw: 8.0, ch: 17.0 }, 1.0),
                colors(&Kit::neutral(false), ColorScheme::Light),
                std::sync::Arc::new(|| {}),
            )
            .unwrap()
        };
        let ended = make("exit 0");
        let live = make("read line");
        for (id, session) in [(1, ended.clone()), (2, live.clone())] {
            state.term.tabs.push(TermTab {
                id,
                session: Some(session),
                frame: None,
                cwd: PathBuf::from("/tmp"),
                title: None,
                pending_command: None,
            });
        }
        state.term.active_tab = 1;
        state.term.session = Some(live.clone());
        let wait = |session: &Session| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while session.ended().is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "isolated shell did not exit"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait(&ended);
        assert!(state.settle_terminal());
        assert_eq!(state.term.tabs.len(), 1);
        assert_eq!(state.term.tabs[0].id, 2);
        assert_eq!(state.tiles.len(), 2);
        assert_eq!(state.focus, 0);
        assert_eq!(state.selected.as_deref(), Some("conversation"));
        assert!(state.draft.focused && !state.term.sink.focused);
        assert_eq!(state.draft.text, "keep this draft");
        assert!(live.ended().is_none());
        // The remaining shell exits while its terminal is a tile, not a dock.
        live.send(Msg::Text("done\n".into()));
        wait(&live);
        assert!(state.settle_terminal());
        assert!(!state.has_terminal_tile());
        assert!(!state.term.open);
        assert!(state.term.tabs.is_empty());
        assert_eq!(state.selected.as_deref(), Some("conversation"));
    }

    #[test]
    fn closing_last_shell_clears_all_stale_terminal_tiles() {
        let (tx, _) = commands::channel();
        let mut state = UiState::new(tx);
        state.tiles = vec![super::super::Tile::empty(), super::super::Tile::empty()];
        state.tiles[0].set_terminal_tab(0);
        state.tiles[1].set_terminal_tab(0);
        state.mode = super::super::Mode::Session;
        state.term.sink.focused = true;
        state.term_close_tab(0);
        assert!(state.tiles.is_empty());
        assert!(!state.term.open && !state.term.sink.focused);
        assert!(state.term.session.is_none() && state.term.tabs.is_empty());
        assert!(!state.term.mounted());
    }

    #[test]
    fn terminal_preference_applies_on_open_without_relocating_existing_surfaces() {
        use crate::config::{ConfigEdit, TerminalOpenMode};
        for mode in [
            TerminalOpenMode::Docked,
            TerminalOpenMode::Split,
            TerminalOpenMode::Floating,
        ] {
            let (tx, _) = commands::channel();
            let mut state = UiState::new(tx);
            // Old configurations without the new field retain docking.
            state.cfg = serde_json::from_str("{}").unwrap();
            state.toggle_terminal();
            assert!(state.term.open && !state.has_terminal_tile());
            state.close_terminal();
            state.cfg.apply(ConfigEdit::TerminalOpenMode(mode));
            let stored = serde_json::to_string(&state.cfg).unwrap();
            state.cfg = serde_json::from_str(&stored).unwrap();
            state.toggle_terminal();
            match mode {
                TerminalOpenMode::Docked => assert!(state.term.open && !state.has_terminal_tile()),
                TerminalOpenMode::Split | TerminalOpenMode::Floating => {
                    assert!(!state.term.open && state.in_split());
                    let tile = &state.tiles[state.tile_of_terminal().unwrap()];
                    assert_eq!(tile.popped, mode == TerminalOpenMode::Floating);
                }
            }
            let tiles = state.tiles.len();
            let popped = state.tiles.iter().filter(|t| t.popped).count();
            let open = state.term.open;
            state
                .cfg
                .apply(ConfigEdit::TerminalOpenMode(TerminalOpenMode::Floating));
            state.open_terminal_default();
            assert_eq!(state.tiles.len(), tiles);
            assert_eq!(state.tiles.iter().filter(|t| t.popped).count(), popped);
            assert_eq!(state.term.open, open);
        }
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
                cmd: !crate::platform::is_macos(),
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
                cmd: !crate::platform::is_macos(),
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
                cmd: !crate::platform::is_macos(),
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
                cmd: !crate::platform::is_macos(),
                alt: false,
                ctrl: true,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 2);
        // A custom binding replaces the old aliases immediately.
        rt.state.edit(crate::config::ConfigEdit::SetKeybind {
            action: "term_cycle_tab".into(),
            spec: Some("ctrl+shift+q".into()),
        });
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Mods {
                ctrl: true,
                ..Default::default()
            },
        });
        rt.frame(0.2);
        assert_eq!(rt.state.term.active_tab, 2);
        rt.key(KeyEvent {
            key: Key::Command('q'),
            mods: Mods {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        });
        rt.frame(0.3);
        assert_eq!(rt.state.term.active_tab, 1);
    }

    #[test]
    fn fallback_terminal_colors_maintain_high_contrast_in_light_and_dark_modes() {
        for dark in [false, true] {
            let kit = Kit::neutral(dark);
            let c = fallback_colors(&kit);
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
    fn ctrl_c_in_session_mode_does_not_interrupt_terminal() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.mode = super::super::Mode::Session;
        state.open_terminal();
        assert!(state.term.open);
        // User moves focus to session / timeline (blurring the terminal sink).
        state.term.sink.focused = false;

        let mut rt = Runtime::new(
            state,
            super::super::view,
            (1440.0, 900.0),
            Env::new(true, 1.0),
        );
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);

        // Press Ctrl+C in session
        rt.key(key(Key::Command('c'), true, false));
        rt.frame(2.0 / 60.0);

        // Terminal must NOT have received Ctrl+C
        assert!(
            !rt.state
                .term
                .sent_log
                .borrow()
                .iter()
                .any(|k| k.contains("ctrl=true")),
            "terminal must not receive Ctrl+C when session is active"
        );
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

    #[test]
    fn herdr_ctrl_space_prefix_state_machine_and_focus_toggle() {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.mode = super::super::Mode::Session;
        s.open_terminal();

        let mut rt = Runtime::new(s, super::super::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);

        // Click inside the terminal grid so `terminal-keys` holds Runtime focus.
        rt.pointer_down(1200.0, 300.0);
        rt.pointer_up(1200.0, 300.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.sink.focused);
        assert!(!rt.state.draft.focused);

        rt.state.term_new_tab_in(PathBuf::from("/tmp/tab1"), None);
        assert_eq!(rt.state.term.tabs.len(), 1);
        assert_eq!(rt.state.term.active_tab, 0);

        let ctrl_space = KeyEvent {
            key: Key::Text(" ".into()),
            mods: Mods {
                shift: false,
                cmd: true,
                alt: false,
                ctrl: true,
                logo: false,
            },
        };
        let plain = |c: char| KeyEvent {
            key: Key::Text(c.to_string()),
            mods: Mods::default(),
        };

        // 1. `Ctrl+Space` then `c` creates a new terminal tab.
        rt.key(ctrl_space.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.is_prefix_armed());
        rt.key(plain('c'));
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.term.is_prefix_armed());
        assert_eq!(rt.state.term.tabs.len(), 2);
        assert_eq!(rt.state.term.active_tab, 1);

        // 2. `Ctrl+Space` then `1` / `2` switches tabs.
        rt.key(ctrl_space.clone());
        rt.key(plain('1'));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 0);

        rt.key(ctrl_space.clone());
        rt.key(plain('2'));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.term.active_tab, 1);

        // 3. `Ctrl+Space` then `[` opens terminal scrollback search.
        assert!(!rt.state.term.search.open);
        rt.key(ctrl_space.clone());
        rt.key(plain('['));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.search.open);
        rt.state.close_terminal_search_no_tx();
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.term.search.open);

        // 4. `Ctrl+Space` twice sends literal `Ctrl+Space` (`\x00`) to the PTY shell.
        rt.state.term.sent_log.borrow_mut().clear();
        rt.key(ctrl_space.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.is_prefix_armed());
        rt.key(ctrl_space);
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.term.is_prefix_armed());
        assert_eq!(*rt.state.term.sent_log.borrow(), ["text \0"]);

        // 5. `Ctrl+;` toggles focus between terminal and session composer without closing terminal.
        rt.state.mode = super::super::Mode::Session;
        let ctrl_semi = KeyEvent {
            key: Key::Command(';'),
            mods: Mods {
                shift: false,
                cmd: true,
                alt: false,
                ctrl: !crate::platform::is_macos(),
                logo: crate::platform::is_macos(),
            },
        };
        assert!(rt.state.term.open && rt.state.term.sink.focused);
        rt.key(ctrl_semi.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.open);
        assert!(!rt.state.term.sink.focused);
        assert!(rt.state.draft.focused);

        rt.key(ctrl_semi);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.open);
        assert!(rt.state.term.sink.focused);
        assert!(!rt.state.draft.focused);
    }

    #[test]
    fn term_new_local_tab_creates_local_tab_alongside_remote_tab_when_on_remote_computer() {
        let (tx, _rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.cfg.current_computer = "mini".into();
        state.cfg.general.last_agent = Some("codex".into());
        let local_dir = std::env::temp_dir();
        state
            .cfg
            .general
            .last_cwd
            .insert("codex".into(), local_dir.clone());
        state.sessions.push(
            serde_json::from_value(serde_json::json!({
                "asid": "remote-s1",
                "agent_id": "codex",
                "title": "Remote work",
                "status": "idle",
                "directory": "/Users/mini/nonexistent-remote-project",
                "updated_ms": 1
            }))
            .unwrap(),
        );
        state.selected = Some("remote-s1".into());
        assert_eq!(state.active_session_computer(), "mini");

        // 1. Default new tab on a remote computer creates a remote tab.
        state.term_new_tab();
        assert_eq!(state.term.tabs.len(), 1);
        let remote_tab_id = state.term.tabs[0].id;
        assert_eq!(
            state
                .term
                .remote_tabs
                .get(&remote_tab_id)
                .map(String::as_str),
            Some("mini")
        );
        assert_eq!(
            state.term.tabs[0].cwd,
            PathBuf::from("/Users/mini/nonexistent-remote-project")
        );
        assert_eq!(tab_remote_computer(&state, 0), Some("mini"));

        // 2. term_new_local_tab creates a guaranteed-local tab coexisting cleanly with the remote tab.
        state.term_new_local_tab();
        assert_eq!(state.term.tabs.len(), 2);
        assert_eq!(state.term.active_tab, 1);
        let local_tab_id = state.term.tabs[1].id;
        assert!(!state.term.remote_tabs.contains_key(&local_tab_id));
        assert_eq!(state.term.tabs[1].cwd, local_dir);
        assert!(state.term.tabs[1].cwd.is_dir());
        assert_eq!(tab_remote_computer(&state, 0), Some("mini"));
        assert_eq!(tab_remote_computer(&state, 1), None);
    }
}
