//! The session timeline: per-session state and the virtualised list.
//!
//! Public entry points, all a function of an explicit session so a window can
//! show several timelines at once:
//!
//! - [`session_view`]`(s, k, asid)`: one session's timeline, filling the
//!   space it is given (any width; tiles down to ~480 pt). Call it once per
//!   visible session.
//! - [`view`]`(s, k)`: the selected session's, for the single-session page.
//! - [`overlay`]`(s, k)`: the image zoom, when one is open; push it onto the
//!   root layer.
//! - Split tiles (`ui::tiles`) open sessions beside the focused one; every
//!   open session's timeline is kept in `UiState::timelines`.
//!
//! Each session's [`SessionTimeline`] (items, parsed diffs, expanded cards,
//! loaded-history state) lives in `UiState::timelines`; the scroll anchor is
//! the list's own (Rune keeps the reader's row when rows are prepended).
//! Cards are drawn by `ui::parts`, one module per part kind.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};

use muqun_gateway::agents::{
    AgentPart, AgentSessionId, AgentSessionStatus, TimelineItem, TimelineRole,
};
use rune::{col, row, text, El, GrowthSpec, ScrollSpec};
use rune_kit::diff::{DiffState, DiffView, FileDiff};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit,
    ScrollBar, Spinner,
};

use super::copy::tr_ref;
use super::parts::{self, Tcx};
use super::{muted, UiState};
use crate::model::activity::{self, Block};
use crate::model::commands::Command;

pub use super::parts::approval::permission_card;
pub use super::parts::detail::DetailSheet;
pub use super::parts::question::form_card;

thread_local! {
    static EMPTY: &'static DiffState = Box::leak(Box::new(DiffState::default()));
}

/// A diff with no files, for views whose state went away between frames.
pub fn empty_diff() -> &'static DiffState {
    EMPTY.with(|d| *d)
}

pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The least time between two older pages asked for by scrolling.
const PAGE_PAUSE: std::time::Duration = std::time::Duration::from_millis(700);

/// Rows a list estimates before it has measured them.
const ROW_ESTIMATE: f32 = 64.0;
/// Readable line length on a wide window; tiles are narrower anyway. The
/// session composer shares it, so the two line up.
pub const FEED_MAX_W: f32 = 860.0;

/// An item's change, parsed the first time a card draws it: a long session
/// of whole-file edits would otherwise diff every one of them on open.
pub struct LazyDiff {
    source: u64,
    build: RefCell<Option<parts::files::BuildFiles>>,
    files: OnceCell<Vec<FileDiff>>,
}

impl LazyDiff {
    pub fn files(&self) -> &[FileDiff] {
        self.files.get_or_init(|| {
            self.build
                .borrow_mut()
                .take()
                .map(|build| build())
                .unwrap_or_default()
        })
    }
}

/// One session's timeline as the UI holds it.
#[derive(Default)]
pub struct SessionTimeline {
    pub items: Vec<TimelineItem>,
    /// Diffs drawn inline, by item id, with the source they were built from.
    pub diffs: HashMap<String, LazyDiff>,
    /// Cards the reader flipped from their default (open ↔ closed), by id.
    pub flipped: HashSet<String>,
    /// No older history to page in.
    pub earlier_exhausted: bool,
    /// A page of older history is on its way.
    pub loading_earlier: bool,
    /// When the last older page landed. While a fling holds the list at its
    /// top, each page would ask for the next at once and pull the whole
    /// history in; the next one waits until the reader moves again.
    pub paged_at: Option<std::time::Instant>,
    /// Bumped whenever rows are added, removed or reordered (the virtual
    /// list's key version).
    pub rev: u64,
    /// The list's rows: items, and runs of tool activity folded together.
    pub blocks: Vec<Block>,
    /// The row the keyboard is on (an item id, or a group's key).
    pub selected: Option<String>,
    /// The reader scrolled up off the bottom: the list stopped following it
    /// and the "Latest" pill shows.
    pub away: bool,
    /// Messages that arrived while away, for the pill's "N new".
    pub unseen: usize,
    /// The row a search hit opened, lit until a moment after `Instant`.
    pub flash: Option<(String, std::time::Instant)>,
}

/// A folded group's key: its first item's.
pub fn group_key(first: &TimelineItem) -> String {
    format!("grp-{}", first.id)
}

impl SessionTimeline {
    /// A snapshot: the whole list, as of now. Reader state stays.
    pub fn replace(&mut self, items: Vec<TimelineItem>) {
        let started = std::time::Instant::now();
        let keep: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        self.diffs.retain(|id, _| keep.contains(id.as_str()));
        for item in &items {
            self.ingest(item);
        }
        self.items = items;
        self.regroup();
        tracing::debug!(
            rows = self.items.len(),
            us = started.elapsed().as_micros() as u64,
            "timeline replaced"
        );
    }

    /// Live rows: replace by id, else append.
    pub fn upsert(&mut self, items: Vec<TimelineItem>) {
        let mut regroup = false;
        for item in items {
            self.ingest(&item);
            // New and updated rows are nearly always at the tail.
            match self.items.iter().rposition(|t| t.id == item.id) {
                Some(at) => {
                    // Streaming words never change the folding; anything
                    // else (a call's picture, a blank turning into words) may.
                    let words = |t: &TimelineItem| {
                        matches!(t.part, AgentPart::Text { .. }) && !activity::invisible(t)
                    };
                    regroup |= !(words(&self.items[at]) && words(&item));
                    self.items[at] = item;
                }
                None => {
                    if self.away && matches!(item.part, AgentPart::Text { .. }) {
                        self.unseen += 1;
                    }
                    self.items.push(item);
                    regroup = true;
                }
            }
        }
        if regroup {
            self.regroup();
        }
    }

    /// The list's viewport reached its bottom (`true`) or left it. Arriving
    /// back is when the new messages count as seen.
    pub fn at_bottom(&mut self, at: bool) {
        if at && self.away {
            self.unseen = 0;
        }
        self.away = !at;
    }

    /// Fold the rows again, after the list changed.
    fn regroup(&mut self) {
        self.blocks = activity::blocks(&self.items);
        self.rev += 1;
    }

    pub fn remove(&mut self, ids: &[String]) {
        self.items.retain(|t| !ids.contains(&t.id));
        self.regroup();
        for id in ids {
            self.diffs.remove(id);
        }
    }

    /// An older page, above what is loaded.
    pub fn prepend(&mut self, older: Vec<TimelineItem>, exhausted: bool) {
        self.loading_earlier = false;
        self.paged_at = Some(std::time::Instant::now());
        self.earlier_exhausted = exhausted;
        let have: HashSet<String> = self.items.iter().map(|i| i.id.clone()).collect();
        let mut older: Vec<TimelineItem> = older
            .into_iter()
            .filter(|i| !have.contains(&i.id))
            .collect();
        for item in &older {
            self.ingest(item);
        }
        older.append(&mut self.items);
        self.items = older;
        self.regroup();
    }

    /// Parse the item's change (a diff part, an edit's files) once, for drawing.
    fn ingest(&mut self, item: &TimelineItem) {
        match parts::files::diff_source(item) {
            None => {
                self.diffs.remove(&item.id);
            }
            Some((source, build)) => {
                if self.diffs.get(&item.id).is_none_or(|d| d.source != source) {
                    self.diffs.insert(
                        item.id.clone(),
                        LazyDiff {
                            source,
                            build: RefCell::new(Some(build)),
                            files: OnceCell::new(),
                        },
                    );
                }
            }
        }
    }

    /// Whether card `id` is open, given whether it opens by default.
    pub fn is_open(&self, id: &str, default: bool) -> bool {
        default != self.flipped.contains(id)
    }

    pub fn flip(&mut self, id: &str) {
        if !self.flipped.remove(id) {
            self.flipped.insert(id.to_string());
        }
    }
}

impl UiState {
    /// Drop the timelines no longer on screen.
    pub(super) fn retain_timelines(&mut self) {
        let selected = self.selected.clone();
        let mut keep: Vec<String> = self.tiles.iter().filter_map(|t| t.asid.clone()).collect();
        keep.extend(self.detail_asids());
        self.timelines
            .retain(|asid, _| selected.as_deref() == Some(asid.as_str()) || keep.contains(asid));
    }

    /// Open a file a timeline card names (a presented file, a glob or grep
    /// hit) in the viewer. Relative paths are the selected session's; `~/` is
    /// home.
    pub fn open_timeline_file(&mut self, path: &str) {
        let path = path.trim();
        let resolved = if let Some(rest) = path.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest))
        } else if std::path::Path::new(path).is_absolute() {
            Some(std::path::PathBuf::from(path))
        } else {
            self.current()
                .and_then(|i| i.directory.as_deref())
                .map(|d| std::path::Path::new(d).join(path))
        };
        match resolved.filter(|p| p.exists()) {
            Some(p) => self.open_file(p),
            None => {
                let title = super::copy::tr(
                    self.cfg.general.language,
                    "File not found on this computer",
                    "这台电脑上找不到该文件",
                );
                self.toast(title, Some(path.to_string()));
            }
        }
    }

    pub fn session_busy(&self, asid: &str) -> bool {
        self.session_by_id(asid).is_some_and(|s| {
            matches!(
                s.status,
                AgentSessionStatus::Busy | AgentSessionStatus::Retry
            )
        })
    }
}

/// A split tile's timeline (the tiles call this). The rows come from the
/// session's own [`SessionTimeline`], whatever the tile hands in.
#[allow(dead_code)] // kept for callers that hand in their own rows
pub fn view_items(
    s: &UiState,
    k: &Kit,
    asid: Option<&str>,
    _items: &[TimelineItem],
    _earlier_exhausted: bool,
    _busy: bool,
) -> El {
    match asid {
        Some(asid) => session_view(s, k, asid),
        None => empty(s, k, "feed"),
    }
}

/// The selected session's timeline.
pub fn view(s: &UiState, k: &Kit) -> El {
    match s.selected.clone() {
        Some(asid) => session_view(s, k, &asid),
        None => empty(s, k, "feed"),
    }
}

fn empty(s: &UiState, k: &Kit, key: &str) -> El {
    let language = s.cfg.general.language;
    col()
        .key(key.to_string())
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .child(
            text(if s.selected.is_some() {
                tr_ref(language, "No messages yet.", "还没有消息。")
            } else {
                tr_ref(
                    language,
                    "Select a session or start a new one.",
                    "选择一个会话，或开始新会话。",
                )
            })
            .key("empty-feed")
            .font(k.ty.ui)
            .color(k.c.text_muted),
        )
}

/// What row `i` of a session's list is.
enum Row<'a> {
    Top,
    Item(&'a TimelineItem),
    Group(&'a [TimelineItem]),
    Working,
}

fn row_at<'a>(s: &'a UiState, asid: &str, i: usize) -> Option<Row<'a>> {
    let tl = s.timelines.get(asid)?;
    if i == 0 {
        return Some(Row::Top);
    }
    match tl.blocks.get(i - 1) {
        Some(Block::Item(at)) => tl.items.get(*at).map(Row::Item),
        Some(Block::Group { start, end }) => tl.items.get(*start..*end).map(Row::Group),
        None => Some(Row::Working),
    }
}

fn row_count(s: &UiState, asid: &str) -> usize {
    let n = s.timelines.get(asid).map_or(0, |t| t.blocks.len());
    1 + n + usize::from(s.session_busy(asid))
}

/// One session's timeline: a virtualised list that only builds the rows
/// near the viewport, follows the bottom while the agent writes, keeps the
/// reader's row when older history lands above it, and pages that history
/// in as the reader nears the top.
pub fn session_view(s: &UiState, k: &Kit, asid: &str) -> El {
    let key = format!("feed-{asid}");
    let Some(tl) = s.timelines.get(asid) else {
        // Opened, its history not read yet.
        let language = s.cfg.general.language;
        return col().key(key).w_full().grow(1.0).min_h(0.0).center().child(
            row()
                .gap(8.0)
                .center_y()
                .child(Spinner::new().size(14.0).build(k))
                .child(
                    text(tr_ref(language, "Opening the session…", "正在打开会话…"))
                        .font(k.ty.ui)
                        .color(k.c.text_muted),
                ),
        );
    };
    if tl.items.is_empty() && tl.earlier_exhausted {
        return empty(s, k, &key);
    }
    let count = row_count(s, asid);
    let (key_asid, build_asid, edge_asid) = (asid.to_string(), asid.to_string(), asid.to_string());
    let kit = k.clone();
    let narrow = super::Fit::of(&format!("feed-{asid}")) == super::Fit::Tight;
    let rows = col()
        .key(format!("rows-{asid}"))
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            count,
            ROW_ESTIMATE,
            move |s: &UiState, i| -> Cow<'_, str> {
                match row_at(s, &key_asid, i) {
                    Some(Row::Item(item)) => Cow::Borrowed(item.id.as_str()),
                    Some(Row::Group(items)) => Cow::Owned(group_key(&items[0])),
                    Some(Row::Top) => Cow::Borrowed("tl-top"),
                    _ => Cow::Borrowed("tl-working"),
                }
            },
            move |s: &UiState, i| {
                let mut cx = Tcx::new(s, &kit, &build_asid);
                cx.narrow = narrow;
                let mut tail = false;
                let row = row_at(s, &build_asid, i);
                let lit =
                    cx.tl()
                        .and_then(|t| t.flash.as_ref())
                        .is_some_and(|(key, _)| match &row {
                            Some(Row::Item(item)) => &item.id == key,
                            Some(Row::Group(items)) => &group_key(&items[0]) == key,
                            _ => false,
                        });
                let body = match row {
                    Some(Row::Top) => top_row(&cx),
                    Some(Row::Item(item)) => {
                        // The answer being written: Rune eases its growth.
                        tail = s.session_busy(&build_asid)
                            && matches!(item.part, AgentPart::Text { .. })
                            && cx.tl().and_then(|t| t.items.last()).map(|l| &l.id)
                                == Some(&item.id);
                        item_row(&cx, item)
                    }
                    Some(Row::Group(items)) => parts::group::view(&cx, items),
                    Some(Row::Working) | None => working_row(&cx),
                };
                // Rows space themselves with padding: virtual rows take no gaps.
                let mut column = col().key("tl-column").w_full().max_w(FEED_MAX_W);
                if lit {
                    column = column.child(super::search::flash(s, &kit));
                }
                col()
                    .w_full()
                    .items_center()
                    .px(16.0)
                    .py(3.0)
                    .streaming(tail)
                    .child(column.child(body.w_full()))
            },
        )
        .virtual_rows_version((tl.rev, s.session_busy(asid)))
        .on_visible_rows(move |s: &mut UiState, range: std::ops::Range<usize>| {
            // Near the top with more history upstream: ask for the next page.
            if range.start > 2 {
                return false;
            }
            let Some(tl) = s.timelines.get_mut(&edge_asid) else {
                return false;
            };
            if tl.earlier_exhausted || tl.loading_earlier || tl.items.is_empty() {
                return false;
            }
            if tl.paged_at.is_some_and(|at| at.elapsed() < PAGE_PAUSE) {
                return false;
            }
            tl.loading_earlier = true;
            s.send(Command::LoadEarlier(AgentSessionId(edge_asid.clone())));
            true
        });
    let bottom_asid = asid.to_string();
    let feed = col()
        .key(key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(rows)
        .scroll(ScrollSpec { stick_bottom: true })
        .on_at_bottom(move |s: &mut UiState, at| {
            if let Some(tl) = s.timelines.get_mut(&bottom_asid) {
                tl.at_bottom(at);
            }
        })
        .scroll_bar(k);
    let mut stage = col()
        .key(format!("feed-stage-{asid}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(feed);
    if tl.away {
        stage = stage.child(latest_pill(s, k, asid, tl.unseen));
    }
    stage
}

/// "↓ Latest" (or "↓ 3 new") over the timeline's bottom-right corner while
/// the reader is scrolled up: a press glides back down and follows again.
fn latest_pill(s: &UiState, k: &Kit, asid: &str, unseen: usize) -> El {
    let language = s.cfg.general.language;
    let words = if unseen == 0 {
        tr_ref(language, "Latest", "最新").to_string()
    } else {
        super::copy::tr(
            language,
            &format!("{unseen} new"),
            &format!("{unseen} 条新消息"),
        )
    };
    let tip = super::keymap::tip(
        &super::keymap::entry(super::keymap::Action::JumpLatest).what(language),
        super::keymap::Action::JumpLatest,
    );
    let fresh = unseen > 0;
    let (ink, fill) = if fresh {
        (k.c.text_accent, k.c.bg_accent.alpha(0.16))
    } else {
        (k.c.text_muted, k.c.layer_02)
    };
    let target = asid.to_string();
    let mut pill = row()
        .key(format!("latest-{asid}"))
        .h(28.0)
        .px(12.0)
        .gap(6.0)
        .center_y()
        .radius(14.0)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_muted)
        .child(
            row()
                .absolute(0.0, 0.0)
                .w_full()
                .h_full()
                .radius(14.0)
                .fill(fill)
                .a11y_hidden(),
        )
        .child(rune::icon(&super::icons::MOVE_DOWN, 13.0).color(ink))
        .child(text(words.clone()).font(k.ty.small_medium).color(ink))
        .hover(k.c.hover)
        .role("button")
        .label(words)
        .help(tip)
        .on_press_tx(move |s: &mut UiState, tx: &mut rune::Tx| s.jump_to_latest(&target, tx));
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        pill = pill.box_shadow(shadow);
    }
    let pill = if s.cfg.appearance.reduced_motion {
        pill.transition(rune::Transition::opacity())
    } else {
        pill.transition(
            rune::Transition::opacity()
                .combined(rune::Transition::offset(0.0, 8.0))
                .combined(rune::Transition::scale(0.94)),
        )
        .motion(rune::Spring::new(0.3, 0.2))
    };
    // On the reading column's right edge, wherever the column sits.
    row()
        .key(format!("latest-band-{asid}"))
        .pin(Some(0.0), None, Some(0.0), Some(12.0))
        .justify_center()
        .px(16.0)
        .child(row().w_full().max_w(FEED_MAX_W).justify_end().child(pill))
}

impl UiState {
    /// Glide `asid`'s timeline to its bottom; it follows the end again once
    /// there (reduce motion jumps).
    pub fn jump_to_latest(&mut self, asid: &str, tx: &mut rune::Tx) {
        tx.scroll_to_bottom(format!("feed-{asid}"));
    }
}

/// Row 0: the start of the conversation, or older history arriving.
fn top_row(cx: &Tcx) -> El {
    let tl = cx.tl();
    let exhausted = tl.is_none_or(|t| t.earlier_exhausted);
    if exhausted {
        return col().key("tl-top").h(8.0);
    }
    let loading = tl.is_some_and(|t| t.loading_earlier);
    let mut line = row().key("tl-top").gap(8.0).center().py(6.0);
    if loading {
        line = line
            .child(Spinner::new().size(12.0).build(cx.k).key("top-spin"))
            .child(
                text(cx.t("Loading earlier messages…", "正在载入更早的消息…"))
                    .font(cx.k.ty.small)
                    .color(cx.k.c.text_faint),
            );
    } else {
        let asid = cx.asid.to_string();
        line = line.child(
            Button::new(cx.t("Load earlier messages", "载入更早的消息"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    if let Some(tl) = s.timelines.get_mut(&asid) {
                        tl.loading_earlier = true;
                    }
                    s.send(Command::LoadEarlier(AgentSessionId(asid.clone())));
                })
                .build(cx.k)
                .key("load-earlier"),
        );
    }
    line
}

fn working_row(cx: &Tcx) -> El {
    row()
        .key("tl-working")
        .gap(8.0)
        .center_y()
        .py(4.0)
        .child(Spinner::new().size(12.0).build(cx.k).key("work-spin"))
        .child(
            text(cx.t("Working…", "正在工作…"))
                .font(cx.k.ty.small)
                .color(cx.k.c.text_muted),
        )
}

/// Roll back and Fork from here: two small icon buttons on a raised chip,
/// shown while the pointer is on the message. Each names itself in a
/// tooltip. The chip stays inside the message's own box, so while it is
/// hidden the pointer can't reach it without showing it.
fn message_actions(cx: &Tcx, it: &TimelineItem) -> El {
    let k = cx.k;
    let asid = AgentSessionId(cx.asid.to_string());
    let (rollback_asid, rollback_id) = (asid.clone(), it.message_id.clone());
    let fork_id = it.message_id.clone();
    let action = |icon: &'static rune::Icon, label: &str| {
        IconButton::new(icon)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(label)
            .help(label)
    };
    let chip = row()
        .key(format!("actions-{}", it.id))
        .gap(2.0)
        .padding(2.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_base)
        .group_opacity(0.0, 1.0)
        .child(
            action(&super::icons::UNDO, cx.t("Roll back to here", "回滚到这里"))
                .on_press(move |s: &mut UiState| {
                    s.send(Command::StageRevert {
                        asid: rollback_asid.clone(),
                        message_id: rollback_id.clone(),
                    })
                })
                .build(k)
                .key(format!("rollback-{}", it.id)),
        )
        .child(
            action(&super::icons::FORK, cx.t("Fork from here", "从这里分叉"))
                .on_press(move |s: &mut UiState| {
                    s.send(Command::ForkSession {
                        asid: asid.clone(),
                        message_id: Some(fork_id.clone()),
                    })
                })
                .build(k)
                .key(format!("fork-{}", it.id)),
        );
    k.elevate(chip, &k.elevation.floating)
}

fn item_row(cx: &Tcx, it: &TimelineItem) -> El {
    let is_text = matches!(it.part, AgentPart::Text { .. });
    let user_text = it.role == TimelineRole::User && is_text;
    let mut wrap = col().key(format!("item-{}", it.id)).w_full().gap(4.0);
    if user_text {
        // The owner's words: a quiet bubble on the trailing side, any
        // pasted pictures above it, a little air before the answer.
        let (body, gallery) = parts::message::text_parts(cx, it);
        wrap = wrap.pt(10.0).pb(4.0);
        if let Some(g) = gallery {
            wrap = wrap.child(g.justify_end().key("user-imgs"));
        }
        if !body.is_empty() {
            let bubble = col()
                .key("user")
                .max_w(620.0)
                .min_w(0.0)
                .shrink()
                .px(14.0)
                .py(8.0)
                .radius(cx.k.radius.lg)
                .fill(cx.k.c.layer_02)
                .child(parts::message::rich_text(cx, &it.id, &body));
            // The actions sit beside the bubble, where they cover none of it.
            wrap = wrap.hover_group().child(
                row()
                    .key("user-line")
                    .w_full()
                    .justify_end()
                    .items_start()
                    .gap(6.0)
                    .child(message_actions(cx, it))
                    .child(bubble),
            );
        }
    } else {
        if is_text {
            wrap = wrap.py(4.0);
        }
        wrap = wrap.child(parts::part(cx, it));
    }
    if is_text && it.role == TimelineRole::Assistant {
        let actions = message_actions(cx, it).pin(None, Some(0.0), Some(0.0), None);
        wrap = wrap.hover_group().child(actions);
    }
    wrap
}

/// What the timeline draws over the window: an opened picture, else a
/// subagent's detail sheet, else the context sheet.
pub fn overlay(s: &UiState, k: &Kit) -> Option<El> {
    parts::images::zoom_overlay(s, k)
        .or_else(|| parts::detail::sheet(s, k))
        .or_else(|| parts::context::sheet(s, k))
}

/// The right-hand detail pane, while a row is open in it.
pub fn detail_pane(s: &UiState, k: &Kit) -> Option<El> {
    parts::pane::view(s, k)
}

/// The session header's context meter (how full the model's context is);
/// a click opens the context and account-limits sheet. `None` when the
/// session reports no usage.
pub fn context_meter(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    parts::context::meter(s, k, asid)
}

/// The session's working-tree changes, full height.
pub fn changes(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    if s.diff.is_none() {
        let mut list = col()
            .key("changes-list")
            .gap(6.0)
            .w_full()
            .px(24.0)
            .py(24.0);
        list = list.child(muted(
            k,
            match language {
                crate::config::Language::En => format!(
                    "{} changed file(s); the agent gave no patch.",
                    s.diff_stat.files
                ),
                _ => super::copy::tr(
                    language,
                    "",
                    &format!("{} 个文件有改动；agent 没有给出补丁。", s.diff_stat.files),
                ),
            },
        ));
        for f in &s.diff_stat.paths {
            let (asid, path) = (s.selected.clone(), f.path.clone());
            list = list.child(
                row()
                    .key(format!("chg-{}", f.path))
                    .gap(8.0)
                    .center_y()
                    .child(
                        Button::new(f.path.clone())
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| {
                                if let Some(asid) = asid.clone() {
                                    s.send(Command::OpenDiff {
                                        asid: AgentSessionId(asid),
                                        file: Some(path.clone()),
                                    });
                                }
                            })
                            .build(k)
                            .key(format!("chg-open-{}", f.path))
                            .grow(1.0),
                    )
                    .child(
                        Badge::new(format!("+{} −{}", f.added, f.removed))
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    ),
            );
        }
        return list
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .grow(1.0)
            .min_h(0.0)
            .scroll_bar(k);
    }
    col().key("changes").w_full().grow(1.0).min_h(0.0).child(
        DiffView::new(
            |s: &UiState| s.diff.as_ref().unwrap_or_else(|| empty_diff()),
            |s: &mut UiState| s.diff.get_or_insert_with(DiffState::default),
        )
        .key("changes-diff")
        .build(s, k),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::TimelineRole;

    fn text_item(id: &str) -> TimelineItem {
        TimelineItem {
            id: id.into(),
            message_id: format!("m-{id}"),
            role: TimelineRole::Assistant,
            part: AgentPart::Text { text: id.into() },
            seq: 0,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        }
    }

    #[test]
    fn pages_and_live_rows_keep_one_ordered_list() {
        let mut tl = SessionTimeline::default();
        tl.replace(vec![text_item("c"), text_item("d")]);
        // A live update replaces in place; a new row appends.
        tl.upsert(vec![text_item("d"), text_item("e")]);
        // An older page goes above, without the rows already held.
        tl.loading_earlier = true;
        tl.prepend(vec![text_item("a"), text_item("b"), text_item("c")], false);
        let ids: Vec<&str> = tl.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d", "e"]);
        assert!(!tl.loading_earlier && !tl.earlier_exhausted);
        // A reader's choice survives a snapshot that still has the row.
        tl.flip("c");
        tl.replace(vec![text_item("c")]);
        assert!(tl.is_open("c", false));
    }

    #[test]
    fn the_latest_pill_shows_off_the_bottom_counts_arrivals_and_jumps_back() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .expect("the session scene");
        scene(&mut s);
        let asid = s.selected.clone().expect("a session");
        let history: Vec<_> = (0..40).map(|i| text_item(&format!("h{i}"))).collect();
        s.timelines
            .get_mut(&asid)
            .expect("its rows")
            .upsert(history);
        let (feed, pill) = (format!("feed-{asid}"), format!("latest-{asid}"));
        let mut rt = rune::Runtime::new(
            s,
            crate::ui::view,
            (1100.0, 560.0),
            rune::Env::new(true, 1.0),
        );
        let mut now = 0.0;
        let mut settle = |rt: &mut rune::Runtime<UiState>| {
            for _ in 0..90 {
                now += 1.0 / 60.0;
                rt.frame(now);
            }
        };
        settle(&mut rt);
        assert!(rt.find(&pill).is_none(), "at the bottom: no pill");
        // A wheel turn up leaves the bottom.
        let (x, y) = rt.find(&feed).expect("the feed");
        rt.wheel(x, y, 600.0);
        settle(&mut rt);
        assert!(rt.state.timelines[&asid].away);
        assert!(rt.find(&pill).is_some(), "scrolled up: the pill");
        // Three messages arrive (a fourth row only updates one in place).
        let rows: Vec<_> = ["n1", "n2", "n3", "n1"]
            .iter()
            .map(|id| text_item(id))
            .collect();
        for row in rows {
            rt.state.timelines.get_mut(&asid).unwrap().upsert(vec![row]);
        }
        assert_eq!(rt.state.timelines[&asid].unseen, 3);
        // Reading on without reaching the end keeps them unseen.
        rt.wheel(x, y, 100.0);
        settle(&mut rt);
        assert_eq!(rt.state.timelines[&asid].unseen, 3);
        // The pill's jump glides down and follows again.
        let target = asid.clone();
        rt.update(move |s, tx| s.jump_to_latest(&target, tx));
        settle(&mut rt);
        let tl = &rt.state.timelines[&asid];
        assert!(!tl.away && tl.unseen == 0);
        assert!(rt.find(&pill).is_none());
    }

    #[test]
    fn live_rows_reach_the_focused_session_and_a_parked_tile() {
        use crate::model::inbox::Update;
        use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId};
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.splash = None;
        let info = |asid: &str| -> muqun_gateway::agents::AgentSessionInfo {
            serde_json::from_value(serde_json::json!({
                "asid": asid, "agent_id": "codex", "title": "", "status": "busy",
                "updated_ms": 1u64,
            }))
            .unwrap()
        };
        let snapshot = |asid: &str| {
            Update::Snapshot(Box::new(
                muqun_gateway::agents::ports::AgentSessionSnapshot {
                    info: info(asid),
                    timeline: vec![text_item(&format!("{asid}-1"))],
                    permissions: vec![],
                    forms: vec![],
                    inbox: vec![],
                    seq: 1,
                },
            ))
        };
        s.open_session("a");
        s.open_in_tile("b");
        s.apply_updates(vec![snapshot("a"), snapshot("b")]);
        let upsert = |asid: &str, id: &str, seq| {
            Update::event(AgentDomainEvent::TimelineUpsert {
                asid: AgentSessionId(asid.into()),
                items: vec![text_item(id)],
                seq,
            })
        };
        s.apply_updates(vec![
            upsert("a", "a-2", 2),
            upsert("b", "b-2", 2),
            upsert("z", "z-1", 2),
        ]);
        let ids = |asid: &str| -> Vec<String> {
            s.timelines[asid]
                .items
                .iter()
                .map(|i| i.id.clone())
                .collect()
        };
        assert_eq!(ids("a"), ["a-1", "a-2"]);
        assert_eq!(ids("b"), ["b-1", "b-2"]);
        assert!(
            !s.timelines.contains_key("z"),
            "a session not on screen keeps no rows"
        );
    }
}
