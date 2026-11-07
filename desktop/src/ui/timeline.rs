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
use std::collections::{HashMap, HashSet};

use muqun_gateway::agents::{
    AgentPart, AgentSessionId, AgentSessionStatus, TimelineItem, TimelineRole,
};
use rune::{col, row, text, El, GrowthSpec, ScrollSpec};
use rune_kit::diff::{DiffState, DiffView};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, ScrollBar, Spinner};

use super::copy::tr_ref;
use super::parts::{self, Tcx};
use super::{muted, UiState};
use crate::model::commands::Command;

pub use super::parts::approval::permission_card;
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

/// Rows a list estimates before it has measured them.
const ROW_ESTIMATE: f32 = 64.0;
/// Readable line length on a wide window; tiles are narrower anyway.
const FEED_MAX_W: f32 = 860.0;

/// One session's timeline as the UI holds it.
#[derive(Default)]
pub struct SessionTimeline {
    pub items: Vec<TimelineItem>,
    /// Diffs drawn inline, by item id, with the source they were built from.
    pub diffs: HashMap<String, (String, DiffState)>,
    /// Cards the reader flipped from their default (open ↔ closed), by id.
    pub flipped: HashSet<String>,
    /// No older history to page in.
    pub earlier_exhausted: bool,
    /// A page of older history is on its way.
    pub loading_earlier: bool,
    /// Bumped whenever rows are added, removed or reordered (the virtual
    /// list's key version).
    pub rev: u64,
}

impl SessionTimeline {
    /// A snapshot: the whole list, as of now. Reader state stays.
    pub fn replace(&mut self, items: Vec<TimelineItem>) {
        let keep: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        self.diffs.retain(|id, _| keep.contains(id.as_str()));
        for item in &items {
            self.ingest(item);
        }
        self.items = items;
        self.rev += 1;
    }

    /// Live rows: replace by id, else append.
    pub fn upsert(&mut self, items: Vec<TimelineItem>) {
        for item in items {
            self.ingest(&item);
            // New and updated rows are nearly always at the tail.
            match self.items.iter().rposition(|t| t.id == item.id) {
                Some(at) => self.items[at] = item,
                None => {
                    self.items.push(item);
                    self.rev += 1;
                }
            }
        }
    }

    pub fn remove(&mut self, ids: &[String]) {
        self.items.retain(|t| !ids.contains(&t.id));
        self.rev += 1;
        for id in ids {
            self.diffs.remove(id);
        }
    }

    /// An older page, above what is loaded.
    pub fn prepend(&mut self, older: Vec<TimelineItem>, exhausted: bool) {
        self.loading_earlier = false;
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
        self.rev += 1;
    }

    /// Parse the item's change (a diff part, an edit's files) once, for drawing.
    fn ingest(&mut self, item: &TimelineItem) {
        match parts::files::diff_source(item) {
            None => {
                self.diffs.remove(&item.id);
            }
            Some((source, files)) => {
                if self
                    .diffs
                    .get(&item.id)
                    .is_none_or(|(old, _)| *old != source)
                {
                    self.diffs
                        .insert(item.id.clone(), (source, DiffState::new(files())));
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
        let tiles: Vec<String> = self.tiles.iter().filter_map(|t| t.asid.clone()).collect();
        self.timelines
            .retain(|asid, _| selected.as_deref() == Some(asid.as_str()) || tiles.contains(asid));
    }

    /// Open a file a timeline card names (a presented file, a glob or grep
    /// hit). The one hook for the file viewer: route it there when it lands;
    /// until then the system opens it. Relative paths are the selected
    /// session's; `~/` is home.
    pub fn open_file(&mut self, path: &str) {
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
            Some(p) => crate::platform::shell::open_path(&p),
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
    Working,
}

fn row_at<'a>(s: &'a UiState, asid: &str, i: usize) -> Option<Row<'a>> {
    let tl = s.timelines.get(asid)?;
    if i == 0 {
        return Some(Row::Top);
    }
    match tl.items.get(i - 1) {
        Some(item) => Some(Row::Item(item)),
        None => Some(Row::Working),
    }
}

fn row_count(s: &UiState, asid: &str) -> usize {
    let n = s.timelines.get(asid).map_or(0, |t| t.items.len());
    1 + n + usize::from(s.session_busy(asid))
}

/// One session's timeline: a virtualised list that only builds the rows
/// near the viewport, follows the bottom while the agent writes, keeps the
/// reader's row when older history lands above it, and pages that history
/// in as the reader nears the top.
pub fn session_view(s: &UiState, k: &Kit, asid: &str) -> El {
    let key = format!("feed-{asid}");
    let Some(tl) = s.timelines.get(asid) else {
        return empty(s, k, &key);
    };
    if tl.items.is_empty() && tl.earlier_exhausted {
        return empty(s, k, &key);
    }
    let count = row_count(s, asid);
    let (key_asid, build_asid, edge_asid) = (asid.to_string(), asid.to_string(), asid.to_string());
    let kit = k.clone();
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
                    Some(Row::Top) => Cow::Borrowed("tl-top"),
                    _ => Cow::Borrowed("tl-working"),
                }
            },
            move |s: &UiState, i| {
                let cx = Tcx::new(s, &kit, &build_asid);
                let mut tail = false;
                let body = match row_at(s, &build_asid, i) {
                    Some(Row::Top) => top_row(&cx),
                    Some(Row::Item(item)) => {
                        // The answer being written: Rune eases its growth.
                        tail = s.session_busy(&build_asid)
                            && matches!(item.part, AgentPart::Text { .. })
                            && cx.tl().and_then(|t| t.items.last()).map(|l| &l.id)
                                == Some(&item.id);
                        item_row(&cx, item)
                    }
                    Some(Row::Working) | None => working_row(&cx),
                };
                // Rows space themselves with padding: virtual rows take no gaps.
                col()
                    .w_full()
                    .items_center()
                    .px(16.0)
                    .py(5.0)
                    .streaming(tail)
                    .child(body.w_full().max_w(FEED_MAX_W))
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
            tl.loading_earlier = true;
            s.send(Command::LoadEarlier(AgentSessionId(edge_asid.clone())));
            true
        });
    col()
        .key(key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(rows)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_bar(k)
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

/// Rollback and Fork from here, under a message's text, shown on hover.
fn message_actions(cx: &Tcx, it: &TimelineItem) -> El {
    let asid = AgentSessionId(cx.asid.to_string());
    let (rollback_asid, rollback_id) = (asid.clone(), it.message_id.clone());
    let fork_id = it.message_id.clone();
    let user = it.role == TimelineRole::User;
    let mut line = row()
        .key(format!("actions-{}", it.id))
        .gap(2.0)
        .center_y()
        .group_opacity(0.0, 1.0);
    if user {
        line = line.self_end();
    }
    line.child(
        Button::new(cx.t("Roll back to here", "回滚到这里"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                s.send(Command::StageRevert {
                    asid: rollback_asid.clone(),
                    message_id: rollback_id.clone(),
                })
            })
            .build(cx.k)
            .key(format!("rollback-{}", it.id)),
    )
    .child(
        Button::new(cx.t("Fork from here", "从这里分叉"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                s.send(Command::ForkSession {
                    asid: asid.clone(),
                    message_id: Some(fork_id.clone()),
                })
            })
            .build(cx.k)
            .key(format!("fork-{}", it.id)),
    )
}

fn item_row(cx: &Tcx, it: &TimelineItem) -> El {
    let body = parts::part(cx, it);
    let is_text = matches!(it.part, AgentPart::Text { .. });
    let user_text = it.role == TimelineRole::User && is_text;
    let mut wrap = col().key(format!("item-{}", it.id)).w_full().gap(2.0);
    wrap = if user_text {
        wrap.child(
            col()
                .key("user")
                .self_end()
                .max_w(620.0)
                .px(14.0)
                .py(9.0)
                .radius(cx.k.radius.lg)
                .fill(cx.k.c.layer_02)
                .child(body),
        )
    } else {
        wrap.child(body)
    };
    if is_text && matches!(it.role, TimelineRole::User | TimelineRole::Assistant) {
        wrap = wrap.hover_group().child(message_actions(cx, it));
    }
    wrap
}

/// The image zoom over the window, when a picture is open.
pub fn overlay(s: &UiState, k: &Kit) -> Option<El> {
    parts::images::zoom_overlay(s, k)
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
                crate::config::Language::ZhCn => {
                    format!("{} 个文件有改动；agent 没有给出补丁。", s.diff_stat.files)
                }
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
}
