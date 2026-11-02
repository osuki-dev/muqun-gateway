//! Message markdown: a thin renderer over `pulldown-cmark` (architecture
//! §8.2 R4).
//!
//! Blinc's own `markdown()` renders each inline run as a `no_wrap` text
//! inside a wrapping flex row (`blinc_layout/src/markdown/renderer.rs:724-747`
//! `txt.v_baseline().no_wrap()`, `:763` `flex_row().flex_wrap()`), so a
//! paragraph only breaks *between* differently styled runs: a long plain
//! paragraph is one run and overflows instead of wrapping. It also drops the
//! fence language (`:808-820`), re-parses on every render, and colours text
//! from a fixed config. This renderer instead builds one wrapping
//! [`RichText`] per paragraph, with bold/italic/strike/code/link *spans*, so
//! wrapping happens inside runs; it highlights fences with syntect
//! ([`highlight`]), takes every colour from the theme, and caches the
//! parsed blocks by content.
//!
//! Parsing is separate from building elements: [`parse`] turns text into
//! [`Block`]s (cached, so a row rebuilt by the windowed list does not parse
//! again), [`render`] turns blocks into Blinc elements.

pub mod highlight;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use blinc_core::Color;
use blinc_layout::prelude::*;
use blinc_layout::styled_text::{StyledLine, StyledText, TextSpan};
use blinc_layout::FontFamily;
use blinc_theme::{ColorToken, ThemeState};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::parts::scale;
use super::text::{MONO_FONT, UI_FONT};

/// Inline styling of one span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// A run of text with styled byte ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub spans: Vec<(Range<usize>, Style)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: &Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if *style != Style::default() {
            self.spans.push((start..self.text.len(), style.clone()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// `Some(done)` for a task-list item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code {
        lang: Option<String>,
        code: String,
    },
    Quote(Vec<Block>),
    List {
        /// The first number of an ordered list.
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
    Rule,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

enum Frame {
    Quote(Vec<Block>),
    List(Option<u64>, Vec<ListItem>),
    Item(Option<bool>, Vec<Block>),
    Table {
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        row: Vec<Inline>,
        in_head: bool,
    },
}

struct Builder {
    root: Vec<Block>,
    stack: Vec<Frame>,
    inline: Inline,
    style: Style,
    /// Inline content is being collected for a paragraph, heading or cell.
    code: Option<(Option<String>, String)>,
    /// A tight list item's text arrives without a paragraph around it.
    loose_text: bool,
}

impl Builder {
    fn blocks(&mut self) -> &mut Vec<Block> {
        for frame in self.stack.iter_mut().rev() {
            match frame {
                Frame::Quote(blocks) | Frame::Item(_, blocks) => return blocks,
                Frame::List(..) | Frame::Table { .. } => continue,
            }
        }
        &mut self.root
    }

    fn push_block(&mut self, block: Block) {
        self.blocks().push(block);
    }

    /// Text that arrived outside a paragraph (a tight list item) becomes one.
    fn flush_loose(&mut self) {
        if self.loose_text {
            self.loose_text = false;
            let inline = std::mem::take(&mut self.inline);
            if !inline.is_empty() {
                self.push_block(Block::Paragraph(inline));
            }
        }
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = self.code.as_mut() {
            code.push_str(text);
            return;
        }
        if matches!(self.stack.last(), Some(Frame::Item(..))) && self.inline.text.is_empty() {
            self.loose_text = true;
        }
        let style = self.style.clone();
        self.inline.push(text, &style);
    }
}

/// Parse markdown into blocks.
pub fn parse_uncached(text: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut b = Builder {
        root: Vec::new(),
        stack: Vec::new(),
        inline: Inline::default(),
        style: Style::default(),
        code: None,
        loose_text: false,
    };
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.flush_loose(),
                Tag::Heading { .. } => b.flush_loose(),
                Tag::BlockQuote => {
                    b.flush_loose();
                    b.stack.push(Frame::Quote(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush_loose();
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info
                            .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                            .next()
                            .filter(|l| !l.is_empty())
                            .map(str::to_string),
                        CodeBlockKind::Indented => None,
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush_loose();
                    b.stack.push(Frame::List(start, Vec::new()));
                }
                Tag::Item => b.stack.push(Frame::Item(None, Vec::new())),
                Tag::Table(_) => b.stack.push(Frame::Table {
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    in_head: false,
                }),
                Tag::TableHead => {
                    if let Some(Frame::Table { in_head, .. }) = b.stack.last_mut() {
                        *in_head = true;
                    }
                }
                Tag::TableRow | Tag::TableCell => {}
                Tag::Emphasis => b.style.italic = true,
                Tag::Strong => b.style.bold = true,
                Tag::Strikethrough => b.style.strike = true,
                Tag::Link { dest_url, .. } => b.style.link = Some(dest_url.to_string()),
                Tag::Image { dest_url, .. } => {
                    b.style.link = Some(dest_url.to_string());
                    b.text("🖼 ");
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    let inline = std::mem::take(&mut b.inline);
                    b.loose_text = false;
                    if !inline.is_empty() {
                        b.push_block(Block::Paragraph(inline));
                    }
                }
                TagEnd::Heading(level) => {
                    let inline = std::mem::take(&mut b.inline);
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    b.push_block(Block::Heading(level, inline));
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, mut code)) = b.code.take() {
                        if code.ends_with('\n') {
                            code.pop();
                        }
                        b.push_block(Block::Code { lang, code });
                    }
                }
                TagEnd::BlockQuote => {
                    b.flush_loose();
                    if let Some(Frame::Quote(blocks)) = b.stack.pop() {
                        b.push_block(Block::Quote(blocks));
                    }
                }
                TagEnd::Item => {
                    b.flush_loose();
                    if let Some(Frame::Item(task, blocks)) = b.stack.pop() {
                        if let Some(Frame::List(_, items)) = b.stack.last_mut() {
                            items.push(ListItem { task, blocks });
                        }
                    }
                }
                TagEnd::List(_) => {
                    if let Some(Frame::List(start, items)) = b.stack.pop() {
                        b.push_block(Block::List { start, items });
                    }
                }
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut b.inline);
                    if let Some(Frame::Table { row, .. }) = b.stack.last_mut() {
                        row.push(cell);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(Frame::Table {
                        head, row, in_head, ..
                    }) = b.stack.last_mut()
                    {
                        *head = std::mem::take(row);
                        *in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(Frame::Table { rows, row, .. }) = b.stack.last_mut() {
                        rows.push(std::mem::take(row));
                    }
                }
                TagEnd::Table => {
                    if let Some(Frame::Table { head, rows, .. }) = b.stack.pop() {
                        b.push_block(Block::Table { head, rows });
                    }
                }
                TagEnd::Emphasis => b.style.italic = false,
                TagEnd::Strong => b.style.bold = false,
                TagEnd::Strikethrough => b.style.strike = false,
                TagEnd::Link | TagEnd::Image => b.style.link = None,
                _ => {}
            },
            Event::Text(text) => b.text(&text),
            Event::Code(code) => {
                let mut style = b.style.clone();
                style.code = true;
                if matches!(b.stack.last(), Some(Frame::Item(..))) && b.inline.text.is_empty() {
                    b.loose_text = true;
                }
                b.inline.push(&code, &style);
            }
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.text("\n"),
            Event::Html(html) | Event::InlineHtml(html) => b.text(&html),
            Event::Rule => {
                b.flush_loose();
                b.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(task, _)) = b.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::FootnoteReference(label) => b.text(&format!("[{label}]")),
        }
    }
    b.flush_loose();
    let rest = std::mem::take(&mut b.inline);
    if !rest.is_empty() {
        b.root.push(Block::Paragraph(rest));
    }
    b.root
}

const PARSE_CACHE: usize = 512;

struct Cache {
    entries: HashMap<u64, (Arc<Vec<Block>>, u64)>,
    clock: u64,
}

fn content_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Parse, remembering the result by content.
pub fn parse(text: &str) -> Arc<Vec<Block>> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: HashMap::new(),
            clock: 0,
        })
    });
    let key = content_hash(text);
    {
        let mut c = cache.lock().unwrap();
        c.clock += 1;
        let now = c.clock;
        if let Some((blocks, used)) = c.entries.get_mut(&key) {
            *used = now;
            return blocks.clone();
        }
    }
    let blocks = Arc::new(parse_uncached(text));
    let mut c = cache.lock().unwrap();
    if c.entries.len() >= PARSE_CACHE {
        // Drop the least recently used quarter.
        let mut ages: Vec<(u64, u64)> = c.entries.iter().map(|(k, (_, u))| (*u, *k)).collect();
        ages.sort_unstable();
        for (_, k) in ages.into_iter().take(PARSE_CACHE / 4) {
            c.entries.remove(&k);
        }
    }
    let now = c.clock;
    c.entries.insert(key, (blocks.clone(), now));
    blocks
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The theme colours prose is drawn with. Read once per render.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub subtle: Color,
    pub link: Color,
    pub code: Color,
}

impl Palette {
    pub fn current() -> Self {
        let t = ThemeState::get();
        Self {
            text: t.color(ColorToken::TextPrimary),
            muted: t.color(ColorToken::TextSecondary),
            subtle: t.color(ColorToken::TextTertiary),
            link: t.color(ColorToken::TextLink),
            code: t.color(ColorToken::Info),
        }
    }
}

/// How prose is set: colour, size and line height.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub palette: Palette,
    /// The body colour (prose text, or muted for reasoning).
    pub color: Color,
    pub size: f32,
    pub line_height: f32,
    /// The width the blocks are laid out in. Flex rows (list items) give
    /// their text column this width explicitly: a wrapping text whose
    /// container width is not yet definite is measured at its min-content
    /// width and breaks after every character.
    pub width: f32,
}

/// A list marker's column, and the gap after it (`.mq-md-li` has none).
const MARKER: f32 = 22.0;
const LI_GAP: f32 = 6.0;
/// A quote's bar and padding.
const QUOTE_INSET: f32 = 15.0;

/// What a code block needs from the row it is in.
pub struct CodeChrome<'a> {
    /// Prefix for the stable ids of the block's controls.
    pub key: &'a str,
    pub copy_label: &'static str,
}

/// A wrapping rich text for `inline` in `setting`.
pub fn inline_text(inline: &Inline, setting: &Setting, size: f32, bold: bool) -> RichText {
    let p = &setting.palette;
    let mut spans = Vec::with_capacity(inline.spans.len() * 2 + 1);
    let mut at = 0;
    let plain = |start, end| TextSpan {
        start,
        end,
        color: setting.color,
        bold,
        italic: false,
        underline: false,
        strikethrough: false,
        code: false,
        link_url: None,
        token_type: None,
    };
    for (range, style) in &inline.spans {
        if range.start > at {
            spans.push(plain(at, range.start));
        }
        let color = if style.link.is_some() {
            p.link
        } else if style.code {
            p.code
        } else {
            setting.color
        };
        spans.push(TextSpan {
            start: range.start,
            end: range.end,
            color,
            bold: bold || style.bold,
            italic: style.italic,
            underline: style.link.is_some(),
            strikethrough: style.strike,
            code: style.code,
            link_url: style.link.clone(),
            token_type: None,
        });
        at = range.end;
    }
    if at < inline.text.len() {
        spans.push(plain(at, inline.text.len()));
    }
    let styled = StyledText::from_lines(vec![StyledLine::new(inline.text.clone(), spans)]);
    let mut rich = rich_text_styled(styled)
        .size(size)
        .line_height(setting.line_height)
        .default_color(setting.color)
        .font_family(FontFamily::named(UI_FONT));
    if bold {
        rich = rich.bold();
    }
    if setting.width > 0.0 {
        rich = rich.w(setting.width);
    }
    rich
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => scale::FS_24,
        2 => scale::FS_18,
        _ => scale::FS_15,
    }
}

/// `blocks` as one column of elements.
pub fn render(blocks: &[Block], setting: &Setting, chrome: &CodeChrome<'_>) -> Div {
    let mut column = div().class("mq-md").w_full().flex_col();
    for (i, block) in blocks.iter().enumerate() {
        column = column.child(render_block(block, setting, chrome, i));
    }
    column
}

fn render_block(block: &Block, setting: &Setting, chrome: &CodeChrome<'_>, index: usize) -> Div {
    match block {
        Block::Paragraph(inline) => {
            div()
                .class("mq-md-p")
                .w_full()
                .child(inline_text(inline, setting, setting.size, false))
        }
        Block::Heading(level, inline) => div().class("mq-md-h").w_full().child(inline_text(
            inline,
            setting,
            heading_size(*level),
            true,
        )),
        Block::Code { lang, code } => code_block(
            lang.as_deref(),
            code,
            &format!("{}.{index}", chrome.key),
            chrome,
            setting.width,
        ),
        Block::Quote(blocks) => {
            let sub = CodeChrome {
                key: &format!("{}.{index}", chrome.key),
                copy_label: chrome.copy_label,
            };
            let quiet = Setting {
                color: setting.palette.muted,
                width: setting.width - QUOTE_INSET,
                ..*setting
            };
            div()
                .class("mq-md-quote")
                .w_full()
                .child(render(blocks, &quiet, &sub))
        }
        Block::List { start, items } => {
            let mut list = div().class("mq-md-list").w_full().flex_col();
            let inner = Setting {
                width: (setting.width - MARKER - LI_GAP).max(80.0),
                ..*setting
            };
            for (n, item) in items.iter().enumerate() {
                let marker = match (item.task, start) {
                    (Some(true), _) => "✓".to_string(),
                    (Some(false), _) => "○".to_string(),
                    (None, Some(first)) => format!("{}.", first + n as u64),
                    (None, None) => "•".to_string(),
                };
                let key = format!("{}.{index}.{n}", chrome.key);
                let sub = CodeChrome {
                    key: &key,
                    copy_label: chrome.copy_label,
                };
                list = list.child(
                    div()
                        .class("mq-md-li")
                        .w_full()
                        .flex_row()
                        .items_start()
                        .child(
                            div().class("mq-md-marker").w(MARKER).flex_shrink_0().child(
                                text(marker)
                                    .font(UI_FONT)
                                    .size(setting.size)
                                    .line_height(setting.line_height)
                                    .class("mq-text-subtle")
                                    .no_wrap(),
                            ),
                        )
                        .child(
                            div()
                                .w(inner.width)
                                .ml(LI_GAP / 4.0)
                                .flex_shrink_0()
                                .child(render(&item.blocks, &inner, &sub)),
                        ),
                );
            }
            list
        }
        Block::Table { head, rows } => table(head, rows, setting),
        Block::Rule => div().class("mq-md-rule").w_full().h(1.0),
    }
}

fn table(head: &[Inline], rows: &[Vec<Inline>], setting: &Setting) -> Div {
    use blinc_cn::prelude::{table as cn_table, table_body, table_cell, table_header, table_row};
    // Cell widths are the table's to decide: no explicit text width.
    let loose = Setting {
        width: 0.0,
        ..*setting
    };
    let cell = |inline: &Inline, bold: bool| {
        table_cell().child(inline_text(inline, &loose, scale::FS_13, bold))
    };
    let mut header = table_row();
    for h in head {
        header = header.child(cell(h, true));
    }
    let mut body = table_body();
    for row in rows {
        let mut r = table_row();
        for c in row {
            r = r.child(cell(c, false));
        }
        body = body.child(r);
    }
    div().class("mq-md-table").w_full().child(
        cn_table()
            .w_full()
            .child(table_header().child(header))
            .child(body),
    )
}

/// A fenced code block: a header with the language and a copy control,
/// then the highlighted code, wrapping only lines longer than the block.
/// Not a horizontal scroller: one under the pointer would take the
/// timeline's wheel events (`renderer/scroll.rs` routes a wheel to the
/// innermost scroller).
///
/// The text layout drops whitespace at the start of every line (no-break
/// spaces and joiners included), so indentation cannot live in the text.
/// Consecutive lines with the same indentation become one rich text behind
/// a spacer of that many monospace cells: indentation survives, and a block
/// costs a node per indentation change rather than per line (a streaming
/// reply's row is rebuilt as blocks settle, at a cost per node).
pub fn code_block(
    lang: Option<&str>,
    code: &str,
    key: &str,
    chrome: &CodeChrome<'_>,
    width: f32,
) -> Div {
    let highlighted = highlight::highlight(lang, code);
    let p = Palette::current();
    let label = lang.unwrap_or("text").to_string();
    let copy_text = code.to_string();
    let inner = if width > 0.0 {
        (width - CODE_INSET).max(80.0)
    } else {
        0.0
    };
    let mut body = div()
        .class("mq-code-body")
        .w_full()
        .flex_col()
        .overflow_clip();
    for group in indent_groups(&highlighted.lines) {
        body = body.child(code_group(
            &highlighted.lines[group.lines],
            group.cells,
            inner,
            p.text,
        ));
    }
    div()
        .class("mq-code")
        .w_full()
        .flex_col()
        .child(
            div()
                .class("mq-code-head")
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    text(label)
                        .font(MONO_FONT)
                        .size(scale::FS_11)
                        .class("mq-text-subtle")
                        .no_wrap(),
                )
                .child(copy_button(
                    &format!("{key}.copy"),
                    chrome.copy_label,
                    copy_text,
                )),
        )
        .child(body)
}

/// The code block's hairlines and padding (`.mq-code`, `.mq-code-body`).
const CODE_INSET: f32 = 2.0 + 2.0 * 12.0;

/// The advance of one monospace cell at the code size.
fn mono_cell() -> f32 {
    static CELL: OnceLock<f32> = OnceLock::new();
    *CELL.get_or_init(|| {
        let options =
            blinc_layout::text_measure::TextLayoutOptions::new().with_font_name(MONO_FONT);
        let w = blinc_layout::text_measure::measure_text_with_options(
            "MMMMMMMMMM",
            scale::MONO,
            &options,
        )
        .width
            / 10.0;
        if w > 0.0 {
            w
        } else {
            scale::MONO * 0.6
        }
    })
}

/// How many monospace cells a line's indentation takes (a tab is four),
/// and where its text starts.
pub fn indentation(line: &str) -> (usize, usize) {
    let mut cells = 0;
    let mut bytes = 0;
    for b in line.bytes() {
        match b {
            b' ' => cells += 1,
            b'\t' => cells += 4,
            _ => break,
        }
        bytes += 1;
    }
    (cells, bytes)
}

/// A run of lines sharing one indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentGroup {
    pub lines: std::ops::Range<usize>,
    pub cells: usize,
}

/// Split lines into runs of equal indentation. A blank line joins the run
/// it is in.
pub fn indent_groups(lines: &[StyledLine]) -> Vec<IndentGroup> {
    let mut groups: Vec<IndentGroup> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let blank = line.text.trim().is_empty();
        let cells = indentation(&line.text).0;
        match groups.last_mut() {
            Some(g) if blank || g.cells == cells => g.lines.end = i + 1,
            _ => groups.push(IndentGroup {
                lines: i..i + 1,
                cells: if blank { 0 } else { cells },
            }),
        }
    }
    groups
}

/// One indentation run: a spacer, then its lines (indentation removed) as
/// one wrapping rich text.
fn code_group(lines: &[StyledLine], cells: usize, width: f32, color: Color) -> Div {
    let stripped: Vec<StyledLine> = lines
        .iter()
        .map(|line| {
            let (_, indent) = indentation(&line.text);
            let rest = &line.text[indent..];
            let spans = line
                .spans
                .iter()
                .filter_map(|s| {
                    let start = s.start.max(indent) - indent;
                    let end = s.end.saturating_sub(indent).min(rest.len());
                    (start < end).then(|| {
                        let mut s = s.clone();
                        s.start = start;
                        s.end = end;
                        s
                    })
                })
                .collect();
            StyledLine::new(rest, spans)
        })
        .collect();
    let pad = cells as f32 * mono_cell();
    let mut text = rich_text_styled(StyledText::from_lines(stripped))
        .size(scale::MONO)
        .line_height(1.5)
        .default_color(color)
        .font_family(FontFamily::named(MONO_FONT));
    if width > 0.0 {
        text = text.w((width - pad).max(40.0));
    }
    let mut row = div().w_full().flex_row();
    if cells > 0 {
        row = row.child(div().w(pad).flex_shrink_0());
    }
    row.child(text)
}

/// A small text control that copies `content`.
pub fn copy_button(id: &str, label: &'static str, content: String) -> Div {
    div()
        .id(id)
        .class("mq-copy")
        .flex_shrink_0()
        .cursor_pointer()
        .child(
            text(label)
                .font(UI_FONT)
                .size(scale::FS_11)
                .no_wrap()
                .no_cursor(),
        )
        .on_click(move |_| {
            if !blinc_layout::widgets::text_edit::clipboard_write(&content) {
                tracing::warn!("the clipboard did not take the copy");
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(blocks: &[Block], i: usize) -> &Inline {
        match &blocks[i] {
            Block::Paragraph(p) => p,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_long_paragraph_is_one_run_with_spans_not_many_runs() {
        let text = format!(
            "{} **bold** and `code` and [a link](https://x.y).",
            "word ".repeat(200)
        );
        let blocks = parse_uncached(&text);
        assert_eq!(blocks.len(), 1);
        let p = para(&blocks, 0);
        assert!(p.text.starts_with("word word"));
        let styles: Vec<&Style> = p.spans.iter().map(|(_, s)| s).collect();
        assert!(styles[0].bold);
        assert!(styles[1].code);
        assert_eq!(styles[2].link.as_deref(), Some("https://x.y"));
        assert_eq!(&p.text[p.spans[0].0.clone()], "bold");
    }

    #[test]
    fn fences_keep_their_language() {
        let blocks = parse_uncached("```rust title=x\nfn main() {}\n```\n\n~~~\nplain\n~~~");
        assert_eq!(
            blocks[0],
            Block::Code {
                lang: Some("rust".into()),
                code: "fn main() {}".into()
            }
        );
        assert_eq!(
            blocks[1],
            Block::Code {
                lang: None,
                code: "plain".into()
            }
        );
    }

    #[test]
    fn lists_quotes_tables_and_headings() {
        let md = "# Title\n\n- [x] done\n- [ ] todo\n\n3. three\n4. four\n\n> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n";
        let blocks = parse_uncached(md);
        assert!(matches!(&blocks[0], Block::Heading(1, i) if i.text == "Title"));
        match &blocks[1] {
            Block::List { start: None, items } => {
                assert_eq!(items[0].task, Some(true));
                assert_eq!(items[1].task, Some(false));
                assert!(matches!(&items[0].blocks[0], Block::Paragraph(p) if p.text == "done"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&blocks[2], Block::List { start: Some(3), items } if items.len() == 2));
        assert!(matches!(&blocks[3], Block::Quote(inner) if inner.len() == 1));
        match &blocks[4] {
            Block::Table { head, rows } => {
                assert_eq!(head.len(), 2);
                assert_eq!(rows[0][1].text, "2");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(blocks[5], Block::Rule);
    }

    #[test]
    fn soft_breaks_join_and_hard_breaks_stay() {
        let blocks = parse_uncached("one\ntwo  \nthree");
        assert_eq!(para(&blocks, 0).text, "one two\nthree");
    }

    #[test]
    fn code_lines_group_by_indentation_and_blank_lines_join() {
        let lines: Vec<StyledLine> = ["fn a() {", "    x;", "", "    y;", "}", "\tz"]
            .iter()
            .map(|t| StyledLine::new(*t, vec![]))
            .collect();
        let groups = indent_groups(&lines);
        let shape: Vec<(std::ops::Range<usize>, usize)> =
            groups.into_iter().map(|g| (g.lines, g.cells)).collect();
        assert_eq!(shape, vec![(0..1, 0), (1..4, 4), (4..5, 0), (5..6, 4)]);
        assert_eq!(indentation("\tlet"), (4, 1));
    }

    #[test]
    fn the_cache_returns_the_same_parse() {
        let a = parse("cached *text*");
        let b = parse("cached *text*");
        assert!(Arc::ptr_eq(&a, &b));
    }
}
