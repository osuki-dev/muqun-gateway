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
use rune_kit::{Badge, BadgeVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use super::copy::tr_ref;
use super::UiState;
use crate::term::engine::{Colors, MouseInput, MousePhase, Size};
use crate::term::grid::{CursorShape, Frame, Rgb, Row};
use crate::term::input::{self, Mods, Route};
use crate::term::session::{Msg, Session};
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

/// The grid's type: the mono slot (JetBrains Mono where installed).
const FONT_SIZE: f32 = 13.0;
const LINE: f32 = 17.0;
/// Inside the grid's card, around the cells.
const INSET: f32 = 8.0;

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
    pub session: Option<Session>,
    pub frame: Option<Frame>,
    /// The keyboard sink on the cursor cell (see the module docs).
    pub sink: TextEdit,
    pub waker: Option<rune::Waker>,
    /// Screenshot scenes: what a program would print at a size, replayed
    /// into a fresh emulator instead of a live shell.
    pub fixture: Option<fn(u16, u16) -> Vec<u8>>,
    pub error: Option<String>,
    /// The grid's size the last layout gave, in cells.
    size: Option<Size>,
    /// The theme's colours as the last view resolved them; [`poll`] hands
    /// them to the emulator when they changed.
    pub want: Cell<Option<Colors>>,
    sent: Option<Colors>,
    wheel: f32,
    /// What the panel sent the shell, for the routing test.
    #[cfg(test)]
    pub sent_log: std::cell::RefCell<Vec<String>>,
}

impl TermPanel {
    pub fn new(width: f32) -> Self {
        Self {
            open: false,
            pane: PaneState::new(560.0)
                .range(360.0, 760.0)
                .with_width(width)
                .collapsed(),
            session: None,
            frame: None,
            sink: TextEdit::new(""),
            waker: None,
            fixture: None,
            error: None,
            size: None,
            want: Cell::new(None),
            sent: None,
            wheel: 0.0,
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
        self.term.open = false;
        self.term.pane.set_open(false);
        self.term.sink.focused = false;
        if self.mode == super::Mode::Session {
            self.draft.focused = true;
        }
    }

    /// End the shell and start a new one in the focused session's folder.
    pub fn restart_terminal(&mut self) {
        self.term.session = None;
        self.term.frame = None;
        self.term.error = None;
        self.start_terminal();
    }

    /// Where a new shell starts: the focused session's folder, else home.
    fn terminal_cwd(&self) -> PathBuf {
        self.current()
            .and_then(|info| info.directory.clone())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn start_terminal(&mut self) {
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
        match Session::start(self.terminal_cwd(), size, colors, waker) {
            Ok(session) => {
                self.term.session = Some(session);
                self.term.sent = Some(colors);
                self.term.error = None;
            }
            Err(err) => self.term.error = Some(err.to_string()),
        }
    }

    /// The grid was laid out at `[x, y, w, h]`: the shell's size follows.
    fn terminal_layout(&mut self, rect: [f32; 4], scale: f32) {
        if !self.term.open || rect[2] < 40.0 || rect[3] < 20.0 {
            return;
        }
        let m = metrics();
        let size = Size {
            cols: ((rect[2] / m.cw).floor() as u16).max(2),
            rows: ((rect[3] / m.ch).floor() as u16).max(1),
            cell_w: (m.cw * scale).round().max(1.0) as u32,
            cell_h: (m.ch * scale).round().max(1.0) as u32,
        };
        if self.term.size == Some(size)
            && (self.term.session.is_some() || self.term.frame.is_some())
        {
            return;
        }
        self.term.size = Some(size);
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

    /// Before each frame: take the emulator's newest picture, and pass on a
    /// theme change.
    pub fn poll_terminal(&mut self) -> bool {
        let mut changed = false;
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
            self.term.frame = Some(frame);
            changed = true;
        }
        changed
    }

    /// A key while the terminal has the keyboard (the sink's key handler).
    fn terminal_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let mac = crate::platform::keys::Style::current() == crate::platform::keys::Style::Symbols;
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
                if self.term.session.as_ref().is_some_and(Session::exited) {
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
pub fn view(s: &UiState, k: &Kit, scheme: ColorScheme, scale: f32) -> El {
    let language = s.cfg.general.language;
    let colors = colors(k, scheme);
    s.term.want.set(Some(colors));
    let m = metrics();
    let frame = s.term.frame.as_ref();
    let bg = color(frame.map_or(colors.bg, |f| f.bg));
    let exited = s.term.session.as_ref().is_some_and(Session::exited);

    let title = frame
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
        });
    let mut header = row()
        .key("terminal-head")
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            rune::icon(&TERMINAL_WINDOW, 15.0)
                .color(k.c.icon_muted)
                .a11y_hidden(),
        )
        .child(
            text(title)
                .key("terminal-title")
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .shrink()
                .min_w(0.0)
                .grow(1.0),
        );
    if exited {
        header = header.child(
            Badge::new(tr_ref(language, "Exited", "已退出"))
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("terminal-exited"),
        );
    }
    header = header
        .child(
            IconButton::new(&RESTART)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "New shell", "新的 Shell"))
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

    let grid = grid(s, k, frame, m, bg, scale);
    let mut card = col()
        .key("terminal-card")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(INSET)
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(bg)
        .border(1.0, k.c.border_muted)
        .clip_rounded(crate::theme::look::CARD_RADIUS)
        .child(grid);
    if let Some(err) = &s.term.error {
        card = card.child(
            text(err.clone())
                .key("terminal-error")
                .font(k.ty.body)
                .color(k.c.danger.fg)
                .wrap(),
        );
    } else if exited {
        card = card.child(
            text(tr_ref(
                language,
                "The shell ended. Press Return for a new one.",
                "Shell 已结束，按回车开一个新的。",
            ))
            .key("terminal-ended")
            .font(k.ty.caption())
            .color(k.c.text_muted),
        );
    }
    col()
        .key("terminal-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .gap(8.0)
        .px(10.0)
        .pt(10.0)
        .pb(10.0)
        .child(header)
        .child(card)
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
    if let Some(cursor) = frame.cursor {
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
    let composing = s.term.sink.preedit.is_some();
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
        state.open_terminal();
        let mut rt = Runtime::new(
            state,
            super::super::view,
            (1440.0, 900.0),
            Env::new(true, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        for ev in [
            key(Key::Escape, false, false),
            key(Key::Tab, false, false),
            key(Key::Command('c'), true, false),
            key(Key::Text("x".into()), false, false),
        ] {
            rt.key(ev);
            rt.frame(1.0 / 60.0);
        }
        // An input method's commit goes to the shell as typed.
        rt.ime(Ime::Commit("中文".into()));
        rt.frame(1.0 / 60.0);
        rt.key(key(Key::Command('k'), true, true));
        rt.frame(1.0 / 60.0);
        let s = &rt.state;
        assert_eq!(
            *s.term.sent_log.borrow(),
            [
                "key Escape ctrl=false",
                "key Tab ctrl=false",
                "key C ctrl=true",
                "key X ctrl=false",
                "text 中文",
            ]
        );
        // Ctrl+Shift+K reached the app's Ctrl+K: the palette.
        assert!(s.palette.is_some());
    }
}
