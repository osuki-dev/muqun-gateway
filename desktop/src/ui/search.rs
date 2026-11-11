//! Searching inside sessions: the sidebar's "In messages" group under the
//! title matches (`GET /api/search`, `docs/session-search.md` in the
//! gateway), and opening a hit at its row.
//!
//! The title filter stays instant (`sessions.rs`); message hits follow a
//! moment later, a newer query cancelling the one in flight. ↑ / ↓ in the
//! field walk both groups, Enter opens. A hit opens its session, scrolls to
//! the row (`item_id`, else the message), paging older history in when it
//! is not loaded yet, and lights it up briefly. A hub without the index
//! (the in-process one, an older gateway) answers `Unsupported` and the
//! group stays hidden.

use std::time::{Duration, Instant};

use rune::{col, el, rich, row, text, El, Span, Tx};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Spinner};

use super::copy::{tr, tr_ref};
use super::{ago, now_ms, title_of, Mode, UiState};
use crate::hub::{SearchHit, SearchIndexing, SearchPage, SearchQuery};
use crate::model::activity::Block;
use crate::model::commands::Command;
use crate::model::inbox::SearchOutcome;

/// Hits per page.
const PAGE: usize = 20;
/// The gateway takes at most this many characters.
const MAX_QUERY: usize = 200;
/// While the index is still being built, ask again this often.
const INDEXING_REFRESH: Duration = Duration::from_secs(4);
/// How long a row opened from a hit stays lit.
pub const FLASH: Duration = Duration::from_millis(1800);
/// Older pages read looking for a hit's row before landing at the nearest.
const MAX_PAGES: u8 = 12;
/// A hit whose session never opens is given up after this.
const JUMP_TIMEOUT: Duration = Duration::from_secs(20);

/// The row the keyboard is on while searching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    /// A session whose title matches.
    Session(String),
    /// A message hit, by its index in [`ContentSearch::hits`].
    Hit(usize),
}

/// The "In messages" group's state.
#[derive(Debug, Default)]
pub struct ContentSearch {
    /// Ctrl+Shift+F: the sidebar shows message hits only.
    pub only: bool,
    /// The hub answered with results at least once: it has an index.
    pub supported: bool,
    /// The query `hits` answer (trimmed).
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub next_cursor: Option<String>,
    /// Set while the index is still being built (results are partial).
    pub indexing: Option<SearchIndexing>,
    /// The search awaited, and whether it is a next page.
    pub pending: Option<(u64, bool)>,
    pub failed: bool,
    pub cursor: Option<Cursor>,
    pub refresh_at: Option<Instant>,
    generation: u64,
}

/// Opening a hit: the row to reveal once its session's timeline is here.
#[derive(Debug, Clone)]
pub struct PendingJump {
    pub asid: String,
    pub item_id: String,
    pub message_id: String,
    pub time_ms: u64,
    pages: u8,
    /// Rows loaded when the last older page was asked for: a page that
    /// brings none (a failed read) ends the paging.
    loaded: usize,
    since: Instant,
    /// The timeline was on screen for a frame already.
    seen: bool,
}

/// What the gateway would be asked, or `None` when the text is not worth a
/// search (empty, or a single Latin letter that matches nearly everything).
pub fn searchable(text: &str) -> Option<String> {
    let q = text.trim();
    let mut chars = q.chars();
    match (chars.next(), chars.next()) {
        (None, _) => None,
        (Some(c), None) if c.is_ascii() => None,
        _ => Some(q.chars().take(MAX_QUERY).collect()),
    }
}

/// A UTF-16 offset into `s` as a byte offset, rounded down to a character
/// boundary (an offset inside a surrogate pair lands before the pair).
pub fn utf16_to_byte(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (at, c) in s.char_indices() {
        if units + c.len_utf16() > utf16 {
            return at;
        }
        units += c.len_utf16();
    }
    s.len()
}

/// The gateway's `[start, end)` UTF-16 highlights as byte ranges of
/// `snippet`, empty and out-of-range ones dropped.
pub fn highlight_ranges(snippet: &str, highlights: &[[usize; 2]]) -> Vec<(usize, usize)> {
    highlights
        .iter()
        .map(|[a, b]| (utf16_to_byte(snippet, *a), utf16_to_byte(snippet, *b)))
        .filter(|(a, b)| a < b)
        .collect()
}

impl ContentSearch {
    /// Whether the group shows: there is something searched and an index.
    pub fn shows(&self, text: &str) -> bool {
        self.supported && searchable(text).is_some()
    }
}

impl UiState {
    /// The search field changed: the title filter follows by itself; ask
    /// for message hits.
    pub fn search_changed(&mut self) {
        self.content.cursor = None;
        self.content.refresh_at = None;
        match searchable(&self.search.text) {
            Some(q) => self.request_search(q, None),
            None => {
                self.content.pending = None;
                self.content.hits.clear();
                self.content.next_cursor = None;
                self.content.indexing = None;
                self.content.query.clear();
                self.content.failed = false;
            }
        }
    }

    fn request_search(&mut self, q: String, cursor: Option<String>) {
        self.content.generation += 1;
        let generation = self.content.generation;
        self.content.pending = Some((generation, cursor.is_some()));
        self.send(Command::Search {
            generation,
            query: SearchQuery {
                q,
                limit: PAGE,
                cursor,
            },
        });
    }

    /// Whether a search is showing in the sidebar (a query is typed).
    pub fn searching(&self) -> bool {
        !self.search.text.is_empty()
    }

    /// × or Esc: the query cleared, the field blurred and the sidebar back
    /// to its session list.
    pub fn close_search(&mut self) {
        self.search = rune::TextEdit::new("");
        self.content.only = false;
        self.search_changed();
    }

    /// Ctrl+F (`only == false`) and Ctrl+Shift+F: the field, focused.
    pub fn focus_search(&mut self, only: bool) {
        self.sidebar.set_open(true);
        self.search.focused = true;
        // Without an index there is nothing to narrow to.
        self.content.only = only && self.content.supported;
        self.content.cursor = None;
    }

    pub fn on_search(&mut self, generation: u64, outcome: SearchOutcome) {
        let Some((awaited, more)) = self.content.pending else {
            return;
        };
        if awaited != generation {
            return;
        }
        self.content.pending = None;
        match outcome {
            SearchOutcome::Page(SearchPage {
                items,
                next_cursor,
                indexing,
            }) => {
                self.content.supported = true;
                self.content.failed = false;
                if more {
                    self.content.hits.extend(items);
                } else {
                    self.content.hits = items;
                    self.content.query = searchable(&self.search.text).unwrap_or_default();
                    if matches!(self.content.cursor, Some(Cursor::Hit(_))) {
                        self.content.cursor = None;
                    }
                }
                self.content.next_cursor = next_cursor;
                let building = indexing.done < indexing.total;
                self.content.indexing = building.then_some(indexing);
                self.content.refresh_at =
                    (building && !more).then(|| Instant::now() + INDEXING_REFRESH);
            }
            SearchOutcome::Unsupported => {
                self.content.supported = false;
                self.content.only = false;
                self.content.hits.clear();
                self.content.indexing = None;
            }
            SearchOutcome::Failed => self.content.failed = true,
        }
    }

    /// The next page of hits.
    fn search_more(&mut self) {
        if self.content.pending.is_some() {
            return;
        }
        let Some(cursor) = self.content.next_cursor.clone() else {
            return;
        };
        let q = self.content.query.clone();
        self.request_search(q, Some(cursor));
    }

    /// What ↑ / ↓ walk, top to bottom: title matches, then hits.
    fn search_rows(&self) -> Vec<Cursor> {
        let mut rows = Vec::new();
        if self.search.text.trim().is_empty() {
            return rows;
        }
        if !self.content.only {
            rows.extend(
                super::sessions::listed(self)
                    .into_iter()
                    .map(Cursor::Session),
            );
        }
        if self.content.shows(&self.search.text) {
            rows.extend((0..self.content.hits.len()).map(Cursor::Hit));
        }
        rows
    }

    /// The search field's keys: ↑ / ↓ move through both groups, Enter opens,
    /// Esc closes the search (with nothing typed it only blurs the field).
    pub fn search_key(&mut self, ev: &rune::KeyEvent, tx: &mut Tx) -> bool {
        if (ev.mods.alt || ev.mods.primary()) && ev.key != rune::Key::Enter {
            return false;
        }
        if ev.key == rune::Key::Escape {
            if !self.searching() {
                return false;
            }
            self.close_search();
            return true;
        }
        let rows = self.search_rows();
        let at = self
            .content
            .cursor
            .as_ref()
            .and_then(|c| rows.iter().position(|r| r == c));
        match ev.key {
            rune::Key::Down | rune::Key::Up if !rows.is_empty() => {
                let next = match (at, ev.key == rune::Key::Down) {
                    (None, true) => 0,
                    (None, false) => rows.len() - 1,
                    (Some(i), true) => (i + 1).min(rows.len() - 1),
                    (Some(i), false) => i.saturating_sub(1),
                };
                let row = rows[next].clone();
                tx.scroll_into_view_in("sidebar-scroll", row_key(&row));
                self.content.cursor = Some(row);
                true
            }
            rune::Key::Enter => {
                let Some(row) = at
                    .or((!rows.is_empty()).then_some(0))
                    .map(|i| rows[i].clone())
                else {
                    return false;
                };
                self.content.cursor = Some(row.clone());
                let in_tile = ev.mods.ctrl || ev.mods.cmd || ev.mods.alt || ev.mods.shift;
                match row {
                    Cursor::Session(asid) => {
                        if in_tile {
                            self.open_in_tile(&asid);
                        } else {
                            self.open_session(&asid);
                        }
                    }
                    Cursor::Hit(i) => {
                        if in_tile {
                            self.open_hit_in_tile(i);
                        } else {
                            self.open_hit(i);
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Open hit `i`'s session and reveal its row once it is loaded.
    pub fn open_hit(&mut self, i: usize) {
        self.open_hit_inner(i, false);
    }

    /// Open hit `i`'s session in a split tile and reveal its row once it is loaded.
    pub fn open_hit_in_tile(&mut self, i: usize) {
        self.open_hit_inner(i, true);
    }

    fn open_hit_inner(&mut self, i: usize, in_tile: bool) {
        let Some(hit) = self.content.hits.get(i).cloned() else {
            return;
        };
        self.content.cursor = Some(Cursor::Hit(i));
        self.jump = Some(PendingJump {
            asid: hit.asid.clone(),
            item_id: hit.item_id,
            message_id: hit.message_id,
            time_ms: hit.time_ms,
            pages: 0,
            loaded: 0,
            since: Instant::now(),
            seen: false,
        });
        if in_tile {
            self.open_in_tile(&hit.asid);
        } else {
            self.open_session(&hit.asid);
        }
    }

    /// Before each frame: reveal a hit's row, put out a lit row, and ask
    /// again while the index is still being built.
    pub fn poll_search(&mut self, tx: &mut Tx) -> bool {
        let mut changed = self.poll_jump(tx);
        for tl in self.timelines.values_mut() {
            if let Some((_, at)) = &tl.flash {
                let left = FLASH.saturating_sub(at.elapsed());
                if left.is_zero() {
                    tl.flash = None;
                    tl.rev += 1;
                    changed = true;
                } else {
                    tx.wake_after(left);
                }
            }
        }
        if let Some(at) = self.content.refresh_at {
            let left = at.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                tx.wake_after(left);
            } else if self.content.pending.is_none() {
                self.content.refresh_at = None;
                if let Some(q) = searchable(&self.search.text) {
                    self.request_search(q, None);
                }
            }
        }
        changed
    }

    fn poll_jump(&mut self, tx: &mut Tx) -> bool {
        let Some(jump) = self.jump.as_mut() else {
            return false;
        };
        if jump.since.elapsed() > JUMP_TIMEOUT || self.selected.as_deref() != Some(&jump.asid) {
            self.jump = None;
            return false;
        }
        let Some(tl) = self.timelines.get_mut(&jump.asid) else {
            return false;
        };
        if tl.items.is_empty() && !tl.earlier_exhausted {
            return false;
        }
        // Let the list lay out (and settle at its bottom) once first.
        if !jump.seen {
            jump.seen = true;
            tx.wake_after(Duration::from_millis(30));
            return false;
        }
        let key = match locate(tl, &jump.item_id, &jump.message_id) {
            Some(key) => key,
            None if tl.loading_earlier => return false,
            None if !tl.earlier_exhausted
                && jump.pages < MAX_PAGES
                && (jump.pages == 0 || tl.items.len() > jump.loaded) =>
            {
                jump.pages += 1;
                jump.loaded = tl.items.len();
                tl.loading_earlier = true;
                let asid = jump.asid.clone();
                self.send(Command::LoadEarlier(muqun_gateway::agents::AgentSessionId(
                    asid,
                )));
                return false;
            }
            // Not found: the oldest row loaded when more is upstream, else
            // the row nearest in time.
            None if !tl.earlier_exhausted => match tl.blocks.first().and_then(|b| block_key(tl, b))
            {
                Some(key) => key,
                None => {
                    self.jump = None;
                    return false;
                }
            },
            None => match nearest(tl, jump.time_ms) {
                Some(key) => key,
                None => {
                    self.jump = None;
                    return false;
                }
            },
        };
        let asid = jump.asid.clone();
        self.jump = None;
        tx.scroll_center_in(format!("feed-{asid}"), key.clone());
        if let Some(tl) = self.timelines.get_mut(&asid) {
            tl.selected = Some(key.clone());
            tl.flash = Some((key, Instant::now()));
            tl.rev += 1;
        }
        tx.wake_after(FLASH);
        true
    }
}

/// The list row (its virtual key) holding item `item_id`, else the first
/// holding a part of message `message_id`.
fn locate(
    tl: &super::timeline::SessionTimeline,
    item_id: &str,
    message_id: &str,
) -> Option<String> {
    let find = |hit: &dyn Fn(&muqun_gateway::agents::TimelineItem) -> bool| {
        tl.blocks.iter().find_map(|b| match *b {
            Block::Item(i) => tl.items.get(i).filter(|it| hit(it)).map(|it| it.id.clone()),
            Block::Group { start, end } => tl
                .items
                .get(start..end)
                .filter(|run| run.iter().any(hit))
                .map(|run| super::timeline::group_key(&run[0])),
        })
    };
    (!item_id.is_empty())
        .then(|| find(&|it| it.id == item_id))
        .flatten()
        .or_else(|| {
            (!message_id.is_empty())
                .then(|| find(&|it| it.message_id == message_id))
                .flatten()
        })
}

fn block_key(tl: &super::timeline::SessionTimeline, b: &Block) -> Option<String> {
    match *b {
        Block::Item(i) => tl.items.get(i).map(|it| it.id.clone()),
        Block::Group { start, .. } => tl.items.get(start).map(super::timeline::group_key),
    }
}

/// The row whose time is closest to `time_ms`.
fn nearest(tl: &super::timeline::SessionTimeline, time_ms: u64) -> Option<String> {
    let first = |b: &Block| match *b {
        Block::Item(i) | Block::Group { start: i, .. } => tl.items.get(i),
    };
    let b = tl
        .blocks
        .iter()
        .filter(|b| first(b).is_some())
        .min_by_key(|b| first(b).map_or(u64::MAX, |it| it.updated_ms.abs_diff(time_ms)))?;
    block_key(tl, b)
}

fn row_key(row: &Cursor) -> String {
    match row {
        Cursor::Session(asid) => format!("session-{asid}"),
        Cursor::Hit(i) => format!("hit-{i}"),
    }
}

/// A faint line in the sidebar: an empty state or a status.
pub fn note(k: &Kit, key: &str, words: String) -> El {
    text(words)
        .key(key.to_string())
        .font(k.ty.small)
        .color(k.c.text_faint)
        .wrap()
        .w_full()
        .px(12.0)
        .py(6.0)
}

/// The "In messages" group, when there is one to show.
pub fn group(s: &UiState, k: &Kit) -> Option<El> {
    let c = &s.content;
    if !c.shows(&s.search.text) {
        return None;
    }
    let language = s.cfg.general.language;
    let mut out = col().key("in-messages").role("group").w_full().gap(2.0);
    let mut header = row()
        .key("in-messages-header")
        .w_full()
        .h(28.0)
        .pt(6.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .role("heading")
        .child(
            text(tr(language, "In messages"))
                .font(k.ty.text_xsmall_medium)
                .color(k.c.text_muted),
        );
    if !c.hits.is_empty() {
        header = header.child(
            text(c.hits.len().to_string() + if c.next_cursor.is_some() { "+" } else { "" })
                .font(k.ty.text_xsmall_medium)
                .color(k.c.text_faint),
        );
    }
    header = header.child(el().grow(1.0));
    if c.pending.is_some() {
        header = header.child(Spinner::new().size(11.0).build(k).key("search-spin"));
    }
    out = out.child(header);
    if let Some(ix) = c.indexing {
        out = out.child(
            text(tr(
                language,
                &format!(
                    "Indexing… {}/{} sessions, results may be partial",
                    ix.done, ix.total
                ),
            ))
            .key("search-indexing")
            .font(k.ty.caption())
            .color(k.c.text_faint)
            .wrap()
            .w_full()
            .px(8.0)
            .pb(2.0),
        );
    }
    if c.hits.is_empty() {
        let words = if c.failed {
            tr(language, "Couldn't search messages")
        } else if c.pending.is_some() {
            tr(language, "Searching…")
        } else {
            tr(language, "No messages match")
        };
        return Some(out.child(note(k, "search-none", words)));
    }
    let now = now_ms();
    let mut i = 0;
    while i < c.hits.len() {
        let asid = &c.hits[i].asid;
        let end = c.hits[i..]
            .iter()
            .position(|h| &h.asid != asid)
            .map_or(c.hits.len(), |n| i + n);
        out = out.child(session_block(s, k, i, end, now));
        i = end;
    }
    if c.next_cursor.is_some() {
        out = out.child(
            Button::new(tr_ref(language, "More results"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.search_more())
                .build(k)
                .key("search-more"),
        );
    }
    Some(out)
}

/// One session's hits (`start..end` of the list, at most three): its mark
/// and title, then each hit's snippet and where it came from.
fn session_block(s: &UiState, k: &Kit, start: usize, end: usize, now: u64) -> El {
    let language = s.cfg.general.language;
    let first = &s.content.hits[start];
    let title = s
        .session_by_id(&first.asid)
        .map(title_of)
        .filter(|t| !t.is_empty())
        .or_else(|| (!first.title.trim().is_empty()).then(|| first.title.clone()))
        .unwrap_or_else(|| s.agent_name(&first.agent_id));
    let mut block = col()
        .key(format!("hits-{}", first.asid))
        .w_full()
        .gap(1.0)
        .pt(4.0)
        .child(
            row()
                .key("hit-session")
                .w_full()
                .h(22.0)
                .px(8.0)
                .gap(6.0)
                .center_y()
                .child(
                    row()
                        .size(16.0, 16.0)
                        .center()
                        .child(super::agents::mark(&first.agent_id, 14.0, s.dark))
                        .a11y_hidden(),
                )
                .child(
                    text(title)
                        .font(rune::TextStyle {
                            weight: 500.0,
                            ..k.ty.ui_compact
                        })
                        .color(k.c.text_base)
                        .truncate()
                        .grow(1.0)
                        .shrink(),
                ),
        );
    for i in start..end {
        let hit = &s.content.hits[i];
        let who = match (hit.kind.as_str(), hit.role.as_str()) {
            ("tool", _) => tr(language, "Tool call"),
            (_, "user") => tr(language, "You"),
            _ => s.agent_name(&hit.agent_id),
        };
        let when = ago(hit.time_ms, now, language);
        let mut meta = if when.is_empty() {
            who
        } else {
            format!("{who} · {when}")
        };
        let more = hit.session_hits.saturating_sub(end - start);
        if i + 1 == end && more > 0 {
            meta = format!(
                "{meta} · {}",
                tr(language, &format!("+{more} more in this session"))
            );
        }
        let spans = highlight_ranges(&hit.snippet, &hit.highlights)
            .into_iter()
            .map(|(a, b)| {
                Span::new(a, b)
                    .color(k.c.text_strong)
                    .weight(600.0)
                    .bg(k.c.bg_accent.alpha(if s.dark { 0.30 } else { 0.20 }), 1.0)
            })
            .collect();
        let snippet = hit.snippet.replace(['\n', '\r', '\t'], " ");
        let on = s.content.cursor == Some(Cursor::Hit(i));
        let mut line = col()
            .key(format!("hit-{i}"))
            .w_full()
            .gap(2.0)
            .pl(30.0)
            .pr(8.0)
            .py(5.0)
            .radius(k.radius.md)
            .role("option")
            .label(hit.snippet.clone())
            .description(meta.clone())
            .child(
                rich(snippet, spans)
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap()
                    .max_lines(2)
                    .w_full()
                    .a11y_hidden(),
            )
            .child(
                text(meta)
                    .font(k.ty.caption())
                    .color(k.c.text_faint)
                    .truncate()
                    .w_full()
                    .a11y_hidden(),
            );
        if on {
            line = line.fill(k.c.pressed);
        }
        block = block.child(
            k.interactive(line)
                .cursor(rune::Cursor::Pointer)
                .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
                    let mods = tx.mods();
                    if mods.ctrl || mods.cmd || mods.alt || mods.shift {
                        s.open_hit_in_tile(i);
                    } else {
                        s.open_hit(i);
                    }
                }),
        );
    }
    block
}

/// The light a row opened from a search hit wears for a moment: the theme's
/// accent glowing from behind it, no outline. It fades in and out.
pub fn flash(s: &UiState, k: &Kit) -> El {
    let peak = if s.dark { 0.17 } else { 0.13 };
    let accent = k.c.bg_accent;
    let light = el()
        .key("hit-flash")
        .pin(Some(-14.0), Some(-2.0), Some(-14.0), Some(-2.0))
        .radius(k.radius.lg)
        // Brightest at the leading edge, where reading starts, and
        // fading across the row.
        .linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [
                (0.0, accent.alpha(peak)),
                (0.45, accent.alpha(peak * 0.55)),
                (1.0, accent.alpha(peak * 0.12)),
            ],
        )
        .a11y_hidden();
    if s.cfg.appearance.reduced_motion {
        light.transition(rune::Transition::opacity())
    } else {
        light
            .transition(rune::Transition::opacity().combined(rune::Transition::scale(0.98)))
            .motion(rune::Spring::new(0.45, 0.0))
    }
}

/// Whether the sidebar's title tree shows: not while only message hits do.
pub fn titles_shown(s: &UiState) -> bool {
    !(s.content.only && s.content.shows(&s.search.text))
}

/// Whether the open session's row in the sidebar wears the selection: the
/// keyboard's row wins while it is on one.
pub fn row_selected(s: &UiState, asid: &str) -> bool {
    match &s.content.cursor {
        Some(Cursor::Session(at)) if !s.search.text.trim().is_empty() => at == asid,
        Some(Cursor::Hit(_)) if !s.search.text.trim().is_empty() => false,
        _ => s.mode == Mode::Session && s.selected.as_deref() == Some(asid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_highlights_land_on_rust_char_boundaries() {
        // CJK is one UTF-16 unit and three bytes; the emoji two units, four bytes.
        let s = "\u{6570}\u{636e}\u{5e93} \u{1f600} billing\u{8868}";
        // "database"
        assert_eq!(highlight_ranges(s, &[[0, 3]]), vec![(0, 9)]);
        // "billing" after "database 😀 " = 7 units.
        let r = highlight_ranges(s, &[[7, 14]]);
        assert_eq!(&s[r[0].0..r[0].1], "billing");
        // "table", the last character.
        let r = highlight_ranges(s, &[[14, 99]]);
        assert_eq!(&s[r[0].0..r[0].1], "\u{8868}");
        // Inside the surrogate pair: rounds down to the pair's start.
        assert_eq!(utf16_to_byte(s, 5), "\u{6570}\u{636e}\u{5e93} ".len());
        // Empty and inverted ranges are dropped.
        assert!(highlight_ranges(s, &[[3, 3], [5, 2]]).is_empty());
    }

    #[test]
    fn a_single_latin_letter_is_not_searched_but_one_cjk_character_is() {
        assert_eq!(searchable("  "), None);
        assert_eq!(searchable(" a "), None);
        assert_eq!(searchable("\u{5e93}").as_deref(), Some("\u{5e93}"));
        assert_eq!(searchable(" ab ").as_deref(), Some("ab"));
        assert_eq!(
            searchable(&"x".repeat(300)).map(|q| q.len()),
            Some(MAX_QUERY)
        );
    }
}
