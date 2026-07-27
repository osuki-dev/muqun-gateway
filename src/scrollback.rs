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
//! Agreement is scored rather than demanded. Requiring every row to match would
//! make one turning spinner look like a full redraw, and the no-overlap case
//! appends a whole screen; at 150ms that is how a buffer explodes. An overlap is
//! believed when [`MATCH_THRESHOLD`] of its rows agree -- exactly, below
//! [`MIN_MATCH_ROWS`], where a ratio means nothing. The spinner then places at
//! full overlap: the screen changed, nothing scrolled, the buffer does not grow.
//!
//! Where nothing agrees the screen was redrawn into something else. Then, and
//! only then, the previous screen is dropped and the new one takes its place --
//! so growth stays tied to rows that demonstrably scrolled away, and the history
//! above the screen is never what pays for a redraw.
//!
//! On exact matches this is the same placement `mergeTerminalWindow` makes,
//! which is the point: the two ends of the same pane must not disagree about
//! where a row belongs.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Rows kept per pane and source, matching the ceiling on a single read.
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

/// Where this read sits against the end of what is held: how many held rows the
/// read re-sends.
///
/// `None` where nothing agrees -- the screen was redrawn into something else and
/// the caller has to decide what to do with the screen it replaced.
fn placement(held: &[u64], incoming: &[u64]) -> Option<usize> {
    let widest = held.len().min(incoming.len()).min(MAX_OVERLAP);
    (1..=widest)
        .rev()
        .find(|overlap| rows_agree(held, incoming, *overlap))
}

#[derive(Debug, Default)]
struct PaneBuffer {
    lines: VecDeque<String>,
    hashes: VecDeque<u64>,
    bytes: usize,
    /// Rows of the most recent read: where history ends and the screen begins.
    screen: usize,
    /// When this buffer was last fed, on the store's own clock.
    touched: u64,
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
        self.screen = self.screen.min(self.lines.len());
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
    total_bytes: usize,
    clock: u64,
}

/// `session/pane`, the key the zero-backlog verdict is held under.
pub fn pane_key(session_id: &str, pane_id: &str) -> String {
    format!("{session_id}/{pane_id}")
}

/// `session/pane/source/format`. Rows read as ANSI and rows read as plain text
/// are different rows and cannot be spliced against each other, so each read
/// shape keeps its own buffer.
pub fn read_key(session_id: &str, pane_id: &str, source: &str, format: &str) -> String {
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
            if let Some(maximum) = scroll
                .get("max_offset_from_bottom")
                .and_then(Value::as_f64)
            {
                self.kept
                    .insert(pane_key(session_id, pane_id), maximum <= 0.0);
            }
        });
    }

    /// Whether this pane is one the gateway keeps rows for.
    ///
    /// A pane nobody has reported on yet answers `false`: not knowing is a
    /// reason to stay out of the way, not a reason to guess.
    pub fn keeps(&self, session_id: &str, pane_id: &str) -> bool {
        self.kept
            .get(&pane_key(session_id, pane_id))
            .copied()
            .unwrap_or(false)
    }

    /// Fold a read into what is already held.
    pub fn record(&mut self, key: &str, text: &str) {
        let incoming = split_lines(text);
        if incoming.is_empty() {
            return;
        }
        self.clock += 1;
        let clock = self.clock;
        let buffer = self.buffers.entry(key.to_owned()).or_default();
        let before = buffer.bytes;
        buffer.touched = clock;

        let screen = incoming.len();
        if !buffer.lines.is_empty() {
            let incoming_hashes: Vec<u64> = incoming.iter().map(|line| line_hash(line)).collect();
            let held = buffer.tail(MAX_OVERLAP);
            // Nothing agreed, so the screen became something else. Dropping the
            // screen it replaced -- and only that, never the history above it --
            // is what keeps growth tied to rows that actually scrolled away.
            let discard = placement(&held, &incoming_hashes).unwrap_or(buffer.screen);
            buffer.drop_back(discard.min(buffer.lines.len()));
        }
        for line in incoming {
            buffer.push(line);
        }

        buffer.screen = screen;
        buffer.trim();
        self.total_bytes = self.total_bytes + buffer.bytes - before;
        self.evict();
    }

    /// The last `rows` rows held for this read, or `None` where the buffer has
    /// nothing more than the caller already has.
    pub fn window(&self, key: &str, rows: usize) -> Option<String> {
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

    /// How many rows are held for this pane, across every read shape.
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
                scroll.insert(
                    "max_offset_from_bottom".into(),
                    Value::from(*offset),
                );
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
    for pointer in ["/result/read/text", "/result/text"] {
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
    use serde_json::json;

    fn screen(rows: &[&str]) -> String {
        rows.join("\n")
    }

    /// A sixty-five row screen scrolled one row at a time until the buffer holds
    /// `rows`, the shape every real pane in the fleet has.
    fn scroll_a_screen(store: &mut ScrollbackStore, key: &str, rows: usize) {
        for top in 0..rows.saturating_sub(64) {
            let screen: Vec<String> = (top..top + 65).map(|row| format!("row {row}")).collect();
            store.record(key, &screen.join("\n"));
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
        store.record("k", &screen(&["1", "2", "3"]));
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3");
    }

    #[test]
    fn an_unchanged_screen_adds_nothing() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]));
        store.record("k", &screen(&["1", "2", "3"]));
        store.record("k", &screen(&["1", "2", "3"]));
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3");
    }

    #[test]
    fn a_scrolled_screen_keeps_what_rolled_off_the_top() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]));
        store.record("k", &screen(&["3", "4", "5", "6"]));
        assert_eq!(store.window("k", 10).unwrap(), "1\n2\n3\n4\n5\n6");
    }

    #[test]
    fn the_seam_is_neither_duplicated_nor_dropped() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["a", "b", "c", "d", "e"]));
        store.record("k", &screen(&["b", "c", "d", "e", "f"]));
        store.record("k", &screen(&["c", "d", "e", "f", "g"]));
        assert_eq!(store.window("k", 20).unwrap(), "a\nb\nc\nd\ne\nf\ng");
    }

    #[test]
    fn one_turning_spinner_does_not_read_as_a_redraw() {
        // The case that decides whether this is a buffer or a leak: at 150ms a
        // screen whose clock ticks must not append itself every time.
        let mut store = ScrollbackStore::default();
        let mut rows: Vec<String> = (0..20).map(|row| format!("row {row}")).collect();
        store.record("k", &rows.join("\n"));
        for tick in 0..50 {
            rows[7] = format!("working {tick}");
            store.record("k", &rows.join("\n"));
        }
        let held = store.window("k", 500).unwrap();
        assert_eq!(split_lines(&held).len(), 20);
        assert!(held.contains("working 49"));
    }

    #[test]
    fn a_screen_redrawn_into_something_else_replaces_rather_than_appends() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["old 1", "old 2", "old 3", "old 4"]));
        store.record("k", &screen(&["new 1", "new 2", "new 3", "new 4"]));
        assert_eq!(store.window("k", 20).unwrap(), "new 1\nnew 2\nnew 3\nnew 4");
    }

    #[test]
    fn history_survives_a_redraw_of_the_screen_above_it() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]));
        // Scrolls by two, so 1 and 2 become history.
        store.record("k", &screen(&["3", "4", "5", "6"]));
        // Then the screen is replaced outright. What already scrolled off is
        // not the screen's to take with it.
        store.record("k", &screen(&["x", "y", "z", "w"]));
        assert_eq!(store.window("k", 20).unwrap(), "1\n2\nx\ny\nz\nw");
    }

    #[test]
    fn a_window_asks_for_no_more_than_it_holds() {
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3"]));
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
            store.record("k", &rows.join("\n"));
        }
        let held = store.window("k", MAX_PANE_LINES * 2).unwrap();
        let rows = split_lines(&held);
        assert_eq!(rows.len(), MAX_PANE_LINES);
        assert_eq!(rows.last().unwrap(), &format!("row {}", total + 18));
        assert_eq!(rows.first().unwrap(), &format!("row {}", total + 19 - MAX_PANE_LINES));
    }

    #[test]
    fn the_buffer_ceiling_forgets_the_pane_nobody_looked_at() {
        let mut store = ScrollbackStore::default();
        for index in 0..(MAX_BUFFERS + 5) {
            store.record(&format!("pane-{index}"), "hello");
        }
        assert!(store.buffers.len() <= MAX_BUFFERS);
        assert!(store.window("pane-0", 10).is_none());
        assert!(store.window(&format!("pane-{}", MAX_BUFFERS + 4), 10).is_some());
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
        assert!(store.keeps("default", "wM:p1"));
        assert!(!store.keeps("default", "wM:pT"));
        // A pane nobody has reported on is not guessed at.
        assert!(!store.keeps("default", "wN:p9"));
        // Nor is a pane on another session with the same id.
        assert!(!store.keeps("other", "wM:p1"));
    }

    #[test]
    fn a_pane_that_grows_scrollback_stops_being_kept() {
        let mut store = ScrollbackStore::default();
        let zero = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } });
        let grown = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 40, "viewport_rows": 65 } });
        store.observe("s", &zero);
        assert!(store.keeps("s", "p"));
        store.observe("s", &grown);
        assert!(!store.keeps("s", "p"));
    }

    #[test]
    fn the_pane_entity_answers_for_what_the_buffer_holds() {
        let mut store = ScrollbackStore::default();
        scroll_a_screen(&mut store, &read_key("s", "p", "recent_unwrapped", "text"), 236);
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
            value.pointer("/result/panes/0/scroll/max_offset_from_bottom").unwrap(),
            &json!(236 - 65)
        );
    }

    #[test]
    fn a_pane_with_real_scrollback_is_left_exactly_as_it_arrived() {
        let mut store = ScrollbackStore::default();
        scroll_a_screen(&mut store, &read_key("s", "p", "recent_unwrapped", "text"), 300);
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
        store.record(&read_key("s", "p", "recent_unwrapped", "text"), "one\ntwo");
        let original = json!({ "pane_id": "p", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 65 } });
        let mut value = original.clone();
        store.amend("s", &mut value);
        assert_eq!(value, original);
    }

    #[test]
    fn ansi_and_text_reads_of_one_pane_do_not_splice_into_each_other() {
        let mut store = ScrollbackStore::default();
        store.record(&read_key("s", "p", "recent_unwrapped", "text"), "plain");
        store.record(&read_key("s", "p", "recent_unwrapped", "ansi"), "\u{1b}[31mred");
        assert_eq!(
            store.window(&read_key("s", "p", "recent_unwrapped", "text"), 10).unwrap(),
            "plain"
        );
        assert_eq!(
            store.window(&read_key("s", "p", "recent_unwrapped", "ansi"), 10).unwrap(),
            "\u{1b}[31mred"
        );
    }

    #[test]
    fn spliced_rows_go_back_where_herdr_put_them() {
        let mut nested = json!({ "id": "1", "result": { "read": { "text": "old", "revision": 7 } } });
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

    #[test]
    fn a_wider_read_of_the_same_screen_does_not_double_it() {
        // The reader's paging re-requests the whole window at a wider limit, so
        // the same rows arrive again inside a longer read.
        let mut store = ScrollbackStore::default();
        store.record("k", &screen(&["1", "2", "3", "4"]));
        store.record("k", &screen(&["3", "4", "5", "6"]));
        let held_before = store.window("k", 100).unwrap();
        store.record("k", &screen(&["1", "2", "3", "4", "5", "6"]));
        assert_eq!(store.window("k", 100).unwrap(), held_before);
    }
}
