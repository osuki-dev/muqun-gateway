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
//! each read is placed against the end of it. The placement is the largest
//! overlap `o` where the buffer's last `o` rows agree with the read's first `o`
//! rows; the read then replaces those rows and the buffer keeps everything
//! above them. Largest rather than smallest, because the smallest overlap that
//! happens to line up on blank rows is how a buffer grows a copy of itself.
//!
//! Taking the incoming rows over the ones they overlap, rather than keeping
//! what was there, is what makes the stale half of a repaint go away: the
//! newest rendering of a row is the true one.
//!
//! Agreement is scored rather than demanded. This is the whole difference
//! between a buffer and a leak: requiring every row to match would make one
//! turning spinner look like a screen with nothing in common, and a screen with
//! nothing in common is kept whole -- sixty-five rows, six times a second. An
//! overlap is believed when [`MATCH_THRESHOLD`] of its rows agree, or outright
//! below [`MIN_MATCH_ROWS`] where a ratio means nothing. The spinner then places
//! at full overlap: the screen changed, nothing scrolled, the buffer does not
//! grow.
//!
//! ## The pinned box, and why a ratio alone is not enough
//!
//! An agent pane is not a uniformly scrolling rectangle. Claude Code pins a
//! composer at the bottom -- a rule, the prompt row, another rule, the mode
//! line, the agent roster: eight rows on the pane this was measured against --
//! and scrolls only the transcript above it. So when the transcript moves by
//! `k` rows, the aligned overlap of width `N - k` contains `k`-worth of matching
//! transcript *and the whole pinned box mismatched against transcript rows it
//! never was*. The ratio is therefore capped below one however quiet the pane
//! is, and it falls as `k` grows:
//!
//! ```text
//! agreement(k) = (rows_above_the_box - 1) / (N - k)
//! ```
//!
//! with the `- 1` paid to the volatile timer row (`4m 46s · ↓ 2.9k tokens`).
//! For the measured pane -- `N = 64`, an eight-row box -- that crosses
//! [`MATCH_THRESHOLD`] at `k = 19`. Below it every read places; at and above it
//! **no** overlap is believed and the whole sixty-four row screen is appended,
//! of which only `k` rows are new. A burst of output scrolls further than
//! nineteen rows between two 150ms polls routinely, so the buffer grew a copy
//! of most of a screen per poll for as long as the burst lasted. Measured on
//! the live loopback against Ellen's own pane: 62% of the substantial rows in
//! the served window were duplicates, one row repeated twelve times, and the
//! count climbed 54 -> 131 over twenty-five seconds of watching.
//!
//! So a second placement runs where the ratio finds nothing: the read is
//! anchored by the **run of rows agreeing from its own head**. At a true
//! placement the first row of the overlap is the first row the read re-sends,
//! so the agreeing run starts at incoming row 0 (or row 1, forgiving one
//! repainted row at the seam) and is as long as the transcript that did not
//! scroll. The widest alignment where the pinned box happens to line up with
//! itself has no such run -- its matches sit at the *end* of the overlap, sixty
//! rows from the head -- which is what stops the fallback from swallowing a
//! whole screen of history to buy an eight-row coincidence. Longest anchored
//! run wins, ties going to the wider overlap.
//!
//! The anchor is allowed to reach [`ANCHOR_REACH`] reads back rather than one,
//! because a screen does not only move forward. When a long line re-wraps or a
//! tool block collapses, rows that had scrolled off the top come back onto the
//! screen -- and the buffer has already promoted those rows into history above
//! it. Reaching back lets the placement take them down again instead of writing
//! them a second time. On the six minutes of live frames this was measured
//! against, that one allowance is the difference between thirteen duplicated
//! rows and none.
//!
//! Where neither placement believes anything, the screen moved further than it
//! can be followed -- output arrived faster than the poll, and the two screens
//! are consecutive rather than overlapping. The read is then kept on top of the
//! screen it followed, because that is what the pane showed and dropping it
//! would throw away the very rows this exists to keep -- but no longer kept
//! *whole*. Whatever prefix of it is already sitting at the end of the buffer,
//! exactly, is not written down twice. That last step is what makes duplication
//! impossible by construction rather than merely unlikely: the appended rows
//! are, by definition, rows the buffer does not already end with.
//!
//! Every one of these paths drops from the tail and appends -- none of them
//! splices into the middle -- so the buffer stays a supersequence of every read
//! that fed it, in arrival order. That is the invariant the reader actually
//! needs: a shorter history is a nuisance, a reordered one is a bug report.
//!
//! On exact matches this is the same placement `mergeTerminalWindow` makes,
//! which is the point: the two ends of the same pane must not disagree about
//! where a row belongs.
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
//! | ratio floor | [`MIN_MATCH_ROWS`] | `SCREEN_MIN_MATCH_ROWS` |
//! | anchored run | [`ANCHOR_ROWS`] | `SCREEN_ANCHOR_ROWS` |
//! | seam skew | [`ANCHOR_SKEW`] | `SCREEN_ANCHOR_SKEW` |
//! | backward reach | [`ANCHOR_REACH`] | `SCREEN_ANCHOR_REACH` |
//! | furniture cap | [`FURNITURE_SHARE`] | `FURNITURE_SHARE` |
//! | volatile allowance | [`FURNITURE_VOLATILE_ROWS`] | `FURNITURE_VOLATILE_ROWS` |
//!
//! A number that changes on one side and not the other is a bug, and the shape
//! it takes is a row written down twice.
//!
//! ## The four invariants
//!
//! - **(a) supersequence, in arrival order.** Every path here drops from the
//!   tail and appends; none splices. Stated above and still true.
//! - **(b) depth is never reduced.** The gateway grows a buffer or trims it from
//!   the *top* at [`MAX_PANE_LINES`]. It has no operation that shortens history
//!   from the bottom, and must not grow one.
//! - **(c) furniture is never history.** [`ScrollbackStore::record`] -- and see
//!   the correction below, because this rule was not working.
//! - **(d) identical adjacent blocks never accumulate.** `already_held` is the
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

/// How much of the overlap has to agree before a shift is believed.
const MATCH_THRESHOLD: f64 = 0.8;

/// The widest overlap looked for. Bounds the alignment scan on the wide reads
/// (`lines=2000`) the reader's paging makes.
const MAX_OVERLAP: usize = 2_048;

/// Rows below which a ratio means nothing and the rows have to agree outright:
/// two screens matching on one blank row is not evidence of anything.
const MIN_MATCH_ROWS: usize = 4;

/// How long the run of rows agreeing from the read's own head has to be before
/// the anchored placement is believed.
///
/// Three consecutive identical rows at exactly the seam is weak evidence taken
/// alone and strong evidence taken here, because the alternative on the table
/// is appending a screen the pane never scrolled. Two would admit a pair of
/// blank rows; four would miss the tightest real bursts, where the transcript
/// scrolls to within a few rows of the whole screen.
const ANCHOR_ROWS: usize = 3;

/// How far into the read the anchoring run may start.
///
/// Zero is the true seam. One forgives a single row repainted across the seam
/// -- a wrapped line finishing, a spinner that happened to land on the first
/// row the read re-sends -- without opening the search up to matches that sit
/// anywhere at all, which is the coincidence the whole anchor exists to refuse.
const ANCHOR_SKEW: usize = 1;

/// How far back the anchored placement may reach, as a multiple of the read.
///
/// One read's worth would only ever let a screen be replaced by itself, and a
/// screen does not only move forward. A pane re-wraps when a long line resolves,
/// an agent collapses a tool block, a `\[2J` lands a row off: content that had
/// scrolled off the top comes *back*, and the rows now on screen are rows the
/// buffer has already promoted into history above it. Measured on the live pane,
/// that is the whole of the residual duplication -- two backward jumps of twelve
/// rows in six minutes, each one appending a screen the buffer already held.
///
/// Two lets the placement reach a full screen back into history and take those
/// rows down again instead of writing them twice. It is also the ceiling on how
/// much history one read can retract, which is why it is a small number: a read
/// may correct the screen it followed, not rewrite the session.
const ANCHOR_REACH: usize = 2;

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

fn line_hash(line: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish()
}

/// Whether the last `overlap` rows of `held` are the first `overlap` rows of
/// `incoming`, allowing for the cells a repaint changed without scrolling.
fn rows_agree(held: &[u64], incoming: &[u64], overlap: usize) -> bool {
    if overlap == 0 {
        return false;
    }
    let start = held.len() - overlap;
    let agreed = held[start..]
        .iter()
        .zip(incoming.iter())
        .filter(|(left, right)| left == right)
        .count();
    if overlap < MIN_MATCH_ROWS {
        return agreed == overlap;
    }
    agreed as f64 >= overlap as f64 * MATCH_THRESHOLD
}

/// How many rows carrying something agree, running from the read's own head.
///
/// The run starts at incoming row `skew` and stops at the first disagreement;
/// what is returned is the rows in it that were not blank. Blank rows hold a run
/// together but are never evidence for one, because thirty aligned blank rows
/// say only that both screens have room at the bottom.
///
/// Zero where the read's head does not agree with where this overlap says it
/// belongs. That is the answer that keeps a pinned composer matching itself from
/// passing as a placement: its agreeing rows sit at the *end* of the widest
/// overlap, sixty rows from the head, so the run there is empty.
fn anchor_score(
    held: &[u64],
    incoming: &[u64],
    carries: &[bool],
    overlap: usize,
    skew: usize,
) -> usize {
    if overlap <= skew {
        return 0;
    }
    let start = held.len() - overlap + skew;
    held[start..]
        .iter()
        .zip(incoming[skew..].iter())
        .zip(carries[skew..].iter())
        .take_while(|((left, right), _)| left == right)
        .filter(|(_, carries)| **carries)
        .count()
}

/// The overlap whose anchored run carries the most, or `None` where no overlap
/// carries [`ANCHOR_ROWS`] of them. Ties go to the wider overlap: the alignment
/// that re-sends more of what is held is the one that grows the buffer less.
fn anchored_placement(
    held: &[u64],
    incoming: &[u64],
    carries: &[bool],
    widest: usize,
) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for overlap in 1..=widest {
        let score = (0..=ANCHOR_SKEW)
            .map(|skew| anchor_score(held, incoming, carries, overlap, skew))
            .max()
            .unwrap_or(0);
        if score >= ANCHOR_ROWS && best.is_none_or(|(seen, _)| score >= seen) {
            best = Some((score, overlap));
        }
    }
    best.map(|(_, overlap)| overlap)
}

/// Where this read sits against the end of what is held: how many held rows the
/// read re-sends.
///
/// The anchored run answers first. It is the rule that compares candidate
/// alignments against each other instead of accepting the widest one that clears
/// a bar, and that difference is worth two bugs: a pinned composer no longer
/// hides the seam, and a screen with a repeating shape -- a test suite printing
/// the same row per case -- can no longer clear [`MATCH_THRESHOLD`] at full
/// overlap by lining its own period up with itself and taking the entire history
/// with it.
///
/// The scored alignment answers where the anchor finds nothing, which is where a
/// row repaints too close to the seam for a run to get going. It keeps the
/// tolerance the anchor is too strict for, on the reads the anchor has already
/// declined to explain.
///
/// `None` where neither believes anything: the screen moved further than one
/// read can be followed, and nothing held is being re-sent.
fn placement(held: &[u64], incoming: &[u64], carries: &[bool]) -> Option<usize> {
    // The anchor may reach past the read's own length, back into history, for
    // the rows a backward jump has put on screen again. The scored alignment may
    // not: its ratio is only meaningful where every row of the overlap has a row
    // of the read to be compared against.
    let aligned = held.len().min(incoming.len()).min(MAX_OVERLAP);
    let reach = held
        .len()
        .min(incoming.len().saturating_mul(ANCHOR_REACH))
        .min(MAX_OVERLAP);
    anchored_placement(held, incoming, carries, reach).or_else(|| {
        (1..=aligned)
            .rev()
            .find(|overlap| rows_agree(held, incoming, *overlap))
    })
}

/// How much of the read the buffer already ends with, exactly.
///
/// The last resort, for reads no placement believed. Appending rows the buffer
/// verbatim ends with is the one thing that cannot be right: they are already
/// there, already contiguous, already in this order. Dropping them cannot lose
/// anything and cannot reorder anything, and it is what makes a duplicate
/// impossible rather than unlikely.
fn already_held(held: &[u64], incoming: &[u64]) -> usize {
    let widest = held.len().min(incoming.len()).min(MAX_OVERLAP);
    (1..=widest)
        .rev()
        .find(|count| held[held.len() - count..] == incoming[..*count])
        .unwrap_or(0)
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
/// So the run is scored, exactly as `rows_agree` scores an overlap:
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
}

impl PaneBuffer {
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
        if self.lines.len() > MAX_PANE_LINES {
            self.drop_front(self.lines.len() - MAX_PANE_LINES);
        }
        while self.bytes > MAX_PANE_BYTES && self.lines.len() > 1 {
            self.drop_front(1);
        }
    }

    fn tail(&self, rows: usize) -> Vec<u64> {
        let start = self.hashes.len().saturating_sub(rows);
        self.hashes.iter().skip(start).copied().collect()
    }
}

/// Every pane's kept rows, and which panes are worth keeping them for.
#[derive(Debug, Default)]
pub struct ScrollbackStore {
    buffers: HashMap<String, PaneBuffer>,
    /// Panes Herdr reports no scrollback for, by `session/pane`. Only these are
    /// ever recorded or answered from.
    kept: HashMap<String, bool>,
    /// Panes whose foreground program owns an alternate screen (tmux's
    /// `#{alternate_on}`, riding `Pane::alternate_on` -- see that field's own
    /// doc), by `session/pane`. Absent means unknown, which `owns_screen`
    /// below reads as `false`: a pane this store cannot positively identify as
    /// screen-owning keeps the accumulate-and-place behaviour every pane had
    /// before this field existed.
    owns_screen: HashMap<String, bool>,
    total_bytes: usize,
    clock: u64,
}

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

impl ScrollbackStore {
    /// Learn from a Herdr answer which panes hold no scrollback of their own.
    ///
    /// Takes any `pane.list`, `pane.get` or `session.snapshot` body and walks it
    /// for pane objects, so it does not have to know the shape of each. The
    /// approval watcher already calls `pane.list` every 1500ms, which keeps this
    /// current without asking Herdr for anything new.
    pub fn observe(&mut self, session_id: &str, value: &Value) {
        visit_panes(value, &mut |pane_id, scroll| {
            if let Some(maximum) = scroll.get("max_offset_from_bottom").and_then(Value::as_f64) {
                self.kept
                    .insert(pane_key(session_id, pane_id), maximum <= 0.0);
            }
            if let Some(alternate) = scroll.get("alternate_on").and_then(Value::as_bool) {
                self.owns_screen
                    .insert(pane_key(session_id, pane_id), alternate);
            }
        });
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

    /// Whether this pane's foreground program owns an alternate screen --
    /// repaints in place rather than prints and scrolls -- so a read that
    /// overlaps nothing held should replace rather than accumulate. See
    /// `record`'s own doc on why the two cases need different answers, and
    /// `owns_screen`'s field doc on why unknown reads as `false` here.
    fn owns_screen(&self, session_id: &str, pane_id: &str) -> bool {
        self.owns_screen
            .get(&pane_key(session_id, pane_id))
            .copied()
            .unwrap_or(false)
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
    /// `owns_screen` is `Pane::alternate_on`, forwarded by the caller: a pane
    /// whose foreground program repaints an alternate screen -- nvim, an agent
    /// wrapped in one -- has no real history above its current screen for this
    /// buffer to reconstruct, unlike the printing-and-scrolling pane the rest
    /// of this function's placement logic was written for. Every read of one
    /// *is* the whole of the pane's current truth, so it replaces outright
    /// rather than being placed against what came before: the two-stacked-
    /// frames bug this exists to prevent is exactly what "neither placement
    /// believed anything, so keep the read on top of what we had" produces
    /// for a screen that repainted rather than scrolled -- the client fixes
    /// the identical mistake in `foldPaneRead`'s own `ownsScreen` (see
    /// `src/terminal/history.ts` in the Muqun repo, card #795, defect 2).
    fn record(&mut self, key: &str, text: &str, owns_screen: bool) {
        let incoming = split_lines(text);
        if incoming.is_empty() {
            return;
        }
        self.clock += 1;
        let clock = self.clock;
        let buffer = self.buffers.entry(key.to_owned()).or_default();
        let before = buffer.bytes;
        buffer.touched = clock;

        if owns_screen {
            buffer.drop_back(buffer.lines.len());
            for line in incoming {
                buffer.push(line);
            }
            buffer.trim();
            buffer.last_frame = buffer.hashes.iter().copied().collect();
            self.total_bytes = self.total_bytes + buffer.bytes - before;
            self.evict();
            return;
        }

        // What this frame and the one before it end with identically is the
        // pane's furniture, not its history: an agent's composer -- the rule,
        // the prompt, the mode line -- is pinned to the bottom of the screen
        // while the transcript scrolls above it. Left alone, a copy of it
        // lands in the middle of the transcript every time the screen jumps
        // further than a read can follow, and the reader scrolls back through
        // their own conversation past prompt boxes that were never there.
        // Capped at a third of a screen, so a pane that legitimately repaints
        // the same tail keeps its history.
        let frame: Vec<u64> = incoming.iter().map(|line| line_hash(line)).collect();
        let carries: Vec<bool> = incoming
            .iter()
            .map(|line| !line.trim().is_empty())
            .collect();
        let furniture =
            common_suffix(&buffer.last_frame, &frame, &carries).min(frame.len() / FURNITURE_SHARE);
        if furniture > 0 {
            let held = furniture.min(buffer.lines.len());
            let ends_with_it = buffer
                .hashes
                .iter()
                .rev()
                .take(held)
                .zip(buffer.last_frame.iter().rev())
                .all(|(seen, before)| seen == before);
            if ends_with_it {
                buffer.drop_back(held);
            }
        }

        // How much of the read is already the end of the buffer, and must not be
        // written down a second time. Only ever a prefix of the read, so what is
        // appended stays contiguous and in arrival order.
        let mut skip = 0;
        if !buffer.lines.is_empty() {
            let held = buffer.tail(MAX_OVERLAP);
            match placement(&held, &frame, &carries) {
                // The read re-sends this many held rows; they are dropped so the
                // newest rendering of each of them is the one kept.
                Some(discard) => buffer.drop_back(discard.min(buffer.lines.len())),
                // Neither placement believed anything: the screen moved further
                // than one read can be followed, so nothing held is dropped --
                // but whatever the buffer verbatim ends with is not appended
                // again.
                None => skip = already_held(&held, &frame),
            }
        }
        for line in incoming.into_iter().skip(skip) {
            buffer.push(line);
        }
        buffer.trim();
        buffer.last_frame = frame;
        self.total_bytes = self.total_bytes + buffer.bytes - before;
        self.evict();
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
    pub fn serve_read(
        &mut self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
        backend_text: &str,
        rows: usize,
    ) -> String {
        if !self.keeps(session_id, pane_id) {
            return backend_text.to_owned();
        }

        let key = read_key(session_id, pane_id, source, format);
        let owns_screen = self.owns_screen(session_id, pane_id);
        self.record(&key, backend_text, owns_screen);
        let backend_rows = split_lines(backend_text).len();
        self.window(&key, rows)
            .filter(|served| split_lines(served).len() > backend_rows)
            .unwrap_or_else(|| backend_text.to_owned())
    }

    /// Record a sampled stream frame under the same policy as a direct read.
    /// This deliberately returns nothing: serving is decided only when a client
    /// asks for a bounded read window.
    pub fn record_frame(
        &mut self,
        session_id: &str,
        pane_id: &str,
        source: &str,
        format: &str,
        output: &str,
    ) {
        if output.is_empty() || !self.keeps(session_id, pane_id) {
            return;
        }
        let key = read_key(session_id, pane_id, source, format);
        let owns_screen = self.owns_screen(session_id, pane_id);
        self.record(&key, output, owns_screen);
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
    /// It only ever raises the number, and only for panes Herdr reported zero
    /// for, so a pane with real scrollback is left exactly as it arrived.
    pub fn amend(&self, session_id: &str, value: &mut Value) {
        let mut updates: Vec<(String, u64)> = Vec::new();
        visit_panes(value, &mut |pane_id, scroll| {
            let maximum = scroll
                .get("max_offset_from_bottom")
                .and_then(Value::as_f64)
                .unwrap_or(-1.0);
            if maximum != 0.0 {
                return;
            }
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
            }
        }
    }
}

/// Every object in a Herdr body that names a pane and reports its scroll.
///
/// `pane.list`, `pane.get` and `session.snapshot` nest panes differently and the
/// gateway models none of them; walking for the pair of fields is what lets one
/// function serve all three and survive a shape it has not seen.
fn visit_panes(value: &Value, visit: &mut impl FnMut(&str, &serde_json::Map<String, Value>)) {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(pane_id)), Some(Value::Object(scroll))) =
                (map.get("pane_id"), map.get("scroll"))
            {
                visit(pane_id, scroll);
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

    /// Replay a captured pane through the store and measure what it kept.
    ///
    /// The evidence half of card #721's question: with the anchor, the furniture
    /// rule and `already_held` in place, does the ring still duplicate a real
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
    fn an_unknown_pane_keeps_accumulating_until_alternate_on_is_observed() {
        // `owns_screen` defaults to `false` for a pane this store has not
        // been told about (the same conservatism `keeps` already applies to
        // `max_offset_from_bottom`), so a caller too old to report
        // `alternate_on`, or a pane not yet listed once, must not change
        // behaviour for a pane that already worked.
        let store = ScrollbackStore::default();
        assert!(!store.owns_screen("s", "p"));
    }

    #[test]
    fn observing_alternate_on_is_what_flips_owns_screen() {
        let mut store = ScrollbackStore::default();
        store.observe(
            "s",
            &json!({
                "pane_id": "p",
                "scroll": { "max_offset_from_bottom": 0, "alternate_on": true },
            }),
        );
        assert!(store.owns_screen("s", "p"));
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
        store.amend("s", &mut value);
        // 236 rows held, 65 of them on screen: 171 the reader can reach for.
        assert_eq!(
            value
                .pointer("/result/panes/0/scroll/max_offset_from_bottom")
                .unwrap(),
            &json!(236 - 65)
        );
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
                    { "pane_id": "p", "scroll": { "max_offset_from_bottom": 908, "viewport_rows": 65 } }
                ]
            }
        });
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
        let original = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } });
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
        for scroll in [1_usize, 5, 19, 20, 30, 40, 50] {
            let mut store = ScrollbackStore::default();
            let mut reads: Vec<Vec<String>> = Vec::new();
            for poll in 0..12 {
                let screen = agent_screen(poll * scroll, poll);
                reads.push(split_lines(&screen));
                store.record("k", &screen, false);
            }
            let held = store.window("k", MAX_PANE_LINES).unwrap();
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
        let mut store = ScrollbackStore::default();
        store.record("k", &agent_screen(0, 0), false);
        store.record("k", &agent_screen(50, 1), false);
        let rows = split_lines(&store.window("k", MAX_PANE_LINES).unwrap());
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
        let mut store = ScrollbackStore::default();
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
            store.record("k", &screen, false);
        }
        let held = store.window("k", MAX_PANE_LINES).unwrap();
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
}
