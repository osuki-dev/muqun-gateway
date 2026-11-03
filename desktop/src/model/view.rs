//! The UI's model of the sessions, owned by the main thread.
//!
//! [`ViewState`] is plain data: [`ViewState::apply`] folds one [`Update`] in
//! and reports, in [`Changes`], which parts moved -- so it is testable
//! without a window. [`Bindings`] are the keyed Blinc `State` handles the
//! views depend on; `drain.rs` writes only the ones `Changes` names, so a
//! streaming delta to one item rebuilds that item's view and nothing else.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use blinc_core::State;
use muqun_gateway::agents::directories::DirectoryEntry;
use muqun_gateway::agents::{
    AgentDomainEvent, AgentPart, AgentProject, AgentSessionInfo, AgentSessionStatus, FormRequest,
    PermissionRequest, TimelineItem, TimelineRole,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;

use super::inbox::{UiAction, UiError, Update};
use super::projects::DirInfo;
use super::turn::{self, RowTurn, TurnState};

/// How many sessions the sidebar lists. P0 has no paging.
pub const SESSION_LIST_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub struct SessionView {
    pub info: AgentSessionInfo,
    /// The timeline in display order.
    pub items: Vec<Arc<TimelineItem>>,
    /// `items[i]`'s rows: one, or several for a long assistant text
    /// (`stream::segments`), each with its size descriptor.
    rows: Vec<Vec<Shape>>,
    by_id: HashMap<String, Arc<TimelineItem>>,
    pub pending_permissions: Vec<PermissionRequest>,
    pub pending_forms: Vec<FormRequest>,
    /// No older history: `load_earlier_timeline` returned an empty page.
    pub exhausted: bool,
}

impl SessionView {
    fn new(
        info: AgentSessionInfo,
        timeline: Vec<TimelineItem>,
        pending_permissions: Vec<PermissionRequest>,
        pending_forms: Vec<FormRequest>,
    ) -> Self {
        let mut view = Self {
            info,
            items: Vec::with_capacity(timeline.len()),
            rows: Vec::with_capacity(timeline.len()),
            by_id: HashMap::with_capacity(timeline.len()),
            pending_permissions,
            pending_forms,
            exhausted: false,
        };
        let mut timeline = timeline;
        timeline.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        for item in timeline {
            view.upsert(item);
        }
        view
    }

    /// Streaming updates land on the newest rows, so search from the end.
    fn position(&self, id: &str) -> Option<usize> {
        if !self.by_id.contains_key(id) {
            return None;
        }
        self.items.iter().rposition(|item| item.id == id)
    }

    /// Replace the item with `item`'s id, or insert it where its
    /// [`TimelineItem::sort_key`] puts it.
    fn upsert(&mut self, item: TimelineItem) -> Upserted {
        let item = Arc::new(item);
        let rows = Shape::rows(&item);
        self.by_id.insert(item.id.clone(), item.clone());
        if let Some(at) = self.position(&item.id) {
            self.items[at] = item;
            let regrown = self.rows[at].len() != rows.len();
            self.rows[at] = rows;
            return if regrown {
                Upserted::Resegmented
            } else {
                Upserted::Replaced
            };
        }
        let at = self
            .items
            .partition_point(|existing| existing.sort_key() <= item.sort_key());
        self.items.insert(at, item);
        self.rows.insert(at, rows);
        Upserted::Inserted(at)
    }

    /// Hold on to `kept`'s copy of every item that shows the same, and name
    /// the rest in `changes.items`.
    fn keep_unchanged(&mut self, kept: &SessionView, changes: &mut Changes) {
        for item in self.items.iter_mut() {
            match kept.by_id.get(&item.id) {
                Some(same) if same_content(same, item) => {
                    *item = Arc::clone(same);
                    self.by_id.insert(same.id.clone(), Arc::clone(same));
                }
                _ => {
                    if !changes.items.contains(&item.id) {
                        changes.items.push(item.id.clone());
                    }
                }
            }
        }
    }

    /// The list row ids of item `id`.
    pub fn row_ids(&self, id: &str) -> Vec<String> {
        let n = self.position(id).map(|at| self.rows[at].len()).unwrap_or(1);
        (0..n).map(|k| row_id(id, k)).collect()
    }

    fn remove(&mut self, gone: &HashSet<&String>) -> bool {
        let before = self.items.len();
        let mut keep = Vec::with_capacity(before);
        let mut rows = Vec::with_capacity(before);
        for (item, row) in self.items.drain(..).zip(self.rows.drain(..)) {
            if gone.contains(&item.id) {
                self.by_id.remove(&item.id);
            } else {
                keep.push(item);
                rows.push(row);
            }
        }
        self.items = keep;
        self.rows = rows;
        self.items.len() != before
    }

    /// The state of the newest turn.
    pub fn turn_state(&self) -> TurnState {
        let range = match self.items.len() {
            0 => return TurnState::Done,
            n => turn::range_of(&self.items, n - 1),
        };
        turn::derive(
            &self.items[range],
            Some(self.info.status),
            &self.pending_permissions,
            &self.pending_forms,
        )
    }

    /// What item `id`'s row needs to know about its turn.
    pub fn row_turn(&self, id: &str) -> Option<RowTurn> {
        let at = self.position(id)?;
        Some(turn::row_turn(
            &self.items,
            at,
            self.info.status,
            &self.pending_permissions,
            &self.pending_forms,
        ))
    }

    fn turn_snap(&self) -> turn::Snap {
        turn::snap(
            &self.info.asid.0,
            &self.items,
            self.info.status,
            &self.pending_permissions,
            &self.pending_forms,
        )
    }

    /// The part being written right now: the newest item, when it is the
    /// assistant's text or reasoning and the newest turn is `Streaming`
    /// (never while a tool runs or an approval or form waits).
    pub fn live(&self) -> Option<&Arc<TimelineItem>> {
        if self.turn_state() != TurnState::Streaming {
            return None;
        }
        self.items.last().filter(|item| {
            item.role == TimelineRole::Assistant
                && matches!(
                    item.part,
                    AgentPart::Text { .. } | AgentPart::Reasoning { .. }
                )
        })
    }
}

/// What an upsert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upserted {
    Inserted(usize),
    Replaced,
    /// Replaced, and the item is now shown as a different number of rows.
    Resegmented,
}

/// Separates an item id from its row number in a list row id.
const ROW_SEP: char = '\u{1F}';

/// The id of row `k` of item `id`: the item id itself for the first row.
pub fn row_id(id: &str, k: usize) -> String {
    if k == 0 {
        id.to_string()
    } else {
        format!("{id}{ROW_SEP}{k}")
    }
}

/// The item id and row number in a list row id.
pub fn split_row_id(row: &str) -> (&str, usize) {
    match row.split_once(ROW_SEP) {
        Some((id, k)) => (id, k.parse().unwrap_or(0)),
        None => (row, 0),
    }
}

/// Whether an item's text is shown in segments (`stream::segments`).
pub fn segmented(item: &TimelineItem) -> Option<&str> {
    match (&item.part, item.role) {
        (AgentPart::Text { text }, TimelineRole::Assistant) => Some(text),
        _ => None,
    }
}

/// What kind of row an item makes, and how much text it carries: enough to
/// guess its height before it has ever been laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    User,
    Prose,
    Reasoning,
    Tool,
    Diff,
    Card,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub kind: ShapeKind,
    /// Characters of the main text.
    pub chars: u32,
    /// Hard line breaks in it, plus one.
    pub lines: u32,
}

impl Shape {
    /// The shapes of the rows `item` is shown as.
    pub fn rows(item: &TimelineItem) -> Vec<Shape> {
        match segmented(item) {
            Some(text) => super::stream::segments(text)
                .into_iter()
                .map(|r| Self::text(ShapeKind::Prose, &text[r]))
                .collect(),
            None => vec![Self::of(item)],
        }
    }

    fn text(kind: ShapeKind, text: &str) -> Self {
        Self {
            kind,
            chars: text.chars().count().min(u32::MAX as usize) as u32,
            lines: text.bytes().filter(|b| *b == b'\n').count() as u32 + 1,
        }
    }

    pub fn of(item: &TimelineItem) -> Self {
        let text_shape = |kind, text: &str| {
            let lines = text.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
            Self {
                kind,
                chars: text.chars().count().min(u32::MAX as usize) as u32,
                lines,
            }
        };
        let fixed = |kind| Self {
            kind,
            chars: 0,
            lines: 1,
        };
        match (&item.part, item.role) {
            (AgentPart::Text { text }, TimelineRole::User) => text_shape(ShapeKind::User, text),
            (AgentPart::Text { text }, _) => text_shape(ShapeKind::Prose, text),
            (AgentPart::Reasoning { .. }, _) => fixed(ShapeKind::Reasoning),
            (AgentPart::Tool(_), _) | (AgentPart::Shell { .. }, _) => fixed(ShapeKind::Tool),
            (AgentPart::Diff { diff, .. }, _) => text_shape(ShapeKind::Diff, diff),
            (AgentPart::Todo { items }, _) => Self {
                kind: ShapeKind::Card,
                chars: 0,
                lines: items.len() as u32 + 1,
            },
            (AgentPart::Approval { .. }, _) | (AgentPart::Form { .. }, _) => Self {
                kind: ShapeKind::Card,
                chars: 0,
                lines: 5,
            },
            (AgentPart::Synthetic { text, .. }, _) | (AgentPart::System { text, .. }, _) => {
                text_shape(ShapeKind::Prose, text)
            }
            _ => fixed(ShapeKind::Notice),
        }
    }
}

/// What a session row and the session header show, for change detection.
/// `updated_ms` is in it only through the list's order.
#[derive(PartialEq, Eq)]
struct Shown<'a> {
    asid: &'a str,
    agent: &'a str,
    title: &'a str,
    status: AgentSessionStatus,
    directory: Option<&'a str>,
    model: Option<&'a str>,
    variant: Option<&'a str>,
    mode: Option<&'a str>,
}

fn shown(s: &AgentSessionInfo) -> Shown<'_> {
    Shown {
        asid: s.asid.0.as_str(),
        agent: s.agent_id.as_str(),
        title: s.title.as_str(),
        status: s.status,
        directory: s.directory.as_deref(),
        model: s.model.as_ref().map(|m| m.model_id.as_str()),
        variant: s.model.as_ref().and_then(|m| m.variant.as_deref()),
        mode: s.mode.as_deref(),
    }
}

/// Whether two discovery answers describe the agents the same way. The
/// gateway's type has no `PartialEq`; its serialised form is the contract.
fn same_agents(a: &[AgentDiscoveryInfo], b: &[AgentDiscoveryInfo]) -> bool {
    a.len() == b.len()
        && serde_json::to_value(a)
            .ok()
            .is_some_and(|a| serde_json::to_value(b).ok().is_some_and(|b| a == b))
}

/// Whether a re-read item shows the same as the one held: equal but for the
/// bookkeeping (`seq`, `updated_ms`) a re-read restamps.
fn same_content(held: &TimelineItem, read: &TimelineItem) -> bool {
    let mut read = read.clone();
    read.seq = held.seq;
    read.updated_ms = held.updated_ms;
    *held == read
}

/// An agent's known projects, tagged with the agent so a list that arrives
/// after the New-session page moved to another agent is not shown for it.
#[derive(Debug, Clone, Default)]
pub struct ProjectList {
    pub agent: String,
    pub projects: Vec<AgentProject>,
}

/// The local calendar day `ms` falls on, as days since the epoch.
pub fn local_day(ms: u64, utc_offset_s: i64) -> i64 {
    (ms as i64 / 1000 + utc_offset_s).div_euclid(86_400)
}

/// One listed session as the sidebar lays the list out: which session, in
/// which place, under which project. Only what decides the layout -- what a
/// row shows is in its [`SessionRow`], so a status change, a new title or the
/// selection moving rewrites one row and leaves the list alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub asid: String,
    pub directory: Option<String>,
}

/// One changed path from `get_vcs_diff`, for the top-bar diff panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

/// What `get_vcs_diff` says about the open session's working tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub asid: String,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    /// Paths shown in the panel (capped); empty when the agent only gave counts.
    pub paths: Vec<DiffFile>,
}

/// The sidebar's sessions, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListing {
    pub sessions: Vec<ListedSession>,
}

/// What one sidebar row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub asid: String,
    pub agent_id: String,
    pub title: String,
    pub status: AgentSessionStatus,
    /// "5m ago", as of when the row was last written.
    pub since: String,
    pub active: bool,
    pub waiting: bool,
}

/// The timeline's rows, by id, for the list view to key its item views on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineOrder {
    pub asid: String,
    pub ids: Vec<String>,
    /// `ids[i]`'s shape.
    pub shapes: Vec<Shape>,
    /// No older history to load.
    pub exhausted: bool,
}

#[derive(Debug, Default)]
pub struct ViewState {
    /// Every listed session of every agent, newest first.
    pub sessions: Vec<AgentSessionInfo>,
    /// The session the user picked; its snapshot may still be on the way.
    pub selected: Option<String>,
    pub active: Option<SessionView>,
    pub error: Option<UiError>,
    /// Every agent the gateway knows, as discovery last described them.
    pub agents: Vec<AgentDiscoveryInfo>,
    /// Permissions waiting for an answer, in every session this process has
    /// seen asking, oldest first.
    pub attention: Vec<PermissionRequest>,
    pub projects: ProjectList,
    pub completions: Vec<DirectoryEntry>,
    /// The prefix the shown completions answer.
    pub completion_prefix: String,
    /// Forms waiting for an answer, in every session seen asking.
    pub waiting_forms: Vec<FormRequest>,
    /// What each session folder resolved to, by folder (`projects.rs`).
    pub dirs: HashMap<String, DirInfo>,
    /// Each agent's own project list, by agent.
    pub known: std::collections::BTreeMap<String, Vec<AgentProject>>,
    /// The open session's working-tree change count.
    pub diff: Option<DiffStat>,
}

/// What one batch moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub sessions: bool,
    pub selected: bool,
    pub header: bool,
    pub order: bool,
    /// Items whose content changed, by id, in the active session.
    pub items: Vec<String>,
    /// New rows after the previous last row.
    pub appended: usize,
    /// New rows before the previous first row (an older page).
    pub prepended: usize,
    pub permissions: bool,
    pub forms: bool,
    /// An older-history reply for the active session arrived.
    pub earlier_reply: bool,
    pub error: bool,
    pub agents: bool,
    pub attention: bool,
    pub projects: bool,
    pub completions: bool,
    pub dirs: bool,
    pub known: bool,
    pub diff: bool,
    /// A different session (or a fresh snapshot) is now shown.
    pub opened: bool,
    pub actions: Vec<UiAction>,
}

impl Changes {
    /// The bindings this batch writes, for the drain trace.
    pub fn summary(&self) -> String {
        let flags = [
            ("sessions", self.sessions),
            ("selected", self.selected),
            ("header", self.header),
            ("order", self.order),
            ("permissions", self.permissions),
            ("forms", self.forms),
            ("earlier", self.earlier_reply),
            ("error", self.error),
            ("agents", self.agents),
            ("attention", self.attention),
            ("projects", self.projects),
            ("completions", self.completions),
            ("dirs", self.dirs),
            ("known", self.known),
            ("diff", self.diff),
            ("opened", self.opened),
        ];
        let mut out: Vec<String> = flags
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name.to_string())
            .collect();
        if !self.items.is_empty() {
            out.push(format!("items×{}", self.items.len()));
        }
        if out.is_empty() {
            "-".into()
        } else {
            out.join(",")
        }
    }

    pub fn timeline(&self) -> bool {
        self.order || !self.items.is_empty() || self.permissions || self.forms || self.opened
    }
}

impl ViewState {
    /// The session the user picked, before its snapshot arrives.
    pub fn select(&mut self, asid: &str) -> bool {
        if self.selected.as_deref() == Some(asid) {
            return false;
        }
        self.selected = Some(asid.to_string());
        self.diff = None;
        true
    }

    /// Fold `update` in. Then, if the newest turn changed (its state, its
    /// last item, a tool waiting on an approval), the rows that show that
    /// are named in `changes.items` so they render again.
    pub fn apply(&mut self, update: Update, changes: &mut Changes) {
        let before = self.active.as_ref().map(SessionView::turn_snap);
        self.apply_update(update, changes);
        let after = self.active.as_ref().map(SessionView::turn_snap);
        if let (Some(before), Some(after)) = (before, after) {
            if before.asid == after.asid && before != after {
                for id in before.touched().into_iter().chain(after.touched()) {
                    if !changes.items.contains(&id) {
                        changes.items.push(id);
                    }
                }
            }
        }
    }

    fn apply_update(&mut self, update: Update, changes: &mut Changes) {
        match update {
            Update::Agents(agents) => {
                // Discovery is polled while cold probes settle; most answers
                // are the same as the last one and must not rebuild anything.
                if !same_agents(&self.agents, &agents) {
                    self.agents = agents;
                    changes.agents = true;
                }
            }
            Update::AgentSessions {
                agent,
                mut sessions,
            } => {
                for session in &mut sessions {
                    if session.agent_id.is_empty() {
                        session.agent_id = agent.clone();
                    }
                }
                sessions.truncate(SESSION_LIST_LIMIT);
                let before = std::mem::take(&mut self.sessions);
                self.sessions = before
                    .iter()
                    .filter(|s| s.agent_id != agent)
                    .cloned()
                    .chain(sessions)
                    .collect();
                self.sort_sessions();
                changes.sessions |= self.sessions != before;
                if self.error.as_ref().is_some_and(|e| e.unreachable) {
                    // A list that arrived answers "OpenCode is not reachable".
                    self.error = None;
                    changes.error = true;
                }
            }
            Update::SessionCreated(info) => {
                let asid = info.asid.0.clone();
                self.upsert_session(*info, changes);
                changes.selected |= self.select(&asid);
            }
            Update::SessionGone(asid) => {
                let before = self.sessions.len();
                self.sessions.retain(|s| s.asid.0 != asid);
                if self.sessions.len() != before {
                    changes.sessions = true;
                }
                let was_selected = self.selected.as_deref() == Some(asid.as_str());
                let was_active = self.active.as_ref().is_some_and(|a| a.info.asid.0 == asid);
                if was_selected {
                    self.selected = None;
                    changes.selected = true;
                }
                if was_active || was_selected {
                    self.active = None;
                    self.diff = None;
                    changes.header = true;
                    changes.order = true;
                    changes.opened = true;
                }
            }
            Update::Snapshot(snapshot) => {
                if self.selected.as_deref() != Some(snapshot.info.asid.0.as_str()) {
                    return;
                }
                self.upsert_session(snapshot.info.clone(), changes);
                let snapshot = *snapshot;
                // A fresh read of the session already shown (a resync) keeps
                // the older pages it had loaded and what it knew about them.
                let kept = self
                    .active
                    .take()
                    .filter(|a| a.info.asid == snapshot.info.asid);
                // An empty permissions/forms list on a resync can be the
                // gateway follower lagging; keep what we already showed until
                // a non-empty read or a resolve event clears it.
                let pending_permissions = match &kept {
                    Some(kept)
                        if snapshot.permissions.is_empty()
                            && !kept.pending_permissions.is_empty() =>
                    {
                        kept.pending_permissions.clone()
                    }
                    _ => snapshot.permissions.clone(),
                };
                let pending_forms = match &kept {
                    Some(kept) if snapshot.forms.is_empty() && !kept.pending_forms.is_empty() => {
                        kept.pending_forms.clone()
                    }
                    _ => snapshot.forms.clone(),
                };
                let asid = snapshot.info.asid.clone();
                let before = self.attention.clone();
                self.attention.retain(|p| p.asid != asid);
                self.attention.extend(pending_permissions.iter().cloned());
                changes.attention |= self.attention != before;
                let before = self.waiting_forms.clone();
                self.waiting_forms.retain(|f| f.asid != asid);
                self.waiting_forms.extend(pending_forms.iter().cloned());
                changes.attention |= self.waiting_forms != before;
                let mut view = SessionView::new(
                    snapshot.info,
                    snapshot.timeline,
                    pending_permissions,
                    pending_forms,
                );
                let Some(kept) = kept else {
                    self.active = Some(view);
                    let active = self.active.as_ref().expect("just set");
                    changes.items = active.items.iter().map(|i| i.id.clone()).collect();
                    changes.order = true;
                    changes.header = true;
                    changes.permissions = true;
                    changes.forms = true;
                    changes.opened = true;
                    return;
                };
                let newest = view.items.first().map(|i| i.sort_key().0.to_string());
                for item in &kept.items {
                    let older = newest
                        .as_deref()
                        .is_some_and(|n| item.message_id.as_str() < n);
                    if older && !view.by_id.contains_key(&item.id) {
                        let _ = view.upsert((**item).clone());
                    }
                }
                view.exhausted = kept.exhausted;
                // Only what differs is a change: a terminal-followed session
                // is re-read every time its transcript moves, and that must
                // render the rows that moved, not reopen the timeline.
                view.keep_unchanged(&kept, changes);
                let before = ViewState::order_of(&kept);
                let after = ViewState::order_of(&view);
                // As for an upsert: a row that grew is the row's change; the
                // order moves when rows come, go or re-segment.
                if before.ids != after.ids {
                    changes.order = true;
                    // Rows past the old last one, for "new below" and following.
                    if let Some(last) = kept.items.last() {
                        changes.appended += view
                            .items
                            .iter()
                            .filter(|i| {
                                i.sort_key() > last.sort_key() && !kept.by_id.contains_key(&i.id)
                            })
                            .count();
                    }
                }
                changes.permissions |= view.pending_permissions != kept.pending_permissions;
                changes.forms |= view.pending_forms != kept.pending_forms;
                self.active = Some(view);
            }
            Update::Event(event) => self.apply_event(*event, changes),
            Update::Error(error) => {
                if self.error.as_ref() != Some(&error) {
                    self.error = Some(error);
                    changes.error = true;
                }
            }
            // Routed past the view by the drain (`drain.rs` `apply`).
            Update::Theme(_) | Update::Composer(_) => {}
            Update::Projects { agent, projects } => {
                self.projects = ProjectList { agent, projects };
                changes.projects = true;
            }
            Update::Completions {
                prefix, entries, ..
            } => {
                self.completion_prefix = prefix;
                self.completions = entries;
                changes.completions = true;
            }
            Update::Ui(action) => changes.actions.push(action),
            Update::Directories(found) => {
                for (dir, info) in found {
                    if self.dirs.get(&dir) != Some(&info) {
                        self.dirs.insert(dir, info);
                        changes.dirs = true;
                    }
                }
            }
            Update::KnownProjects { agent, projects } => {
                if self.known.get(&agent) != Some(&projects) {
                    self.known.insert(agent, projects);
                    changes.known = true;
                }
            }
            Update::Diff(stat) => {
                let fresh = self.selected.as_deref() == Some(stat.asid.as_str());
                let stat = fresh.then_some(stat);
                if self.diff != stat {
                    self.diff = stat;
                    changes.diff = true;
                }
            }
            Update::Earlier {
                asid,
                items,
                exhausted,
            } => {
                let Some(active) = self.active_for(&asid) else {
                    return;
                };
                changes.earlier_reply = true;
                let first = active.items.first().cloned();
                for item in items {
                    let id = item.id.clone();
                    let older = first
                        .as_ref()
                        .is_none_or(|f| item.sort_key() < f.sort_key());
                    match active.upsert(item) {
                        Upserted::Inserted(_) => {
                            changes.order = true;
                            if older {
                                changes.prepended += 1;
                            }
                        }
                        Upserted::Resegmented => changes.order = true,
                        Upserted::Replaced => {}
                    }
                    if !changes.items.contains(&id) {
                        changes.items.push(id);
                    }
                }
                if active.exhausted != exhausted {
                    active.exhausted = exhausted;
                    changes.order = true;
                }
            }
        }
    }

    pub fn dismiss_error(&mut self) -> bool {
        self.error.take().is_some()
    }

    /// Back to Home: no session selected.
    pub fn deselect(&mut self) -> bool {
        self.active = None;
        self.diff = None;
        self.selected.take().is_some()
    }

    fn sort_sessions(&mut self) {
        self.sessions
            .sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    }

    fn active_for(&mut self, asid: &str) -> Option<&mut SessionView> {
        self.active
            .as_mut()
            .filter(|active| active.info.asid.0 == asid)
    }

    /// Fold in a session's new info. Only what the views show counts as a
    /// change: an agent that is working sends `SessionUpdated` with every
    /// step, and rebuilding the list each time would replace its rows under
    /// the pointer between a press and its release.
    fn upsert_session(&mut self, mut info: AgentSessionInfo, changes: &mut Changes) {
        // An event's info may not carry the agent; the listed row does.
        if info.agent_id.is_empty() {
            if let Some(known) = self.sessions.iter().find(|s| s.asid == info.asid) {
                info.agent_id = known.agent_id.clone();
            }
        }
        if let Some(active) = self.active_for(&info.asid.0) {
            if shown(&active.info) != shown(&info) {
                changes.header = true;
            }
            active.info = info.clone();
        }
        let asid = info.asid.clone();
        let before = self.sessions.iter().position(|s| s.asid == asid);
        let same_row = before.is_some_and(|at| shown(&self.sessions[at]) == shown(&info));
        match before {
            Some(at) => self.sessions[at] = info,
            None => self.sessions.push(info),
        }
        self.sort_sessions();
        let after = self.sessions.iter().position(|s| s.asid == asid);
        if !same_row || before != after {
            changes.sessions = true;
        }
    }

    fn set_status(&mut self, asid: &str, status: AgentSessionStatus, changes: &mut Changes) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.asid.0 == asid) {
            if session.status != status {
                session.status = status;
                changes.sessions = true;
            }
        }
        if let Some(active) = self.active_for(asid) {
            if active.info.status != status {
                active.info.status = status;
                changes.header = true;
            }
        }
    }

    fn apply_event(&mut self, event: AgentDomainEvent, changes: &mut Changes) {
        match event {
            AgentDomainEvent::SessionUpdated { info, .. } => {
                // Child sessions (subagents) are not listed in P0.
                if info.parent_id.is_none() || self.sessions.iter().any(|s| s.asid == info.asid) {
                    self.upsert_session(*info, changes);
                }
            }
            AgentDomainEvent::StatusChanged {
                asid,
                status,
                error,
                ..
            } => {
                self.set_status(&asid.0, status, changes);
                if let Some(error) = error.filter(|_| self.selected.as_deref() == Some(&asid.0)) {
                    self.error = Some(UiError {
                        message: format!("{}: {}", error.name, error.message),
                        detail: None,
                        unreachable: false,
                    });
                    changes.error = true;
                }
            }
            AgentDomainEvent::TimelineUpsert {
                asid, mut items, ..
            } => {
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                // Same attach the HTTP routes do: Desktop never hits those.
                muqun_gateway::agents::message_images::enrich_timeline(
                    &asid.0,
                    active.info.directory.as_deref(),
                    &mut items,
                );
                for item in items {
                    let id = item.id.clone();
                    match active.upsert(item) {
                        Upserted::Inserted(at) => {
                            changes.order = true;
                            if at + 1 == active.items.len() {
                                changes.appended += 1;
                            }
                        }
                        Upserted::Resegmented => changes.order = true,
                        Upserted::Replaced => {}
                    }
                    if !changes.items.contains(&id) {
                        changes.items.push(id);
                    }
                }
            }
            AgentDomainEvent::TimelineRemoved { asid, ids, .. } => {
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                let gone: HashSet<&String> = ids.iter().collect();
                changes.order |= active.remove(&gone);
            }
            AgentDomainEvent::PermissionPending { asid, request, .. } => {
                match self.attention.iter().position(|p| p.id == request.id) {
                    Some(at) => self.attention[at] = request.clone(),
                    None => self.attention.push(request.clone()),
                }
                changes.attention = true;
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                match active
                    .pending_permissions
                    .iter()
                    .position(|p| p.id == request.id)
                {
                    Some(at) => active.pending_permissions[at] = request,
                    None => active.pending_permissions.push(request),
                }
                changes.permissions = true;
            }
            AgentDomainEvent::PermissionResolved {
                asid, request_id, ..
            } => {
                let before = self.attention.len();
                self.attention.retain(|p| p.id != request_id);
                changes.attention |= self.attention.len() != before;
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                let before = active.pending_permissions.len();
                active.pending_permissions.retain(|p| p.id != request_id);
                changes.permissions |= active.pending_permissions.len() != before;
            }
            AgentDomainEvent::FormPending { asid, request, .. } => {
                match self.waiting_forms.iter().position(|f| f.id == request.id) {
                    Some(at) => self.waiting_forms[at] = request.clone(),
                    None => self.waiting_forms.push(request.clone()),
                }
                changes.attention = true;
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                match active.pending_forms.iter().position(|f| f.id == request.id) {
                    Some(at) => active.pending_forms[at] = request,
                    None => active.pending_forms.push(request),
                }
                changes.forms = true;
            }
            AgentDomainEvent::FormResolved { asid, form_id, .. } => {
                let before = self.waiting_forms.len();
                self.waiting_forms.retain(|f| f.id != form_id);
                changes.attention |= self.waiting_forms.len() != before;
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                let before = active.pending_forms.len();
                active.pending_forms.retain(|f| f.id != form_id);
                changes.forms |= active.pending_forms.len() != before;
            }
            // Compaction, revert, inbox and worktrees are P1; a resync is
            // answered on the core side with a fresh snapshot.
            _ => {}
        }
    }

    /// How the sidebar lays the listed sessions out.
    pub fn listing(&self) -> SessionListing {
        SessionListing {
            sessions: self
                .sessions
                .iter()
                .map(|s| ListedSession {
                    asid: s.asid.0.clone(),
                    directory: s.directory.clone().filter(|d| !d.is_empty()),
                })
                .collect(),
        }
    }

    /// What each listed session's sidebar row shows.
    pub fn rows(&self) -> impl Iterator<Item = SessionRow> + '_ {
        self.sessions.iter().map(|s| SessionRow {
            asid: s.asid.0.clone(),
            agent_id: s.agent_id.clone(),
            title: s.title.clone(),
            status: s.status,
            since: muqun_gateway::relative_since(u128::from(s.updated_ms)),
            active: self.selected.as_deref() == Some(s.asid.0.as_str()),
            waiting: self.attention.iter().any(|p| p.asid == s.asid)
                || self.waiting_forms.iter().any(|f| f.asid == s.asid),
        })
    }

    pub fn order(&self) -> TimelineOrder {
        self.active.as_ref().map(Self::order_of).unwrap_or_default()
    }

    fn order_of(active: &SessionView) -> TimelineOrder {
        TimelineOrder {
            asid: active.info.asid.0.clone(),
            ids: active
                .items
                .iter()
                .zip(&active.rows)
                .flat_map(|(i, r)| (0..r.len()).map(move |k| row_id(&i.id, k)))
                .collect(),
            shapes: active.rows.iter().flatten().copied().collect(),
            exhausted: active.exhausted,
        }
    }

    pub fn item(&self, id: &str) -> Option<Arc<TimelineItem>> {
        self.active.as_ref()?.by_id.get(id).cloned()
    }

    /// Item `id`'s turn, for its row.
    pub fn row_turn(&self, id: &str) -> Option<RowTurn> {
        self.active.as_ref()?.row_turn(id)
    }

    /// The streaming item of the active session, if one is streaming.
    pub fn live(&self) -> Option<&Arc<TimelineItem>> {
        self.active.as_ref()?.live()
    }
}

/// The keyed Blinc state the views depend on. Created once, on the main
/// thread, after Blinc's context exists (the first `build_ui`).
#[derive(Clone)]
pub struct Bindings {
    /// Every listed session, in full. The sidebar does not read it (see
    /// `listing` and [`row_state`]); Home and other summaries do.
    pub sessions: State<Arc<Vec<AgentSessionInfo>>>,
    /// The sidebar list's layout, written only when it changes.
    pub listing: State<Arc<SessionListing>>,
    /// What each session folder resolved to (`projects.rs`).
    pub dirs: State<Arc<HashMap<String, DirInfo>>>,
    /// Every agent's own projects, together.
    pub known: State<Arc<Vec<AgentProject>>>,
    /// Forms waiting in any session, for the Inbox count.
    pub waiting_forms: State<Arc<Vec<FormRequest>>>,
    /// The open session's working-tree change count.
    pub diff: State<Option<DiffStat>>,
    /// The top-bar diff file list is open. UI-owned.
    pub diff_panel: State<bool>,
    /// The Inbox page, while it is open. UI-owned.
    pub inbox_open: State<bool>,
    pub selected: State<Option<String>>,
    pub header: State<Option<Arc<AgentSessionInfo>>>,
    pub order: State<Arc<TimelineOrder>>,
    pub permissions: State<Arc<Vec<PermissionRequest>>>,
    pub error: State<Option<UiError>>,
    pub agents: State<Arc<Vec<AgentDiscoveryInfo>>>,
    pub attention: State<Arc<Vec<PermissionRequest>>>,
    /// The New-session page, while it is open. UI-owned.
    pub draft: State<Option<NewSessionDraft>>,
    pub projects: State<Arc<ProjectList>>,
    pub completions: State<Arc<Vec<DirectoryEntry>>>,
    /// The Settings page, while it is open. UI-owned.
    pub settings_open: State<bool>,
    pub settings_tab: State<String>,
    /// An agent setting changed since start: show "restart to apply".
    pub restart_note: State<bool>,
    pub timeline_scroll: blinc_layout::selector::ScrollRef,
    /// Forms waiting for an answer in the active session.
    pub forms: State<Arc<Vec<FormRequest>>>,
    /// The streaming item's tail, shown through `Text::bound` (`stream.rs`).
    pub stream_tail: State<String>,
    /// Bumped when the colour scheme changes: rows bake theme colours into
    /// their rich text and render again.
    pub theme_epoch: State<u64>,
    /// Each built row's version signal; see [`RowSignals`].
    pub rows: RowSignals,
}

impl Bindings {
    pub fn new(timeline_scroll: blinc_layout::selector::ScrollRef) -> Self {
        let ctx = blinc_core::BlincContextState::get();
        Self {
            sessions: ctx.use_state_keyed("mq.sessions", || Arc::new(Vec::new())),
            listing: ctx.use_state_keyed("mq.listing", || Arc::new(SessionListing::default())),
            dirs: ctx.use_state_keyed("mq.dirs", || Arc::new(HashMap::new())),
            known: ctx.use_state_keyed("mq.known", || Arc::new(Vec::new())),
            waiting_forms: ctx.use_state_keyed("mq.waiting_forms", || Arc::new(Vec::new())),
            diff: ctx.use_state_keyed("mq.diff", || None),
            diff_panel: ctx.use_state_keyed("mq.diff_panel", || false),
            inbox_open: ctx.use_state_keyed("mq.inbox_open", || false),
            selected: ctx.use_state_keyed("mq.selected", || None),
            header: ctx.use_state_keyed("mq.header", || None),
            order: ctx.use_state_keyed("mq.order", || Arc::new(TimelineOrder::default())),
            permissions: ctx.use_state_keyed("mq.permissions", || Arc::new(Vec::new())),
            error: ctx.use_state_keyed("mq.error", || None),
            agents: ctx.use_state_keyed("mq.agents", || Arc::new(Vec::new())),
            attention: ctx.use_state_keyed("mq.attention", || Arc::new(Vec::new())),
            draft: ctx.use_state_keyed("mq.draft", || None),
            projects: ctx.use_state_keyed("mq.projects", || Arc::new(ProjectList::default())),
            completions: ctx.use_state_keyed("mq.completions", || Arc::new(Vec::new())),
            settings_open: ctx.use_state_keyed("mq.settings_open", || false),
            settings_tab: ctx.use_state_keyed("mq.settings_tab", || "appearance".to_string()),
            restart_note: ctx.use_state_keyed("mq.restart_note", || false),
            timeline_scroll,
            forms: ctx.use_state_keyed("mq.forms", || Arc::new(Vec::new())),
            stream_tail: ctx.use_state_keyed("mq.stream_tail", String::new),
            theme_epoch: ctx.use_state_keyed("mq.theme_epoch", || 0),
            rows: RowSignals::default(),
        }
    }
}

/// The New-session page's choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionDraft {
    pub agent: String,
}

/// One sidebar row's state, keyed by session, so the row view finds it again
/// whenever the list is rebuilt.
pub fn row_state(asid: &str) -> State<Option<Arc<SessionRow>>> {
    blinc_core::BlincContextState::get().use_state_keyed(&format!("mq.srow.{asid}"), || None)
}

/// The version signal of every timeline row that has been built.
///
/// A row reads its item from the view when it renders and depends on its
/// own signal; the drain bumps the signal of a changed item only when that
/// row exists. Rows outside the list's window are never built, so a session
/// of 10 000 items costs 10 000 signals only if all of them are visited.
#[derive(Clone, Default)]
pub struct RowSignals {
    inner: Arc<Mutex<RowSignalsInner>>,
}

#[derive(Default)]
struct RowSignalsInner {
    asid: String,
    rows: HashMap<String, State<u64>>,
    version: u64,
}

impl RowSignals {
    /// The signal of `asid`'s row `id`, created on first use. Main thread,
    /// with Blinc's context up. Rows of another session are forgotten.
    pub fn signal(&self, asid: &str, id: &str) -> State<u64> {
        let mut inner = self.inner.lock().unwrap();
        if inner.asid != asid {
            inner.asid = asid.to_string();
            inner.rows.clear();
        }
        if let Some(state) = inner.rows.get(id) {
            return state.clone();
        }
        let state = blinc_core::BlincContextState::get()
            .use_state_keyed(&format!("mq.row.{asid}.{id}"), || 0u64);
        inner.rows.insert(id.to_string(), state.clone());
        state
    }

    /// The built rows among `ids`, each with a fresh version to write.
    pub fn bump<'a>(
        &self,
        asid: &str,
        ids: impl IntoIterator<Item = &'a String>,
    ) -> Vec<(State<u64>, u64)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.asid != asid {
            return Vec::new();
        }
        inner.version += 1;
        let version = inner.version;
        ids.into_iter()
            .filter_map(|id| inner.rows.get(id).map(|s| (s.clone(), version)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::ports::AgentSessionSnapshot;
    use muqun_gateway::agents::{AgentPart, AgentSessionId, TimelineRole};

    fn info(asid: &str, updated_ms: u64) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid, "title": asid, "status": "idle", "updated_ms": updated_ms
        }))
        .unwrap()
    }

    fn item(id: &str, message: &str, ordinal: u64) -> TimelineItem {
        TimelineItem {
            id: id.into(),
            message_id: message.into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text { text: id.into() },
            seq: 0,
            updated_ms: 0,
            ordinal,
            attachments: None,
            image_assets: None,
            generation: None,
        }
    }

    fn snapshot(asid: &str, items: Vec<TimelineItem>) -> Update {
        Update::Snapshot(Box::new(AgentSessionSnapshot {
            info: info(asid, 5),
            timeline: items,
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 1,
        }))
    }

    fn opened(asid: &str, items: Vec<TimelineItem>) -> ViewState {
        let mut view = ViewState::default();
        view.select(asid);
        view.apply(snapshot(asid, items), &mut Changes::default());
        view
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let mut view = ViewState::default();
        let mut changes = Changes::default();
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![info("a", 1), info("b", 3), info("c", 2)],
            },
            &mut changes,
        );
        let order: Vec<_> = view.sessions.iter().map(|s| s.asid.0.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
        assert!(changes.sessions);
    }

    #[test]
    fn each_agent_replaces_only_its_own_list_and_the_merge_stays_newest_first() {
        let mut view = ViewState::default();
        let mut changes = Changes::default();
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![info("o1", 1), info("o2", 4)],
            },
            &mut changes,
        );
        view.apply(
            Update::AgentSessions {
                agent: "codex".into(),
                sessions: vec![info("c1", 3)],
            },
            &mut changes,
        );
        let order: Vec<_> = view.sessions.iter().map(|s| s.asid.0.as_str()).collect();
        assert_eq!(order, ["o2", "c1", "o1"]);
        assert_eq!(view.sessions[1].agent_id, "codex", "stamped when missing");
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![info("o3", 2)],
            },
            &mut changes,
        );
        let order: Vec<_> = view.sessions.iter().map(|s| s.asid.0.as_str()).collect();
        assert_eq!(order, ["c1", "o3"]);
    }

    #[test]
    fn a_snapshot_for_a_session_no_longer_selected_is_ignored() {
        let mut view = ViewState::default();
        view.select("a");
        view.select("b");
        let mut changes = Changes::default();
        view.apply(snapshot("a", vec![item("x", "m1", 0)]), &mut changes);
        assert!(view.active.is_none());
        assert_eq!(changes, Changes::default());
    }

    #[test]
    fn an_update_to_one_item_changes_only_that_item() {
        let mut view = opened("s", vec![item("x", "m1", 0), item("y", "m1", 1)]);
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::TimelineUpsert {
                asid: AgentSessionId("s".into()),
                items: vec![item("y", "m1", 1)],
                seq: 2,
            }),
            &mut changes,
        );
        assert_eq!(changes.items, ["y"]);
        assert!(!changes.order, "an existing row keeps its place");
        assert!(!changes.sessions && !changes.header);
    }

    #[test]
    fn new_items_go_where_their_sort_key_puts_them() {
        let mut view = opened("s", vec![item("a", "m1", 0), item("c", "m3", 0)]);
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::TimelineUpsert {
                asid: AgentSessionId("s".into()),
                items: vec![item("b", "m2", 0), item("d", "m4", 0)],
                seq: 2,
            }),
            &mut changes,
        );
        assert_eq!(view.order().ids, ["a", "b", "c", "d"]);
        assert!(changes.order);
    }

    #[test]
    fn events_for_another_session_leave_the_timeline_alone() {
        let mut view = opened("s", vec![item("a", "m1", 0)]);
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::TimelineUpsert {
                asid: AgentSessionId("t".into()),
                items: vec![item("z", "m9", 0)],
                seq: 2,
            }),
            &mut changes,
        );
        assert!(!changes.timeline());
        assert_eq!(view.order().ids, ["a"]);
    }

    #[test]
    fn status_reaches_both_the_list_and_the_header() {
        let mut view = opened("s", vec![]);
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::StatusChanged {
                asid: AgentSessionId("s".into()),
                status: AgentSessionStatus::Busy,
                error: None,
                seq: 2,
            }),
            &mut changes,
        );
        assert!(changes.sessions && changes.header);
        assert_eq!(view.sessions[0].status, AgentSessionStatus::Busy);
        assert_eq!(view.active.unwrap().info.status, AgentSessionStatus::Busy);
    }

    #[test]
    fn permissions_come_and_go() {
        let mut view = opened("s", vec![]);
        let request: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "per_1", "asid": "s", "action": "bash", "resources": ["ls"],
            "prompt": "Run ls?", "options": []
        }))
        .unwrap();
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::PermissionPending {
                asid: AgentSessionId("s".into()),
                request,
                seq: 2,
            }),
            &mut changes,
        );
        assert!(changes.permissions);
        assert_eq!(view.active.as_ref().unwrap().pending_permissions.len(), 1);
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::PermissionResolved {
                asid: AgentSessionId("s".into()),
                request_id: "per_1".into(),
                seq: 3,
            }),
            &mut changes,
        );
        assert!(changes.permissions);
        assert!(view.active.unwrap().pending_permissions.is_empty());
    }

    #[test]
    fn a_busy_session_touching_its_timestamp_does_not_rebuild_the_list() {
        let mut view = ViewState::default();
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![info("a", 5), info("b", 3)],
            },
            &mut Changes::default(),
        );
        let mut changes = Changes::default();
        view.apply(
            Update::event(AgentDomainEvent::SessionUpdated {
                asid: AgentSessionId("a".into()),
                info: Box::new(info("a", 6)),
                seq: 1,
            }),
            &mut changes,
        );
        assert!(!changes.sessions, "same order, same row");
        view.apply(
            Update::event(AgentDomainEvent::SessionUpdated {
                asid: AgentSessionId("b".into()),
                info: Box::new(info("b", 9)),
                seq: 1,
            }),
            &mut changes,
        );
        assert!(changes.sessions, "a reorder is a change");
    }

    #[test]
    fn an_unchanged_list_or_discovery_answer_changes_nothing() {
        let mut view = ViewState::default();
        let list = || Update::AgentSessions {
            agent: "opencode".into(),
            sessions: vec![info("a", 5), info("b", 3)],
        };
        view.apply(list(), &mut Changes::default());
        let agents: Vec<AgentDiscoveryInfo> = serde_json::from_value(serde_json::json!([{
            "id": "opencode", "name": "OpenCode", "kind": "opencode", "status": "connected",
            "enabled": true, "models": [], "modes": [], "features": {
                "streaming": true, "reasoningEffort": false, "modelSelection": true,
                "toolApprovals": true, "worktrees": false, "revert": false,
                "stagedRevert": false, "inbox": false, "modes": false, "skills": false,
                "slashCommands": false, "compaction": false, "backgroundShells": false,
                "attachments": false, "attachmentsByPath": false
            }
        }]))
        .unwrap();
        view.apply(Update::Agents(agents.clone()), &mut Changes::default());
        let mut changes = Changes::default();
        view.apply(list(), &mut changes);
        view.apply(Update::Agents(agents), &mut changes);
        assert_eq!(changes, Changes::default());
    }

    #[test]
    fn rows_follow_selection_and_attention_while_the_listing_stays() {
        let mut view = ViewState::default();
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![info("a", 5), info("b", 3)],
            },
            &mut Changes::default(),
        );
        let listing = view.listing();
        view.select("b");
        let rows: Vec<_> = view.rows().collect();
        assert!(!rows[0].active && rows[1].active);
        assert_eq!(view.listing(), listing, "selection is not layout");
        view.apply(
            Update::event(AgentDomainEvent::StatusChanged {
                asid: AgentSessionId("a".into()),
                status: AgentSessionStatus::Busy,
                error: None,
                seq: 1,
            }),
            &mut Changes::default(),
        );
        assert_eq!(view.rows().next().unwrap().status, AgentSessionStatus::Busy);
        assert_eq!(view.listing(), listing, "status is not layout");
    }

    #[test]
    fn a_reread_of_the_open_session_changes_only_what_differs() {
        let mut view = opened("s", vec![item("x", "m1", 0), item("y", "m1", 1)]);
        let mut changes = Changes::default();
        let mut changed = item("y", "m1", 1);
        changed.part = AgentPart::Text { text: "new".into() };
        view.apply(
            snapshot("s", vec![item("x", "m1", 0), changed]),
            &mut changes,
        );
        assert_eq!(changes.items, ["y"]);
        assert!(!changes.opened && !changes.order && !changes.permissions);
        let mut changes = Changes::default();
        view.apply(
            snapshot("s", vec![item("x", "m1", 0), item("z", "m2", 0)]),
            &mut changes,
        );
        assert!(changes.order, "a row came and went");
        assert_eq!(view.order().ids, ["x", "z"]);
    }

    #[test]
    fn a_reread_keeps_older_pages_and_names_only_the_new_rows() {
        let mut view = opened("s", vec![item("x", "m2", 0)]);
        view.apply(
            Update::Earlier {
                asid: "s".into(),
                items: vec![item("old", "m1", 0)],
                exhausted: true,
            },
            &mut Changes::default(),
        );
        let mut changes = Changes::default();
        view.apply(
            snapshot("s", vec![item("x", "m2", 0), item("y", "m3", 0)]),
            &mut changes,
        );
        assert_eq!(view.order().ids, ["old", "x", "y"], "the older page stays");
        assert!(view.order().exhausted);
        assert_eq!(changes.items, ["y"]);
        assert!(changes.order && !changes.opened);
        assert_eq!(changes.appended, 1);
    }

    #[test]
    fn the_cursor_and_the_waiting_state_exclude_each_other() {
        let mut busy = info("s", 5);
        busy.status = AgentSessionStatus::Busy;
        let user = TimelineItem {
            role: TimelineRole::User,
            ..item("u", "m1", 0)
        };
        let items = vec![user, item("a", "m2", 0)];
        let read = |permissions: Vec<PermissionRequest>| {
            let mut view = ViewState::default();
            view.select("s");
            view.apply(
                Update::Snapshot(Box::new(AgentSessionSnapshot {
                    info: busy.clone(),
                    timeline: items.clone(),
                    permissions,
                    forms: vec![],
                    inbox: vec![],
                    seq: 1,
                })),
                &mut Changes::default(),
            );
            view
        };
        let streaming = read(vec![]);
        assert!(
            streaming.live().is_some(),
            "the cursor belongs to Streaming"
        );
        assert_eq!(
            streaming.active.as_ref().unwrap().turn_state(),
            TurnState::Streaming
        );
        assert!(streaming.row_turn("a").unwrap().items.is_none());

        let request: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "p", "asid": "s", "action": "bash", "resources": ["ls"],
            "prompt": "run ls", "options": [],
        }))
        .unwrap();
        let waiting = read(vec![request]);
        assert!(
            waiting.live().is_none(),
            "no cursor while an approval waits"
        );
        assert_eq!(
            waiting.active.as_ref().unwrap().turn_state(),
            TurnState::WaitingApproval(None)
        );
    }

    #[test]
    fn an_empty_resync_keeps_pending_approvals() {
        let mut view = ViewState::default();
        view.select("s");
        let request: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "p", "asid": "s", "action": "bash", "resources": ["ls"],
            "prompt": "run ls", "options": [],
        }))
        .unwrap();
        let snap = |permissions: Vec<PermissionRequest>| {
            Update::Snapshot(Box::new(AgentSessionSnapshot {
                info: info("s", 5),
                timeline: vec![item("a", "m1", 0)],
                permissions,
                forms: vec![],
                inbox: vec![],
                seq: 1,
            }))
        };
        view.apply(snap(vec![request.clone()]), &mut Changes::default());
        assert_eq!(view.active.as_ref().unwrap().pending_permissions.len(), 1);
        let mut changes = Changes::default();
        view.apply(snap(vec![]), &mut changes);
        assert_eq!(
            view.active.as_ref().unwrap().pending_permissions.len(),
            1,
            "empty resync must not wipe a waiting approval"
        );
        assert!(!changes.permissions);
        assert_eq!(view.attention.len(), 1);
    }

    #[test]
    fn a_turn_that_ends_names_the_rows_that_change() {
        let mut view = ViewState::default();
        view.select("s");
        let mut busy = info("s", 5);
        busy.status = AgentSessionStatus::Busy;
        let tool: muqun_gateway::agents::ToolCall = serde_json::from_value(serde_json::json!({
            "id": "c", "name": "bash", "input": {}, "state": "running", "status": "running"
        }))
        .unwrap();
        let items = vec![
            TimelineItem {
                role: TimelineRole::User,
                ..item("u", "m1", 0)
            },
            TimelineItem {
                part: AgentPart::Tool(tool),
                ..item("t", "m2", 0)
            },
        ];
        let snap = |info: AgentSessionInfo| {
            Update::Snapshot(Box::new(AgentSessionSnapshot {
                info,
                timeline: items.clone(),
                permissions: vec![],
                forms: vec![],
                inbox: vec![],
                seq: 1,
            }))
        };
        view.apply(snap(busy), &mut Changes::default());
        let mut changes = Changes::default();
        view.apply(snap(info("s", 6)), &mut changes);
        assert!(
            changes.items.contains(&"t".to_string()),
            "the stale tool and the summary bar's row render again: {:?}",
            changes.items
        );
    }

    #[test]
    fn a_created_session_is_listed_and_selected() {
        let mut view = ViewState::default();
        let mut changes = Changes::default();
        view.apply(
            Update::SessionCreated(Box::new(info("new", 9))),
            &mut changes,
        );
        assert_eq!(view.selected.as_deref(), Some("new"));
        assert!(changes.sessions && changes.selected);
    }

    #[test]
    fn a_session_list_clears_a_reachability_error() {
        let mut view = ViewState::default();
        let mut changes = Changes::default();
        view.apply(
            Update::Error(UiError {
                message: "opencode is not reachable".into(),
                detail: None,
                unreachable: true,
            }),
            &mut changes,
        );
        view.apply(
            Update::AgentSessions {
                agent: "opencode".into(),
                sessions: vec![],
            },
            &mut changes,
        );
        assert!(view.error.is_none());
    }
}
