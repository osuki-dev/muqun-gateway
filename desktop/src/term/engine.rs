//! The emulator: libghostty-vt's terminal, its render state and its input
//! encoders, on one thread (every libghostty type is `!Send`).
//!
//! Bytes from the shell go in through [`Engine::feed`]; keys, mouse, paste and
//! scrolling through the other methods. What the terminal writes back (query
//! answers, encoded keys) collects in an outbox the owner flushes to the pty.
//! [`Engine::frame`] turns the dirty rows into [`Row`]s for the UI.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libghostty_vt::key::{self, Encoder as KeyEncoder, Event as KeyEvent};
use libghostty_vt::mouse::{self, Encoder as MouseEncoder, Event as MouseEvent};
use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::selection::gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent};
use libghostty_vt::selection::FormatOptions;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point,
    PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
    SizeReportSize,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

use super::grid::{build_row, Attrs, CellIn, Cursor, CursorShape, Frame, Rgb, Row, Scroll};
use super::input::{KeyInput, Mods};

/// Lines of history kept per terminal.
pub const SCROLLBACK: usize = 10_000;

/// The terminal's colours, from the theme (`ui/terminal_panel.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// Already blended over `bg`: the cell background of selected text.
    pub selection: Rgb,
    pub ansi: [Rgb; 16],
}

/// The grid's size in cells and the cell's size in device pixels (for
/// pixel size reports and mouse encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MousePhase {
    Down,
    Move,
    Up,
}

/// A pointer event over the grid, in pixels from the grid's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseInput {
    pub phase: MousePhase,
    pub x: f32,
    pub y: f32,
    pub mods: Mods,
}

fn from_ghostty(c: RgbColor) -> Rgb {
    Rgb(c.r, c.g, c.b)
}

fn ghostty_rgb(c: Rgb) -> RgbColor {
    RgbColor {
        r: c.0,
        g: c.1,
        b: c.2,
    }
}

fn mods(m: Mods) -> key::Mods {
    let mut out = key::Mods::empty();
    out.set(key::Mods::SHIFT, m.shift);
    out.set(key::Mods::CTRL, m.ctrl);
    out.set(key::Mods::ALT, m.alt);
    out.set(key::Mods::SUPER, m.logo);
    out
}

type Result<T> = std::result::Result<T, libghostty_vt::Error>;

pub struct Engine {
    term: Terminal<'static, 'static>,
    render: RenderState<'static>,
    rows_it: RowIterator<'static>,
    cells_it: CellIterator<'static>,
    key_enc: KeyEncoder<'static>,
    key_ev: KeyEvent<'static>,
    mouse_enc: MouseEncoder<'static>,
    mouse_ev: MouseEvent<'static>,
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    release: ReleaseEvent<'static>,
    drag: DragEvent<'static>,
    /// Bytes for the pty: query answers from libghostty, encoded input.
    out: Rc<RefCell<Vec<u8>>>,
    size: Rc<Cell<Size>>,
    colors: Colors,
    grid: Vec<Arc<Row>>,
    /// Redraw every row on the next frame (colours, resize, selection).
    force: bool,
    /// The left button went down on a selection gesture, not a report.
    selecting: bool,
    selection: Option<String>,
    clock: Instant,
    title: String,
}

impl Engine {
    pub fn new(size: Size, colors: Colors) -> Result<Self> {
        let mut term = Terminal::new(TerminalOptions {
            cols: size.cols.max(1),
            rows: size.rows.max(1),
            max_scrollback: SCROLLBACK,
        })?;
        term.resize(
            size.cols.max(1),
            size.rows.max(1),
            size.cell_w.max(1),
            size.cell_h.max(1),
        )?;
        let out = Rc::new(RefCell::new(Vec::new()));
        let shared_size = Rc::new(Cell::new(size));
        {
            let out = out.clone();
            term.on_pty_write(move |_, data| out.borrow_mut().extend_from_slice(data))?;
        }
        {
            let size = shared_size.clone();
            term.on_size(move |_| {
                let s = size.get();
                Some(SizeReportSize {
                    rows: s.rows,
                    columns: s.cols,
                    cell_width: s.cell_w,
                    cell_height: s.cell_h,
                })
            })?;
        }
        term.on_device_attributes(|_| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[
                        DeviceAttributeFeature::COLUMNS_132,
                        DeviceAttributeFeature::SELECTIVE_ERASE,
                        DeviceAttributeFeature::ANSI_COLOR,
                    ],
                ),
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                tertiary: Default::default(),
            })
        })?;
        term.on_xtversion(|_| Some("muqun-desktop"))?;
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(Duration::from_millis(500))?;
        let mut engine = Self {
            term,
            render: RenderState::new()?,
            rows_it: RowIterator::new()?,
            cells_it: CellIterator::new()?,
            key_enc: KeyEncoder::new()?,
            key_ev: KeyEvent::new()?,
            mouse_enc: MouseEncoder::new()?,
            mouse_ev: MouseEvent::new()?,
            gesture: Gesture::new()?,
            press,
            release: ReleaseEvent::new()?,
            drag: DragEvent::new()?,
            out,
            size: shared_size,
            colors,
            grid: Vec::new(),
            force: true,
            selecting: false,
            selection: None,
            clock: Instant::now(),
            title: String::new(),
        };
        engine.set_colors(colors)?;
        Ok(engine)
    }

    /// Theme colours: defaults for fg/bg/cursor and the 16 ANSI entries of
    /// the palette (the 256-colour cube and greys stay xterm's).
    pub fn set_colors(&mut self, colors: Colors) -> Result<()> {
        let mut palette = Palette::default();
        for (i, c) in colors.ansi.iter().enumerate() {
            palette.set(PaletteIndex(i as u8), ghostty_rgb(*c));
        }
        self.term
            .set_default_fg_color(Some(ghostty_rgb(colors.fg)))?
            .set_default_bg_color(Some(ghostty_rgb(colors.bg)))?
            .set_default_cursor_color(Some(ghostty_rgb(colors.cursor)))?
            .set_default_color_palette(Some(palette))?;
        self.colors = colors;
        self.force = true;
        Ok(())
    }

    /// Parse output from the shell.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
    }

    /// Bytes waiting for the pty.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.out.borrow_mut())
    }

    pub fn size(&self) -> Size {
        self.size.get()
    }

    pub fn resize(&mut self, size: Size) -> Result<()> {
        if size == self.size.get() {
            return Ok(());
        }
        self.size.set(size);
        self.term.resize(
            size.cols.max(1),
            size.rows.max(1),
            size.cell_w.max(1),
            size.cell_h.max(1),
        )?;
        self.force = true;
        Ok(())
    }

    /// A key press, encoded for the modes the program set (application
    /// cursor keys, Kitty keyboard protocol …). Typing returns to the bottom.
    pub fn key(&mut self, input: &KeyInput) -> Result<()> {
        let m = mods(input.mods);
        let mut consumed = key::Mods::empty();
        if input.text.is_some() && input.mods.shift {
            consumed |= key::Mods::SHIFT;
        }
        self.key_ev
            .set_action(if input.repeat {
                key::Action::Repeat
            } else {
                key::Action::Press
            })
            .set_key(input.key)
            .set_mods(m)
            .set_consumed_mods(consumed)
            .set_unshifted_codepoint(input.unshifted)
            .set_utf8(input.text.clone());
        let mut out = Vec::new();
        self.key_enc
            .set_options_from_terminal(&self.term)
            .encode_to_vec(&self.key_ev, &mut out)?;
        if out.is_empty() {
            if let Some(text) = &input.text {
                out.extend_from_slice(text.as_bytes());
            }
        }
        if !out.is_empty() {
            self.follow_output();
            self.out.borrow_mut().extend_from_slice(&out);
        }
        Ok(())
    }

    /// Forget the scrollback and the screen, then ask the shell for a fresh
    /// prompt with Ctrl+L, as a terminal's Clear does.
    pub fn clear(&mut self) -> Result<()> {
        self.term.vt_write(b"\x1b[H\x1b[2J\x1b[3J");
        self.follow_output();
        self.force = true;
        self.key(&KeyInput {
            key: key::Key::L,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            text: None,
            unshifted: 'l',
            repeat: false,
        })
    }

    /// Text an input method composed: sent as typed.
    pub fn text(&mut self, text: &str) {
        self.follow_output();
        self.out.borrow_mut().extend_from_slice(text.as_bytes());
    }

    /// A paste: control bytes defused, bracketed when the program asked.
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self.term.mode(Mode::BRACKETED_PASTE)?;
        let mut data = text.as_bytes().to_vec();
        let mut buf = vec![0u8; data.len() + 16];
        let n = match libghostty_vt::paste::encode(&mut data, bracketed, &mut buf) {
            Ok(n) => n,
            Err(libghostty_vt::Error::OutOfSpace { required }) => {
                buf.resize(required, 0);
                let mut data = text.as_bytes().to_vec();
                libghostty_vt::paste::encode(&mut data, bracketed, &mut buf)?
            }
            Err(e) => return Err(e),
        };
        self.follow_output();
        self.out.borrow_mut().extend_from_slice(&buf[..n]);
        Ok(())
    }

    fn follow_output(&mut self) {
        if self.scroll().in_history() {
            self.term.scroll_viewport(ScrollViewport::Bottom);
            self.force = true;
        }
    }

    fn scroll(&self) -> Scroll {
        self.term
            .scrollbar()
            .map(|s| Scroll {
                total: s.total,
                offset: s.offset,
                len: s.len,
            })
            .unwrap_or_default()
    }

    /// The wheel: reported to a program that tracks the mouse, else the
    /// alternate screen gets arrow keys (less, man), else the viewport moves
    /// through history. `lines` < 0 scrolls up.
    pub fn wheel(&mut self, lines: i32, x: f32, y: f32, m: Mods) -> Result<()> {
        if lines == 0 {
            return Ok(());
        }
        if self.term.is_mouse_tracking()? {
            let button = if lines < 0 {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            self.prepare_mouse(false);
            for _ in 0..lines.unsigned_abs().min(10) {
                let mut out = Vec::new();
                self.mouse_ev
                    .set_mods(mods(m))
                    .set_position(mouse::Position { x, y })
                    .set_button(Some(button))
                    .set_action(mouse::Action::Press);
                self.mouse_enc.encode_to_vec(&self.mouse_ev, &mut out)?;
                self.mouse_ev.set_action(mouse::Action::Release);
                self.mouse_enc.encode_to_vec(&self.mouse_ev, &mut out)?;
                self.out.borrow_mut().extend_from_slice(&out);
            }
            return Ok(());
        }
        let alt = matches!(
            self.term.active_screen(),
            Ok(libghostty_vt::screen::Screen::Alternate)
        );
        if alt && self.term.mode(Mode::ALT_SCROLL).unwrap_or(true) {
            let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
            let app = self.term.mode(Mode::DECCKM).unwrap_or(false);
            for _ in 0..lines.unsigned_abs() {
                if app {
                    self.out.borrow_mut().extend_from_slice(if lines < 0 {
                        b"\x1bOA"
                    } else {
                        b"\x1bOB"
                    });
                } else {
                    self.out.borrow_mut().extend_from_slice(seq);
                }
            }
            return Ok(());
        }
        self.term
            .scroll_viewport(ScrollViewport::Delta(lines as isize));
        self.force = true;
        Ok(())
    }

    fn prepare_mouse(&mut self, any_pressed: bool) {
        let s = self.size.get();
        self.mouse_enc
            .set_options_from_terminal(&self.term)
            .set_size(mouse::EncoderSize {
                screen_width: u32::from(s.cols) * s.cell_w,
                screen_height: u32::from(s.rows) * s.cell_h,
                cell_width: s.cell_w.max(1),
                cell_height: s.cell_h.max(1),
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            })
            .set_any_button_pressed(any_pressed)
            .set_track_last_cell(true);
    }

    fn point(&self, x: f32, y: f32) -> Point {
        let s = self.size.get();
        let col = (x / s.cell_w.max(1) as f32).clamp(0.0, f32::from(s.cols.max(1) - 1)) as u16;
        let row = (y / s.cell_h.max(1) as f32).clamp(0.0, f32::from(s.rows.max(1) - 1)) as u32;
        Point::Viewport(PointCoordinate { x: col, y: row })
    }

    /// A pointer event: reported to a program that tracks the mouse (Shift
    /// overrides, as in every terminal), else a selection gesture.
    pub fn mouse(&mut self, input: MouseInput) -> Result<()> {
        let tracking = self.term.is_mouse_tracking()?;
        let report = tracking && !input.mods.shift && !self.selecting;
        if report {
            let button = mouse::Button::Left;
            self.prepare_mouse(input.phase == MousePhase::Move);
            self.mouse_ev
                .set_mods(mods(input.mods))
                .set_position(mouse::Position {
                    x: input.x,
                    y: input.y,
                })
                .set_button(Some(button))
                .set_action(match input.phase {
                    MousePhase::Down => mouse::Action::Press,
                    MousePhase::Move => mouse::Action::Motion,
                    MousePhase::Up => mouse::Action::Release,
                });
            let mut out = Vec::new();
            self.mouse_enc.encode_to_vec(&self.mouse_ev, &mut out)?;
            self.out.borrow_mut().extend_from_slice(&out);
            return Ok(());
        }
        let point = self.point(input.x, input.y);
        let s = self.size.get();
        match input.phase {
            MousePhase::Down => {
                self.selecting = true;
                let grid_ref = self.term.grid_ref(point)?;
                let selection = self
                    .press
                    .set_repeat_distance(f64::from(s.cell_w))?
                    .set_time(self.clock.elapsed())?
                    .set_position(f64::from(input.x), f64::from(input.y))?
                    .apply(&mut self.gesture, &self.term, grid_ref)?;
                self.term.set_selection(selection.as_ref())?;
            }
            MousePhase::Move if self.selecting => {
                let grid_ref = self.term.grid_ref(point)?;
                let geometry = Geometry {
                    columns: u32::from(s.cols),
                    cell_width: s.cell_w.max(1),
                    padding_left: 0,
                    screen_height: u32::from(s.rows) * s.cell_h.max(1),
                };
                let selection = self
                    .drag
                    .set_rectangle(input.mods.alt)?
                    .set_position(f64::from(input.x), f64::from(input.y))?
                    .apply(&mut self.gesture, &self.term, grid_ref, geometry)?;
                self.term.set_selection(selection.as_ref())?;
            }
            MousePhase::Move => return Ok(()),
            MousePhase::Up => {
                self.selecting = false;
                let grid_ref = self.term.grid_ref(point).ok();
                self.release
                    .apply(&mut self.gesture, &self.term, grid_ref)?;
            }
        }
        self.selection = self
            .term
            .format_selection_alloc(None, FormatOptions::new().with_unwrap(true).with_trim(true))?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .filter(|t| !t.is_empty());
        self.force = true;
        Ok(())
    }

    /// The picture to draw, when anything changed since the last one.
    pub fn frame(&mut self) -> Result<Option<Frame>> {
        let force = std::mem::take(&mut self.force);
        let title = self.term.title().unwrap_or_default().to_string();
        let scroll = self.scroll();
        let mouse_tracking = self.term.is_mouse_tracking().unwrap_or(false);
        let snapshot = self.render.update(&self.term)?;
        let dirty = snapshot.dirty()?;
        if dirty == Dirty::Clean && !force && title == self.title {
            return Ok(None);
        }
        self.title = title;
        let colors = snapshot.colors()?;
        let (fg, bg) = (
            from_ghostty(colors.foreground),
            from_ghostty(colors.background),
        );
        let rows = usize::from(snapshot.rows()?);
        let cols = snapshot.cols()?;
        let all = force || dirty == Dirty::Full || self.grid.len() != rows;
        self.grid.resize_with(rows, Default::default);
        let mut texts: Vec<String> = Vec::new();
        let mut metas: Vec<(u8, Rgb, Option<Rgb>, Attrs, bool)> = Vec::new();
        let mut row_it = self.rows_it.update(&snapshot)?;
        let mut y = 0usize;
        while let Some(row) = row_it.next() {
            if y >= rows {
                break;
            }
            if all || row.dirty()? {
                let selected = row.selection()?.map(|s| (s.start_x, s.end_x));
                let mut cell_it = self.cells_it.update(row)?;
                texts.resize_with(usize::from(cols), String::new);
                metas.clear();
                let mut x: u16 = 0;
                while let Some(cell) = cell_it.next() {
                    if usize::from(x) >= texts.len() {
                        break;
                    }
                    let text = &mut texts[usize::from(x)];
                    text.clear();
                    if cell.graphemes_len()? > 0 {
                        cell.graphemes_utf8(text)?;
                    }
                    let width = match cell.raw_cell()?.wide()? {
                        CellWide::Wide => 2,
                        CellWide::SpacerTail => 0,
                        _ => 1,
                    };
                    let mut cfg = cell.fg_color()?.map_or(fg, from_ghostty);
                    let mut cbg = cell.bg_color()?.map(from_ghostty);
                    let mut attrs = Attrs::default();
                    let mut invisible = false;
                    if cell.has_styling()? {
                        let style = cell.style()?;
                        attrs = Attrs {
                            bold: style.bold,
                            italic: style.italic,
                            faint: style.faint,
                            underline: !matches!(
                                style.underline,
                                libghostty_vt::style::Underline::None
                            ),
                            strike: style.strikethrough,
                        };
                        invisible = style.invisible;
                        if style.inverse {
                            let back = cbg.unwrap_or(bg);
                            cbg = Some(cfg);
                            cfg = back;
                        }
                    }
                    if selected.is_some_and(|(a, b)| x >= a && x <= b) {
                        cbg = Some(self.colors.selection);
                    }
                    metas.push((width, cfg, cbg, attrs, invisible));
                    x += 1;
                }
                let built = build_row(texts.iter().zip(&metas).map(
                    |(text, &(width, fg, bg, attrs, invisible))| CellIn {
                        text,
                        width,
                        fg,
                        bg,
                        attrs,
                        invisible,
                    },
                ));
                if self.grid[y].hash != built.hash || all {
                    self.grid[y] = Arc::new(built);
                }
                row.set_dirty(false)?;
            }
            y += 1;
        }
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|vp| Cursor {
                col: vp.x,
                row: vp.y,
                shape: match snapshot.cursor_visual_style() {
                    Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                    Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                    _ => CursorShape::Block,
                },
                wide: vp.at_wide_tail,
            })
        } else {
            None
        };
        let cursor_color = colors.cursor.map_or(self.colors.cursor, from_ghostty);
        let alternate_screen = matches!(
            self.term.active_screen(),
            Ok(libghostty_vt::screen::Screen::Alternate)
        );
        snapshot.set_dirty(Dirty::Clean)?;
        Ok(Some(Frame {
            cols,
            rows: rows as u16,
            grid: self.grid.clone(),
            cursor,
            bg,
            cursor_color,
            title: self.title.clone(),
            scroll,
            mouse_tracking,
            alternate_screen,
            selection: self.selection.clone(),
        }))
    }
}

/// Run `bytes` through a fresh terminal of `size` and return what it shows:
/// the screenshot scenes' fixtures, and tests.
pub fn render_fixture(bytes: &[u8], size: Size, colors: Colors) -> Option<Frame> {
    let mut engine = Engine::new(size, colors).ok()?;
    engine.feed(bytes);
    engine.frame().ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        let grey = |v| Rgb(v, v, v);
        let mut ansi = [grey(128); 16];
        ansi[1] = Rgb(205, 49, 49);
        Colors {
            fg: grey(220),
            bg: grey(20),
            cursor: grey(250),
            selection: grey(70),
            ansi,
        }
    }

    #[test]
    fn theme_palette_colours_cells_and_alt_screen_restores_the_shell() {
        let size = Size {
            cols: 20,
            rows: 4,
            cell_w: 8,
            cell_h: 16,
        };
        let mut engine = Engine::new(size, colors()).unwrap();
        // Red from the theme's ANSI 1, truecolor as given.
        engine.feed(b"\x1b[31mred\x1b[0m \x1b[38;2;1;2;3mtc\x1b[0m\r\n$ ");
        let frame = engine.frame().unwrap().unwrap();
        let runs = &frame.grid[0].runs;
        assert_eq!(
            (runs[0].text.as_str(), runs[0].fg),
            ("red", Rgb(205, 49, 49))
        );
        assert_eq!((runs[1].text.as_str(), runs[1].fg), ("tc", Rgb(1, 2, 3)));
        // vim: the alternate screen takes over, and leaving it brings the
        // shell's screen back.
        engine.feed(b"\x1b[?1049h\x1b[H\x1b[2Jvim");
        let frame = engine.frame().unwrap().unwrap();
        assert_eq!(frame.grid[0].runs[0].text, "vim");
        assert!(frame.alternate_screen);
        engine.feed(b"\x1b[?1049l");
        let frame = engine.frame().unwrap().unwrap();
        assert_eq!(frame.grid[0].runs[0].text, "red");
        assert!(!frame.alternate_screen);
        assert_eq!(frame.cursor.map(|c| (c.col, c.row)), Some((2, 1)));
    }
}
