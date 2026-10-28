use crate::agents::ports::agent::AgentError;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

pub fn validate_snapshot(detail: &Value, id: &str) -> Result<(), AgentError> {
    if detail
        .get("snapshotSequence")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(AgentError::Protocol("V2 snapshot without sequence".into()));
    }
    let projection = detail
        .get("projection")
        .ok_or_else(|| AgentError::Protocol("V2 snapshot without projection".into()))?;
    if projection.pointer("/thread/id").and_then(Value::as_str) != Some(id) {
        return Err(AgentError::Protocol(
            "V2 snapshot thread identity mismatch".into(),
        ));
    }
    for field in [
        "runs",
        "nodes",
        "providerSessions",
        "providerThreads",
        "runtimeRequests",
        "messages",
        "turnItems",
        "visibleTurnItems",
        "checkpointScopes",
        "checkpoints",
    ] {
        if projection.get(field).and_then(Value::as_array).is_none() {
            return Err(AgentError::Protocol(format!(
                "V2 projection missing {field}"
            )));
        }
    }
    for field in [
        "runs",
        "nodes",
        "providerSessions",
        "providerThreads",
        "runtimeRequests",
        "messages",
        "turnItems",
        "checkpointScopes",
        "checkpoints",
    ] {
        for entity in rows(projection, field) {
            if string(entity, "id").is_none() {
                return Err(AgentError::Protocol(format!("invalid V2 {field} entity")));
            }
            if field == "turnItems" {
                validate_turn_item(entity)?;
            }
        }
    }
    for row in rows(projection, "visibleTurnItems") {
        validate_visible_row(row)?;
    }
    Ok(())
}

fn validate_turn_item(item: &Value) -> Result<(), AgentError> {
    let kind = string(item, "type")
        .ok_or_else(|| AgentError::Protocol("turn item missing type".into()))?;
    let text = match kind {
        "user_message" | "assistant_message" | "reasoning" => Some("text"),
        "proposed_plan" => Some("markdown"),
        "command_execution" => Some("input"),
        "system_notice" | "run_interrupt_request" | "run_interrupt_result" => Some("message"),
        _ => None,
    };
    if text.is_some_and(|field| string(item, field).is_none())
        || (kind == "user_input_request"
            && item.get("questions").and_then(Value::as_array).is_none())
    {
        return Err(AgentError::Protocol("malformed known V2 turn item".into()));
    }
    Ok(())
}

fn known_turn_item(kind: &str) -> bool {
    matches!(
        kind,
        "notification"
            | "user_message"
            | "assistant_message"
            | "reasoning"
            | "proposed_plan"
            | "todo_list"
            | "user_input_request"
            | "file_change"
            | "command_execution"
            | "file_search"
            | "web_search"
            | "approval_request"
            | "checkpoint"
            | "run_interrupt_request"
            | "run_interrupt_result"
            | "system_notice"
            | "error"
            | "compaction"
            | "handoff"
            | "fork"
            | "thread_created"
            | "secret_request"
            | "subagent"
            | "dynamic_tool"
    )
}

fn validate_visible_row(row: &Value) -> Result<(), AgentError> {
    if row.get("position").and_then(Value::as_u64).is_none()
        || string(row, "sourceThreadId").is_none()
        || string(row, "sourceItemId").is_none()
        || row.pointer("/item/id").and_then(Value::as_str).is_none()
        || row.pointer("/item/type").and_then(Value::as_str).is_none()
    {
        return Err(AgentError::Protocol("invalid V2 visible row".into()));
    }
    validate_turn_item(&row["item"])?;
    Ok(())
}

/// Keep current entities on overlap: a historical page is not a live snapshot.
pub fn merge_history(detail: &mut Value, page: &Value) -> Result<(), AgentError> {
    let incoming = page
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Protocol("invalid V2 history page".into()))?;
    if page
        .get("snapshotSequence")
        .and_then(Value::as_u64)
        .is_none()
        || page
            .get("hasMoreHistory")
            .and_then(Value::as_bool)
            .is_none()
        || !(page
            .get("nextCursor")
            .is_some_and(|v| v.is_null() || v.is_string()))
    {
        return Err(AgentError::Protocol("invalid V2 history metadata".into()));
    }
    let held = detail
        .pointer_mut("/projection/visibleTurnItems")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AgentError::Protocol("invalid V2 timeline".into()))?;
    // The cursor proves this is the older prefix. Positions are page-local,
    // reset to zero by upstream, and cannot be compared between responses.
    let older = chronological_rows(incoming)?;
    let current = chronological_rows(held)?;
    let current_ids: HashSet<_> = current.iter().map(identity).collect::<Result<_, _>>()?;
    // Check shared identity order; a page cannot reverse current entities.
    let older_shared: Vec<_> = older
        .iter()
        .filter_map(|row| identity(row).ok())
        .filter(|id| current_ids.contains(id))
        .collect();
    let shared_set: HashSet<_> = older_shared.iter().cloned().collect();
    let current_shared: Vec<_> = current
        .iter()
        .filter_map(|row| identity(row).ok())
        .filter(|id| shared_set.contains(id))
        .collect();
    if older_shared != current_shared {
        return Err(AgentError::Protocol(
            "history page reverses shared identities".into(),
        ));
    }
    // Overlap is safe only where the older page's suffix meets the current
    // window's prefix. Crossing/interior spans contradict the cursor context.
    let shared_count = older_shared.len();
    if shared_count > 0
        && (older
            .iter()
            .rev()
            .take(shared_count)
            .map(identity)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            != older_shared
            || current
                .iter()
                .take(shared_count)
                .map(identity)
                .collect::<Result<Vec<_>, _>>()?
                != older_shared)
    {
        return Err(AgentError::Protocol(
            "ambiguous history page/window overlap".into(),
        ));
    }
    let mut merged: Vec<Value> = older
        .into_iter()
        .filter(|row| !current_ids.contains(&identity(row).expect("validated identity")))
        .collect();
    merged.extend(current);
    for (position, row) in merged.iter_mut().enumerate() {
        row["position"] = Value::from(position as u64);
    }
    *held = merged;
    // Wire bytes bound this walk. A row count is not end-of-history and must
    // not discard a valid page solely because messages contain many items.
    detail["historyCursor"] = page["nextCursor"].clone();
    detail["hasMoreHistory"] = page["hasMoreHistory"].clone();
    Ok(())
}

type RowIdentity = (String, String);

fn identity(row: &Value) -> Result<RowIdentity, AgentError> {
    Ok((
        string(row, "sourceThreadId")
            .ok_or_else(|| AgentError::Protocol("missing source thread".into()))?
            .into(),
        string(row, "sourceItemId")
            .ok_or_else(|| AgentError::Protocol("missing source item".into()))?
            .into(),
    ))
}

pub(crate) fn chronological_rows(rows: &[Value]) -> Result<Vec<Value>, AgentError> {
    let mut result = rows.to_vec();
    let mut ids = HashSet::new();
    let mut positions = HashSet::new();
    for row in &result {
        validate_visible_row(row)?;
        if !ids.insert(identity(row)?)
            || !positions.insert(row["position"].as_u64().expect("validated position"))
        {
            return Err(AgentError::Protocol(
                "duplicate V2 window identity or position".into(),
            ));
        }
    }
    result.sort_by_key(|row| row["position"].as_u64().expect("validated position"));
    Ok(result)
}

#[derive(Clone)]
struct OrderedRow {
    row: Option<Value>,
    label: u64,
    sequence: u64,
}

/// Source-identity stitching, shared by HTTP reads and streamed projections.
/// Stable labels leave room for older pages without changing held mirror keys.
/// Payloads prevent an older in-flight history read from replacing live text.
#[derive(Clone, Default)]
pub struct TimelineOrder {
    order: Vec<RowIdentity>,
    rows: HashMap<RowIdentity, OrderedRow>,
    latest_sequence: u64,
    // A complete projection is deletion authority for every absent identity.
    // One per-incarnation watermark replaces an unbounded tombstone list.
    complete_sequence: Option<u64>,
    complete_ids: HashSet<RowIdentity>,
    oldest_supported_sequence: u64,
}

impl TimelineOrder {
    pub fn canonical_row(&self, source_thread: &str, source_item: &str) -> Option<Value> {
        self.rows
            .get(&(source_thread.to_owned(), source_item.to_owned()))?
            .row
            .clone()
    }

    pub fn invalidate_before(&mut self, sequence: u64) {
        self.oldest_supported_sequence = self.oldest_supported_sequence.max(sequence);
    }

    pub fn install(&mut self, detail: &mut Value) -> Result<(), AgentError> {
        let sequence = detail["snapshotSequence"]
            .as_u64()
            .ok_or_else(|| AgentError::Protocol("missing sequence".into()))?;
        if sequence < self.oldest_supported_sequence {
            return Err(AgentError::HistoryCursorLost);
        }
        let mut incoming = chronological_rows(rows(&detail["projection"], "visibleTurnItems"))?;
        if self
            .complete_sequence
            .is_some_and(|complete| sequence <= complete)
        {
            incoming.retain(|row| {
                self.complete_ids
                    .contains(&identity(row).expect("validated identity"))
            });
        }
        let incoming_ids: Vec<_> = incoming.iter().map(identity).collect::<Result<_, _>>()?;
        let complete = detail["hasMoreHistory"] != true
            && detail["gatewayHistoricalPrefix"] != true
            && sequence >= self.latest_sequence;
        if complete {
            let retained: HashSet<_> = incoming_ids.iter().cloned().collect();
            self.order.retain(|id| retained.contains(id));
            self.rows.retain(|id, _| retained.contains(id));
            self.complete_ids = retained;
            self.complete_sequence = Some(sequence);
        }
        let overlaps: Vec<_> = incoming_ids
            .iter()
            .filter(|id| self.rows.contains_key(*id))
            .cloned()
            .collect();
        if !self.order.is_empty() && !incoming_ids.is_empty() && overlaps.is_empty() {
            return Err(AgentError::Protocol("V2 partial window has no identity overlap; load bridging history or a complete snapshot".into()));
        }
        let overlap_set: HashSet<_> = overlaps.iter().cloned().collect();
        let existing: Vec<_> = self
            .order
            .iter()
            .filter(|id| overlap_set.contains(*id))
            .cloned()
            .collect();
        if existing != overlaps {
            return Err(AgentError::Protocol(
                "V2 window reverses held identities".into(),
            ));
        }
        let mut start = 0;
        while start < incoming_ids.len() {
            if self.rows.contains_key(&incoming_ids[start]) {
                start += 1;
                continue;
            }
            let end = (start..incoming_ids.len())
                .find(|index| self.rows.contains_key(&incoming_ids[*index]))
                .unwrap_or(incoming_ids.len());
            let right = incoming_ids
                .get(end)
                .and_then(|id| self.order.iter().position(|held| held == id));
            let left = start.checked_sub(1).and_then(|index| {
                self.order
                    .iter()
                    .position(|held| held == &incoming_ids[index])
            });
            if left
                .zip(right)
                .is_some_and(|(left, right)| left + 1 != right)
                || (right.is_none() && left.is_some_and(|left| left + 1 != self.order.len()))
            {
                return Err(AgentError::Protocol(
                    "ambiguous V2 window insertion between held identities".into(),
                ));
            }
            let at = right.unwrap_or(self.order.len());
            let count = end - start;
            let previous = at
                .checked_sub(1)
                .map(|index| self.rows[&self.order[index]].label);
            let next = self.order.get(at).map(|id| self.rows[id].label);
            const GAP: u64 = 1 << 32;
            let (base, step) = match (previous, next) {
                (Some(left), Some(right)) => {
                    let step = (right - left) / (count as u64 + 1);
                    if step == 0 {
                        return Err(AgentError::Protocol(
                            "stable V2 order labels exhausted".into(),
                        ));
                    }
                    (left, step)
                }
                (None, Some(right)) => (
                    right.checked_sub(GAP * (count as u64 + 1)).ok_or_else(|| {
                        AgentError::Protocol("stable V2 prefix labels exhausted".into())
                    })?,
                    GAP,
                ),
                (Some(left), None) => (left, GAP),
                (None, None) => (1 << 63, GAP),
            };
            for (offset, index) in (start..end).enumerate() {
                let label = base
                    .checked_add(step * (offset as u64 + 1))
                    .ok_or_else(|| {
                        AgentError::Protocol("stable V2 suffix labels exhausted".into())
                    })?;
                let id = incoming_ids[index].clone();
                self.rows.insert(
                    id.clone(),
                    OrderedRow {
                        row: Some(incoming[index].clone()),
                        label,
                        sequence,
                    },
                );
                self.order.insert(at + offset, id);
            }
            start = end;
        }
        let mut normalized = Vec::with_capacity(incoming.len());
        for (id, row) in incoming_ids.iter().zip(incoming) {
            let held = self.rows.get_mut(id).expect("installed identity");
            if sequence >= held.sequence {
                held.row = Some(row);
                held.sequence = sequence;
            }
            // A compacted anchor still fixes order, but cannot invent a newer
            // payload for an old in-flight page. Keep the reader's old window.
            let mut row = held.row.clone().ok_or(AgentError::HistoryCursorLost)?;
            row["position"] = Value::from(held.label);
            normalized.push(row);
        }
        self.latest_sequence = self.latest_sequence.max(sequence);
        detail["projection"]["visibleTurnItems"] = Value::Array(normalized);
        // Historical callers may merge stale rows, but cannot carry full
        // deletion authority into a later mirror reconciliation.
        detail["gatewayCompleteCoverage"] = Value::Bool(complete);
        self.compact(&incoming_ids);
        Ok(())
    }

    fn compact(&mut self, incoming: &[RowIdentity]) {
        const WINDOW_ROWS: usize = 20_000;
        const WINDOW_BYTES: usize = 8 * 1024 * 1024;
        const EDGE_ANCHORS: usize = 128;
        if self.order.len() <= WINDOW_ROWS && self.retained_bytes() <= WINDOW_BYTES {
            return;
        }
        let incoming: HashSet<_> = incoming.iter().cloned().collect();
        let mut keep = incoming.clone();
        keep.extend(self.order.iter().take(EDGE_ANCHORS).cloned());
        keep.extend(self.order.iter().rev().take(EDGE_ANCHORS).cloned());
        self.order.retain(|id| keep.contains(id));
        self.rows.retain(|id, _| keep.contains(id));
        for (id, row) in &mut self.rows {
            if !incoming.contains(id) {
                row.row = None;
            }
        }
        // Current cursor reads can keep joining at the prefix. An older cut
        // whose canonical replacements were compacted must refresh instead of
        // replacing newer text or fabricating exhaustion.
        self.oldest_supported_sequence = self.latest_sequence;
    }

    pub fn is_stale(&self, detail: &Value) -> bool {
        detail["snapshotSequence"]
            .as_u64()
            .is_some_and(|sequence| sequence < self.latest_sequence)
    }

    pub fn retained_bytes(&self) -> usize {
        let payload = self
            .rows
            .values()
            .filter_map(|row| row.row.as_ref())
            .map(|row| serde_json::to_vec(row).map_or(usize::MAX, |bytes| bytes.len()))
            .fold(0, usize::saturating_add);
        self.rows
            .keys()
            .chain(self.complete_ids.iter())
            .fold(payload, |bytes, id| {
                bytes
                    .saturating_add(id.0.len())
                    .saturating_add(id.1.len())
                    .saturating_add(64)
            })
    }
}

pub fn active_root_run(projection: &Value) -> Option<&Value> {
    rows(projection, "runs")
        .iter()
        .filter(|run| {
            matches!(
                string(run, "status"),
                Some("starting" | "running" | "waiting")
            )
        })
        .filter(|run| {
            let Some(root) = string(run, "rootNodeId") else {
                return false;
            };
            rows(projection, "nodes").iter().any(|node| {
                string(node, "id") == Some(root)
                    && string(node, "kind") == Some("root_turn")
                    && node.get("parentNodeId").is_some_and(Value::is_null)
                    && string(node, "runId") == string(run, "id")
            })
        })
        .max_by_key(|run| run.get("ordinal").and_then(Value::as_u64).unwrap_or(0))
}

pub fn has_active_run(projection: &Value) -> bool {
    rows(projection, "runs").iter().any(|run| {
        matches!(
            string(run, "status"),
            Some("preparing" | "starting" | "running" | "waiting")
        )
    })
}

/// Runtime requests are actionable only with their request item and a matching
/// live provider identity, or the upstream's proven asynchronous question path.
pub fn pending_request<'a>(projection: &'a Value, id: &str) -> Option<(&'a Value, &'a Value)> {
    let request = rows(projection, "runtimeRequests")
        .iter()
        .find(|r| string(r, "id") == Some(id) && string(r, "status") == Some("pending"))?;
    if matches!(
        string(request, "kind"),
        Some("dynamic_tool_call" | "auth_refresh")
    ) {
        return None;
    }
    let item = rows(projection, "turnItems")
        .iter()
        .chain(
            rows(projection, "visibleTurnItems")
                .iter()
                .filter_map(|row| row.get("item")),
        )
        .find(|item| {
            string(item, "requestId") == Some(id)
                && string(item, "nodeId") == string(request, "nodeId")
                && matches!(
                    string(item, "status"),
                    Some("pending" | "running" | "waiting")
                )
                && matches!(
                    string(item, "type"),
                    Some("approval_request" | "user_input_request")
                )
        })?;
    let capability = request.get("responseCapability")?;
    match string(capability, "type")? {
        "message"
            if string(request, "kind") == Some("user_input")
                && string(item, "type") == Some("user_input_request") => {}
        "live" => {
            let session_id = string(capability, "providerSessionId")?;
            let session = rows(projection, "providerSessions").iter().find(|s| {
                string(s, "id") == Some(session_id)
                    && matches!(
                        string(s, "status"),
                        Some("starting" | "ready" | "running" | "waiting")
                    )
            })?;
            let node = rows(projection, "nodes")
                .iter()
                .find(|n| string(n, "id") == string(request, "nodeId"))?;
            let provider_thread = rows(projection, "providerThreads")
                .iter()
                .find(|t| string(t, "id") == string(node, "providerThreadId"))?;
            if let Some(turn_id) = string(request, "providerTurnId") {
                let turn = rows(projection, "providerTurns")
                    .iter()
                    .find(|turn| string(turn, "id") == Some(turn_id))?;
                if string(turn, "providerThreadId") != string(node, "providerThreadId")
                    || !matches!(string(turn, "status"), Some("pending" | "running"))
                {
                    return None;
                }
            }
            if string(provider_thread, "providerSessionId") != Some(session_id)
                || string(provider_thread, "providerInstanceId")
                    != string(session, "providerInstanceId")
            {
                return None;
            }
        }
        _ => return None,
    }
    Some((request, item))
}

/// Known entities must decode before their sequence can be installed. Unknown
/// future event types can be skipped, as the released upstream decoder does.
pub fn validate_event(item: &Value, thread_id: &str) -> Result<bool, AgentError> {
    let event = item
        .get("event")
        .ok_or_else(|| AgentError::Protocol("V2 event missing envelope".into()))?;
    let kind = string(event, "type")
        .ok_or_else(|| AgentError::Protocol("V2 event missing type".into()))?;
    let known = matches!(
        kind,
        "thread.created"
            | "thread.archived"
            | "thread.unarchived"
            | "thread.deleted"
            | "thread.settled"
            | "thread.unsettled"
            | "thread.snoozed"
            | "thread.unsnoozed"
            | "thread.pinned"
            | "thread.auto-settle-set"
            | "thread.unpinned"
            | "thread.pin-reordered"
            | "thread.active-reordered"
            | "thread.visited"
            | "thread.marked-unread"
            | "thread.pull-request-synced"
            // v0.1.0: a thread's pull-request link changed. Nothing here
            // shows it; accepted so it is not skipped as an unknown type.
            | "thread.pull-request-link-updated"
            | "thread.metadata-updated"
            | "thread.runtime-mode-updated"
            | "thread.interaction-mode-updated"
            | "thread.model-selection-updated"
            | "thread.provider-switched"
    ) || matches!(
        kind,
        "run.created"
            | "run.updated"
            | "run.background-work-cancelled"
            | "run-attempt.created"
            | "run-attempt.updated"
            | "node.updated"
            | "subagent.updated"
            | "provider-session.attached"
            | "provider-session.updated"
            | "provider-session.detached"
            | "provider-thread.updated"
            | "provider-turn.updated"
            | "runtime-request.updated"
            | "message.updated"
            | "turn-item.updated"
            | "plan.updated"
            | "checkpoint-scope.created"
            | "checkpoint.captured"
            | "checkpoint.rollback-requested"
            | "context-handoff.updated"
            | "context-transfer.created"
            | "context-transfer.updated"
    );
    if item.get("sequence").and_then(Value::as_u64).is_none() {
        return Err(AgentError::Protocol(
            "V2 event missing outer sequence".into(),
        ));
    }
    if !known {
        return Ok(false);
    }
    if string(event, "threadId") != Some(thread_id)
        || !event.get("payload").is_some_and(Value::is_object)
    {
        return Err(AgentError::Protocol("invalid known V2 event".into()));
    }
    let payload = &event["payload"];
    if kind == "turn-item.updated" {
        if string(payload, "type").is_some_and(|kind| !known_turn_item(kind)) {
            return Ok(false);
        }
        validate_turn_item(payload)?;
    }
    let required: &[&str] = match kind {
        "message.updated" => &["id", "threadId", "role", "text", "createdAt", "updatedAt"],
        "turn-item.updated" => &["id", "threadId", "type", "status", "updatedAt"],
        "run.created" | "run.updated" => &[
            "id",
            "threadId",
            "status",
            "providerInstanceId",
            "userMessageId",
        ],
        "runtime-request.updated" => &["id", "nodeId", "kind", "status"],
        "checkpoint.rollback-requested" => &["scopeId", "checkpointId", "requestedAt"],
        "provider-session.detached" => &[],
        "run.background-work-cancelled" => &[],
        // v0.1.0: the link is the payload; there is no entity id to require.
        "thread.pull-request-link-updated" => &[],
        _ => &["id"],
    };
    if required
        .iter()
        .any(|field| string(payload, field).is_none())
    {
        return Err(AgentError::Protocol("malformed known V2 entity".into()));
    }
    if matches!(kind, "message.updated" | "turn-item.updated")
        && (string(payload, "id").is_none()
            || (kind == "message.updated" && string(payload, "text").is_none())
            || (kind == "turn-item.updated" && string(payload, "type").is_none()))
    {
        return Err(AgentError::Protocol("invalid V2 entity update".into()));
    }
    Ok(true)
}
