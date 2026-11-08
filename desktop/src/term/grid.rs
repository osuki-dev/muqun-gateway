//! What the panel draws: a terminal frame as plain, `Send` data.
//!
//! The emulator (`engine.rs`) reads libghostty's render state cell by cell and
//! folds each row into [`Row`]: background runs (one rect per stretch of one
//! colour) and text runs (one text element per stretch of one style). The UI
//! (`ui/terminal_panel.rs`) never sees a cell, only these runs, so a busy
//! 120×40 grid is a few hundred elements rather than 4 800.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// An sRGB colour as the emulator resolved it (palette, truecolor or default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// A stretch of cells with one background.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BgRun {
    pub col: u16,
    pub len: u16,
    pub color: Rgb,
}

/// Text attributes that change how a run is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strike: bool,
}

/// A stretch of text in one style, starting at column `col` and covering
/// `cells` columns. Runs only hold glyphs the monospace face draws exactly one
/// cell wide; anything else (wide CJK, emoji, Nerd Font icons) is a run of its
/// own, placed at its column so a fallback face's advance can't push the rest
/// of the row out of the grid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextRun {
    pub col: u16,
    pub cells: u16,
    pub text: String,
    pub fg: Rgb,
    pub attrs: Attrs,
}

/// One grid row, ready to draw. `hash` names its content (memo version).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub bgs: Vec<BgRun>,
    pub runs: Vec<TextRun>,
    pub hash: u64,
}

/// One cell as the emulator hands it to [`build_row`], colours resolved
/// (inverse and selection already applied).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellIn<'a> {
    /// The grapheme, empty for a blank cell.
    pub text: &'a str,
    /// 2 for the head of a wide character, 0 for its spacer tail, else 1.
    pub width: u8,
    pub fg: Rgb,
    /// `None` is the terminal's default background (drawn by the panel).
    pub bg: Option<Rgb>,
    pub attrs: Attrs,
    /// Hidden text (SGR 8) keeps its background but draws no glyph.
    pub invisible: bool,
}

/// Whether the monospace face draws `text` exactly one cell wide, so it can
/// share a run with its neighbours: ASCII, Latin-1, box drawing and blocks.
fn fits_cell(text: &str) -> bool {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    matches!(c, ' '..='~' | '\u{a0}'..='\u{17f}' | '\u{2500}'..='\u{259f}')
}

/// Fold a row of cells into background and text runs.
pub fn build_row<'a>(cells: impl IntoIterator<Item = CellIn<'a>>) -> Row {
    let mut bgs: Vec<BgRun> = Vec::new();
    let mut runs: Vec<TextRun> = Vec::new();
    // The run being extended: it only takes cells that fit one column.
    let mut open = false;
    let mut col: u16 = 0;
    for cell in cells {
        let width = u16::from(cell.width);
        if width == 0 {
            // A wide character's spacer: its head already covered it.
            continue;
        }
        if let Some(color) = cell.bg {
            match bgs.last_mut() {
                Some(last) if last.color == color && last.col + last.len == col => {
                    last.len += width;
                }
                _ => bgs.push(BgRun {
                    col,
                    len: width,
                    color,
                }),
            }
        }
        let text = if cell.text.is_empty() || cell.invisible {
            " "
        } else {
            cell.text
        };
        let blank = text == " ";
        let decorated = cell.attrs.underline || cell.attrs.strike;
        if blank && !decorated {
            // Spaces only join a run already open in the same style (so
            // `a b` stays one run); they never start one.
            match runs.last_mut() {
                Some(last)
                    if open
                        && last.col + last.cells == col
                        && last.fg == cell.fg
                        && last.attrs == cell.attrs =>
                {
                    last.text.push(' ');
                    last.cells += 1;
                }
                _ => open = false,
            }
            col += width;
            continue;
        }
        let simple = width == 1 && fits_cell(text);
        match runs.last_mut() {
            Some(last)
                if simple
                    && open
                    && last.col + last.cells == col
                    && last.fg == cell.fg
                    && last.attrs == cell.attrs =>
            {
                last.text.push_str(text);
                last.cells += 1;
            }
            _ => {
                runs.push(TextRun {
                    col,
                    cells: width,
                    text: text.to_string(),
                    fg: cell.fg,
                    attrs: cell.attrs,
                });
                open = simple;
            }
        }
        col += width;
    }
    // Trailing spaces a run picked up draw nothing.
    for run in &mut runs {
        if run.attrs.underline || run.attrs.strike {
            continue;
        }
        let trimmed = run.text.trim_end_matches(' ').len();
        let dropped = (run.text.len() - trimmed) as u16;
        run.text.truncate(trimmed);
        run.cells -= dropped;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bgs.hash(&mut hasher);
    runs.hash(&mut hasher);
    Row {
        hash: hasher.finish(),
        bgs,
        runs,
    }
}

/// The cursor's look (DECSCUSR), and whether it sits on a wide character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub wide: bool,
}

/// Where the viewport sits in the scrollback, in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scroll {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

impl Scroll {
    /// Scrolled up into history (not following the output).
    pub fn in_history(&self) -> bool {
        self.offset + self.len < self.total
    }
}

/// One complete picture of the terminal.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub grid: Vec<Arc<Row>>,
    pub cursor: Option<Cursor>,
    pub bg: Rgb,
    pub cursor_color: Rgb,
    pub title: String,
    pub scroll: Scroll,
    /// The program asked for mouse reports (vim with `mouse=a`, htop).
    pub mouse_tracking: bool,
    /// The selection's text, when there is one.
    pub selection: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Rgb = Rgb(200, 200, 200);
    const R: Rgb = Rgb(200, 0, 0);
    const B: Rgb = Rgb(0, 0, 90);

    fn cell(text: &str, fg: Rgb, bg: Option<Rgb>) -> CellIn<'_> {
        CellIn {
            text,
            width: 1,
            fg,
            bg,
            attrs: Attrs::default(),
            invisible: false,
        }
    }

    fn texts(row: &Row) -> Vec<(u16, u16, &str)> {
        row.runs
            .iter()
            .map(|r| (r.col, r.cells, r.text.as_str()))
            .collect()
    }

    #[test]
    fn runs_split_on_style_and_merge_backgrounds() {
        // `ls --color`: "a.rs" white, two blanks, "dir" red on blue, blanks.
        let mut cells = vec![
            cell("a", W, None),
            cell(".", W, None),
            cell("r", W, None),
            cell("s", W, None),
            cell("", W, None),
            cell(" ", W, None),
            cell("d", R, Some(B)),
            cell("i", R, Some(B)),
            cell("r", R, Some(B)),
            cell(" ", W, Some(B)),
        ];
        cells.extend((0..5).map(|_| cell("", W, None)));
        let row = build_row(cells);
        assert_eq!(texts(&row), vec![(0, 4, "a.rs"), (6, 3, "dir")]);
        assert_eq!(
            row.bgs,
            vec![BgRun {
                col: 6,
                len: 4,
                color: B
            }]
        );
    }

    #[test]
    fn wide_and_fallback_glyphs_sit_alone_at_their_column() {
        let wide = |t| CellIn {
            width: 2,
            ..cell(t, W, None)
        };
        let tail = CellIn {
            width: 0,
            ..cell("", W, None)
        };
        let row = build_row(vec![
            cell("$", W, None),
            cell(" ", W, None),
            wide("中"),
            tail,
            wide("文"),
            tail,
            cell("o", W, None),
            cell("k", W, None),
            cell("", W, None),
            cell("\u{e0b0}", W, None),
        ]);
        assert_eq!(
            texts(&row),
            vec![
                (0, 1, "$"),
                (2, 2, "中"),
                (4, 2, "文"),
                (6, 2, "ok"),
                (9, 1, "\u{e0b0}")
            ]
        );
    }
}
