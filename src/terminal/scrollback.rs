//! What a pane showed, for panes Herdr keeps nothing above.
//!
//! Herdr holds scrollback for programs that print and let the text roll off the
//! top. It holds none for programs that repaint an alternate screen -- Claude
//! Code wrapped by the app, opencode, nvim -- and reports
//! `scroll.max_offset_from_bottom: 0` for them. The reader can never pull those
//! panes back, because there is nothing above the viewport to pull: card #646
//! measured the same pane at 240, 480, 2000 and 5000 lines and got the same
//! 7398 bytes every time.
//!
//! The gateway already reads those panes, repeatedly, for its own reasons. This
//! keeps what it saw.
//!
//! ## What this can and cannot do
//!
//! It can only keep what it watched. Nothing printed before the first read is
//! recoverable, because nobody kept it. It is memory only: **a gateway restart
//! loses every buffer**, which is the deliberate trade -- the alternative is
//! writing terminal contents to disk, and the push design already refuses to do
//! that for privacy.
//!
//! It never touches a pane Herdr reports real scrollback for. Those panes are
//! answered exactly as they were before this module existed.
//!
//! ## How a repainting screen becomes history
//!
//! A stream can be spliced on a seam: the app's `mergeTerminalWindow` finds the
//! longest prefix of the incoming window that is a suffix of the retained one
//! and appends the remainder. A repainting screen has no such seam. It is a
//! mutable rectangle, and two consecutive reads of it differ wherever a spinner
//! turned, whether or not anything scrolled.
//!
//! So the buffer is modelled as `[history] + [the screen as last seen]`, and
//! each read is placed against it. Wherever a read is placed, it replaces
//! everything held from that point down -- the newest rendering of a row is the
//! true one, which is what makes the stale half of a repaint go away -- and
//! everything held above it is history.
//!
//! ## Placing a read
//!
//! This is `PaneBuffer::fold`. It was rewritten against six hours of recorded
//! reads (`MUQUN_SCROLLBACK_TRACE_DIR`, replayed by `scrollback_replay`), on
//! which the placement it replaced held the same 77-row Claude Code screen six
//! times over within twelve minutes, and the phone showed every copy.
//!
//! **Rows are compared by their text.** A row is matched on what a reader sees
//! of it -- escape sequences stripped, trailing blanks dropped (`row_text`) --
//! never on its bytes. Claude Code repaints rows it has not changed and is free
//! to encode them differently each time: `● ` with the space inside the colour
//! run on one frame and outside it on the next, a blank row as `""`, `" "` or
//! `"  "`. Compared as bytes, a repainted screen shared almost nothing with the
//! frame before it, nothing placed, and the whole read went on the end. That
//! one change removed most of the duplication on the ANSI buffer the terminal
//! view and the phone's chat view read; the text buffer `/parts` reads had the
//! same fault with blank rows.
//!
//! **Blank rows are not evidence.** The read is placed by the longest run of
//! text rows it shares with what is held at one alignment, blank rows skipped
//! on both sides (`alignment`), grown upward past rows repainted in place while
//! [`MATCH_THRESHOLD`] of what it covers agrees. A longest run anywhere, rather
//! than a run anchored at either end of the read, is what survives everything a
//! full-screen agent does to its viewport: a read taken halfway through a
//! repaint (the top already moved, the bottom not yet), a prompt pinned to the
//! top row (Codex, and Claude Code while its transcript is scrolled back), a
//! transcript that moved *down* because the area under it shrank (a feedback
//! box dismissed, an agent gone from the roster), runs of blank rows Claude
//! Code leaves in its own transcript and closes up again.
//!
//! **Furniture is left out.** What a read and the one before it end with is the
//! pane's furniture -- composer, mode line and its clock, roster, a status row
//! repainted in place (`common_suffix`, `status_rows_above`) -- and is cut from
//! both sides before the search, so a pinned box never lines up with itself
//! and passes for a placement.
//!
//! What the read's own rows above the run are decides what happens to them:
//! the same as in the frame before (a pinned row), blank, or mostly held just
//! above the run already (the run broke on a row the buffer lacks) -- skipped;
//! otherwise rows the buffer never had, which the screen moving down brought
//! into view, and kept in place above the run.
//!
//! ## A screen that moved back
//!
//! An agent does not only scroll forward. Claude Code can land its screen on an
//! older stretch of the transcript, and a block collapsing or a line re-wrapping
//! moves it back a few rows. A run that lies wholly in history (above the
//! screen as last seen) is an older stretch redrawn: the read changes nothing,
//! because the newer rows below it are still the truth. A run that reaches the
//! screen, in a read that shows nothing below it while newer text is held
//! there, moved the screen back over those rows: they come off but are set
//! aside, and go back where they were when a read comes down past them -- or
//! when nothing agrees any more, under the new screen, because they were shown.
//!
//! ## A read that agrees with nothing
//!
//! Output that outran the poll, a cleared screen, or a panel drawn over the
//! whole screen. The read goes on top of the screen it followed, less that
//! screen's furniture and less whatever text rows of it the buffer already
//! ends with (`text_seam`). If the read kept none of the furniture -- a panel
//! like `/usage` hides the composer, output never does -- the screen it
//! covered is remembered (`Covered`), and the read that brings that screen
//! back takes the panel off again. A pane resized under the reader re-wraps
//! every row; what is held was laid out for a screen that no longer exists,
//! and the buffer starts again from the reads after it ([`RESIZE_SETTLING_READS`]).
//!
//! Every path drops from the tail and appends -- none splices into the middle --
//! so the buffer stays in arrival order. That is the invariant the reader
//! needs: a shorter history is a nuisance, a reordered one is a bug report.
//!
//! # The pane read/hold contract (card #721)
//!
//! This module is one half of a contract whose other half is
//! `muqun/src/terminal/history.ts`. The full statement lives there, at the top
//! of that file, because that is the end which has to reconcile *four* sources
//! into one window. This is the part of it that binds the gateway.
//!
//! ## What the gateway is authoritative for
//!
//! **The rows it was given, in the order it was given them. Nothing else.**
//!
//! It is not authoritative for depth. It may hand back a window deeper than the
//! read it just took -- that is the whole purpose of the ring -- but it may
//! never be *assumed* to hand back one at least as deep as the reader already
//! holds, because it has no way of knowing what that is. A gateway restart
//! empties every buffer, and the answer that follows is one screen. If the app
//! replaced its window with that, a reader would lose their history to a restart
//! they never saw.
//!
//! So the app treats every HTTP answer as *placeable*, not authoritative, and
//! nothing in this module may assume otherwise. That is the reason the
//! reconciliation lives at the app end and this end simply tells the truth about
//! what it has.
//!
//! ## The rules both halves keep, in the same words
//!
//! | | here | there |
//! |---|---|---|
//! | agreement threshold | [`MATCH_THRESHOLD`] | `SCREEN_MATCH_THRESHOLD` |
//! | anchored run | [`ANCHOR_ROWS`] | `SCREEN_ANCHOR_ROWS` |
//! | furniture cap | [`FURNITURE_SHARE`] | `FURNITURE_SHARE` |
//! | volatile allowance | [`FURNITURE_VOLATILE_ROWS`] | `FURNITURE_VOLATILE_ROWS` |
//!
//! A number that changes on one side and not the other is a bug, and the shape
//! it takes is a row written down twice. The app's ratio floor, seam skew and
//! backward reach have no counterpart here any more: this end now places by
//! the longest run of text rows ("Placing a read" above), and the app's own
//! placement should follow it -- matching on text, not bytes, is the part that
//! matters most.
//!
//! ## The four invariants
//!
//! - **(a) arrival order.** Every path here drops from the tail and appends;
//!   none splices. Rows a read repaints are replaced by their newest rendering.
//! - **(b) depth is never reduced.** The gateway grows a buffer or trims it from
//!   the *top* at [`MAX_PANE_LINES`]. It has no operation that shortens history
//!   from the bottom, and must not grow one.
//! - **(c) furniture is never history.** [`ScrollbackStore::record`] -- and see
//!   the correction below, because this rule was not working.
//! - **(d) identical adjacent blocks never accumulate.** `text_seam` is the
//!   floor here; the app collapses whatever gets past it.
//!
//! ## The correction card #721 made here
//!
//! The furniture rule was written against an exact common suffix, and on the
//! very pane it was written for it therefore almost never fired.
//!
//! An agent's composer is not a still image. Its mode line carries a timer --
//! `4m 46s · ↓ 2.9k tokens` -- that changes on **every** frame whether or not
//! anything scrolled, and it sits *inside* the box rather than below it. An
//! exact `common_suffix` walks up from the bottom, meets the clock, and stops:
//! it reports one or two rows of furniture where there are eight. The rule that
//! exists precisely to keep a composer out of a transcript was being defeated by
//! the single most volatile row in that composer.
//!
//! Agreement is scored here now, for the same reason it is scored everywhere
//! else in this file: a repainting rectangle cannot be asked to match exactly.
//! [`FURNITURE_VOLATILE_ROWS`] rows may disagree outright, and past that
//! [`MATCH_THRESHOLD`] applies -- the same shape the app uses, reached from the
//! same measurement.
//!
//! The app also found, by soaking a live pane for an hour, that the *order*
//! matters: the furniture is what hides the seam, so it has to come off before a
//! placement is given up on. This module already had that right -- `record`
//! strips before it places -- which is why that half of the bug showed up over
//! there and not here.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Rows kept per pane and source, matching the ceiling on a single read.
/// Whether the gateway keeps rows for panes the backend reports no scrollback for.
pub const SCROLLBACK_ENABLED: bool = true;

pub const MAX_PANE_LINES: usize = 5_000;

/// Bytes kept per pane and source. A row carrying escape sequences has no
/// useful maximum length, so the row count alone does not bound memory.
pub const MAX_PANE_BYTES: usize = 2 * 1024 * 1024;

/// Bytes kept across every pane. The whole store cannot outgrow this.
pub const MAX_TOTAL_BYTES: usize = 24 * 1024 * 1024;

/// How many pane-and-source buffers are kept at once. The least recently fed is
/// evicted whole; a pane nobody has looked at in a long time is the cheapest
/// thing to forget.
pub const MAX_BUFFERS: usize = 48;

/// How many reads after a pane is resized replace the buffer rather than being
/// placed against it -- see `PaneBuffer::fold`. A resize is not one repaint:
/// the read that first shows the new width can still carry rows wrapped for the
/// old one.
const RESIZE_SETTLING_READS: usize = 3;

/// How much of a run grown past rows repainted in place, or of a composer, has
/// to agree.
const MATCH_THRESHOLD: f64 = 0.8;

/// How many rows back from the end of the buffer a read is looked for. Bounds
/// the alignment scan on the wide reads (`lines=2000`) the reader's paging
/// makes.
const MAX_OVERLAP: usize = 2_048;

/// How many text rows a run has to share with what is held before a read is
/// placed by it.
///
/// Three consecutive identical text rows is weak evidence taken alone and
/// strong evidence here, because blank rows are not counted and the run has
/// to reach the screen as last seen. Two would admit coincidences between a
/// rule and a prompt; four would miss the tightest real bursts, where the
/// transcript scrolls to within a few rows of the whole screen.
const ANCHOR_ROWS: usize = 3;

/// Rows at the bottom of a read's transcript that may sit over held rows they
/// do not agree with and still be taken for status rather than history.
///
/// An agent's status region is not all furniture: the spinner row above the
/// composer (`* Compacting conversation… (1m 4s)`) and a roster row can change
/// on every frame without two consecutive frames sharing them, so the
/// furniture rule never strips them. Two is that region and nothing more.
const REDRAW_SLACK_ROWS: usize = 2;

/// The most of a read that may be called furniture rather than history, as a
/// divisor: a third. An agent's composer is eight rows of sixty-five, and a pane
/// that legitimately repaints a long identical tail keeps its history.
const FURNITURE_SHARE: usize = 3;

/// How many rows of a composer may disagree outright before the run is refused.
///
/// One: the mode-line timer. It is the row that changes on every frame whether
/// or not anything scrolled, and it is the reason an exact common suffix never
/// recognised the box this rule was written to recognise. The app's
/// `FURNITURE_VOLATILE_ROWS` is the same number for the same reason.
const FURNITURE_VOLATILE_ROWS: usize = 1;

/// How many rows carrying text a repeated tail must have before it is furniture.
///
/// Two. No matching rows is a pair of screens with room at the bottom, and one
/// is a coincidence; a composer is a rule, a prompt and a mode line, and never
/// fewer than two of them survive the clock sitting among them.
const FURNITURE_MIN_ROWS: usize = 2;

/// Rows of a read, with the line endings the app's own splitter normalizes.
///
/// `\r\n` and a bare `\r` both become one break. This is load-bearing rather
/// than tidy: the app aligns windows on whole-line identity after exactly this
/// normalization, and a buffer that kept `\r` would fail to find overlaps the
/// app finds and would hand back rows the app then failed to splice.
pub fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// What a reader sees of a row: its text without escape sequences, without
/// trailing blanks.
///
/// Rows are matched on this, never on their bytes. A full-screen agent repaints
/// rows it has not changed, and the repaint is free to encode the same cells
/// differently: Claude Code draws `● ` with the space inside the colour run on
/// one frame and `●` + reset + ` ` on the next, and leaves a blank row as `""`,
/// `" "` or `"  "` depending on what the cells held before. Hashed as bytes,
/// such a screen shared almost no rows with the frame before it, no placement
/// believed it, and the whole read went on the end: the live gateway held the
/// same 77-row screen six times over in twelve minutes of one Claude pane
/// (`scrollback_replay`, `w17:p1`, 21:10-21:24).
///
/// Nor on a sidebar. OpenCode draws one down the right-hand side of the same
/// rows its transcript scrolls in -- the session title, a token count, MCP
/// status -- and it stays put while the transcript moves under it, so every
/// row that scrolled carries a different piece of sidebar after it. Compared
/// whole, no row of a read matched the row it had been one read before, and
/// the transcript went on again and again (`wZ:p2`, 23:49-23:52: one run held
/// four times). Whatever follows a gap of [`SIDEBAR_GAP`] blanks from column
/// [`SIDEBAR_COLUMN`] on is left out (`without_sidebar`).
fn row_text(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(without_sidebar(line.trim_end()));
    }
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            text.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates up to one final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC (a hyperlink, a title): up to BEL or ST.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    let kept = without_sidebar(text.trim_end()).len();
    text.truncate(kept);
    std::borrow::Cow::Owned(text)
}

/// From this display column on, text after a gap is taken for a sidebar.
const SIDEBAR_COLUMN: usize = 60;

/// How many blank columns separate a transcript row from a sidebar beside it.
const SIDEBAR_GAP: usize = 3;

/// `text` up to a sidebar drawn beside it -- see `row_text`. A row that is
/// only sidebar comes back blank.
fn without_sidebar(text: &str) -> &str {
    let (mut column, mut blanks, mut end) = (0, 0, 0);
    for (at, c) in text.char_indices() {
        if c == ' ' {
            blanks += 1;
        } else {
            if blanks >= SIDEBAR_GAP && column >= SIDEBAR_COLUMN {
                return text[..end].trim_end();
            }
            blanks = 0;
            end = at + c.len_utf8();
        }
        column += if is_wide(c) { 2 } else { 1 };
    }
    text
}

/// Whether a terminal draws `c` two columns wide: the East Asian wide and
/// fullwidth blocks and the emoji planes, which is what a transcript holds.
fn is_wide(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD)
}

/// Whether a row shows anything once its escapes and blanks are gone.
fn row_carries(line: &str) -> bool {
    !row_text(line).trim().is_empty()
}

fn line_hash(line: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    row_text(line).hash(&mut hasher);
    hasher.finish()
}

/// How many of the read's rows the buffer already ends with, by text rows: the
/// longest run of the read's first text rows that are the last text rows held,
/// blank rows ignored on both sides, as the count of the read's rows through
/// the last of them. A seam too short to be an alignment is still not written
/// down twice.
fn text_seam(held: &[u64], frame: &[u64], carries: &[bool]) -> usize {
    let blank = line_hash("");
    let read: Vec<usize> = (0..frame.len()).filter(|row| carries[*row]).collect();
    let kept: Vec<u64> = held.iter().copied().filter(|row| *row != blank).collect();
    (1..=read.len().min(kept.len()))
        .rev()
        .find(|count| {
            kept[kept.len() - count..]
                .iter()
                .zip(&read[..*count])
                .all(|(held_row, read_row)| *held_row == frame[*read_row])
        })
        .map_or(0, |count| read[count - 1] + 1)
}

/// Where a read agrees with what is held: the longest run of text rows the two
/// share at one alignment, blank rows left out of the comparison on both
/// sides, grown upward past rows repainted in place while
/// [`MATCH_THRESHOLD`] of what it covers still agrees.
///
/// Blank rows say nothing about where a screen is, and they are what a repaint
/// most often adds or takes away: Claude Code leaves runs of them in its own
/// transcript and closes them up again, and draws a blank row as `""`, `" "`
/// or `"  "` as the cells happen to hold. The longest run, rather than a run
/// from either end of the read, is what survives a read taken mid-repaint (the
/// top already moved, the bottom not yet), a pinned top row, and a screen that
/// moved down when the area below the transcript shrank.
///
/// A run reaching into the screen as last seen (`screen_start` on) is
/// preferred to any run wholly in history, and needs [`ANCHOR_ROWS`] text rows
/// -- or every text row of a read, or of what is held, with fewer. A run wholly
/// in history is only reported when it holds a quarter of the read's text
/// rows: an older screen redrawn, which the caller leaves alone. Ties go to the run
/// nearer the end.
fn alignment(
    held: &[u64],
    body: &[u64],
    carries: &[bool],
    screen_start: usize,
) -> Option<Alignment> {
    let blank = line_hash("");
    let read: Vec<usize> = (0..body.len()).filter(|row| carries[*row]).collect();
    let kept: Vec<usize> = (0..held.len()).filter(|row| held[*row] != blank).collect();
    if read.is_empty() || kept.is_empty() {
        return None;
    }
    // Longest common runs, by the usual table kept one row at a time: the
    // best ending in the screen, and the best anywhere.
    let mut previous = vec![0usize; read.len() + 1];
    let mut current = vec![0usize; read.len() + 1];
    let (mut screen_best, mut any_best) = ((0usize, 0usize, 0usize), (0usize, 0usize, 0usize));
    for (h, held_row) in kept.iter().enumerate() {
        for (r, read_row) in read.iter().enumerate() {
            let run = if held[*held_row] == body[*read_row] {
                previous[r] + 1
            } else {
                0
            };
            current[r + 1] = run;
            if run > 0 && run >= any_best.0 {
                any_best = (run, h, r);
            }
            if run > 0 && *held_row >= screen_start && run >= screen_best.0 {
                screen_best = (run, h, r);
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let floor = ANCHOR_ROWS.min(read.len()).min(kept.len());
    let (run, last_held, last_read) = if screen_best.0 >= floor {
        screen_best
    } else if any_best.0 >= floor.max(read.len() / 4) {
        any_best
    } else {
        return None;
    };
    let (mut first_held, mut first_read) = (last_held + 1 - run, last_read + 1 - run);
    // Grow upward across rows repainted in place: a counter, a spinner word.
    let (mut agreed, mut compared) = (run, run);
    let (mut held_at, mut read_at) = (first_held, first_read);
    while held_at > 0 && read_at > 0 {
        held_at -= 1;
        read_at -= 1;
        compared += 1;
        if held[kept[held_at]] == body[read[read_at]] {
            agreed += 1;
            if agreed as f64 >= compared as f64 * MATCH_THRESHOLD {
                first_held = held_at;
                first_read = read_at;
            }
        } else if (agreed as f64) < compared as f64 * MATCH_THRESHOLD {
            break;
        }
    }
    Some(Alignment {
        held_start: kept[first_held],
        held_end: kept[last_held] + 1,
        read_start: read[first_read],
        read_end: read[last_read] + 1,
    })
}

/// Where [`alignment`] placed a read: the held rows `held_start..held_end` are
/// the read's rows `read_start..read_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Alignment {
    held_start: usize,
    held_end: usize,
    read_start: usize,
    read_end: usize,
}

/// The width of the widest box rule on a screen (`────…`, ten or more), which
/// a full-screen agent draws edge to edge: the pane's width, where it can be
/// read off the rows at all. `None` for a screen with no rule, whose width is
/// unknown rather than changed.
fn rule_width(lines: &[String]) -> Option<usize> {
    lines
        .iter()
        .map(|line| row_text(line))
        .map(|text| text.trim().to_owned())
        .filter(|text| text.chars().count() >= 10 && text.chars().all(|c| c == '─'))
        .map(|text| text.chars().count())
        .max()
}

/// A row's shape: its text with every run of digits made one `#`, so a clock
/// or a counter repainted in place keeps its shape while its text changes --
/// `(9s)` and `(10s)` included.
fn shape_hash(line: &str) -> u64 {
    let text = row_text(line);
    let mut shape = String::with_capacity(text.len());
    for c in text.chars() {
        if !c.is_ascii_digit() {
            shape.push(c);
        } else if !shape.ends_with('#') {
            shape.push('#');
        }
    }
    line_hash(&shape)
}

/// The furniture, grown upward by the status rows just above it that were
/// repainted in place: a spinner and its clock (`✻ Swirling… (1m 22s · ↓ 3.5k
/// tokens)`), which no two frames share, so `common_suffix` stops below it.
///
/// A row counts when it sits at the same distance from the bottom in both
/// frames, changed, kept its shape (`shape_hash`), and no other row of the
/// read has that shape -- a transcript row rarely has a twin on the same
/// screen once its digits are ignored, but a numbered list does, and is left
/// alone. At most [`REDRAW_SLACK_ROWS`] of them, and only above furniture that
/// was found, because a status region is what furniture sits under.
///
/// Without this the spinner of the frame before was kept whenever a read could
/// not be placed -- the first read after a gap, or after the gateway started --
/// and stayed in the middle of history with the transcript resuming below it.
fn status_rows_above(
    previous: &[u64],
    previous_shapes: &[u64],
    current: &[u64],
    shapes: &[u64],
    carries: &[bool],
    furniture: usize,
) -> usize {
    if furniture == 0 || previous_shapes.len() != previous.len() {
        return furniture;
    }
    let mut rows = furniture;
    while rows - furniture < REDRAW_SLACK_ROWS && rows < previous.len() && rows < current.len() {
        let here = current.len() - rows - 1;
        let there = previous.len() - rows - 1;
        let repainted = carries[here]
            && current[here] != previous[there]
            && shapes[here] == previous_shapes[there]
            && shapes
                .iter()
                .filter(|shape| **shape == shapes[here])
                .count()
                == 1;
        if !repainted {
            break;
        }
        rows += 1;
    }
    rows
}

/// How many rows two frames end with in common, allowing for the ones a repaint
/// changed without anything scrolling.
///
/// This used to be an exact `take_while`, and on an agent pane it therefore
/// stopped at the first row of the composer -- the mode-line timer, which
/// changes on every frame and sits *inside* the box. Eight rows of furniture
/// were reported as one or two, and the rule that keeps a composer out of the
/// transcript did not fire on the pane it was written for. See the header.
///
/// So the run is scored:
/// [`FURNITURE_VOLATILE_ROWS`] rows may disagree outright -- the clock -- and
/// beyond that [`MATCH_THRESHOLD`] of the run has to agree. The longest run
/// satisfying both wins. `carries` says which rows of `current` hold text: a
/// tail of blank rows is two screens with room at the bottom, not a composer,
/// and dropping it would eat the transcript above it.
fn common_suffix(previous: &[u64], current: &[u64], carries: &[bool]) -> usize {
    let widest = previous.len().min(current.len());
    let mut matched = 0usize;
    let mut carried = 0usize;
    let mut best = 0usize;
    for rows in 1..=widest {
        if previous[previous.len() - rows] == current[current.len() - rows] {
            matched += 1;
            if carries[current.len() - rows] {
                carried += 1;
            }
        }
        let missed = rows - matched;
        let allowed = FURNITURE_VOLATILE_ROWS.max((rows as f64 * (1.0 - MATCH_THRESHOLD)) as usize);
        if missed <= allowed && carried >= FURNITURE_MIN_ROWS {
            best = rows;
        }
    }
    best
}

#[derive(Debug, Default)]
struct PaneBuffer {
    /// Identity for pagination only; legacy tail folding does not use it.
    epoch: Option<uuid::Uuid>,
    snapshot_ready: bool,
    /// Rows dropped off the front at the size cap, ever. A trim slides the
    /// window over the same capture rather than starting a new one, so it
    /// does not change `epoch`; this is how a cursor tells whether the rows it
    /// points at are still held.
    trimmed: u64,
    lines: VecDeque<String>,
    hashes: VecDeque<u64>,
    bytes: usize,
    /// When this buffer was last fed, on the store's own clock.
    touched: u64,
    /// The frame this buffer last saw, by row hash. What two consecutive
    /// frames end with identically is the pane's furniture rather than its
    /// history -- an agent's composer box, a status line, a prompt -- and it
    /// belongs at the bottom of the buffer once, not scattered through it
    /// every time the screen jumped further than a read could follow.
    last_frame: Vec<u64>,
    /// The same frame by row *shape* -- see `shape_hash` -- for recognising a
    /// status row repainted in place, whose text changes on every frame.
    last_shapes: Vec<u64>,
    /// How many rows at the end of the buffer the last read put there: the
    /// screen as it was last seen, every row of which the next read may still
    /// repaint. Everything above it has scrolled off and is history.
    screen: usize,
    /// Rows a read showing an older stretch took off the end, kept until a
    /// read says whether the screen came back down past them (they go back)
    /// or nothing agrees with them any more (they go back too, under the new
    /// screen: they were shown, and are history). See `fold`.
    set_aside: Vec<String>,
    /// The screen as it was before a read that agreed with nothing went on
    /// top of it -- see `Covered` and `fold`.
    covered: Option<Covered>,
    /// Reads still to replace the buffer outright after a resize.
    settling: usize,
    /// The width of the last box rule a read drew -- see `rule_width`.
    last_rule: Option<usize>,
}

/// What a read that agreed with nothing went on top of, kept until the buffer
/// has grown past it: where that screen started, its rows as they were, and
/// the frame it was.
///
/// Such a read is either output that outran the poll -- history, kept -- or a
/// panel drawn over the whole screen for a moment: Claude Code's `/usage`,
/// `/config`, a dialog. When the panel closes, the screen under it comes back
/// and agrees with the rows the panel was put on top of, which by then read as
/// history; without this the panel stayed in the transcript with the prompt box
/// it covered frozen above it (`w17:p1`, read 13530). A read that agrees with
/// the covered screen down to its bottom -- the same screen back, not an older
/// stretch of the transcript -- takes the panel off again.
#[derive(Debug)]
struct Covered {
    start: usize,
    rows: Vec<String>,
    screen: usize,
    last_frame: Vec<u64>,
    last_shapes: Vec<u64>,
}

impl PaneBuffer {
    /// Fold one read into the buffer -- see the module doc, "Placing a read".
    fn fold(&mut self, incoming: Vec<String>, owns_screen: bool) {
        let frame: Vec<u64> = incoming.iter().map(|line| line_hash(line)).collect();
        let carries: Vec<bool> = incoming.iter().map(|line| row_carries(line)).collect();
        let shapes: Vec<u64> = incoming.iter().map(|line| shape_hash(line)).collect();

        // A pane resized under the reader: every row of the screen is wrapped
        // anew, so nothing held lines up with it and the read would go on
        // whole -- the same transcript a second time at a different width.
        // `w17:p1` went from 167 columns to 75 and back within three seconds
        // and kept both copies. What is held was laid out for a screen that no
        // longer exists, so it goes; the read starts the buffer again.
        let rule = rule_width(&incoming);
        let resized = matches!((self.last_rule, rule), (Some(was), Some(now)) if was != now);
        if rule.is_some() {
            self.last_rule = rule;
        }

        // A resize is not one repaint: the read that first shows the new
        // width can still carry rows wrapped for the old one, and the reads
        // after it finish the job. Until then each read replaces the last.
        if resized {
            self.settling = RESIZE_SETTLING_READS;
        }
        let settling = self.settling > 0;
        self.settling = self.settling.saturating_sub(1);
        if owns_screen || settling || self.lines.is_empty() {
            if !self.lines.is_empty() {
                self.epoch = Some(uuid::Uuid::new_v4());
            }
            self.drop_back(self.lines.len());
            self.set_aside.clear();
            self.covered = None;
            self.screen = incoming.len();
            self.keep(incoming, 0, frame, shapes);
            return;
        }
        if frame == self.last_frame {
            return;
        }
        if self.covered.as_ref().is_some_and(|covered| {
            self.lines.len() > covered.start + covered.rows.len() + 2 * frame.len()
        }) {
            self.covered = None;
        }

        // The bottom rows this read shares with the one before it are
        // furniture: a composer, a mode line with its clock, a spinner, a
        // roster. They are left out of the search on both sides, so a pinned
        // box lining up with itself is never taken for a placement.
        let furniture = status_rows_above(
            &self.last_frame,
            &self.last_shapes,
            &frame,
            &shapes,
            &carries,
            common_suffix(&self.last_frame, &frame, &carries),
        )
        .min(frame.len() / FURNITURE_SHARE);
        let cut = furniture.min(self.screen);
        let end = self.hashes.len() - cut;
        let start = end.saturating_sub(MAX_OVERLAP);
        // What is held as the read is placed against it: without the
        // furniture, and with the rows an older screen covered put back.
        let mut held: Vec<u64> = self.hashes.range(start..end).copied().collect();
        held.extend(self.set_aside.iter().map(|line| line_hash(line)));
        let body = &frame[..frame.len() - furniture];
        let screen_start = (self.hashes.len() - self.screen).saturating_sub(start);

        let Some(found) = alignment(&held, body, &carries, screen_start) else {
            // Nothing agrees: the screen moved further than one read can be
            // followed, or was cleared. The read goes on top of what it
            // followed, less that frame's furniture and whatever of it the
            // buffer verbatim ends with.
            // Only a read that kept none of the frame's furniture can be a
            // panel over the screen: output that outran the poll still has the
            // composer under it.
            if self.covered.is_none() && furniture == 0 {
                let start = self.lines.len() - self.screen;
                self.covered = Some(Covered {
                    start,
                    rows: self.lines.range(start..).cloned().collect(),
                    screen: self.screen,
                    last_frame: self.last_frame.clone(),
                    last_shapes: self.last_shapes.clone(),
                });
            }
            self.restore(cut);
            let seam = text_seam(&held, &frame, &carries);
            if seam > 0 {
                while self.hashes.back() == Some(&line_hash("")) {
                    self.drop_back(1);
                }
            }
            self.screen = frame.len() - seam;
            self.keep(incoming, seam, frame, shapes);
            return;
        };
        // Only rows already scrolled into history agree: an older screen
        // redrawn in place of the newest. The newer rows below it are still
        // the truth and the screen comes back to them.
        if found.held_end <= screen_start {
            let uncovers = self.covered.as_ref().is_some_and(|covered| {
                let end = covered.start + covered.rows.len();
                start + found.held_end + covered.rows.len() / FURNITURE_SHARE >= end
            });
            if uncovers {
                if let Some(covered) = self.covered.take() {
                    self.drop_back(self.lines.len() - covered.start);
                    self.set_aside.clear();
                    for line in covered.rows {
                        self.push(line);
                    }
                    self.screen = covered.screen;
                    self.last_frame = covered.last_frame;
                    self.last_shapes = covered.last_shapes;
                    self.fold(incoming, false);
                }
            }
            return;
        }
        self.restore(cut);
        // The screen moved back up over newer rows -- a re-wrap, a block
        // collapsed, or an older stretch shown for a while -- and shows
        // nothing below them. They come off, but are kept aside: a read that
        // comes back down past them puts them back where they were.
        let blank = line_hash("");
        let newer_held = held[found.held_end..].iter().any(|row| *row != blank);
        let newer_read = carries[found.read_end..body.len()].contains(&true);
        if newer_held && !newer_read {
            self.set_aside = self
                .lines
                .range(start + found.held_end..)
                .cloned()
                .collect();
        }
        // The read's own rows above the run: the same rows they were in the
        // frame before -- a prompt Codex pins to the top row -- or blank, and
        // skipped; or rows the buffer never had, brought into view above it by
        // the screen moving down, and kept.
        //
        // A head with half its text rows or more held just above the run is
        // the former: the run broke on a row the buffer lacks, and putting the
        // head in again would write the rest of it twice. (One row is not
        // enough: Claude Code pins the last prompt to the top row while its
        // transcript is scrolled back, and that row is held.)
        let reach = found.held_start.saturating_sub(2 * found.read_start + 2);
        let nearby: HashSet<u64> = held[reach..found.held_start].iter().copied().collect();
        let head: Vec<u64> = (0..found.read_start)
            .filter(|row| carries[*row])
            .map(|row| frame[row])
            .collect();
        let known = head.iter().filter(|row| nearby.contains(row)).count() * 2 >= head.len();
        let pinned = !carries[..found.read_start].contains(&true)
            || known
            || (found.read_start <= self.last_frame.len()
                && frame[..found.read_start] == self.last_frame[..found.read_start]);
        let skip = if pinned { found.read_start } else { 0 };
        self.drop_back(held.len() - found.held_start);
        self.screen = frame.len() - skip;
        self.keep(incoming, skip, frame, shapes);
    }

    /// Put the rows an older screen covered back on the end, in place of the
    /// last frame's `cut` furniture rows, so the end of the buffer is what the
    /// next placement was measured against.
    fn restore(&mut self, cut: usize) {
        self.drop_back(cut);
        for line in std::mem::take(&mut self.set_aside) {
            self.push(line);
        }
    }

    fn keep(&mut self, incoming: Vec<String>, skip: usize, frame: Vec<u64>, shapes: Vec<u64>) {
        for line in incoming.into_iter().skip(skip) {
            self.push(line);
        }
        self.trim();
        self.screen = self.screen.min(self.lines.len());
        self.last_frame = frame;
        self.last_shapes = shapes;
    }

    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.hashes.push_back(line_hash(&line));
        self.lines.push_back(line);
    }

    fn drop_back(&mut self, count: usize) {
        for _ in 0..count {
            match self.lines.pop_back() {
                Some(line) => {
                    self.bytes -= line.len();
                    self.hashes.pop_back();
                }
                None => break,
            }
        }
    }

    fn drop_front(&mut self, count: usize) {
        for _ in 0..count {
            match self.lines.pop_front() {
                Some(line) => {
                    self.bytes -= line.len();
                    self.hashes.pop_front();
                }
                None => break,
            }
        }
    }

    fn trim(&mut self) {
        let before = self.lines.len();
        if self.lines.len() > MAX_PANE_LINES {
            self.drop_front(self.lines.len() - MAX_PANE_LINES);
        }
        while self.bytes > MAX_PANE_BYTES && self.lines.len() > 1 {
            self.drop_front(1);
        }
        if self.lines.len() < before {
            // Positions held for later move with the front.
            self.covered = None;
            self.trimmed += (before - self.lines.len()) as u64;
        }
    }
}

/// Every pane's kept rows, and which panes are worth keeping them for.
#[derive(Debug, Default)]
pub struct ScrollbackStore {
    buffers: HashMap<String, PaneBuffer>,
    /// Panes worth recording synthetic history for, by `session/pane`. Only
    /// these are ever recorded or answered from.
    ///
    /// True for a pane whose own scrollback is frozen rather than merely
    /// small: one on an alternate screen (`alternate_on: true`), which tmux
    /// will never grow no matter how long the reader waits, or -- when the
    /// backend cannot say (Herdr; a tmux pane before its first list, where
    /// `alternate_on` is `None`) -- one Herdr reports zero rows above for.
    /// See `keeps`.
    kept: HashMap<String, bool>,
    /// Panes whose foreground program is a full-screen editor -- see
    /// `is_editor_command` -- by `session/pane`. Absent means unknown, which
    /// `owns_screen` below reads as `false`: a pane this store cannot
    /// positively identify as an editor keeps the accumulate-and-place
    /// behaviour every pane had before this field existed, which includes
    /// every agent pane (`is_editor_command`'s doc says why that has to be
    /// true).
    owns_screen: HashMap<String, bool>,
    identities: HashMap<String, PaneObservation>,
    total_bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct PaneObservation {
    identity: Value,
    size: Value,
    /// Request-start fence, independent of buffer existence and read shape.
    /// Rotated on observed pane/policy resets and destructive buffer resets.
    capture_generation: uuid::Uuid,
}

/// A read can contribute only to the pane generation observed before its I/O.
/// Unknown/ineligible panes get no fence and cannot become eligible mid-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureFence(uuid::Uuid);

/// `session/pane`, the key the zero-backlog verdict is held under.
fn pane_key(session_id: &str, pane_id: &str) -> String {
    format!("{session_id}/{pane_id}")
}

/// `session/pane/source/format`. Rows read as ANSI and rows read as plain text
/// are different rows and cannot be spliced against each other, so each read
/// shape keeps its own buffer.
fn read_key(session_id: &str, pane_id: &str, source: &str, format: &str) -> String {
    format!("{session_id}/{pane_id}/{source}/{format}")
}

/// Whether `command` -- a pane's `foreground_command`, tmux's own
/// `#{pane_current_command}` -- names a full-screen editor.
///
/// This, not `Pane::alternate_on`, is `record`'s discriminator (card #795).
/// `alternate_on` was tried first and measured wrong on a live machine: a
/// Claude Code pane reports `alternate_on=1` exactly like nvim's ("%1|claude|
/// alternate_on=1|history_size=0" next to "%27|nvim|alternate_on=1|
/// history_size=0"), because an agent wrapped in one also owns an alternate
/// screen -- it is precisely one of the panes this module's own doc says it
/// exists to serve. `foreground_command` is what `shortcuts::is_editor_title`
/// answers the same question from for keybindings, except against a pane's
/// title, a signal the program sets for itself and can leave stale or unset;
/// `foreground_command` is tmux's own report of the process actually running
/// and cannot be renamed away. Shares `shortcuts::EDITOR_PROGRAMS` rather than
/// a second list that could quietly drift from it.
fn is_editor_command(command: Option<&str>) -> bool {
    command.is_some_and(|command| {
        crate::shortcuts::EDITOR_PROGRAMS.contains(&command.trim().to_ascii_lowercase().as_str())
            || crate::shortcuts::is_lazygit_command(command)
    })
}

impl ScrollbackStore {
    /// Learn from a Herdr answer which panes have scrollback of their own that
    /// this store must stay out of, versus which are frozen and need it.
    ///
    /// Takes any `pane.list`, `pane.get` or `session.snapshot` body and walks it
    /// for pane objects, so it does not have to know the shape of each. The
    /// approval watcher already calls `pane.list` every 1500ms, which keeps this
    /// current without asking Herdr for anything new.
    ///
    /// A pane is kept when its foreground program owns an alternate screen:
    /// `alternate_on: true` means tmux will never write another row above it,
    /// however many it already has (a residue from before the program took
    /// the screen -- 29 lines for a Claude pane measured live -- is not
    /// meaningfully more useful than none, and unlike none it will never
    /// grow). `max_offset_from_bottom <= 0` remains the fallback for where
    /// `alternate_on` is unknown: a Herdr backend, or a tmux pane before its
    /// first list. Checking the offset alone, as this used to, answers "is
    /// there a little scrollback right now" -- a number that drifts with how
    /// much ran before the program switched screens -- rather than "will this
    /// pane's own scrollback ever include what is about to scroll off", which
    /// is the question this store actually needs answered and the one
    /// `alternate_on` answers directly.
    pub fn observe(&mut self, session_id: &str, value: &Value) {
        let mut passed_through = Vec::new();
        visit_panes(value, &mut |pane_id, pane| {
            let key = pane_key(session_id, pane_id);
            let identity = serde_json::json!([
                pane.get("terminal_id"),
                pane.get("workspace_id"),
                pane.get("tab_id")
            ]);
            let size = serde_json::json!([pane.get("width"), pane.get("height")]);
            if let Some(observed) = self.identities.get(&key) {
                if observed.identity != identity {
                    self.forget_pane(session_id, pane_id);
                } else if observed.size != size {
                    self.invalidate_capture(session_id, pane_id);
                }
            }
            let capture_generation = self
                .identities
                .get(&key)
                .map_or_else(uuid::Uuid::new_v4, |observed| observed.capture_generation);
            self.identities.insert(
                key,
                PaneObservation {
                    identity,
                    size,
                    capture_generation,
                },
            );
            if let Some(scroll) = pane.get("scroll").and_then(Value::as_object) {
                if let Some(maximum) = scroll.get("max_offset_from_bottom").and_then(Value::as_f64)
                {
                    let alternate = scroll.get("alternate_on").and_then(Value::as_bool);
                    let kept = alternate == Some(true) || maximum <= 0.0;
                    let was = self.kept.insert(pane_key(session_id, pane_id), kept);
                    if was.is_some_and(|was| was != kept) {
                        self.invalidate_capture(session_id, pane_id);
                    }
                    if !kept {
                        passed_through.push(pane_id.to_owned());
                    }
                }
            }
            if let Some(command) = pane.get("foreground_command").and_then(Value::as_str) {
                let owns_screen = is_editor_command(Some(command));
                let was = self
                    .owns_screen
                    .insert(pane_key(session_id, pane_id), owns_screen);
                if was.unwrap_or(false) != owns_screen {
                    self.invalidate_capture(session_id, pane_id);
                }
            }
        });
        for pane_id in passed_through {
            self.forget_pane(session_id, &pane_id);
        }
    }

    /// The same, for a listing that is known to be every pane in the session
    /// -- and which therefore also says which panes are *gone*.
    ///
    /// `kept` and `owns_screen` were insert-only. Their key is
    /// `session/pane`, a tmux pane id counts up for the life of the server and
    /// is never reused, and nothing ever removed from either map -- `evict`
    /// bounds `buffers` and only `buffers`. So a gateway left running
    /// accumulated one permanent entry per map for every pane that had ever
    /// existed, which on a machine using tasks is a pane per task, forever.
    ///
    /// `observe` cannot prune, because it is also handed a single `pane.get`,
    /// where an absent pane means nothing. A complete listing is the one
    /// caller that can tell "gone" from "not mentioned", so it is the one that
    /// prunes. The approval watcher calls it every 1500ms with exactly that.
    pub fn observe_listing(&mut self, session_id: &str, value: &Value) {
        self.observe(session_id, value);
        let mut live = HashSet::new();
        visit_panes(value, &mut |pane_id, _| {
            live.insert(pane_key(session_id, pane_id));
        });
        let prefix = format!("{session_id}/");
        let gone: Vec<String> = self
            .identities
            .keys()
            .filter(|key| key.starts_with(&prefix) && !live.contains(*key))
            .map(|key| key[prefix.len()..].to_owned())
            .collect();
        for pane in gone {
            self.forget_pane(session_id, &pane);
        }
        self.identities
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
        self.kept
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
        self.owns_screen
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
    }

    /// How many panes this store is holding facts about, for the tests that
    /// prove those maps are bounded.
    #[cfg(test)]
    fn remembered_pane_count(&self) -> usize {
        self.kept
            .keys()
            .chain(self.owns_screen.keys())
            .collect::<HashSet<_>>()
            .len()
    }

    /// Whether this pane is one the gateway keeps rows for.
    ///
    /// A pane nobody has reported on yet answers `false`: not knowing is a
    /// reason to stay out of the way, not a reason to guess.
    fn keeps(&self, session_id: &str, pane_id: &str) -> bool {
        if !SCROLLBACK_ENABLED {
            return false;
        }

        self.kept
            .get(&pane_key(session_id, pane_id))
            .copied()
            .unwrap_or(false)
    }

    /// Whether this pane's foreground program is a full-screen editor -- so a
    /// read that overlaps nothing held should replace rather than accumulate.
    /// See `record`'s own doc on why the two cases need different answers,
    /// `is_editor_command`'s doc on what this is keyed on and why, and
    /// `owns_screen`'s field doc on why unknown reads as `false` here.
    fn owns_screen(&self, session_id: &str, pane_id: &str) -> bool {
        self.owns_screen
            .get(&pane_key(session_id, pane_id))
            .copied()
            .unwrap_or(false)
    }

    /// Take immediately before initiating a backend read; never hold a guard
    /// during I/O. No mutable map entry is created for an unobserved pane.
    pub(crate) fn begin_capture(&self, session_id: &str, pane_id: &str) -> Option<CaptureFence> {
        if !self.keeps(session_id, pane_id) {
            return None;
        }
        self.identities
            .get(&pane_key(session_id, pane_id))
            .map(|observed| CaptureFence(observed.capture_generation))
    }

    fn advance_capture_generation(&mut self, session_id: &str, pane_id: &str) {
        if let Some(observed) = self.identities.get_mut(&pane_key(session_id, pane_id)) {
            observed.capture_generation = uuid::Uuid::new_v4();
        }
    }

    fn invalidate_capture(&mut self, session_id: &str, pane_id: &str) {
        self.advance_capture_generation(session_id, pane_id);
        let prefix = format!("{}/", pane_key(session_id, pane_id));
        for (read, buffer) in &mut self.buffers {
            if read.starts_with(&prefix) {
                buffer.epoch = Some(uuid::Uuid::new_v4());
                buffer.snapshot_ready = false;
            }
        }
    }

    fn accepts_capture(
        &self,
        session_id: &str,
        pane_id: &str,
        fence: Option<CaptureFence>,
    ) -> bool {
        fence.is_some() && fence == self.begin_capture(session_id, pane_id)
    }

    /// What the observation rule alone says about this pane, with the feature
    /// switch left out of it. The switch is a shipping decision; the rule is
    /// the thing the tests are about.
    #[cfg(test)]
    pub fn observed_as_kept(&self, session_id: &str, pane_id: &str) -> bool {
        self.kept
            .get(&pane_key(session_id, pane_id))
            .copied()
            .unwrap_or(false)
    }

    /// Fold a read into what is already held.
    ///
    /// `owns_screen` is `is_editor_command` on the pane's `foreground_command`,
    /// forwarded by the caller -- not `Pane::alternate_on`. An editor -- nvim
    /// among them -- repaints a static rectangle: nothing scrolls off the top,
    /// `history_size` stays 0, and every read of one *is* the whole of the
    /// pane's current truth, so it replaces outright rather than being placed
    /// against what came before. The two-stacked-frames bug this exists to
    /// prevent is exactly what "neither placement believed anything, so keep
    /// the read on top of what we had" produces for a screen that repainted
    /// rather than scrolled. An agent pane (Claude Code, opencode, codex, each
    /// wrapped in one) also owns an alternate screen -- `alternate_on` is 1
    /// for it exactly as it is for an editor's -- but its conversation
    /// genuinely scrolls away above the viewport, and accumulating what the
    /// gateway saw of it is this whole module's purpose (see its own doc), so
    /// it must not take this branch: `is_editor_command` reads `false` for it,
    /// same as for anything this store cannot positively identify as an
    /// editor. The client fixes the identical mistake in `foldPaneRead`'s own
    /// `ownsScreen` (see `src/terminal/history.ts` in the Muqun repo, card
    /// #795, defect 2).
    fn record(&mut self, key: &str, text: &str, owns_screen: bool) -> bool {
        let incoming = split_lines(text);
        if incoming.is_empty() {
            return false;
        }
        self.clock += 1;
        let clock = self.clock;
        let buffer = self.buffers.entry(key.to_owned()).or_default();
        let previous_epoch = buffer.epoch;
        buffer.epoch.get_or_insert_with(uuid::Uuid::new_v4);
        buffer.snapshot_ready = true;
        let before = buffer.bytes;
        buffer.touched = clock;
        buffer.fold(incoming, owns_screen);
        let reset = previous_epoch.is_some() && buffer.epoch != previous_epoch;
        self.total_bytes = self.total_bytes + buffer.bytes - before;
        self.evict();
        reset
    }

    /// How many rows are held for one read shape of a pane.
    #[cfg(test)]
    pub(crate) fn held_rows(
        &self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
    ) -> usize {
        self.buffers
            .get(&read_key(session_id, pane_id, source, format))
            .map_or(0, |buffer| buffer.lines.len())
    }

    /// The last `rows` rows held for this read, or `None` where the buffer has
    /// nothing more than the caller already has.
    fn window(&self, key: &str, rows: usize) -> Option<String> {
        let buffer = self.buffers.get(key)?;
        if buffer.lines.is_empty() {
            return None;
        }
        let start = buffer.lines.len().saturating_sub(rows.max(1));
        Some(
            buffer
                .lines
                .iter()
                .skip(start)
                .cloned()
                .collect::<Vec<String>>()
                .join("\n"),
        )
    }

    /// Observe one backend read and return the deepest row window this store can
    /// truthfully serve. Callers never compare bytes or assemble storage keys:
    /// history depth is a row property and the source/format pair is part of the
    /// store's identity for that read.
    pub(crate) fn serve_read_fenced(
        &mut self,
        session_id: &str,
        pane_id: &str,
        (source, format): (&str, &str),
        backend_text: &str,
        rows: usize,
        fence: Option<CaptureFence>,
    ) -> String {
        if !self.accepts_capture(session_id, pane_id, fence) {
            // Do not stitch another generation's cache under a stale response.
            return backend_text.to_owned();
        }
        let owns_screen = self.owns_screen(session_id, pane_id);
        trace_read(
            session_id,
            pane_id,
            source,
            format,
            true, // accepts_capture already checked the current keep policy.
            owns_screen,
            backend_text,
        );
        let key = read_key(session_id, pane_id, source, format);
        if self.record(&key, backend_text, owns_screen) {
            self.advance_capture_generation(session_id, pane_id);
        }
        let backend_rows = split_lines(backend_text).len();
        self.window(&key, rows)
            .filter(|served| split_lines(served).len() > backend_rows)
            .unwrap_or_else(|| backend_text.to_owned())
    }

    /// Record a sampled stream frame under the same policy as a direct read.
    /// This deliberately returns nothing: serving is decided only when a client
    /// asks for a bounded read window.
    pub(crate) fn record_frame_fenced(
        &mut self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
        output: &str,
        fence: Option<CaptureFence>,
    ) {
        if output.is_empty() || !self.accepts_capture(session_id, pane_id, fence) {
            return;
        }
        let owns_screen = self.owns_screen(session_id, pane_id);
        trace_read(
            session_id,
            pane_id,
            source,
            format,
            true,
            owns_screen,
            output,
        );
        let key = read_key(session_id, pane_id, source, format);
        if self.record(&key, output, owns_screen) {
            self.advance_capture_generation(session_id, pane_id);
        }
    }

    /// Synchronous test/replay ingestion has no intervening backend I/O.
    #[cfg(test)]
    pub fn serve_read(
        &mut self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
        backend_text: &str,
        rows: usize,
    ) -> String {
        let fence = self.begin_capture(session_id, pane_id);
        self.serve_read_fenced(
            session_id,
            pane_id,
            (source, format),
            backend_text,
            rows,
            fence,
        )
    }

    #[cfg(test)]
    pub fn record_frame(
        &mut self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
        output: &str,
    ) {
        let fence = self.begin_capture(session_id, pane_id);
        self.record_frame_fenced(session_id, pane_id, source, format, output, fence);
    }

    /// Drop every buffer held for this pane.
    ///
    /// Called whenever the pane is not kept -- Herdr reports scrollback of its
    /// own for it and its reads are passed through. A Claude Code pane flips:
    /// working, it draws on the normal screen and Herdr keeps real scrollback
    /// (`max_offset_from_bottom` ~1000); idle, it is back on the alternate
    /// screen at 0. Whatever was held from before such a gap ends with a
    /// screen the next buffered read cannot be lined up with, so that read
    /// went on the end whole and the old screen -- composer, spinner, roster
    /// -- stayed frozen in the middle of history, one per flip. Starting again
    /// from the current screen loses nothing the reader needs: while the pane
    /// was passed through, Herdr's own history was what got served.
    fn forget_pane(&mut self, session_id: &str, pane_id: &str) {
        self.advance_capture_generation(session_id, pane_id);
        let prefix = format!("{}/", pane_key(session_id, pane_id));
        let mut freed = 0;
        self.buffers.retain(|key, buffer| {
            let keep = !key.starts_with(&prefix);
            if !keep {
                freed += buffer.bytes;
            }
            keep
        });
        self.total_bytes = self.total_bytes.saturating_sub(freed);
    }

    /// How many rows are held for this pane, across every read shape.
    ///
    /// The deepest shape wins, and it can therefore promise the reader more than
    /// a *different* shape would hand back -- a pane watched as ANSI has a deep
    /// ANSI buffer and an empty text one until something reads it as text. That
    /// is the same direction Herdr's own metric already overstates in, and the
    /// app has stopped believing the metric once a page comes back no longer
    /// than the last (card #646): the cost is one wasted pull, not a wrong
    /// screen. Reporting the shallowest instead would suppress the affordance on
    /// the shape that does have the history, which is the worse of the two.
    pub fn depth(&self, session_id: &str, pane_id: &str) -> usize {
        let prefix = format!("{}/", pane_key(session_id, pane_id));
        self.buffers
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(_, buffer)| buffer.lines.len())
            .max()
            .unwrap_or(0)
    }

    /// Say in the pane's own entity what the buffer can deliver.
    ///
    /// This is the only place the gateway edits Herdr's answer, and it is what
    /// the reader's pull-for-earlier is gated on: the affordance reads
    /// `max_offset_from_bottom + viewport_rows` off the pane, not off the
    /// output. Without this the rows would be kept and never asked for.
    ///
    /// It only ever raises the number, and only for panes this store `keeps`
    /// -- not, as this used to read, only for panes reporting exactly zero.
    /// `%17` measured live: `alternate_on: true`, a 29-line residue from
    /// before Claude took the screen, and therefore `keeps` (once that
    /// answers off `alternate_on` rather than off this same zero check --
    /// see its own doc) records it. Gating the amendment on the zero check
    /// too would still have been a bug even so: the store would grow the
    /// pane's synthetic history underneath it forever while `amend` kept
    /// republishing Herdr's frozen 29 on every call, and the reader would
    /// never be offered a pull for any of it. `keeps` is the one true
    /// answer to "does this store speak for this pane" and both halves --
    /// whether to record, and whether to say so -- must agree with it.
    /// Reading it here rather than re-deriving the same verdict from this
    /// call's own payload also means the two can never drift: `observe` is
    /// always called immediately before `amend` on the same body (see
    /// `main.rs`), so the map this reads is the map that body just wrote.
    pub fn amend(&self, session_id: &str, value: &mut Value) {
        let mut updates: Vec<(String, u64)> = Vec::new();
        visit_panes(value, &mut |pane_id, pane| {
            if !self.keeps(session_id, pane_id) {
                return;
            }
            let Some(scroll) = pane.get("scroll").and_then(Value::as_object) else {
                return;
            };
            let viewport = scroll
                .get("viewport_rows")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let held = self.depth(session_id, pane_id) as u64;
            let offset = held.saturating_sub(viewport);
            if offset > 0 {
                updates.push((pane_id.to_owned(), offset));
            }
        });
        if updates.is_empty() {
            return;
        }
        visit_panes_mut(value, &mut |pane_id, scroll| {
            if let Some((_, offset)) = updates.iter().find(|(id, _)| id == pane_id) {
                scroll.insert("max_offset_from_bottom".into(), Value::from(*offset));
            }
        });
    }

    fn evict(&mut self) {
        while self.buffers.len() > MAX_BUFFERS || self.total_bytes > MAX_TOTAL_BYTES {
            let Some(oldest) = self
                .buffers
                .iter()
                .min_by_key(|(_, buffer)| buffer.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(buffer) = self.buffers.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(buffer.bytes);
                // A late read must not resurrect a buffer from before eviction.
                // Buffer keys extend the existing pane key by `/source/format`.
                for (pane, observed) in &mut self.identities {
                    if oldest.starts_with(pane.as_str())
                        && oldest.as_bytes().get(pane.len()) == Some(&b'/')
                    {
                        observed.capture_generation = uuid::Uuid::new_v4();
                    }
                }
            }
        }
    }

    pub(crate) fn capture_epoch(&self, scope: &super::history::HistoryScope) -> Option<uuid::Uuid> {
        self.captured_buffer(scope).and_then(|buffer| buffer.epoch)
    }

    fn captured_buffer(&self, scope: &super::history::HistoryScope) -> Option<&PaneBuffer> {
        self.buffers
            .get(&read_key(
                &scope.session,
                &scope.pane,
                "recent_unwrapped",
                &scope.format,
            ))
            .filter(|buffer| buffer.snapshot_ready && buffer.epoch.is_some())
    }

    /// The memory adapter's capture projection. No pagination or pinned state
    /// lives in the fold; metadata-only checks do not copy any rows.
    pub(crate) fn captured_history(
        &self,
        scope: &super::history::HistoryScope,
        include_rows: bool,
    ) -> Result<Option<super::history::Capture>, super::history::HistoryError> {
        let Some(buffer) = self.captured_buffer(scope) else {
            return Ok(None);
        };
        let count = buffer.lines.len().saturating_sub(buffer.screen);
        let rows = if include_rows {
            // Row text only, the same measure the live cap holds the buffer
            // to: a pane at its cap is exactly the pane with history worth
            // paging. Per-row allocation overhead is the snapshot store's to
            // count, once, against its own budget.
            let bytes = buffer
                .lines
                .iter()
                .take(count)
                .map(String::len)
                .sum::<usize>();
            if bytes > MAX_PANE_BYTES
                || buffer
                    .lines
                    .iter()
                    .take(count)
                    .any(|row| row.len() + 1 > super::history::MAX_PAGE_BYTES)
            {
                return Err(super::history::HistoryError::TooLarge);
            }
            buffer.lines.iter().take(count).cloned().collect()
        } else {
            Vec::new()
        };
        Ok(Some(super::history::Capture {
            epoch: buffer.epoch.expect("captured buffers carry an epoch"),
            trimmed: buffer.trimmed,
            len: count,
            rows,
        }))
    }
}

/// Where `MUQUN_SCROLLBACK_TRACE_DIR` says every pane read goes, if it is set.
///
/// The raw-read recorder. Off unless the variable names a directory; then every
/// read this store is handed -- kept or passed through, from a direct read or a
/// stream frame -- is appended as one JSON line to `<dir>/<session>_<pane>.jsonl`
/// with its time, source and format, whether the pane was kept and owns its
/// screen, and the rows themselves. That is enough to replay a real pane
/// through `record` in a test, which is how the next placement bug gets a
/// fixture instead of a guess. It writes terminal contents to disk, which is
/// exactly what this module otherwise refuses to do, so it is for a developer's
/// own machine, switched on for a session and off again.
fn trace_dir() -> Option<&'static std::path::Path> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("MUQUN_SCROLLBACK_TRACE_DIR")
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_deref()
}

fn trace_read(
    session_id: &str,
    pane_id: &str,
    source: &str,
    format: &str,
    kept: bool,
    owns_screen: bool,
    text: &str,
) {
    let Some(dir) = trace_dir() else {
        return;
    };
    let safe = |part: &str| -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let line = serde_json::json!({
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64),
        "session": session_id,
        "pane": pane_id,
        "source": source,
        "format": format,
        "kept": kept,
        "owns_screen": owns_screen,
        "rows": split_lines(text),
    });
    let path = dir.join(format!("{}_{}.jsonl", safe(session_id), safe(pane_id)));
    let written = std::fs::create_dir_all(dir).and_then(|()| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{line}")
    });
    if let Err(err) = written {
        tracing::debug!("scrollback trace to {} failed: {err}", path.display());
    }
}

/// Every object in a Herdr body that names a pane and reports its scroll.
///
/// `pane.list`, `pane.get` and `session.snapshot` nest panes differently and the
/// gateway models none of them; walking for the pair of fields is what lets one
/// function serve all three and survive a shape it has not seen.
///
/// Hands the visitor the whole pane object, not just `scroll`: `observe` needs
/// `foreground_command`, a sibling of `scroll` rather than a member of it (see
/// `is_editor_command`), and a second walk of the same body for one more field
/// would be the kind of thing this function exists to avoid.
fn visit_panes(value: &Value, visit: &mut impl FnMut(&str, &serde_json::Map<String, Value>)) {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(pane_id)), Some(Value::Object(_))) =
                (map.get("pane_id"), map.get("scroll"))
            {
                visit(pane_id, map);
            }
            for nested in map.values() {
                visit_panes(nested, visit);
            }
        }
        Value::Array(items) => {
            for nested in items {
                visit_panes(nested, visit);
            }
        }
        _ => {}
    }
}

fn visit_panes_mut(
    value: &mut Value,
    visit: &mut impl FnMut(&str, &mut serde_json::Map<String, Value>),
) {
    match value {
        Value::Object(map) => {
            let pane_id = match map.get("pane_id") {
                Some(Value::String(pane_id)) => Some(pane_id.clone()),
                _ => None,
            };
            if let Some(pane_id) = pane_id {
                if let Some(Value::Object(scroll)) = map.get_mut("scroll") {
                    visit(&pane_id, scroll);
                }
            }
            for nested in map.values_mut() {
                visit_panes_mut(nested, visit);
            }
        }
        Value::Array(items) => {
            for nested in items {
                visit_panes_mut(nested, visit);
            }
        }
        _ => {}
    }
}

/// Put spliced rows back where the reader's client will find them.
///
/// Herdr's read envelope is passed through untouched everywhere else in the
/// gateway, so the rows are rewritten in place rather than re-enveloped: the
/// revision, and everything else Herdr said, stays Herdr's.
pub fn replace_read_text(value: &mut Value, text: &str) {
    for pointer in [
        "/result/read/output",
        "/result/read/text",
        "/result/output",
        "/result/text",
    ] {
        if let Some(slot) = value.pointer_mut(pointer) {
            if slot.is_string() {
                *slot = Value::from(text);
                return;
            }
        }
    }
    if let Some(Value::String(_)) = value.pointer("/result") {
        if let Some(slot) = value.pointer_mut("/result") {
            *slot = Value::from(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_scope() -> super::super::history::HistoryScope {
        super::super::history::HistoryScope {
            session: "s".into(),
            pane: "p".into(),
            format: "text".into(),
            device: "d".into(),
            generation: "g".into(),
            limit: 2,
        }
    }

    fn history_pane(width: u32, terminal: &str) -> Value {
        serde_json::json!({"pane_id": "p", "terminal_id": terminal,
            "workspace_id": "w", "tab_id": "t", "width": width, "height": 4,
            "scroll": {"max_offset_from_bottom": 0, "viewport_rows": 4}})
    }

    fn captured_history_store() -> ScrollbackStore {
        let mut store = ScrollbackStore::default();
        store.observe_listing(
            "s",
            &serde_json::json!({"panes": [history_pane(80, "term")]}),
        );
        for top in 0..8 {
            let frame = (top..top + 4)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            store.record_frame("s", "p", "recent_unwrapped", "text", &frame);
        }
        store
    }

    #[test]
    fn captured_history_projection_excludes_viewport_and_does_not_convert_formats() {
        let mut store = captured_history_store();
        let first = store
            .captured_history(&history_scope(), true)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.rows,
            (0..7).map(|i| format!("row {i}")).collect::<Vec<_>>()
        );
        assert!(store
            .captured_history(&history_scope(), false)
            .unwrap()
            .unwrap()
            .rows
            .is_empty());
        store.record_frame(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "row 8\nrow 9\nrow 10\nrow 11",
        );
        store.record_frame(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "row 8\nrow 9\nrow 10\nrepaint",
        );
        assert_eq!(store.capture_epoch(&history_scope()), Some(first.epoch));
        let mut ansi = history_scope();
        ansi.format = "ansi".into();
        assert!(store.captured_history(&ansi, true).unwrap().is_none());
    }

    #[test]
    fn captured_history_resize_identity_disappearance_and_native_policy_reset() {
        for reset in 0..4 {
            let mut store = captured_history_store();
            let epoch = store.capture_epoch(&history_scope()).unwrap();
            match reset {
                0 => store.observe("s", &history_pane(120, "term")),
                1 => store.observe("s", &history_pane(80, "recreated")),
                2 => store.observe_listing("s", &serde_json::json!({"panes": []})),
                _ => {
                    let mut pane = history_pane(80, "term");
                    pane["scroll"]["max_offset_from_bottom"] = serde_json::json!(200);
                    store.observe("s", &pane);
                }
            }
            assert_ne!(store.capture_epoch(&history_scope()), Some(epoch));
            assert!(store
                .captured_history(&history_scope(), true)
                .unwrap()
                .is_none());
            if reset == 2 {
                store.observe_listing(
                    "s",
                    &serde_json::json!({"panes": [history_pane(80, "term")]}),
                );
                store.record_frame("s", "p", "recent_unwrapped", "text", "new incarnation");
                assert_eq!(
                    store.depth("s", "p"),
                    1,
                    "no old rows leak after known ID reuse"
                );
            }
        }
    }

    #[test]
    fn captured_history_buffer_eviction_and_rule_resize_reset() {
        for reset in [0, 2] {
            let mut store = captured_history_store();
            let epoch = store.capture_epoch(&history_scope()).unwrap();
            match reset {
                0 => {
                    for i in 0..MAX_BUFFERS {
                        store.record(&format!("other{i}"), "unrelated", false);
                    }
                }
                _ => {
                    store.record_frame("s", "p", "recent_unwrapped", "text", "──────────\na\nb\nc");
                    store.record_frame(
                        "s",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "───────────────\na\nb\nc",
                    );
                }
            }
            assert_ne!(store.capture_epoch(&history_scope()), Some(epoch));
        }
    }

    #[test]
    fn front_truncation_at_the_cap_slides_the_capture_instead_of_resetting_it() {
        let mut store = captured_history_store();
        let fence = store.begin_capture("s", "p");
        let before = store
            .captured_history(&history_scope(), false)
            .unwrap()
            .unwrap();
        let key = read_key("s", "p", "recent_unwrapped", "text");
        let buffer = store.buffers.get_mut(&key).unwrap();
        for _ in 0..MAX_PANE_LINES {
            buffer.push("more".into());
        }
        let held = buffer.lines.len();
        buffer.trim();
        let after = store
            .captured_history(&history_scope(), false)
            .unwrap()
            .unwrap();
        assert_eq!(after.epoch, before.epoch);
        assert_eq!(
            after.trimmed,
            before.trimmed + (held - MAX_PANE_LINES) as u64
        );
        // A read in flight across a trim is still this capture's.
        assert!(store.accepts_capture("s", "p", fence));
    }

    #[test]
    fn capture_fences_reject_unknown_policy_and_observed_resets_without_stitching() {
        let mut unknown = ScrollbackStore::default();
        let fence = unknown.begin_capture("s", "p");
        assert!(fence.is_none());
        unknown.observe("s", &history_pane(80, "B"));
        assert_eq!(
            unknown.serve_read_fenced(
                "s",
                "p",
                ("recent_unwrapped", "text"),
                "unknown A",
                200,
                fence
            ),
            "unknown A"
        );
        assert_eq!(unknown.depth("s", "p"), 0);
        for reset in 0..5 {
            let mut store = captured_history_store();
            let fence = store.begin_capture("s", "p");
            assert!(fence.is_some());
            match reset {
                0 => store.observe("s", &history_pane(80, "B")),
                1 => store.observe("s", &history_pane(120, "term")),
                2 => {
                    store.observe_listing("s", &serde_json::json!({"panes": []}));
                    store.observe("s", &history_pane(80, "term"));
                }
                3 => {
                    let mut pane = history_pane(80, "term");
                    pane["scroll"]["max_offset_from_bottom"] = serde_json::json!(200);
                    store.observe("s", &pane);
                    store.observe("s", &history_pane(80, "term"));
                }
                _ => {
                    let mut pane = history_pane(80, "term");
                    pane["foreground_command"] = serde_json::json!("nvim");
                    store.observe("s", &pane);
                }
            }
            assert!(!store.accepts_capture("s", "p", fence));
            let fresh = store.begin_capture("s", "p");
            store.record_frame_fenced("s", "p", "recent_unwrapped", "text", "B frame", fresh);
            let depth = store.depth("s", "p");
            assert_eq!(
                store.serve_read_fenced(
                    "s",
                    "p",
                    ("recent_unwrapped", "text"),
                    "A late",
                    200,
                    fence
                ),
                "A late"
            );
            store.record_frame_fenced("s", "p", "recent_unwrapped", "text", "A late", fence);
            assert_eq!(store.depth("s", "p"), depth);
        }
    }

    #[test]
    fn full_editor_and_settling_replacements_rotate_epochs_but_normal_polls_do_not() {
        for editor in [true, false] {
            let mut store = captured_history_store();
            let epoch = store.capture_epoch(&history_scope());
            let fence = store.begin_capture("s", "p");
            // Exercise the fold replacement itself independently of observation
            // invalidation: editor ownership and resize-settling are both resets.
            if editor {
                store.owns_screen.insert(pane_key("s", "p"), true);
            } else {
                store
                    .buffers
                    .get_mut(&read_key("s", "p", "recent_unwrapped", "text"))
                    .unwrap()
                    .settling = 1;
            }
            store.record_frame_fenced("s", "p", "recent_unwrapped", "text", "new screen", fence);
            assert_ne!(store.capture_epoch(&history_scope()), epoch);
            assert!(!store.accepts_capture("s", "p", fence));
            assert!(store
                .captured_history(&history_scope(), true)
                .unwrap()
                .unwrap()
                .rows
                .is_empty());
        }
        let mut normal = captured_history_store();
        let epoch = normal.capture_epoch(&history_scope());
        let fence = normal.begin_capture("s", "p");
        normal.record_frame_fenced(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "row 7\nrow 8\nrow 9\nrow 10",
            fence,
        );
        assert_eq!(normal.capture_epoch(&history_scope()), epoch);
        assert!(normal.accepts_capture("s", "p", fence));
    }

    #[test]
    fn capture_fences_do_not_resurrect_evicted_buffers() {
        for evict in [true] {
            let mut store = captured_history_store();
            let fence = store.begin_capture("s", "p");
            if evict {
                for i in 0..MAX_BUFFERS {
                    store.record(&format!("other{i}"), "unrelated", false);
                }
            } else {
                let frame = (0..MAX_PANE_LINES)
                    .map(|i| format!("new row {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                store.record_frame_fenced("s", "p", "recent_unwrapped", "text", &frame, fence);
            }
            assert!(!store.accepts_capture("s", "p", fence));
            let depth = store.depth("s", "p");
            store.record_frame_fenced(
                "s",
                "p",
                "recent_unwrapped",
                "text",
                "late old frame",
                fence,
            );
            assert_eq!(store.depth("s", "p"), depth);
        }
    }

    /// Replay a captured pane through the store and measure what it kept.
    ///
    /// The evidence half of card #721's question: with the anchor, the furniture
    /// rule and the text seam in place, does the ring still duplicate a real
    /// agent pane under real load? Point it at a capture and find out:
    ///
    /// ```text
    /// HERDR_SCROLLBACK_REPLAY=/tmp/frames.jsonl cargo test replay -- --nocapture
    /// ```
    ///
    /// The file is one JSON-encoded string per line, each the full text of one
    /// `pane.read`. `scripts/capture-frames.sh` in the app repo makes one.
    ///
    /// Skipped when the variable is unset, because the capture is somebody's
    /// terminal and does not belong in the repository.
    #[test]
    fn replay_a_captured_pane_and_report_duplication() {
        let Ok(path) = std::env::var("HERDR_SCROLLBACK_REPLAY") else {
            return;
        };
        let body = std::fs::read_to_string(&path).expect("replay capture");
        let frames: Vec<String> = body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<String>(line).expect("json string per line"))
            .collect();

        let mut store = ScrollbackStore::default();
        for frame in &frames {
            store.record("replay", frame, false);
        }
        let held = store.window("replay", MAX_PANE_LINES).unwrap_or_default();
        let rows: Vec<&str> = held.lines().collect();

        // Rows carrying something, and how often each of them was written down.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for row in rows.iter().filter(|row| !row.trim().is_empty()) {
            *counts.entry(row).or_default() += 1;
        }
        let substantial: usize = counts.values().sum();
        let repeated: usize = counts.values().filter(|n| **n > 1).sum();
        let worst = counts.values().copied().max().unwrap_or(0);

        // The metric that actually matters: a run of four or more rows written
        // down twice. A single row repeating is a transcript doing its job --
        // `⎿  Allowed by auto mode classifier` genuinely appears many times --
        // but four consecutive rows repeating verbatim is the buffer copying
        // itself.
        let mut duplicated_blocks = 0usize;
        let mut duplicated_rows = 0usize;
        let hashes: Vec<u64> = rows.iter().map(|row| line_hash(row)).collect();
        let carries: Vec<bool> = rows.iter().map(|row| !row.trim().is_empty()).collect();
        let mut at = 0usize;
        while at + 8 <= hashes.len() {
            let mut hit = 0usize;
            for start in (at + 1)..hashes.len().saturating_sub(3) {
                let mut run = 0usize;
                while start + run < hashes.len()
                    && at + run < start
                    && hashes[at + run] == hashes[start + run]
                {
                    run += 1;
                }
                let carried = (0..run).filter(|step| carries[at + step]).count();
                if run >= 4 && carried >= 4 {
                    hit = run;
                    break;
                }
            }
            if hit > 0 {
                duplicated_blocks += 1;
                duplicated_rows += hit;
                at += hit;
            } else {
                at += 1;
            }
        }

        println!(
            "replay {path}: {} frames -> {} rows held\n  \
             substantial rows {substantial}, of which repeated at all {repeated} ({:.0}%), worst row x{worst}\n  \
             duplicated blocks (>=4 rows) {duplicated_blocks} covering {duplicated_rows} rows",
            frames.len(),
            rows.len(),
            if substantial == 0 { 0.0 } else { repeated as f64 * 100.0 / substantial as f64 },
        );

        assert_eq!(
            duplicated_blocks, 0,
            "the ring copied {duplicated_rows} rows of a real pane into its own history"
        );
    }

    /// The same rule, against a composer whose *last* row is the volatile one.
    ///
    /// Card #721. This is the shape a real Claude pane has -- the mode line with
    /// its `4m 46s · ↓ 2.9k tokens` clock is the bottom row of the box, not a
    /// row above it -- and it is the shape the old exact `common_suffix` could
    /// not see at all: the walk up from the bottom met the clock on its first
    /// step and stopped, reporting zero rows of furniture. The rule that exists
    /// to keep a composer out of a transcript did not fire on the pane it was
    /// written for, and a copy of the box went into history on every jump.
    #[test]
    fn a_composer_is_furniture_even_with_a_clock_on_its_last_row() {
        let mut store = ScrollbackStore::default();
        for round in 0..8 {
            let mut frame: Vec<String> = (0..40)
                .map(|row| format!("line {} of round {round}", row + round * 40))
                .collect();
            frame.extend([
                "─".repeat(60),
                "❯ ".to_owned(),
                "  ⏵⏵ auto mode on".to_owned(),
                // The clock, on the bottom row, changing every single frame.
                format!("  {}m {}s · ↓ {}k tokens", round, round * 7, round * 13),
            ]);
            store.record("pane", &frame.join("\n"), false);
        }
        let held = store.window("pane", 5_000).unwrap();
        let lines: Vec<&str> = held.lines().collect();
        assert_eq!(
            lines.iter().filter(|line| **line == "❯ ").count(),
            1,
            "the composer belongs at the bottom once, and a ticking clock inside \
             it must not hide it from the furniture rule"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| **line == "  ⏵⏵ auto mode on")
                .count(),
            1
        );
        // And the transcript itself is untouched.
        assert!(lines.contains(&"line 0 of round 0"));
        assert!(lines.contains(&"line 319 of round 7"));
    }

    /// A pane repainting a long identical tail is not wearing furniture, and the
    /// cap is what keeps the rule from eating its history.
    #[test]
    fn a_long_repainted_tail_is_history_not_furniture() {
        let mut store = ScrollbackStore::default();
        let tail: Vec<String> = (0..30).map(|row| format!("tail {row}")).collect();
        for round in 0..4 {
            let mut frame: Vec<String> = (0..10)
                .map(|row| format!("head {} of round {round}", row + round * 10))
                .collect();
            frame.extend(tail.iter().cloned());
            store.record("pane", &frame.join("\n"), false);
        }
        let held = store.window("pane", 5_000).unwrap();
        let lines: Vec<&str> = held.lines().collect();
        // A third of a forty-row read is thirteen rows, so the tail is never
        // taken for chrome wholesale.
        assert!(lines.contains(&"head 0 of round 0"));
        assert!(lines.contains(&"head 39 of round 3"));
    }

    /// An agent pins a composer to the bottom of its screen and scrolls only
    /// the transcript above it. The composer is furniture: it belongs at the
    /// end of what the reader scrolls through, once, however many times the
    /// screen jumped while they were away.
    #[test]
    fn pinned_furniture_is_kept_once() {
        let mut store = ScrollbackStore::default();
        let chrome = ["", "> ", "auto mode on"];
        for round in 0..8 {
            // Each frame jumps far enough that no placement is believable.
            let body: Vec<String> = (0..40)
                .map(|row| format!("line {} of round {round}", row + round * 40))
                .collect();
            let frame: Vec<String> = body
                .into_iter()
                .chain(chrome.iter().map(|line| (*line).to_owned()))
                .collect();
            store.record("pane", &frame.join("\n"), false);
        }
        let held = store.window("pane", 5_000).unwrap();
        let lines: Vec<&str> = held.lines().collect();
        assert_eq!(
            lines.iter().filter(|line| **line == "auto mode on").count(),
            1,
            "the composer belongs at the bottom once, not once per jump"
        );
        assert_eq!(lines.last(), Some(&"auto mode on"));
        // And the transcript itself survives whole.
        assert!(lines
            .iter()
            .any(|line| line.starts_with("line 0 of round 0")));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("line 319 of round 7")));
    }

    use serde_json::json;

    fn screen(rows: &[&str]) -> String {
        rows.join("\n")
    }

    /// A sixty-five row screen scrolled one row at a time until the buffer holds
    /// `rows`, the shape every real pane in the fleet has.
    fn scroll_a_screen(store: &mut ScrollbackStore, key: &str, rows: usize) {
        for top in 0..rows.saturating_sub(64) {
            let screen: Vec<String> = (top..top + 65).map(|row| format!("row {row}")).collect();
            store.record(key, &screen.join("\n"), false);
        }
    }

    #[test]
    fn line_splitting_normalizes_every_break_the_app_normalizes() {
        assert_eq!(split_lines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines(""), Vec::<String>::new());
        // A blank final row is only dropped once: a screen really can end on
        // two blank rows and the second one is content.
        assert_eq!(split_lines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn a_first_read_is_kept_whole() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]), false);
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3");
    }

    #[test]
    fn an_unchanged_screen_adds_nothing() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]), false);
        store.record("k", &screen(&["1", "2", "3"]), false);
        store.record("k", &screen(&["1", "2", "3"]), false);
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3");
    }

    #[test]
    fn a_scrolled_screen_keeps_what_rolled_off_the_top() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]), false);
        store.record("k", &screen(&["3", "4", "5", "6"]), false);
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3\n4\n5\n6");
    }

    #[test]
    fn the_seam_is_neither_duplicated_nor_dropped() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["a", "b", "c", "d", "e"]), false);
        store.record("k", &screen(&["b", "c", "d", "e", "f"]), false);
        store.record("k", &screen(&["c", "d", "e", "f", "g"]), false);
        assert_eq!(store.window("k", 20).unwrap(), "a\nb\nc\nd\ne\nf\ng");
    }

    #[test]
    fn one_turning_spinner_does_not_read_as_a_redraw() {
        // The case that decides whether this is a buffer or a leak: at 150ms a
        // screen whose clock ticks must not append itself every time.
        let mut store = ScrollbackStore::default();
        let mut rows: Vec<String> = (0..20).map(|row| format!("row {row}")).collect();
        store.record("k", &rows.join("\n"), false);
        for tick in 0..50 {
            rows[7] = format!("working {tick}");
            store.record("k", &rows.join("\n"), false);
        }
        let held = store.window("k", 500).unwrap();
        assert_eq!(split_lines(&held).len(), 20);
        assert!(held.contains("working 49"));
    }

    #[test]
    fn a_screen_that_shares_nothing_is_kept_on_top_of_the_one_it_followed() {
        // Output arriving faster than the poll: the two reads are consecutive
        // content, not two renderings of the same content, and dropping the
        // first would throw away exactly what this exists to keep.
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["old 1", "old 2", "old 3", "old 4"]), false);
        store.record("k", &screen(&["new 1", "new 2", "new 3", "new 4"]), false);
        assert_eq!(
            store.window("k", 20).unwrap(),
            "old 1\nold 2\nold 3\nold 4\nnew 1\nnew 2\nnew 3\nnew 4"
        );
    }

    // Card #795, defect 2: a detected nvim pane rendered two stacked copies
    // of its own screen. Root-caused to this exact mechanism -- confirmed
    // live against the real gateway, not just here -- `record`'s "neither
    // placement believed anything, so keep the read on top" fallback,
    // written for a genuinely scrolling pane whose output outran the poll,
    // firing for an alternate-screen pane's ordinary repaint instead. A
    // screen that owns its screen has no real history for this buffer to
    // protect, so it must replace, not accumulate, however different two
    // consecutive reads of it are.
    #[test]
    fn a_screen_owning_pane_replaces_rather_than_accumulates() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["old 1", "old 2", "old 3", "old 4"]), true);
        store.record("k", &screen(&["new 1", "new 2", "new 3", "new 4"]), true);
        // The mirror of `a_screen_that_shares_nothing_is_kept_on_top_of_the_one_it_followed`:
        // same two screens sharing nothing, `owns_screen` true instead of
        // false, and the old screen must be gone rather than kept above the
        // new one.
        assert_eq!(store.window("k", 20).unwrap(), "new 1\nnew 2\nnew 3\nnew 4");
    }

    #[test]
    fn a_screen_owning_pane_with_a_real_overlap_still_just_replaces() {
        // Not only the zero-overlap fallback: even a repaint the placement
        // heuristics *could* have matched (an unchanged screen, a small
        // scroll) is exactly the current screen and nothing this buffer
        // needs to reconstruct history from -- there is no "and then" for a
        // repaint to have.
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["row 0", "row 1", "row 2"]), true);
        store.record("k", &screen(&["row 0", "row 1", "row 2 CHANGED"]), true);
        assert_eq!(
            store.window("k", 20).unwrap(),
            "row 0\nrow 1\nrow 2 CHANGED"
        );
    }

    #[test]
    fn a_screen_owning_pane_first_read_is_kept_whole() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]), true);
        assert_eq!(store.window("k", 20).unwrap(), "1\n2\n3");
    }

    #[test]
    fn an_unknown_pane_keeps_accumulating_until_its_foreground_command_is_observed() {
        // `owns_screen` defaults to `false` for a pane this store has not
        // been told about (the same conservatism `keeps` already applies to
        // `max_offset_from_bottom`), so a caller too old to report
        // `foreground_command`, or a pane not yet listed once, must not
        // change behaviour for a pane that already worked.
        let store = ScrollbackStore::default();
        assert!(!store.owns_screen("s", "p"));
    }

    #[test]
    fn observing_an_editor_foreground_command_is_what_flips_owns_screen() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "s",
            &json!({
                "pane_id": "p",
                "foreground_command": "nvim",
                "scroll": { "max_offset_from_bottom": 0 },
            }),
        );
        assert!(store.owns_screen("s", "p"));
    }

    // Card #795 follow-up: the regression this whole file exists to guard
    // against now. `alternate_on` was tried as the switch first and measured
    // wrong live -- a Claude Code pane reports `alternate_on=1` exactly like
    // nvim's -- because an agent pane also owns an alternate screen; it is
    // precisely one of the panes this module's own doc says it exists to
    // serve. `alternate_on` alone must never flip `owns_screen`.
    #[test]
    fn observing_alternate_on_alone_does_not_flip_owns_screen() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "s",
            &json!({
                "pane_id": "p",
                "foreground_command": "claude",
                "scroll": { "max_offset_from_bottom": 0, "alternate_on": true },
            }),
        );
        assert!(!store.owns_screen("s", "p"));
    }

    #[test]
    fn history_survives_a_burst_that_outran_the_poll() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]), false);
        // Scrolls by two, so 1 and 2 become history.
        store.record("k", &screen(&["3", "4", "5", "6"]), false);
        // Then the screen jumps past what can be followed.
        store.record("k", &screen(&["x", "y", "z", "w"]), false);
        assert_eq!(
            store.window("k", 20).unwrap(),
            "1\n2\n3\n4\n5\n6\nx\ny\nz\nw"
        );
    }

    #[test]
    fn a_window_asks_for_no_more_than_it_holds() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]), false);
        assert_eq!(store.window("k", 2).unwrap(), "2\n3");
        assert_eq!(store.window("k", 999).unwrap(), "1\n2\n3");
        assert!(store.window("missing", 10).is_none());
    }

    #[test]
    fn the_row_ceiling_drops_the_oldest_rows_first() {
        let mut store = ScrollbackStore::default();
        // A twenty-row screen scrolling one row at a time, for longer than the
        // ceiling allows.
        let total = MAX_PANE_LINES + 200;
        for top in 0..total {
            let rows: Vec<String> = (top..top + 20).map(|row| format!("row {row}")).collect();
            store.record("k", &rows.join("\n"), false);
        }
        let held = store.window("k", MAX_PANE_LINES * 2).unwrap();
        let rows = split_lines(&held);
        assert_eq!(rows.len(), MAX_PANE_LINES);
        assert_eq!(rows.last().unwrap(), &format!("row {}", total + 18));
        assert_eq!(
            rows.first().unwrap(),
            &format!("row {}", total + 19 - MAX_PANE_LINES)
        );
    }

    #[test]
    fn the_buffer_ceiling_forgets_the_pane_nobody_looked_at() {
        let mut store = ScrollbackStore::default();
        for index in 0..(MAX_BUFFERS + 5) {
            store.record(&format!("pane-{index}"), "hello", false);
        }
        assert!(store.buffers.len() <= MAX_BUFFERS);
        assert!(store.window("pane-0", 10).is_none());
        assert!(store
            .window(&format!("pane-{}", MAX_BUFFERS + 4), 10)
            .is_some());
    }

    #[test]
    fn only_panes_herdr_reports_zero_for_are_kept() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "default",
            &json!({
                "result": {
                    "panes": [
                        { "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } },
                        { "pane_id": "wM:pT", "scroll": { "max_offset_from_bottom": 3823, "viewport_rows": 65 } }
                    ]
                }
            }),
        );
        assert!(store.observed_as_kept("default", "wM:p1"));
        assert!(!store.observed_as_kept("default", "wM:pT"));
        // A pane nobody has reported on is not guessed at.
        assert!(!store.observed_as_kept("default", "wN:p9"));
        // Nor is a pane on another session with the same id.
        assert!(!store.observed_as_kept("other", "wM:p1"));
    }

    // The gap between the two mechanisms, measured live: a Claude pane
    // (`%17`) that ran `nvim .` before Claude took the alternate screen has a
    // 29-line residue from before the switch -- tmux's `history_size`, which
    // `max_offset_from_bottom` mirrors verbatim for the tmux backend -- and
    // that residue will never grow no matter how long the conversation runs,
    // because everything printed since went to the alternate screen instead.
    // The old predicate (`max_offset_from_bottom <= 0.0` alone) answered
    // "false" for it: not kept by tmux in any way that matters (the 29 lines
    // are frozen, not scrollback the reader can page into), and not recorded
    // by this store either. `alternate_on` is the fix because it is the
    // question this store actually needs answered -- will tmux ever add to
    // this pane's own history -- rather than a proxy for it that only agrees
    // with the answer when the residue happens to be zero.
    #[test]
    fn an_alternate_screen_pane_with_a_shallow_residue_is_kept() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "default",
            &json!({
                "result": {
                    "panes": [
                        // %17: history_size 29, alternate_on 1 -- a Claude pane
                        // with residue from before the switch, measured live.
                        { "pane_id": "%17", "scroll": { "max_offset_from_bottom": 29, "viewport_rows": 63, "alternate_on": true } },
                        // %1: history_size 0, alternate_on 1 -- a Claude pane
                        // that switched with nothing printed first. Already
                        // kept under the old predicate; must stay kept.
                        { "pane_id": "%1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 63, "alternate_on": true } }
                    ]
                }
            }),
        );
        assert!(
            store.observed_as_kept("default", "%17"),
            "29 frozen lines is not meaningfully more useful than none, and unlike \
             none it looked like real scrollback under the old rule"
        );
        assert!(store.observed_as_kept("default", "%1"));
    }

    // The other half of the same fix: a pane that genuinely scrolls -- codex,
    // bun, anything tmux keeps real history for -- must not become "kept" just
    // because its `max_offset_from_bottom` happens to be small early on. Only
    // `alternate_on: false` (or a pane observed with no `alternate_on` at all,
    // the pre-card-795 shape) proves that, and it must keep winning over a
    // small offset the same way it always has.
    #[test]
    fn a_genuinely_scrolling_pane_is_never_kept_regardless_of_offset() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "default",
            &json!({
                "result": {
                    "panes": [
                        // %47 (codex): prints and scrolls, alternate_on false,
                        // caught early with only a little scrollback so far.
                        { "pane_id": "%47", "scroll": { "max_offset_from_bottom": 6, "viewport_rows": 63, "alternate_on": false } },
                        // %34 (bun): the same pane, deep into a long build.
                        { "pane_id": "%34", "scroll": { "max_offset_from_bottom": 13_643, "viewport_rows": 63, "alternate_on": false } }
                    ]
                }
            }),
        );
        assert!(!store.observed_as_kept("default", "%47"));
        assert!(!store.observed_as_kept("default", "%34"));
    }

    // Where the backend cannot say (Herdr; the shape every existing test above
    // this one uses), the fallback is exactly the old rule -- unchanged.
    #[test]
    fn unknown_alternate_on_falls_back_to_the_offset_alone() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "default",
            &json!({
                "result": {
                    "panes": [
                        { "pane_id": "h1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 63 } },
                        { "pane_id": "h2", "scroll": { "max_offset_from_bottom": 29, "viewport_rows": 63 } }
                    ]
                }
            }),
        );
        assert!(store.observed_as_kept("default", "h1"));
        assert!(!store.observed_as_kept("default", "h2"));
    }

    /// `kept` and `owns_screen` were insert-only and keyed by a pane id that
    /// is never reused, so a long-running gateway accumulated one entry per
    /// map for every pane that had ever existed.
    #[test]
    fn a_complete_listing_forgets_the_panes_that_are_gone() {
        let mut store = ScrollbackStore::default();
        let listing = |ids: &[&str]| {
            json!({
                "result": {
                    "panes": ids
                        .iter()
                        .map(|id| json!({
                            "pane_id": id,
                            "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 60 },
                            "foreground_command": "vim"
                        }))
                        .collect::<Vec<_>>()
                }
            })
        };

        store.observe_listing("s", &listing(&["p1", "p2", "p3"]));
        assert_eq!(store.remembered_pane_count(), 3);

        // p2 closed.
        store.observe_listing("s", &listing(&["p1", "p3"]));
        assert_eq!(store.remembered_pane_count(), 2);
        assert!(store.observed_as_kept("s", "p1"));
        assert!(!store.observed_as_kept("s", "p2"));

        // Another session's panes are not this listing's business.
        store.observe_listing("other", &listing(&["q1"]));
        assert_eq!(store.remembered_pane_count(), 3);
        assert!(store.observed_as_kept("s", "p1"));
    }

    /// A single `pane.get` is not a statement about which panes exist, so it
    /// must not be allowed to forget the ones it does not mention.
    #[test]
    fn a_single_pane_answer_never_forgets_the_others() {
        let mut store = ScrollbackStore::default();
        let one = |id: &str| {
            json!({
                "pane_id": id,
                "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 60 }
            })
        };
        store.observe("s", &one("p1"));
        store.observe("s", &one("p2"));
        assert_eq!(store.remembered_pane_count(), 2);
        store.observe("s", &one("p1"));
        assert_eq!(store.remembered_pane_count(), 2);
    }

    #[test]
    fn a_pane_that_grows_scrollback_stops_being_kept() {
        let mut store = ScrollbackStore::default();
        let zero = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } });
        let grown = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 40, "viewport_rows": 65 } });
        store.observe("s", &zero);
        assert!(store.observed_as_kept("s", "p"));
        store.observe("s", &grown);
        assert!(!store.observed_as_kept("s", "p"));
    }

    #[test]
    fn the_pane_entity_answers_for_what_the_buffer_holds() {
        let mut store = ScrollbackStore::default();
        scroll_a_screen(
            &mut store,
            &read_key("s", "p", "recent_unwrapped", "text"),
            236,
        );
        let mut value = json!({
            "result": {
                "panes": [
                    { "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } }
                ]
            }
        });
        // `amend` now answers off `keeps`, not off this call's own zero check
        // -- `observe`, always run immediately before it in production, is
        // what populates that.
        store.observe("s", &value);
        store.amend("s", &mut value);
        // 236 rows held, 65 of them on screen: 171 the reader can reach for.
        assert_eq!(
            value
                .pointer("/result/panes/0/scroll/max_offset_from_bottom")
                .unwrap(),
            &json!(236 - 65)
        );
    }

    // The end-to-end shape of the `%17` fix: a pane with a shallow, frozen
    // residue (`alternate_on: true`, `max_offset_from_bottom: 29`, unlike
    // `%1`'s 0) is amended upward as the store accumulates past what tmux
    // ever reported, instead of being stuck republishing 29 forever. Without
    // `amend` also switching to `keeps`, this would still fail even with
    // `keeps` itself fixed: recording is only half of offering the pull.
    #[test]
    fn an_alternate_screen_pane_with_a_residue_is_amended_past_it() {
        let mut store = ScrollbackStore::default();
        let observed = json!({
            "result": {
                "panes": [
                    { "pane_id": "%17", "scroll": { "max_offset_from_bottom": 29, "viewport_rows": 63, "alternate_on": true } }
                ]
            }
        });
        store.observe("default", &observed);
        // The conversation goes on long past the 29-line residue tmux will
        // ever report for this pane.
        scroll_a_screen(
            &mut store,
            &read_key("default", "%17", "recent_unwrapped", "text"),
            300,
        );
        let mut value = observed.clone();
        store.amend("default", &mut value);
        let amended = value
            .pointer("/result/panes/0/scroll/max_offset_from_bottom")
            .unwrap()
            .as_u64()
            .unwrap();
        assert!(
            amended > 29,
            "the store held more than tmux's frozen residue, and the reader \
             must be offered a pull for it: got {amended}"
        );
        assert_eq!(amended, 300 - 63);
    }

    #[test]
    fn a_pane_with_real_scrollback_is_left_exactly_as_it_arrived() {
        let mut store = ScrollbackStore::default();
        scroll_a_screen(
            &mut store,
            &read_key("s", "p", "recent_unwrapped", "text"),
            300,
        );
        let original = json!({
            "result": {
                "panes": [
                    // `alternate_on: false`: a genuinely scrolling pane, kept
                    // by neither predicate however deep its own history goes.
                    { "pane_id": "p", "scroll": { "max_offset_from_bottom": 908, "viewport_rows": 65, "alternate_on": false } }
                ]
            }
        });
        store.observe("s", &original);
        let mut value = original.clone();
        store.amend("s", &mut value);
        assert_eq!(value, original);
    }

    #[test]
    fn a_buffer_shallower_than_the_viewport_promises_nothing() {
        let mut store = ScrollbackStore::default();
        store.record(
            &read_key("s", "p", "recent_unwrapped", "text"),
            "one\ntwo",
            false,
        );
        let original = json!({
            "result": {
                "panes": [
                    { "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } }
                ]
            }
        });
        store.observe("s", &original);
        let mut value = original.clone();
        store.amend("s", &mut value);
        assert_eq!(value, original);
    }

    #[test]
    fn ansi_and_text_reads_of_one_pane_do_not_splice_into_each_other() {
        let mut store = ScrollbackStore::default();
        store.record(
            &read_key("s", "p", "recent_unwrapped", "text"),
            "plain",
            false,
        );
        store.record(
            &read_key("s", "p", "recent_unwrapped", "ansi"),
            "\u{1b}[31mred",
            false,
        );
        assert_eq!(
            store
                .window(&read_key("s", "p", "recent_unwrapped", "text"), 10)
                .unwrap(),
            "plain"
        );
        assert_eq!(
            store
                .window(&read_key("s", "p", "recent_unwrapped", "ansi"), 10)
                .unwrap(),
            "\u{1b}[31mred"
        );
    }

    #[test]
    fn serving_prefers_more_rows_even_when_the_backend_screen_has_more_bytes() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "s",
            &json!({
                "pane_id": "p",
                "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 2 }
            }),
        );

        assert_eq!(
            store.serve_read("s", "p", "recent_unwrapped", "text", "a\nb", 10),
            "a\nb"
        );
        let served = store.serve_read(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "界界界界界界界界界界\nz",
            10,
        );

        assert_eq!(served, "a\nb\n界界界界界界界界界界\nz");
        assert_eq!(split_lines(&served).len(), 4);
    }

    #[test]
    fn spliced_rows_go_back_where_herdr_put_them() {
        let mut nested =
            json!({ "id": "1", "result": { "read": { "text": "old", "revision": 7 } } });
        replace_read_text(&mut nested, "new");
        assert_eq!(nested.pointer("/result/read/text").unwrap(), &json!("new"));
        assert_eq!(nested.pointer("/result/read/revision").unwrap(), &json!(7));

        let mut flat = json!({ "result": { "text": "old" } });
        replace_read_text(&mut flat, "new");
        assert_eq!(flat.pointer("/result/text").unwrap(), &json!("new"));

        let mut bare = json!({ "result": "old" });
        replace_read_text(&mut bare, "new");
        assert_eq!(bare.pointer("/result").unwrap(), &json!("new"));
    }

    /// The shape of a real Claude pane, measured off `wM:p1`: a scrolling
    /// transcript, a volatile timer on its last row, and an eight-row composer
    /// pinned to the bottom that never scrolls with it.
    const AGENT_SCREEN_ROWS: usize = 64;
    const AGENT_BOX_ROWS: usize = 8;
    const AGENT_TRANSCRIPT_ROWS: usize = AGENT_SCREEN_ROWS - AGENT_BOX_ROWS;

    fn agent_screen(top: usize, tick: usize) -> String {
        let mut rows: Vec<String> = (0..AGENT_TRANSCRIPT_ROWS)
            .map(|row| format!("⏺ transcript row {} of the agent's answer", top + row))
            .collect();
        // The row that repaints every poll however still the pane is.
        rows[AGENT_TRANSCRIPT_ROWS - 1] = format!(
            "✢ Recombobulating… ({}m {}s · ↓ {}k tokens)",
            tick / 60,
            tick % 60,
            tick
        );
        rows.extend([
            String::new(),
            "─".repeat(110),
            "❯ ".to_owned(),
            "─".repeat(110),
            "  ⏵⏵ auto mode on · esc to interrupt".to_owned(),
            String::new(),
            "  ⏺ main".to_owned(),
            format!("  ◯ general-purpose  running {} tools", tick % 3),
        ]);
        rows.join("\n")
    }

    /// `session/pane` for the agent-shaped tests below, matched with `read_key`
    /// against `"recent_unwrapped"`/`"text"`.
    const AGENT_SESSION: &str = "s";
    const AGENT_PANE: &str = "p";

    /// Reports the same envelope `compat::pane` emits for a live Claude Code
    /// pane (card #795: `foreground_command: "claude"`, `alternate_on: true`,
    /// `max_offset_from_bottom: 0`), so a test that calls this and then reads
    /// or records through `serve_read`/`record_frame` exercises the value
    /// `owns_screen` production actually computes for an agent pane, not a
    /// hand-passed parameter.
    fn observe_agent_pane(store: &mut ScrollbackStore) {
        store.observe(
            AGENT_SESSION,
            &json!({
                "pane_id": AGENT_PANE,
                "foreground_command": "claude",
                "scroll": {
                    "max_offset_from_bottom": 0,
                    "viewport_rows": AGENT_SCREEN_ROWS,
                    "alternate_on": true,
                },
            }),
        );
    }

    /// The window `observe_agent_pane`'s pane has recorded, through the same
    /// `recent_unwrapped`/`text` shape `serve_read`/`record_frame` key it
    /// under.
    fn agent_window(store: &ScrollbackStore, rows: usize) -> Option<String> {
        store.window(
            &read_key(AGENT_SESSION, AGENT_PANE, "recent_unwrapped", "text"),
            rows,
        )
    }

    /// Every row of every read, in the order the reads arrived, appears in the
    /// buffer in that order -- the buffer is a supersequence of its own input.
    /// A history that lost rows is a nuisance; a history that reordered them is
    /// what a screenshot of scrambled output looks like.
    fn assert_supersequence_in_arrival_order(held: &[String], reads: &[Vec<String>]) {
        for rows in reads {
            let mut cursor = 0;
            for row in rows {
                match held[cursor..].iter().position(|line| line == row) {
                    Some(hit) => cursor += hit + 1,
                    // A row may have rolled off the ceiling or been overwritten
                    // by a newer rendering of itself; what it may never do is
                    // turn up before a row that arrived ahead of it.
                    None => continue,
                }
            }
        }
    }

    /// Rows a duplicate would be visible in: long, and carrying a word rather
    /// than a rule. A composer draws the same horizontal rule above and below
    /// its prompt and a table draws the same border on every row of it, so a
    /// repeated run of box-drawing is what a correct screen looks like, not what
    /// a duplicated one does.
    fn substantial_duplicates(held: &str) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for line in split_lines(held) {
            let row = line.trim().to_owned();
            if row.chars().count() > 20 && row.chars().any(char::is_alphanumeric) {
                *counts.entry(row).or_default() += 1;
            }
        }
        let mut repeated: Vec<(String, usize)> =
            counts.into_iter().filter(|(_, count)| *count > 1).collect();
        repeated.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        repeated
    }

    #[test]
    fn a_pinned_composer_does_not_make_every_poll_a_new_screen() {
        // The P0. A transcript scrolling under a pinned eight-row composer holds
        // the scored agreement under MATCH_THRESHOLD from k = 19 rows a poll
        // upward, however quiet the pane is, because the box is matched against
        // transcript rows it never was. Before the anchored placement every one
        // of these scrolls appended a whole sixty-four row screen.
        //
        // Driven through `observe_agent_pane` -> `record_frame`, the real path
        // production data takes, rather than a hand-passed `owns_screen`: this
        // is the exact pane shape card #795's fix broke (see
        // `an_agent_pane_shaped_like_production_keeps_its_transcript`), so its
        // own coverage has to exercise the same value production computes.
        for scroll in [1_usize, 5, 19, 20, 30, 40, 50] {
            let mut store = ScrollbackStore::default();
            observe_agent_pane(&mut store);
            let mut reads: Vec<Vec<String>> = Vec::new();
            for poll in 0..12 {
                let screen = agent_screen(poll * scroll, poll);
                reads.push(split_lines(&screen));
                store.record_frame(
                    AGENT_SESSION,
                    AGENT_PANE,
                    "recent_unwrapped",
                    "text",
                    &screen,
                );
            }
            let held = agent_window(&store, MAX_PANE_LINES).unwrap();
            let rows = split_lines(&held);

            let duplicates = substantial_duplicates(&held);
            assert!(
                duplicates.is_empty(),
                "scrolling {scroll} rows a poll duplicated {} rows, worst {:?}",
                duplicates.iter().map(|(_, count)| count - 1).sum::<usize>(),
                duplicates.first()
            );

            // Every transcript row the pane ever showed is kept exactly once,
            // and the composer is kept exactly once, at the bottom.
            let produced = AGENT_TRANSCRIPT_ROWS + 11 * scroll;
            assert_eq!(
                rows.len(),
                produced + AGENT_BOX_ROWS,
                "scrolling {scroll} rows a poll held {} rows, expected {}",
                rows.len(),
                produced + AGENT_BOX_ROWS
            );
            assert!(rows[0].contains("transcript row 0 "));
            assert_supersequence_in_arrival_order(&rows, &reads);
        }
    }

    #[test]
    fn a_pinned_composer_matching_itself_is_not_a_placement() {
        // The trap the anchor exists to refuse. At full overlap the composer
        // lines up with itself -- eight agreeing rows, sixty rows from the head
        // of the read -- and believing that would throw away every transcript
        // row that scrolled. The agreeing run has to start where the read does.
        //
        // Driven through `observe_agent_pane` -> `record_frame` (see that
        // test's own note on why).
        let mut store = ScrollbackStore::default();
        observe_agent_pane(&mut store);
        store.record_frame(
            AGENT_SESSION,
            AGENT_PANE,
            "recent_unwrapped",
            "text",
            &agent_screen(0, 0),
        );
        store.record_frame(
            AGENT_SESSION,
            AGENT_PANE,
            "recent_unwrapped",
            "text",
            &agent_screen(50, 1),
        );
        let rows = split_lines(&agent_window(&store, MAX_PANE_LINES).unwrap());
        // 56 transcript rows, then 50 more scrolled in, then the composer.
        assert_eq!(rows.len(), AGENT_TRANSCRIPT_ROWS + 50 + AGENT_BOX_ROWS);
        assert!(rows[0].contains("transcript row 0 "));
    }

    #[test]
    fn output_that_really_repeats_itself_is_kept_every_time() {
        // A test suite printing the same line per case, an agent echoing the
        // same block twice: identical content is not evidence of a re-send, and
        // collapsing it would be inventing a history the pane never had.
        let mut store = ScrollbackStore::default();
        let block = ["  ✓ ok", "  ✓ ok", "  ✓ ok", "  ✓ ok"];
        let mut printed: Vec<String> = Vec::new();
        for round in 0..12 {
            printed.push(format!("── case {round} ──────────────"));
            printed.extend(block.iter().map(|row| (*row).to_owned()));
            let top = printed.len().saturating_sub(20);
            store.record("k", &printed[top..].join("\n"), false);
        }
        let held = store.window("k", MAX_PANE_LINES).unwrap();
        let rows = split_lines(&held);
        assert_eq!(rows.len(), printed.len(), "a real repeat was collapsed");
        assert_eq!(rows, printed);
        assert_eq!(rows.iter().filter(|row| row.trim() == "✓ ok").count(), 48);
    }

    #[test]
    fn a_screen_cleared_and_reprinted_is_new_content() {
        // `clear` then fresh output: nothing is a re-send, and the read has to
        // land whole on top of what it followed rather than be folded into it.
        let mut store = ScrollbackStore::default();
        store.record("k", &agent_screen(0, 0), false);
        let before = split_lines(&store.window("k", MAX_PANE_LINES).unwrap()).len();
        let fresh: Vec<String> = (0..40)
            .map(|row| format!("$ a completely different program, line {row}"))
            .collect();
        store.record("k", &fresh.join("\n"), false);
        let rows = split_lines(&store.window("k", MAX_PANE_LINES).unwrap());
        assert_eq!(rows.len(), before + 40);
        assert!(rows[0].contains("transcript row 0 "));
        assert_eq!(
            rows.last().unwrap(),
            "$ a completely different program, line 39"
        );
    }

    #[test]
    fn rows_that_scroll_back_into_view_are_taken_down_again() {
        // A long line re-wraps, a tool block collapses: the screen moves
        // backward and rows the buffer already promoted into history are on it
        // again. Writing them a second time is the whole of the duplication left
        // once the pinned composer is handled.
        let mut store = ScrollbackStore::default();
        store.record("k", &agent_screen(0, 0), false);
        store.record("k", &agent_screen(12, 1), false);
        let scrolled = split_lines(&store.window("k", MAX_PANE_LINES).unwrap()).len();
        assert_eq!(scrolled, AGENT_TRANSCRIPT_ROWS + 12 + AGENT_BOX_ROWS);
        // Back to where it was.
        store.record("k", &agent_screen(0, 2), false);
        let held = store.window("k", MAX_PANE_LINES).unwrap();
        assert!(substantial_duplicates(&held).is_empty());
        assert_eq!(
            split_lines(&held).len(),
            AGENT_TRANSCRIPT_ROWS + AGENT_BOX_ROWS,
            "the twelve rows that came back were written twice"
        );
    }

    #[test]
    fn a_read_no_placement_believed_is_not_appended_twice() {
        // The floor under everything else. Where neither placement finds a seam,
        // whatever the buffer verbatim ends with is still not written down
        // again: appending rows the buffer already ends with cannot be right
        // whatever the placement thought.
        let mut store = ScrollbackStore::default();
        store.record("k", "keep me\ntail 1\ntail 2\ntail 3", false);
        // Shares its head with the buffer's tail, but is otherwise a different
        // screen -- too different for either placement to believe.
        store.record(
            "k",
            "tail 1\ntail 2\ntail 3\nq\nw\ne\nr\nt\ny\nu\ni\no\np\na\ns\nd",
            false,
        );
        let held = store.window("k", MAX_PANE_LINES).unwrap();
        let rows = split_lines(&held);
        assert_eq!(rows.iter().filter(|row| *row == "tail 1").count(), 1);
        assert_eq!(rows.iter().filter(|row| *row == "tail 3").count(), 1);
        assert_eq!(rows[0], "keep me");
        assert_eq!(rows.last().unwrap(), "d");
    }

    #[test]
    fn a_watched_agent_pane_never_grows_a_duplicate() {
        // The whole failure as the pane actually lives it: bursts of output
        // between polls, quiet spells where only the timer turns, and a couple
        // of jumps that outran the poll entirely.
        //
        // Driven through `observe_agent_pane` -> `record_frame` (see that
        // test's own note on why).
        let mut store = ScrollbackStore::default();
        observe_agent_pane(&mut store);
        let mut reads: Vec<Vec<String>> = Vec::new();
        let mut top = 0;
        for poll in 0..200 {
            let scroll = match poll % 10 {
                0..=2 => 0,  // the timer turns, nothing scrolls
                3 | 4 => 2,  // a line at a time
                5..=7 => 24, // a burst, past where the ratio gives up
                8 => 47,     // a bigger burst
                _ => 3,
            };
            top += scroll;
            let screen = agent_screen(top, poll);
            reads.push(split_lines(&screen));
            store.record_frame(
                AGENT_SESSION,
                AGENT_PANE,
                "recent_unwrapped",
                "text",
                &screen,
            );
        }
        let held = agent_window(&store, MAX_PANE_LINES).unwrap();
        let duplicates = substantial_duplicates(&held);
        assert!(
            duplicates.is_empty(),
            "{} substantial rows duplicated, worst {:?}",
            duplicates.iter().map(|(_, count)| count - 1).sum::<usize>(),
            duplicates.first()
        );
        let rows = split_lines(&held);
        assert_eq!(rows.len(), AGENT_TRANSCRIPT_ROWS + top + AGENT_BOX_ROWS);
        assert_supersequence_in_arrival_order(&rows, &reads);
    }

    #[test]
    fn a_wider_read_of_the_same_screen_does_not_double_it() {
        // The reader's paging re-requests the whole window at a wider limit, so
        // the same rows arrive again inside a longer read.
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]), false);
        store.record("k", &screen(&["3", "4", "5", "6"]), false);
        let held_before = store.window("k", 100).unwrap();
        store.record("k", &screen(&["1", "2", "3", "4", "5", "6"]), false);
        assert_eq!(store.window("k", 100).unwrap(), held_before);
    }

    #[test]
    fn an_agent_pane_shaped_like_production_keeps_its_transcript() {
        // Card #795 confirmation. Measured live: a Claude Code pane reports
        // `alternate_on=1`, exactly like nvim's ("%1|claude|alternate_on=1|
        // history_size=0" vs. "%27|nvim|alternate_on=1|history_size=0"), so
        // gating `record`'s replace fallback on `alternate_on` alone cannot
        // tell an editor's pane from an agent's. This drives an agent-shaped
        // pane through the real `observe()` -> `keeps()` -> `serve_read()`
        // path with the exact envelope shape `compat::pane` emits for one
        // (`alternate_on: true`, `foreground_command: "claude"`) -- not a
        // hand-passed parameter -- and asserts on the transcript this store
        // ends up serving.
        let mut store = ScrollbackStore::default();
        observe_agent_pane(&mut store);
        let mut top = 0;
        for poll in 0..40 {
            let scroll = match poll % 5 {
                0 | 1 => 0,
                2 | 3 => 2,
                _ => 24,
            };
            top += scroll;
            let screen = agent_screen(top, poll);
            store.serve_read(
                AGENT_SESSION,
                AGENT_PANE,
                "recent_unwrapped",
                "text",
                &screen,
                MAX_PANE_LINES,
            );
        }
        let held = agent_window(&store, MAX_PANE_LINES).unwrap();
        let rows = split_lines(&held);
        // The pane's viewport is AGENT_SCREEN_ROWS rows; the transcript has
        // scrolled `top` rows past that. If the store served only the current
        // screen -- the regression -- this holds at most AGENT_SCREEN_ROWS
        // rows and the earliest transcript row is gone.
        assert!(
            rows.len() > AGENT_SCREEN_ROWS,
            "held only {} rows (viewport is {}); the agent pane's transcript \
             collapsed to its current screen",
            rows.len(),
            AGENT_SCREEN_ROWS
        );
        assert!(
            rows[0].contains("transcript row 0 "),
            "the earliest transcript row is gone: {:?}",
            rows.first()
        );
    }

    #[test]
    fn an_nvim_pane_shaped_like_production_replaces_rather_than_accumulates() {
        // The other half of card #795's confirmation, driven the same way as
        // `an_agent_pane_shaped_like_production_keeps_its_transcript`: the
        // exact envelope shape `compat::pane` emits for a live nvim pane
        // (`foreground_command: "nvim"`, `history_size: 0`, so
        // `max_offset_from_bottom: 0`), through the real `observe()` ->
        // `keeps()` -> `serve_read()` path. nvim is a static rectangle --
        // nothing scrolls off the top -- so accumulating two unrelated-looking
        // repaints of it is exactly the doubled-frame bug `69c6df8` fixed, and
        // must not come back.
        let mut store = ScrollbackStore::default();
        store.observe(
            "s",
            &json!({
                "pane_id": "p",
                "foreground_command": "nvim",
                "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 3 },
            }),
        );
        store.serve_read(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "old 1\nold 2\nold 3",
            20,
        );
        let served = store.serve_read(
            "s",
            "p",
            "recent_unwrapped",
            "text",
            "new 1\nnew 2\nnew 3",
            20,
        );
        assert_eq!(served, "new 1\nnew 2\nnew 3");
        assert_eq!(
            store
                .window(&read_key("s", "p", "recent_unwrapped", "text"), 20)
                .unwrap(),
            "new 1\nnew 2\nnew 3"
        );
    }

    /// A Claude-like full-screen agent, as the App's scripted reproduction drove
    /// it: a transcript of numbered messages (with a `✻ Waiting for` banner every
    /// fifth one) scrolling under a pinned five-row composer whose mode line
    /// carries a timer. `top` is `None` for the newest screen, or the transcript
    /// row the view starts at when the agent has redrawn an older screen.
    struct ScriptedAgent {
        log: Vec<String>,
        messages: usize,
        /// Whether a spinner row redrawn on every frame sits between the
        /// transcript and the composer -- the row above Claude Code's composer
        /// that the furniture rule cannot strip, because no two frames share it.
        spinner: bool,
    }

    const SCRIPTED_BODY_ROWS: usize = 25;

    impl ScriptedAgent {
        fn new(spinner: bool) -> Self {
            Self {
                log: Vec::new(),
                messages: 0,
                spinner,
            }
        }

        fn print(&mut self) {
            self.messages += 1;
            let n = self.messages;
            self.log.push(String::new());
            self.log.push(format!(
                "● Message number {n}: the agent says something distinctive {}",
                n * 7919 % 10007
            ));
            if n.is_multiple_of(5) {
                self.log.push(String::new());
                self.log.push(format!(
                    "✻ Waiting for {} background agents to finish",
                    n % 3 + 1
                ));
            }
        }

        fn screen(&self, top: Option<usize>, tick: usize) -> String {
            let transcript = SCRIPTED_BODY_ROWS - usize::from(self.spinner);
            let start = top.unwrap_or(self.log.len().saturating_sub(transcript));
            let mut rows: Vec<String> = (0..transcript)
                .map(|row| self.log.get(start + row).cloned().unwrap_or_default())
                .collect();
            if self.spinner {
                rows.push(format!("✢ Compacting conversation… ({tick}s)"));
            }
            rows.extend([
                String::new(),
                "─".repeat(80),
                "❯ ".to_owned(),
                "─".repeat(80),
                format!("  ⏵⏵ bypass permissions on · {tick}s"),
            ]);
            rows.join("\n")
        }
    }

    /// The message numbers the buffer holds, top to bottom.
    fn held_messages(held: &str) -> Vec<usize> {
        split_lines(held)
            .iter()
            .filter_map(|line| {
                line.strip_prefix("● Message number ")?
                    .split(':')
                    .next()?
                    .parse()
                    .ok()
            })
            .collect()
    }

    #[test]
    fn a_screen_redrawn_far_back_in_history_is_not_appended_again() {
        // The owner's screenshot and the App's scripted reproduction: the agent
        // redraws an older screen (the transcript sixty rows up), the placement
        // cannot anchor it within reach of the buffer's tail, and the whole
        // older screen went on the end -- then the newest screen went on the end
        // after it once the agent came back down. Both copies stayed for as long
        // as the gateway ran.
        for (back, spinner) in [30_usize, 60, 100, 200]
            .into_iter()
            .flat_map(|back| [(back, false), (back, true)])
        {
            let mut agent = ScriptedAgent::new(spinner);
            let mut store = ScrollbackStore::default();
            let mut shown = std::collections::BTreeSet::new();
            for tick in 0..120 {
                agent.print();
                // Long enough that the newest screen has moved on by more than
                // a screen when the agent comes back down to it.
                let top = (40..56)
                    .contains(&tick)
                    .then(|| agent.log.len().saturating_sub(SCRIPTED_BODY_ROWS + back));
                let screen = agent.screen(top, tick);
                shown.extend(held_messages(&screen));
                store.record("k", &screen, false);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            // Every message the pane ever showed, once, in order. (The ones
            // printed while the older screen was up and scrolled away before
            // the agent came back down were never on screen to be kept.)
            assert_eq!(
                held_messages(&held),
                shown.into_iter().collect::<Vec<_>>(),
                "a redraw {back} rows back (spinner: {spinner}) left the transcript duplicated or out of order"
            );
            assert!(substantial_duplicates(&held)
                .iter()
                .all(|(row, _)| row.starts_with("✻ Waiting for")));
            // The composer is kept once, at the bottom.
            let rows = split_lines(&held);
            assert_eq!(rows.iter().filter(|row| row.starts_with("❯")).count(), 1);
            assert!(rows
                .last()
                .unwrap()
                .contains("bypass permissions on · 119s"));
        }
    }

    #[test]
    fn a_strict_rerender_shifted_back_keeps_every_line_once() {
        // The narrowest form of the same thing: steady output, then one read
        // that is an exact re-render of an earlier screen shifted up by `shift`
        // rows, then a burst that outruns the poll, then steady output again.
        for (shift, spinner) in [4_usize, 10, 25, 40, 61, 90, 150]
            .into_iter()
            .flat_map(|shift| [(shift, false), (shift, true)])
        {
            let mut agent = ScriptedAgent::new(spinner);
            let mut store = ScrollbackStore::default();
            let mut shown = std::collections::BTreeSet::new();
            let mut tick = 0;
            let mut read =
                |agent: &ScriptedAgent, top: Option<usize>, store: &mut ScrollbackStore| {
                    let screen = agent.screen(top, tick);
                    shown.extend(held_messages(&screen));
                    store.record("k", &screen, false);
                    tick += 1;
                };
            for _ in 0..80 {
                agent.print();
                read(&agent, None, &mut store);
            }
            let top = agent.log.len().saturating_sub(SCRIPTED_BODY_ROWS + shift);
            read(&agent, Some(top), &mut store);
            for _ in 0..15 {
                agent.print();
            }
            for _ in 0..10 {
                agent.print();
                read(&agent, None, &mut store);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            assert_eq!(
                held_messages(&held),
                shown.into_iter().collect::<Vec<_>>(),
                "a re-render shifted {shift} rows back (spinner: {spinner}) duplicated or reordered the transcript"
            );
        }
    }

    #[test]
    fn an_agent_that_jumps_about_never_duplicates_or_reorders() {
        // Bursts, quiet polls, and redraws of an older screen at random depths
        // for random lengths, on a fixed seed: whatever the sequence, every
        // message the pane showed is held once, in the order it was printed.
        //
        // The one thing the sequence may not do is show rows no read could
        // have kept: output printed while the older screen is up must not
        // scroll it into rows nobody saw at the bottom, nor carry the newest
        // screen clean past the one it left. A screen of rows the buffer has
        // never seen is new output as far as anything can tell.
        for seed in 1_u64..=60 {
            let mut state = seed;
            let mut next = |bound: u64| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) % bound
            };
            let mut agent = ScriptedAgent::new(seed % 2 == 0);
            let mut store = ScrollbackStore::default();
            let mut shown = std::collections::BTreeSet::new();
            // How far up the older screen is, and how long the log was when
            // the agent went up there.
            let mut jump: Option<(usize, usize)> = None;
            for tick in 0..300 {
                for _ in 0..next(4) {
                    agent.print();
                }
                // The agent comes back down at the end, so nothing is left set
                // aside under an older screen when the buffer is checked.
                let roll = if tick >= 297 { 0 } else { next(20) };
                jump = match jump {
                    _ if tick >= 297 => None,
                    None if roll == 0 => Some((next(150) as usize + 1, agent.log.len())),
                    Some(_) if roll <= 3 => None,
                    Some((back, left))
                        if agent.log.len() - left >= back.min(SCRIPTED_BODY_ROWS - 6) =>
                    {
                        None
                    }
                    jump => jump,
                };
                let top =
                    jump.map(|(back, _)| agent.log.len().saturating_sub(SCRIPTED_BODY_ROWS + back));
                let screen = agent.screen(top, tick);
                if top.is_none() {
                    shown.extend(held_messages(&screen));
                }
                store.record("k", &screen, false);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            let messages = held_messages(&held);
            assert!(
                messages.windows(2).all(|pair| pair[0] < pair[1]),
                "seed {seed}: duplicated or reordered: {messages:?}"
            );
            // A row a burst carried past before any newest screen showed it can
            // turn up from an older screen: in order, and harmless. The other
            // way round is allowed one row: a run broken on one row may
            // take a lone row repainted across the seam for the same row
            // repainted, which is the trade it makes.
            let lost: Vec<_> = shown.iter().filter(|n| !messages.contains(n)).collect();
            assert!(lost.len() <= 1, "seed {seed}: lost {lost:?}");
        }
    }

    /// Claude Code as it looks on the owner's pane: a transcript above a
    /// ten-row status region -- the spinner with its clock, a blank, the
    /// prompt between two rules, the mode line, a blank, and the agent roster
    /// -- of which the spinner, the clock and the roster repaint in place.
    /// `painted` is how many status rows have been drawn so far, for a read
    /// taken in the middle of a redraw.
    fn claude_screen(top: usize, tick: usize, painted: usize) -> String {
        const TRANSCRIPT: usize = 30;
        let mut rows: Vec<String> = (0..TRANSCRIPT)
            .map(|row| {
                let n = top + row;
                if n % 3 == 2 {
                    String::new()
                } else {
                    format!("● transcript line {n}: something the agent said")
                }
            })
            .collect();
        let status = [
            format!(
                "✻ Swirling… ({}m {}s · ↓ {}.{}k tokens)",
                tick / 60,
                tick % 60,
                tick / 10,
                tick % 10
            ),
            String::new(),
            "─".repeat(100),
            "❯ 做好了没".to_owned(),
            "─".repeat(100),
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents".to_owned(),
            String::new(),
            "  ● main".to_owned(),
            format!("  ◯ general-purpose  Reading file {}", tick % 4),
            format!("  ◯ general-purpose  Running {} tools", tick % 3),
        ];
        rows.extend(status.iter().enumerate().map(|(row, line)| {
            if row < painted {
                line.clone()
            } else {
                String::new()
            }
        }));
        rows.join("\n")
    }

    fn assert_claude_history(held: &str, last_line: usize) {
        let rows = split_lines(held);
        let lines: Vec<usize> = rows
            .iter()
            .filter_map(|row| {
                row.strip_prefix("● transcript line ")?
                    .split(':')
                    .next()?
                    .parse()
                    .ok()
            })
            .collect();
        let expected: Vec<usize> = (0..=last_line).filter(|n| n % 3 != 2).collect();
        assert_eq!(
            lines, expected,
            "transcript held twice, out of order or lost"
        );
        // The status region is at the bottom, once: no frozen copy of it in
        // the middle of the transcript.
        for marker in ["✻ Swirling", "❯ 做好了没", "⏵⏵ bypass", "● main"] {
            let at: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.contains(marker))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(at.len(), 1, "{marker:?} held {} times", at.len());
            assert!(
                at[0] + 12 > rows.len(),
                "{marker:?} left in the middle of history at {} of {}",
                at[0],
                rows.len()
            );
        }
    }

    #[test]
    fn the_first_read_after_startup_does_not_freeze_its_status_region_into_history() {
        // The gateway starts with an empty buffer and its first read is the
        // live screen, status region and all. Every read after it has to
        // replace that status region, not keep it while the transcript lands
        // above and below it.
        for (scroll, first_painted) in [
            (1_usize, 10_usize),
            (3, 10),
            (7, 10),
            (3, 4),
            (3, 1),
            (12, 6),
        ] {
            let mut store = ScrollbackStore::default();
            let mut top = 0;
            store.record("k", &claude_screen(top, 82, first_painted), false);
            for tick in 83..140 {
                // Quiet polls, where only the clock and the roster move.
                if tick % 4 != 0 {
                    top += scroll;
                }
                store.record("k", &claude_screen(top, tick, 10), false);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            let last = top + 29;
            let last = if last % 3 == 2 { last - 1 } else { last };
            assert_claude_history(&held, last);
        }
    }

    #[test]
    fn a_status_region_that_changes_height_never_freezes_into_history() {
        // The same pane after startup, with a status region that grows and
        // shrinks between reads -- the spinner comes and goes, the roster
        // gains and loses agents, a feedback box opens above the prompt -- on
        // a fixed seed. The prompt is held once, at the bottom, and the
        // transcript once, in order.
        for seed in 1_u64..=60 {
            let mut state = seed;
            let mut next = |bound: u64| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) % bound
            };
            let mut store = ScrollbackStore::default();
            let mut top = 0;
            let mut last = 0;
            for tick in 80..200 {
                if tick > 80 && next(4) != 0 {
                    top += next(6) as usize;
                }
                let spinner = next(3) != 0;
                let roster = next(4) as usize;
                let boxed = next(5) == 0;
                let body = 40 - 6 - usize::from(spinner) * 2 - roster - usize::from(boxed) * 5;
                let mut rows: Vec<String> = (0..body)
                    .map(|row| {
                        let n = top + row;
                        if n % 3 == 2 {
                            String::new()
                        } else {
                            format!("● transcript line {n}: something the agent said")
                        }
                    })
                    .collect();
                last = (0..body)
                    .map(|row| top + row)
                    .filter(|n| n % 3 != 2)
                    .max()
                    .unwrap();
                if spinner {
                    rows.push(format!(
                        "✻ Swirling… (1m {}s · ↓ 3.{}k tokens)",
                        tick % 60,
                        tick % 10
                    ));
                    rows.push(String::new());
                }
                if boxed {
                    rows.push(format!("╭{}", "─".repeat(90)));
                    rows.push("│ ✻ Bug report drafted".to_owned());
                    rows.push("│ 1 to review · 2 to send".to_owned());
                    rows.push(format!("╰{}", "─".repeat(90)));
                    rows.push(String::new());
                }
                rows.push("─".repeat(100));
                rows.push("❯ 做好了没".to_owned());
                rows.push("─".repeat(100));
                rows.push("  ⏵⏵ bypass permissions on (shift+tab to cycle)".to_owned());
                rows.push(String::new());
                rows.push("  ● main".to_owned());
                for agent in 0..roster {
                    rows.push(format!("  ◯ general-purpose  task {agent} at {}", tick % 7));
                }
                store.record("k", &rows.join("\n"), false);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            let rows = split_lines(&held);
            let lines: Vec<usize> = rows
                .iter()
                .filter_map(|row| {
                    row.strip_prefix("● transcript line ")?
                        .split(':')
                        .next()?
                        .parse()
                        .ok()
                })
                .collect();
            let mut seen = HashSet::new();
            let twice: Vec<_> = lines.iter().filter(|n| !seen.insert(**n)).collect();
            let disorder = lines.windows(2).filter(|p| p[0] >= p[1]).count();
            let prompts = rows.iter().filter(|row| row.starts_with("❯")).count();
            let lost = (0..=last)
                .filter(|n| n % 3 != 2 && !lines.contains(n))
                .count();
            assert!(
                twice.is_empty() && disorder == 0 && prompts == 1 && lost == 0,
                "seed {seed}: held twice {twice:?}, {disorder} out of order, \
                 {prompts} prompts, {lost} lost"
            );
        }
    }

    #[test]
    fn a_spinner_is_not_left_in_history_when_the_next_read_does_not_overlap() {
        // Measured live against a scripted pane: a read that could not be
        // placed -- the first one after the gateway started, or one after
        // output outran the poll -- went on top of the screen before it, and
        // that screen's spinner, which no two frames share and so was never
        // furniture, stayed in the middle of history.
        for steady in [0_usize, 1, 5] {
            let mut agent = ScriptedAgent::new(true);
            let mut store = ScrollbackStore::default();
            let mut tick = 0;
            for _ in 0..20 {
                agent.print();
            }
            for _ in 0..=steady {
                agent.print();
                store.record("k", &agent.screen(None, tick), false);
                tick += 1;
            }
            for _ in 0..3 {
                // A burst of more than a screen, then a quiet poll or two.
                for _ in 0..15 {
                    agent.print();
                }
                store.record("k", &agent.screen(None, tick), false);
                tick += 1;
                store.record("k", &agent.screen(None, tick), false);
                tick += 1;
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
            let rows = split_lines(&held);
            let spinners: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.starts_with("✢ Compacting"))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(
                spinners,
                vec![rows.len() - 6],
                "after {steady} steady reads a spinner was kept mid-history"
            );
            let messages = held_messages(&held);
            assert!(messages.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn a_pane_that_flips_to_real_scrollback_and_back_starts_again_from_its_screen() {
        // Claude Code on Herdr: idle on the alternate screen (offset 0, kept),
        // working on the normal screen (Herdr's own scrollback, passed
        // through), idle again with a different screen. The buffer held from
        // before the gap must not end up with the old screen frozen in it.
        fn observe(store: &mut ScrollbackStore, offset: u64) {
            store.observe(
                "s",
                &json!({
                    "pane_id": "p",
                    "foreground_command": "claude",
                    "scroll": { "max_offset_from_bottom": offset, "viewport_rows": 30 },
                }),
            );
        }
        let mut agent = ScriptedAgent::new(true);
        let mut store = ScrollbackStore::default();
        let mut tick = 0;
        let mut read = |store: &mut ScrollbackStore, agent: &ScriptedAgent| {
            tick += 1;
            store.serve_read(
                "s",
                "p",
                "recent_unwrapped",
                "text",
                &agent.screen(None, tick),
                MAX_PANE_LINES,
            )
        };
        observe(&mut store, 0);
        for _ in 0..20 {
            agent.print();
            read(&mut store, &agent);
        }
        assert!(store.depth("s", "p") > 30);

        // Working: Herdr has scrollback of its own, and its text is served.
        observe(&mut store, 1069);
        for _ in 0..40 {
            agent.print();
        }
        let passed = agent.screen(None, 99);
        assert_eq!(
            store.serve_read(
                "s",
                "p",
                "recent_unwrapped",
                "text",
                &passed,
                MAX_PANE_LINES
            ),
            passed
        );
        assert_eq!(
            store.depth("s", "p"),
            0,
            "a passed-through pane kept a buffer"
        );

        // Idle again, on a screen that shares nothing with the one held before.
        observe(&mut store, 0);
        for _ in 0..3 {
            agent.print();
            read(&mut store, &agent);
        }
        let held = agent_window_for(&store, "s", "p");
        let rows = split_lines(&held);
        assert_eq!(rows.iter().filter(|row| row.starts_with('❯')).count(), 1);
        assert_eq!(
            rows.iter()
                .filter(|row| row.starts_with("✢ Compacting"))
                .count(),
            1
        );
        let messages = held_messages(&held);
        assert!(messages.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            messages[0] > 20,
            "the screen from before the gap is still held"
        );
    }

    fn agent_window_for(store: &ScrollbackStore, session: &str, pane: &str) -> String {
        store
            .window(
                &read_key(session, pane, "recent_unwrapped", "text"),
                MAX_PANE_LINES,
            )
            .unwrap()
    }

    #[test]
    fn a_pane_listed_with_real_scrollback_drops_what_was_held() {
        let mut store = ScrollbackStore::default();
        let pane = |offset: u64| json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": offset, "viewport_rows": 3 } });
        store.observe("s", &pane(0));
        store.serve_read("s", "p", "recent_unwrapped", "text", "a\nb\nc", 100);
        store.serve_read("s", "p", "recent_unwrapped", "text", "b\nc\nd", 100);
        assert_eq!(store.depth("s", "p"), 4);
        store.observe("s", &pane(500));
        assert_eq!(store.depth("s", "p"), 0);
        assert_eq!(store.total_bytes, 0);
    }
}
