//! Where a turn is, for the screen (architecture §6.0‴ C, checklist item 4).
//!
//! A *turn* is a user message and everything after it up to the next user
//! message. Only the newest turn can be live; every earlier one is `Done`.
//! The states are mutually exclusive, and every renderer reads this one
//! answer instead of guessing from the session status: the streaming cursor
//! exists only in `Streaming`, a tool row says "waiting for approval" only in
//! `WaitingApproval`, and the summary bar appears only once the turn is
//! `Done` or `Failed`.

// Covered by unit tests; timeline UI wiring lands next.
#![allow(dead_code)]

use std::ops::Range;
use std::sync::Arc;

use muqun_gateway::agents::{
    AgentPart, AgentSessionStatus, FormRequest, PermissionRequest, TimelineItem, TimelineRole,
    ToolCallStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    /// The agent is writing, or about to.
    Streaming,
    /// A tool is running (its name).
    Running(String),
    /// An approval is pending; the tool item it is for, when it can be told.
    WaitingApproval(Option<String>),
    WaitingForm,
    Done,
    Failed,
}

impl TurnState {
    pub fn finished(&self) -> bool {
        matches!(self, TurnState::Done | TurnState::Failed)
    }
}

/// What one timeline row needs to know about its turn.
#[derive(Debug, Clone, PartialEq)]
pub struct RowTurn {
    pub state: TurnState,
    /// The item is the last of its turn.
    pub tail: bool,
    /// The turn's items, when the row is the tail of a finished turn (the
    /// summary bar is made from them).
    pub items: Option<Vec<Arc<TimelineItem>>>,
}

impl RowTurn {
    /// Whether tool item `id` is the one an approval is waiting on.
    pub fn waiting_on(&self, id: &str) -> bool {
        matches!(&self.state, TurnState::WaitingApproval(Some(t)) if t == id)
    }
}

fn starts_turn(item: &TimelineItem) -> bool {
    item.role == TimelineRole::User && matches!(item.part, AgentPart::Text { .. })
}

/// The turn that holds `items[at]`.
pub fn range_of(items: &[Arc<TimelineItem>], at: usize) -> Range<usize> {
    let start = (0..=at)
        .rev()
        .find(|&i| starts_turn(&items[i]))
        .unwrap_or(0);
    let end = (at + 1..items.len())
        .find(|&i| starts_turn(&items[i]))
        .unwrap_or(items.len());
    start..end
}

/// A tool (or shell run) that has not finished: its name.
pub fn unfinished(item: &TimelineItem) -> Option<&str> {
    match &item.part {
        AgentPart::Tool(call)
            if matches!(
                call.state,
                ToolCallStatus::Pending | ToolCallStatus::Streaming | ToolCallStatus::Running
            ) =>
        {
            Some(call.name.as_str())
        }
        AgentPart::Shell { status, exit, .. }
            if exit.is_none() && matches!(status.as_str(), "running" | "pending" | "started") =>
        {
            Some("shell")
        }
        _ => None,
    }
}

/// The item of the tool `request` is for: the call it names, else the
/// newest unfinished call of the tool it names, else the newest unfinished
/// call.
pub fn waiting_tool(items: &[Arc<TimelineItem>], request: &PermissionRequest) -> Option<String> {
    if let Some(call) = request.source_tool_call_id.as_deref() {
        let found = items
            .iter()
            .rev()
            .find(|i| matches!(&i.part, AgentPart::Tool(c) if c.id == call));
        if let Some(item) = found {
            return Some(item.id.clone());
        }
    }
    let named = request.tool.as_deref();
    items
        .iter()
        .rev()
        .find(|i| unfinished(i).is_some_and(|name| Some(name) == named))
        .or_else(|| items.iter().rev().find(|i| unfinished(i).is_some()))
        .map(|i| i.id.clone())
}

/// The state of the turn made of `items`. `status` is the session's, given
/// only for the newest turn (an older turn is finished by definition).
pub fn derive(
    items: &[Arc<TimelineItem>],
    status: Option<AgentSessionStatus>,
    approvals: &[PermissionRequest],
    forms: &[FormRequest],
) -> TurnState {
    let Some(status) = status else {
        return TurnState::Done;
    };
    if status == AgentSessionStatus::Failed {
        return TurnState::Failed;
    }
    if let Some(request) = approvals.first() {
        return TurnState::WaitingApproval(waiting_tool(items, request));
    }
    if !forms.is_empty() {
        return TurnState::WaitingForm;
    }
    if !matches!(status, AgentSessionStatus::Busy | AgentSessionStatus::Retry) {
        return TurnState::Done;
    }
    match items.iter().rev().find_map(|i| unfinished(i)) {
        Some(name) => TurnState::Running(name.to_string()),
        None => TurnState::Streaming,
    }
}

/// The row of `items[at]`'s view of its turn.
pub fn row_turn(
    items: &[Arc<TimelineItem>],
    at: usize,
    status: AgentSessionStatus,
    approvals: &[PermissionRequest],
    forms: &[FormRequest],
) -> RowTurn {
    let range = range_of(items, at);
    let newest = range.end == items.len();
    let turn = &items[range.clone()];
    let state = if newest {
        derive(turn, Some(status), approvals, forms)
    } else {
        TurnState::Done
    };
    let tail = at + 1 == range.end;
    let items = (tail && state.finished()).then(|| turn.to_vec());
    RowTurn { state, tail, items }
}

/// What the newest turn looked like, to find the rows that must render
/// again when it changes: the old and new last item (the summary bar moves
/// with it) and the tools whose bar depends on the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    pub asid: String,
    pub first: Option<String>,
    pub last: Option<String>,
    pub state: TurnState,
    /// Newest `seq` in the turn once finished: a late item changes the bar.
    pub seq: u64,
    pub unfinished: Vec<String>,
    /// The turn did work (a tool, shell run or diff): it gets a summary bar.
    pub worked: bool,
    /// Approval and form items: their cards read what is pending.
    pub asking: Vec<String>,
}

impl Snap {
    /// The item ids whose rows depend on this snapshot.
    pub fn touched(&self) -> Vec<String> {
        let mut ids = self.unfinished.clone();
        ids.extend(self.asking.clone());
        if self.state.finished() && self.worked {
            ids.extend(self.last.clone());
        }
        if let TurnState::WaitingApproval(Some(tool)) = &self.state {
            ids.push(tool.clone());
        }
        ids
    }
}

pub fn snap(
    asid: &str,
    items: &[Arc<TimelineItem>],
    status: AgentSessionStatus,
    approvals: &[PermissionRequest],
    forms: &[FormRequest],
) -> Snap {
    let range = match items.len() {
        0 => 0..0,
        n => range_of(items, n - 1),
    };
    let turn = &items[range];
    let state = derive(turn, Some(status), approvals, forms);
    Snap {
        asid: asid.to_string(),
        first: turn.first().map(|i| i.id.clone()),
        last: turn.last().map(|i| i.id.clone()),
        seq: if state.finished() {
            turn.iter().map(|i| i.seq).max().unwrap_or(0)
        } else {
            0
        },
        unfinished: turn
            .iter()
            .filter(|i| unfinished(i).is_some())
            .map(|i| i.id.clone())
            .collect(),
        worked: turn.iter().any(|i| {
            i.role != TimelineRole::User
                && matches!(
                    i.part,
                    AgentPart::Tool(_) | AgentPart::Shell { .. } | AgentPart::Diff { .. }
                )
        }),
        asking: turn
            .iter()
            .filter(|i| matches!(i.part, AgentPart::Approval { .. } | AgentPart::Form { .. }))
            .map(|i| i.id.clone())
            .collect(),
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::ToolCall;
    use serde_json::json;

    fn item(id: &str, role: TimelineRole, part: AgentPart) -> Arc<TimelineItem> {
        Arc::new(TimelineItem {
            id: id.into(),
            message_id: "m".into(),
            role,
            part,
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        })
    }

    fn user(id: &str) -> Arc<TimelineItem> {
        item(
            id,
            TimelineRole::User,
            AgentPart::Text { text: "go".into() },
        )
    }

    fn text(id: &str) -> Arc<TimelineItem> {
        item(
            id,
            TimelineRole::Assistant,
            AgentPart::Text { text: "ok".into() },
        )
    }

    fn tool(id: &str, call: &str, name: &str, state: &str) -> Arc<TimelineItem> {
        let call: ToolCall = serde_json::from_value(json!({
            "id": call, "name": name, "input": {}, "state": state, "status": state
        }))
        .unwrap();
        item(id, TimelineRole::Assistant, AgentPart::Tool(call))
    }

    fn request(call: Option<&str>, tool: Option<&str>) -> PermissionRequest {
        serde_json::from_value(json!({
            "id": "p1", "asid": "s", "action": "bash", "resources": ["ls"],
            "prompt": "run ls", "options": [],
            "tool": tool, "source_tool_call_id": call,
        }))
        .unwrap()
    }

    const BUSY: Option<AgentSessionStatus> = Some(AgentSessionStatus::Busy);

    #[test]
    fn a_busy_turn_with_nothing_else_is_streaming() {
        let items = [user("u"), text("a")];
        assert_eq!(derive(&items, BUSY, &[], &[]), TurnState::Streaming);
    }

    #[test]
    fn a_running_tool_names_itself() {
        let items = [user("u"), tool("t", "c1", "bash", "running")];
        assert_eq!(
            derive(&items, BUSY, &[], &[]),
            TurnState::Running("bash".into())
        );
    }

    #[test]
    fn a_pending_approval_beats_running_and_finds_its_tool() {
        let items = [
            user("u"),
            tool("t1", "c1", "read", "running"),
            tool("t2", "c2", "bash", "running"),
        ];
        let by_call = request(Some("c1"), Some("bash"));
        assert_eq!(
            derive(&items, BUSY, &[by_call], &[]),
            TurnState::WaitingApproval(Some("t1".into()))
        );
        let by_name = request(None, Some("bash"));
        assert_eq!(
            derive(&items, BUSY, &[by_name], &[]),
            TurnState::WaitingApproval(Some("t2".into()))
        );
        let nameless = request(None, None);
        assert_eq!(
            derive(&items, BUSY, &[nameless], &[]),
            TurnState::WaitingApproval(Some("t2".into()))
        );
    }

    #[test]
    fn an_approval_with_no_tool_waits_on_nothing_in_particular() {
        let items = [user("u"), text("a")];
        assert_eq!(
            derive(&items, BUSY, &[request(None, None)], &[]),
            TurnState::WaitingApproval(None)
        );
    }

    #[test]
    fn idle_is_done_and_failed_is_failed() {
        let items = [user("u"), tool("t", "c", "bash", "running")];
        for status in [
            AgentSessionStatus::Idle,
            AgentSessionStatus::Interrupted,
            AgentSessionStatus::Unknown,
        ] {
            assert_eq!(derive(&items, Some(status), &[], &[]), TurnState::Done);
        }
        assert_eq!(
            derive(&items, Some(AgentSessionStatus::Failed), &[], &[]),
            TurnState::Failed
        );
    }

    #[test]
    fn only_the_newest_turn_is_live() {
        let items = [user("u1"), text("a1"), user("u2"), text("a2")];
        let old = row_turn(&items, 1, AgentSessionStatus::Busy, &[], &[]);
        assert_eq!(old.state, TurnState::Done);
        assert!(old.tail);
        assert!(old.items.is_some());
        let new = row_turn(&items, 3, AgentSessionStatus::Busy, &[], &[]);
        assert_eq!(new.state, TurnState::Streaming);
        assert!(new.items.is_none(), "no summary until the turn is done");
    }

    #[test]
    fn the_summary_belongs_to_the_last_item_only() {
        let items = [user("u"), text("a"), tool("t", "c", "bash", "completed")];
        let first = row_turn(&items, 1, AgentSessionStatus::Idle, &[], &[]);
        assert!(!first.tail && first.items.is_none());
        let last = row_turn(&items, 2, AgentSessionStatus::Idle, &[], &[]);
        assert!(last.tail);
        assert_eq!(last.items.map(|i| i.len()), Some(3));
    }

    #[test]
    fn a_snapshot_changes_when_the_turn_does() {
        let items = [user("u"), text("a")];
        let before = snap("s", &items, AgentSessionStatus::Busy, &[], &[]);
        let after = snap("s", &items, AgentSessionStatus::Idle, &[], &[]);
        assert_ne!(before, after);
        assert_eq!(after.state, TurnState::Done);
        assert!(
            !after.touched().contains(&"a".to_string()),
            "a plain reply has no summary bar to draw"
        );
        let worked = snap(
            "s",
            &[user("u"), tool("t", "c", "bash", "completed")],
            AgentSessionStatus::Idle,
            &[],
            &[],
        );
        assert!(worked.touched().contains(&"t".to_string()));
        let waiting = snap(
            "s",
            &[user("u"), tool("t", "c", "bash", "running")],
            AgentSessionStatus::Busy,
            &[request(Some("c"), None)],
            &[],
        );
        assert!(waiting.touched().contains(&"t".to_string()));
    }
}
