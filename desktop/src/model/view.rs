//! The UI's model of the sessions, owned by the main thread.
//!
//! [`ViewState`] is plain data: [`ViewState::apply`] folds one [`Update`] in
//! and reports, in [`Changes`], which parts moved -- so it is testable
//! without a window. [`Bindings`] are the keyed Blinc `State` handles the
//! views depend on; `drain.rs` writes only the ones `Changes` names, so a
//! streaming delta to one item rebuilds that item's view and nothing else.

use std::collections::HashSet;
use std::sync::Arc;

use blinc_core::State;
use muqun_gateway::agents::directories::DirectoryEntry;
use muqun_gateway::agents::{
    AgentDomainEvent, AgentProject, AgentSessionInfo, AgentSessionStatus, PermissionRequest,
    TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;

use super::inbox::{UiAction, UiError, Update};

/// How many sessions the sidebar lists. P0 has no paging.
pub const SESSION_LIST_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub struct SessionView {
    pub info: AgentSessionInfo,
    pub items: Vec<Arc<TimelineItem>>,
    pub pending_permissions: Vec<PermissionRequest>,
}

impl SessionView {
    pub fn status(&self) -> AgentSessionStatus {
        self.info.status
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    /// Replace the item with `item`'s id, or insert it where its
    /// [`TimelineItem::sort_key`] puts it. True when the order changed.
    fn upsert(&mut self, item: TimelineItem) -> bool {
        if let Some(at) = self.position(&item.id) {
            self.items[at] = Arc::new(item);
            return false;
        }
        let at = self
            .items
            .partition_point(|existing| existing.sort_key() <= item.sort_key());
        self.items.insert(at, Arc::new(item));
        true
    }
}

/// What a session row and the session header show, for change detection.
/// `updated_ms` is in it only through the list's order.
fn shown(
    s: &AgentSessionInfo,
) -> (
    &str,
    &str,
    &str,
    AgentSessionStatus,
    Option<&str>,
    Option<&str>,
) {
    (
        s.asid.0.as_str(),
        s.agent_id.as_str(),
        s.title.as_str(),
        s.status,
        s.directory.as_deref(),
        s.model.as_ref().map(|m| m.model_id.as_str()),
    )
}

/// The timeline's rows, by id, for the list view to key its item views on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineOrder {
    pub asid: String,
    pub ids: Vec<String>,
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
    pub projects: Vec<AgentProject>,
    pub completions: Vec<DirectoryEntry>,
    /// The prefix the shown completions answer.
    pub completion_prefix: String,
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
    pub permissions: bool,
    pub error: bool,
    pub agents: bool,
    pub attention: bool,
    pub projects: bool,
    pub completions: bool,
    /// A different session (or a fresh snapshot) is now shown.
    pub opened: bool,
    pub actions: Vec<UiAction>,
}

impl Changes {
    pub fn timeline(&self) -> bool {
        self.order || !self.items.is_empty() || self.permissions || self.opened
    }
}

impl ViewState {
    /// The session the user picked, before its snapshot arrives.
    pub fn select(&mut self, asid: &str) -> bool {
        if self.selected.as_deref() == Some(asid) {
            return false;
        }
        self.selected = Some(asid.to_string());
        true
    }

    pub fn apply(&mut self, update: Update, changes: &mut Changes) {
        match update {
            Update::Agents(agents) => {
                self.agents = agents;
                changes.agents = true;
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
                self.sessions.retain(|s| s.agent_id != agent);
                self.sessions.extend(sessions);
                self.sort_sessions();
                changes.sessions = true;
                if self.error.as_ref().is_some_and(|e| e.unreachable) {
                    // A list that arrived answers "OpenCode is not reachable".
                    self.error = None;
                    changes.error = true;
                }
            }
            Update::SessionCreated(info) => {
                let asid = info.asid.0.clone();
                self.upsert_session(info, changes);
                changes.selected |= self.select(&asid);
            }
            Update::Snapshot(snapshot) => {
                if self.selected.as_deref() != Some(snapshot.info.asid.0.as_str()) {
                    return;
                }
                self.upsert_session(snapshot.info.clone(), changes);
                let pending_permissions = snapshot.permissions.clone();
                let asid = snapshot.info.asid.clone();
                self.attention.retain(|p| p.asid != asid);
                self.attention.extend(pending_permissions.iter().cloned());
                changes.attention = true;
                let snapshot = *snapshot;
                self.active = Some(SessionView {
                    info: snapshot.info,
                    items: snapshot.timeline.into_iter().map(Arc::new).collect(),
                    pending_permissions,
                });
                let active = self.active.as_ref().expect("just set");
                changes.items = active.items.iter().map(|i| i.id.clone()).collect();
                changes.order = true;
                changes.header = true;
                changes.permissions = true;
                changes.opened = true;
            }
            Update::Event(event) => self.apply_event(event, changes),
            Update::Error(error) => {
                self.error = Some(error);
                changes.error = true;
            }
            // Routed past the view by the drain (`drain.rs` `apply`).
            Update::Theme(_) => {}
            Update::Projects { projects, .. } => {
                self.projects = projects;
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
        }
    }

    pub fn dismiss_error(&mut self) -> bool {
        self.error.take().is_some()
    }

    /// Back to Home: no session selected.
    pub fn deselect(&mut self) -> bool {
        self.active = None;
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
            AgentDomainEvent::TimelineUpsert { asid, items, .. } => {
                let Some(active) = self.active_for(&asid.0) else {
                    return;
                };
                for item in items {
                    let id = item.id.clone();
                    changes.order |= active.upsert(item);
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
                let before = active.items.len();
                active.items.retain(|item| !gone.contains(&item.id));
                changes.order |= active.items.len() != before;
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
            // Forms, compaction, revert, inbox and worktrees are P1; a
            // resync is answered on the core side with a fresh snapshot.
            _ => {}
        }
    }

    pub fn order(&self) -> TimelineOrder {
        self.active
            .as_ref()
            .map(|active| TimelineOrder {
                asid: active.info.asid.0.clone(),
                ids: active.items.iter().map(|i| i.id.clone()).collect(),
            })
            .unwrap_or_default()
    }

    pub fn item(&self, id: &str) -> Option<Arc<TimelineItem>> {
        let active = self.active.as_ref()?;
        active.position(id).map(|at| active.items[at].clone())
    }
}

/// The keyed Blinc state the views depend on. Created once, on the main
/// thread, after Blinc's context exists (the first `build_ui`).
#[derive(Clone)]
pub struct Bindings {
    pub sessions: State<Arc<Vec<AgentSessionInfo>>>,
    pub selected: State<Option<String>>,
    pub header: State<Option<Arc<AgentSessionInfo>>>,
    pub order: State<Arc<TimelineOrder>>,
    pub permissions: State<Arc<Vec<PermissionRequest>>>,
    pub error: State<Option<UiError>>,
    pub agents: State<Arc<Vec<AgentDiscoveryInfo>>>,
    pub attention: State<Arc<Vec<PermissionRequest>>>,
    /// The agent the sidebar is filtered to; `None` is "All". UI-owned.
    pub filter: State<Option<String>>,
    /// Whether the sidebar's Agents panel is open. UI-owned.
    pub agents_open: State<bool>,
    /// The New-session page, while it is open. UI-owned.
    pub draft: State<Option<NewSessionDraft>>,
    pub projects: State<Arc<Vec<AgentProject>>>,
    pub completions: State<Arc<Vec<DirectoryEntry>>>,
    pub timeline_scroll: blinc_layout::selector::ScrollRef,
}

impl Bindings {
    pub fn new(timeline_scroll: blinc_layout::selector::ScrollRef) -> Self {
        let ctx = blinc_core::BlincContextState::get();
        Self {
            sessions: ctx.use_state_keyed("mq.sessions", || Arc::new(Vec::new())),
            selected: ctx.use_state_keyed("mq.selected", || None),
            header: ctx.use_state_keyed("mq.header", || None),
            order: ctx.use_state_keyed("mq.order", || Arc::new(TimelineOrder::default())),
            permissions: ctx.use_state_keyed("mq.permissions", || Arc::new(Vec::new())),
            error: ctx.use_state_keyed("mq.error", || None),
            agents: ctx.use_state_keyed("mq.agents", || Arc::new(Vec::new())),
            attention: ctx.use_state_keyed("mq.attention", || Arc::new(Vec::new())),
            filter: ctx.use_state_keyed("mq.filter", || None),
            agents_open: ctx.use_state_keyed("mq.agents_open", || true),
            draft: ctx.use_state_keyed("mq.draft", || None),
            projects: ctx.use_state_keyed("mq.projects", || Arc::new(Vec::new())),
            completions: ctx.use_state_keyed("mq.completions", || Arc::new(Vec::new())),
            timeline_scroll,
        }
    }
}

/// The New-session page's choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionDraft {
    pub agent: String,
    pub cwd: std::path::PathBuf,
}

/// One timeline item's state, keyed by session and item so its view can
/// find it again on every rebuild of the list.
pub fn item_state(asid: &str, id: &str) -> State<Option<Arc<TimelineItem>>> {
    blinc_core::BlincContextState::get().use_state_keyed(&format!("mq.item.{asid}.{id}"), || None)
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
            Update::Event(AgentDomainEvent::TimelineUpsert {
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
            Update::Event(AgentDomainEvent::TimelineUpsert {
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
            Update::Event(AgentDomainEvent::TimelineUpsert {
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
            Update::Event(AgentDomainEvent::StatusChanged {
                asid: AgentSessionId("s".into()),
                status: AgentSessionStatus::Busy,
                error: None,
                seq: 2,
            }),
            &mut changes,
        );
        assert!(changes.sessions && changes.header);
        assert_eq!(view.sessions[0].status, AgentSessionStatus::Busy);
        assert_eq!(view.active.unwrap().status(), AgentSessionStatus::Busy);
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
            Update::Event(AgentDomainEvent::PermissionPending {
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
            Update::Event(AgentDomainEvent::PermissionResolved {
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
            Update::Event(AgentDomainEvent::SessionUpdated {
                asid: AgentSessionId("a".into()),
                info: Box::new(info("a", 6)),
                seq: 1,
            }),
            &mut changes,
        );
        assert!(!changes.sessions, "same order, same row");
        view.apply(
            Update::Event(AgentDomainEvent::SessionUpdated {
                asid: AgentSessionId("b".into()),
                info: Box::new(info("b", 9)),
                seq: 1,
            }),
            &mut changes,
        );
        assert!(changes.sessions, "a reorder is a change");
    }

    #[test]
    fn a_created_session_is_listed_and_selected() {
        let mut view = ViewState::default();
        let mut changes = Changes::default();
        view.apply(Update::SessionCreated(info("new", 9)), &mut changes);
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
