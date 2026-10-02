//! Terminal plane HTTP routes and event streaming: sessions, workspaces, tabs,
//! panes, pane output/parts/git views, prompt delivery, and the SSE hub.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Json, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::{Stream, StreamExt as _};

use crate::backend::{
    Agent, AgentStatus as BackendAgentStatus, BackendActivity, BackendError, TerminalBackend,
};
use crate::platform::assets::{
    asset_created_payload, ingest_roots, is_scannable_root, read_asset_head, session_asset_roots,
    sniff_asset_type, worktree_event_removed_root, worktree_event_root, ASSET_EVENT_MAX_AGE_MS,
    MAX_ASSET_EVENTS_PER_WORKTREE,
};
use crate::platform::i18n::Locale;
use crate::{
    agents, api_error, approvals, backend, backend_api_error, backend_endpoint, composer,
    content_envelope, current_server_label, find_session, generate_token, git, i18n, native,
    now_unix_ms, ordered_sessions, parts, require_device, scrollback, send_expo_push_notifications,
    shortcuts, still_paired, tasks, terminal_backend, transport, validate_text, AgentNotice,
    AgentPushNotice, ApiResult, AppState, BackendCreateTab, BackendCreateWorkspace,
    BackendOutputFormat, BackendOutputSource, BackendPaneId, BackendReadPane, BackendSendTextMode,
    BackendSplitDirection, BackendSplitPane, BackendTabId, BackendWorkspaceId,
    EncryptedStreamContext, PushDetail, PushTokenRecord, SessionConfig, ACTIVITY_EVENT_CAPACITY,
    APPROVAL_POLL_INTERVAL, APPROVAL_READ_LINES, MAX_OUTPUT_LINES, MAX_SEND_KEYS,
    PARTS_DEFAULT_LINES, STREAM_DEVICE_RECHECK_INTERVAL, STREAM_OUTPUT_POLL_INTERVAL,
    STREAM_OUTPUT_READ_TIMEOUT,
};

type GatewayEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;

/// Terminal plane routes: sessions, workspaces, tabs, panes, and their views.
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{session_id}/events", get(events))
        .route("/api/sessions/{session_id}/snapshot", get(snapshot))
        .route(
            "/api/sessions/{session_id}/workspaces",
            get(workspaces).post(create_workspace),
        )
        .route(
            "/api/sessions/{session_id}/workspaces/{workspace_id}/focus",
            post(focus_workspace),
        )
        .route(
            "/api/sessions/{session_id}/workspaces/{workspace_id}",
            patch(rename_workspace).delete(close_workspace),
        )
        .route(
            "/api/sessions/{session_id}/tabs",
            get(tabs).post(create_tab),
        )
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}/focus",
            post(focus_tab),
        )
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}",
            patch(rename_tab).delete(close_tab),
        )
        .route("/api/keymaps", get(keymaps))
        .route("/api/sessions/{session_id}/recent-cwds", get(recent_cwds))
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/interrupt",
            post(interrupt_pane),
        )
        .route("/api/sessions/{session_id}/panes", get(panes))
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}",
            get(pane).patch(rename_pane).delete(close_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/focus",
            post(focus_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/split",
            post(split_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/zoom",
            post(zoom_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/shortcuts",
            get(pane_shortcuts),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/output",
            get(pane_output),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/parts",
            get(pane_parts),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/files",
            get(pane_files),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/context",
            get(pane_context),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/git/status",
            get(pane_git_status),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/git/diff",
            get(pane_git_diff),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/send-text",
            post(send_text),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/send-keys",
            post(send_keys),
        )
}

pub(crate) async fn sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let sessions = ordered_sessions(&state).await;
    let reachable = state
        .session_liveness
        .lock()
        .ok()
        .map(|cache| cache.reachable.clone())
        .unwrap_or_default();
    let sessions: Vec<Value> = sessions
        .into_iter()
        .map(|session| {
            let mut value = json!(session);
            if let Some(connected) = reachable.get(&session.id) {
                value["connected"] = json!(connected);
            }
            value
        })
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

pub(crate) async fn snapshot(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    // Four reads of the same backend that do not depend on each other. Run in
    // sequence this was four round trips deep -- and on tmux each one is a
    // process -- for an answer the client waits on before it can draw
    // anything. The backend is the same one either way; this only stops
    // waiting for each before starting the next.
    //
    // `agents` is the same list `GET .../agents` answers with, so a client
    // prewarming from the snapshot does not have to ask twice for
    // `instance_id` and `target`.
    let (workspaces, tabs, panes, agents) = tokio::join!(
        backend.list_workspaces(),
        backend.list_tabs(),
        backend.list_panes(),
        backend_agents(session),
    );
    let workspaces = workspaces.map_err(backend_api_error)?;
    let tabs = tabs.map_err(backend_api_error)?;
    let panes = panes.map_err(backend_api_error)?;
    let agents = agents.map_err(backend_api_error)?;
    let answer = backend::compat::snapshot(workspaces, tabs, panes, &agents);
    // Hashed after `note_and_amend_panes`, never before: that call is a read
    // that also writes -- it feeds the scrollback store what it just saw and
    // then amends the answer from what the store holds. Hashing the answer it
    // returns is the only version that matches what the client receives, and
    // running it before the 304 check keeps the store fed even when nothing is
    // sent back.
    let answer = note_and_amend_panes(&state, &session_id, answer);
    Ok(agents::routes::json_etag_response(&headers, answer))
}

/// Let the scrollback store read a Herdr answer, and answer back for whatever
/// it holds.
///
/// Every pane entity the gateway hands out goes through here, because the
/// reader's pull-for-earlier is gated on the pane's `scroll`, not on its output.
/// Panes Herdr reports real scrollback for come out of this untouched.
pub(crate) fn note_and_amend_panes(state: &AppState, session_id: &str, mut value: Value) -> Value {
    if let Some(mut store) = lock_scrollback(state) {
        store.observe(session_id, &value);
        store.amend(session_id, &mut value);
    }
    value
}

pub(crate) async fn workspaces(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let workspaces = terminal_backend(session)
        .list_workspaces()
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::workspace_list(workspaces)))
}

pub(crate) async fn panes(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let answer = terminal_backend(session)
        .list_panes()
        .await
        .map(backend::compat::pane_list)
        .map_err(backend_api_error)?;
    // Same rule as the snapshot: observe and amend first, hash what that
    // produced.
    let answer = note_and_amend_panes(&state, &session_id, answer);
    Ok(agents::routes::json_etag_response(&headers, answer))
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    /// Comma-separated allow-list of event names, e.g. `pane_updated,pane_closed`.
    /// A client that only reacts to output changes should not be woken for the
    /// focus and layout churn that dominates the raw stream -- measured at ~20
    /// events per second on a busy session, almost none of it actionable on a
    /// phone. Absent means forward everything, so an old client is unaffected.
    #[serde(default)]
    pub(crate) types: Option<String>,
    /// When set, `pane.updated` events for THIS pane are enriched with the
    /// pane's current output inline, so the client paints immediately instead of
    /// firing a second read round-trip per update. Other panes' events, and all
    /// other event types, pass through unchanged. Absent means no enrichment, so
    /// an old client still works via its own reads.
    #[serde(default)]
    pub(crate) stream_pane: Option<String>,
    #[serde(default)]
    pub(crate) stream_lines: Option<u32>,
    #[serde(default)]
    pub(crate) stream_source: Option<String>,
    #[serde(default)]
    pub(crate) stream_format: Option<String>,
}

/// Resolved output-streaming settings for one events subscription.
pub(crate) struct StreamOutputOpts {
    pub(crate) pane: Option<String>,
    pub(crate) lines: u32,
    pub(crate) source: String,
    pub(crate) format: String,
}

pub(crate) struct StreamPaneFrame {
    pub(crate) revision: u64,
    pub(crate) output: String,
}

/// Pulls the terminal text out of a Herdr `pane.read` response, tolerating both
/// the bare-string and `{ text: ... }` result shapes across Herdr versions.
pub(crate) fn pane_read_text(value: &Value) -> Option<String> {
    // Herdr nests the text under `result.read.text`; tolerate `result.text` and a
    // bare-string `result` too across versions. Missing all three means no inline
    // output and the client falls back to its own read.
    for ptr in [
        "/result/read/output",
        "/result/read/text",
        "/result/output",
        "/result/text",
    ] {
        if let Some(text) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    value
        .pointer("/result")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Convert one sampled frame into the enriched `pane_updated` payload consumed
/// by Muqun. Output content, rather than revision, is used to decide whether to
/// emit because Herdr can expose new text before its coalesced revision advances.
pub(crate) fn stream_pane_update_payload(frame: &StreamPaneFrame, pane_id: &str) -> Option<String> {
    let payload = json!({
        "event": "pane_updated",
        "data": {
            "pane": {
                "pane_id": pane_id,
                "revision": frame.revision
            },
            "output": frame.output
        }
    });
    serde_json::to_string(&payload).ok()
}

pub(crate) async fn poll_stream_pane_update(
    backend: &dyn TerminalBackend,
    opts: &StreamOutputOpts,
) -> Option<StreamPaneFrame> {
    let pane = opts.pane.as_deref()?;
    let read = tokio::time::timeout(
        STREAM_OUTPUT_READ_TIMEOUT,
        backend.read_pane(&stream_read_request(pane, opts)),
    )
    .await
    .ok()?
    .ok()?;
    Some(StreamPaneFrame {
        revision: read.revision.unwrap_or_default(),
        output: read.text,
    })
}

/// If `line` is a `pane.updated` for the streamed pane, read that pane's output
/// and fold it into the event as `data.output`. Returns `None` to forward the
/// line untouched (wrong pane, wrong event, or a read failure -- the client
/// still has its revision and can fall back to a read).
pub(crate) async fn enrich_pane_update(
    line: &str,
    backend: &dyn TerminalBackend,
    opts: &StreamOutputOpts,
) -> Option<String> {
    let pane = opts.pane.as_deref()?;
    let mut value: Value = serde_json::from_str(line).ok()?;
    if normalize_event_name(value.get("event")?.as_str()?) != "pane_updated" {
        return None;
    }
    if value
        .pointer("/data/pane/pane_id")
        .and_then(Value::as_str)?
        != pane
    {
        return None;
    }
    // Bound the read so a slow or wedged Herdr can never stall the event loop:
    // a stalled enrich would starve the whole stream and drop the client back to
    // its slow safety poll. On timeout we forward the un-enriched line and the
    // client reads on its own.
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        backend.read_pane(&stream_read_request(pane, opts)),
    )
    .await
    .ok()?
    .ok()?;
    value
        .get_mut("data")
        .and_then(Value::as_object_mut)?
        .insert("output".into(), Value::String(read.text));
    serde_json::to_string(&value).ok()
}

pub(crate) fn stream_read_request(pane_id: &str, opts: &StreamOutputOpts) -> BackendReadPane {
    BackendReadPane {
        pane_id: BackendPaneId::new(pane_id),
        source: match opts.source.as_str() {
            "visible" => BackendOutputSource::Visible,
            "recent" => BackendOutputSource::Recent,
            "detection" => BackendOutputSource::Detection,
            _ => BackendOutputSource::RecentUnwrapped,
        },
        format: if opts.format == "text" {
            BackendOutputFormat::Text
        } else {
            BackendOutputFormat::Ansi
        },
        lines: opts.lines,
        start: None,
        end: None,
    }
}

/// Fold a streamed frame into what the gateway keeps for that pane.
///
/// Only for panes Herdr reports no scrollback for; everything else is left
/// exactly as it was, unrecorded. The key carries the source and format because
/// rows read as ANSI and rows read as plain text are different rows.
pub(crate) fn keep_stream_frame(
    store: &Arc<Mutex<scrollback::ScrollbackStore>>,
    session_id: &str,
    pane_id: &str,
    opts: &StreamOutputOpts,
    output: &str,
) {
    let Ok(mut store) = store.lock() else { return };
    store.record_frame(session_id, pane_id, &opts.source, &opts.format, output);
}

/// The output an enriched `pane.updated` carries, so the same frame that
/// reaches the reader also reaches the buffer.
pub(crate) fn enriched_pane_output(payload: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload)
        .ok()?
        .pointer("/data/output")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Normalises a filter token to the underscore form Herdr tags events with, so
/// a client may ask for either `pane.updated` or `pane_updated`.
pub(crate) fn normalize_event_name(value: &str) -> String {
    value.trim().replace('.', "_")
}

/// The SSE event name every encrypted stream record travels under. The real
/// event name is inside the sealed payload, where it is authenticated; the
/// outer name is the one piece of stream metadata deliberately left readable.
pub(crate) const ENCRYPTED_SSE_EVENT: &str = "muqun.encrypted";

/// Seals one connection's events, each under its own AES-256-GCM record.
///
/// The per-stream key binds the device key, a fresh stream id and the request
/// envelope's nonce (see `transport::derive_stream_key`); the nonce is the
/// event's sequence number, and the AAD carries request AAD, stream id and
/// seq. Together: a record that is modified, reordered, replayed -- within
/// this stream or from any other -- or dropped (the client checks seq
/// continuity) fails authentication on the phone.
pub(crate) struct EventStreamSealer {
    pub(crate) key: [u8; 32],
    pub(crate) stream_id: String,
    pub(crate) request_aad: String,
    pub(crate) seq: u64,
}

impl EventStreamSealer {
    pub(crate) fn new(context: &EncryptedStreamContext) -> anyhow::Result<Self> {
        let stream_id = generate_token();
        let key =
            transport::derive_stream_key(&context.material, &stream_id, &context.request_nonce)?;
        Ok(Self {
            key,
            stream_id,
            request_aad: context.request_aad.clone(),
            seq: 0,
        })
    }

    pub(crate) fn seal(&mut self, name: &str, data: &str) -> anyhow::Result<Event> {
        let record = self.seal_record(name, data)?;
        Ok(Event::default().event(ENCRYPTED_SSE_EVENT).data(record))
    }

    /// The `data:` line of one sealed record, split out so a test can open
    /// what left the sealer without reaching inside axum's `Event`.
    pub(crate) fn seal_record(&mut self, name: &str, data: &str) -> anyhow::Result<String> {
        let seq = self.seq;
        let plaintext = serde_json::to_vec(&json!({ "event": name, "data": data }))?;
        let aad = format!("{}\n{}\n{}", self.request_aad, self.stream_id, seq);
        let ciphertext = transport::seal_stream_event(&self.key, seq, aad.as_bytes(), &plaintext)?;
        // Only counted once sealing succeeded, so a failed record does not
        // burn a seq the client would then read as a gap.
        self.seq += 1;
        Ok(json!({
            "v": 1,
            "sid": self.stream_id,
            "seq": seq,
            "ciphertext": ciphertext,
        })
        .to_string())
    }
}

/// One event, sealed when this connection is encrypted and plain when it is
/// not. `None` means the record could not be sealed; the event is dropped
/// rather than ever leaving in the clear.
pub(crate) fn stream_event(
    sealer: &mut Option<EventStreamSealer>,
    name: &str,
    data: &str,
) -> Option<Event> {
    match sealer {
        Some(sealer) => match sealer.seal(name, data) {
            Ok(event) => Some(event),
            Err(error) => {
                tracing::warn!("failed to seal stream event: {error}");
                None
            }
        },
        None => Some(Event::default().event(name).data(data)),
    }
}

pub(crate) async fn events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventsQuery>,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let device_id = require_device(&state, &headers)?;
    // Present exactly when the request arrived through the encrypted
    // transport. From here on every event this connection emits is sealed;
    // a device paired without a transport key keeps the plaintext stream.
    let mut sealer = match stream_crypto {
        Some(Extension(context)) => Some(EventStreamSealer::new(&context).map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
        })?),
        None => None,
    };
    let session = find_session(&state.config, &session_id)?.clone();
    let wanted: Option<std::collections::HashSet<String>> = query.types.as_ref().map(|value| {
        value
            .split(',')
            .map(normalize_event_name)
            .filter(|name| !name.is_empty())
            .collect()
    });
    let stream_opts = StreamOutputOpts {
        pane: query.stream_pane.clone().filter(|value| !value.is_empty()),
        lines: query.stream_lines.unwrap_or(240).min(MAX_OUTPUT_LINES),
        source: match query.stream_source.as_deref() {
            Some("recent-unwrapped") | Some("recent_unwrapped") | None => "recent_unwrapped".into(),
            Some(other) => other.to_string(),
        },
        format: match query.stream_format.as_deref() {
            Some("text") => "text".into(),
            _ => "ansi".into(),
        },
    };
    // `asset.created` is a gateway event on the same stream, so it obeys the
    // same allow-list as the Herdr ones: a client that filtered down to output
    // updates is not woken for artifacts it never asked about.
    let asset_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("asset_created"));
    // Approval transitions are published by the pane watcher, not by Herdr, and
    // obey the same allow-list. A client that asked for nothing else still gets
    // told when an agent is blocked, because that is the one thing it cannot
    // discover by watching output go by.
    let approval_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("approval_pending") || set.contains("approval_resolved"));
    let pane_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("pane_updated"));
    let mut approvals_rx = state.approval_events.subscribe();
    let devices = state.clone();
    let assets = state.assets.clone();
    let scrollback_store = state.scrollback.clone();
    let backend = terminal_backend(&session);
    let mut activity = subscribe_activity(&state, &session);
    // The runtime's channel, not a manager's: a client's stream has to survive
    // OpenCode restarting underneath it.
    let mut agent_events_rx = Some(state.agent_runtime.subscribe_events());
    let stream = async_stream::stream! {
        let mut output_interval = tokio::time::interval(STREAM_OUTPUT_POLL_INTERVAL);
        output_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The authorisation this connection was opened on has to be rechecked
        // for as long as it is open. See `STREAM_DEVICE_RECHECK_INTERVAL`.
        let mut device_recheck = tokio::time::interval(STREAM_DEVICE_RECHECK_INTERVAL);
        device_recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval`'s first tick is immediate, and that one is not a recheck
        // -- `require_device` just ran.
        device_recheck.tick().await;
        let mut last_stream_output: Option<String> = None;
        loop {
            tokio::select! {
                _ = device_recheck.tick() => {
                    if !still_paired(&devices, &device_id) {
                        // Closed without an event. A revoked device is not owed
                        // an explanation, and a legitimate client learns the
                        // same thing from the 403 its reconnect earns.
                        break;
                    }
                },
                next = activity.recv() => match next {
                    Ok(SessionActivity::Event(activity)) => {
                        let data = activity.payload.to_string();
                        let keep = wanted.as_ref().is_none_or(|set| {
                            activity.name.is_empty() || set.contains(&activity.name)
                        });
                        if keep {
                            let payload = if stream_opts.pane.is_some() {
                                enrich_pane_update(&data, backend.as_ref(), &stream_opts)
                                    .await
                                    .unwrap_or_else(|| data.clone())
                            } else {
                                data.clone()
                            };
                            if let Some(pane_id) = stream_opts.pane.as_deref() {
                                if let Some(output) = enriched_pane_output(&payload) {
                                    keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &output);
                                }
                            }
                            if let Some(event) = stream_event(&mut sealer, "herdr", &payload) {
                                yield Ok(event);
                            }
                        }
                        if let Some(root) = worktree_event_root(&session_id, &data) {
                            let created = ingest_roots(assets.clone(), vec![root]).await;
                            if asset_events {
                                let now = now_unix_ms();
                                for entry in created
                                    .into_iter()
                                    .filter(|entry| now.saturating_sub(entry.modified_unix_ms) <= ASSET_EVENT_MAX_AGE_MS)
                                    .take(MAX_ASSET_EVENTS_PER_WORKTREE)
                                {
                                    let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
                                    let payload = asset_created_payload(&entry, asset_type);
                                    if let Some(event) = stream_event(&mut sealer, "asset.created", &payload) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        } else if let Some(removed) = worktree_event_removed_root(&data) {
                            if let Ok(mut index) = assets.lock() {
                                index.forget_under(&removed);
                            }
                        }
                    }
                    Ok(SessionActivity::Failed) => {
                        // The hub logs why and rebuilds the stream itself. This
                        // connection closes, which is the signal the app already
                        // knows how to act on.
                        if let Some(event) = stream_event(&mut sealer, "gateway.error", "Terminal activity stream unavailable") {
                            yield Ok(event);
                        }
                        break;
                    }
                    // Lagged: this subscriber fell behind a burst of layout
                    // changes. The next full refresh reconciles it, and closing
                    // a working connection over a missed redraw would be worse.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = output_interval.tick(), if stream_opts.pane.is_some() && pane_events => {
                    if let Some(frame) = poll_stream_pane_update(backend.as_ref(), &stream_opts).await {
                        if last_stream_output.as_deref() != Some(frame.output.as_str()) {
                            last_stream_output = Some(frame.output.clone());
                            if let Some(pane_id) = stream_opts.pane.as_deref() {
                                keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &frame.output);
                                if let Some(payload) = stream_pane_update_payload(&frame, pane_id) {
                                    if let Some(event) = stream_event(&mut sealer, "herdr", &payload) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        }
                    }
                },
                approval = approvals_rx.recv(), if approval_events => {
                    match approval {
                        Ok(approval) => {
                            let wanted_name = normalize_event_name(approval.name);
                            if wanted.as_ref().is_none_or(|set| set.contains(&wanted_name)) {
                                if let Some(event) = stream_event(&mut sealer, approval.name, &approval.payload) {
                                    yield Ok(event);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                },
                agent_ev = async {
                    if let Some(ref mut rx) = agent_events_rx {
                        rx.recv().await
                    } else {
                        futures::future::pending().await
                    }
                } => {
                    if let Ok(ev) = agent_ev {
                        let ev_name = ev.event_name();
                        let payload = serde_json::to_string(&ev).unwrap_or_default();
                        if let Some(event) = stream_event(&mut sealer, ev_name, &payload) {
                            yield Ok(event);
                        }
                    }
                },
            }
        }
    };
    Ok(Sse::new(Box::pin(stream) as GatewayEventStream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

pub(crate) fn spawn_approval_watchers(state: AppState) {
    for session in state.config.sessions.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            watch_pane_approvals(state, session).await;
        });
    }
}

/// Watch every agent pane in a session for a permission menu appearing or
/// going away.
///
/// Herdr has no event for this -- a menu is drawn output, not a state change it
/// reports -- so the gateway polls. Only panes Herdr says are running an agent
/// are read, which is what keeps the poll to a handful of reads however many
/// shells the user has open.
pub(crate) async fn watch_pane_approvals(state: AppState, session: SessionConfig) {
    // pane id -> fingerprint of the menu that pane is blocked on.
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(APPROVAL_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Ok(panes) = terminal_backend(&session).list_panes().await else {
            continue;
        };
        // This listing is already being fetched, and it is the only place that
        // says which panes Herdr keeps no scrollback for. Reading it here means
        // the buffer knows what to keep before the reader ever opens the pane,
        // and costs Herdr nothing extra.
        if let Some(mut store) = lock_scrollback(&state) {
            // The complete listing for this session, so it can also forget
            // the panes that have gone.
            store.observe_listing(&session.id, &backend::compat::pane_list(panes.clone()));
        }

        // Every agent pane's screen in one request. On a socket backend this
        // is the same handful of reads it always was; on tmux it is one
        // process instead of one per pane, and this poll runs every 1.5
        // seconds for as long as the gateway is up.
        let agent_panes: Vec<(BackendPaneId, String)> = panes
            .iter()
            .filter_map(|pane| {
                let agent = pane.agent.as_deref().filter(|agent| !agent.is_empty())?;
                Some((pane.id.clone(), agent.to_owned()))
            })
            .collect();
        let ids: Vec<BackendPaneId> = agent_panes.iter().map(|(id, _)| id.clone()).collect();
        let screens: HashMap<String, String> = terminal_backend(&session)
            .read_visible_batch(&ids, APPROVAL_READ_LINES)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, text)| (id.as_str().to_owned(), text))
            .collect();

        let mut seen: Vec<String> = Vec::new();
        for (pane_id, agent) in &agent_panes {
            let pane_id = pane_id.as_str();
            let agent = agent.as_str();
            seen.push(pane_id.to_owned());

            let Some(text) = screens.get(pane_id) else {
                continue;
            };
            match approvals::detect(text) {
                Some(approval) => {
                    if pending.get(pane_id) == Some(&approval.fingerprint) {
                        continue;
                    }
                    // A different menu in the same pane is the old one resolved
                    // and a new one asked, in that order.
                    if let Some(previous) = pending.remove(pane_id) {
                        publish_approval(
                            &state,
                            "approval.resolved",
                            &session.id,
                            pane_id,
                            agent,
                            Some(&previous),
                            None,
                        );
                    }
                    pending.insert(pane_id.to_owned(), approval.fingerprint.clone());
                    publish_approval(
                        &state,
                        "approval.pending",
                        &session.id,
                        pane_id,
                        agent,
                        None,
                        Some(&approval),
                    );
                    deliver_agent_notification(
                        &state,
                        approval_notification(
                            &state.config.server_id,
                            &current_server_label(&state.config.label),
                            &session.id,
                            pane_id,
                            agent,
                            &approval,
                        ),
                    )
                    .await;
                }
                None => {
                    if let Some(previous) = pending.remove(pane_id) {
                        publish_approval(
                            &state,
                            "approval.resolved",
                            &session.id,
                            pane_id,
                            agent,
                            Some(&previous),
                            None,
                        );
                    }
                }
            }
        }

        // A pane that closed while blocked is a resolved approval too: whatever
        // the client was showing is no longer answerable.
        let vanished: Vec<String> = pending
            .keys()
            .filter(|pane_id| !seen.contains(pane_id))
            .cloned()
            .collect();
        for pane_id in vanished {
            let previous = pending.remove(&pane_id);
            publish_approval(
                &state,
                "approval.resolved",
                &session.id,
                &pane_id,
                "",
                previous.as_deref(),
                None,
            );
        }
    }
}

pub(crate) fn publish_approval(
    state: &AppState,
    name: &'static str,
    session_id: &str,
    pane_id: &str,
    agent: &str,
    fingerprint: Option<&str>,
    approval: Option<&approvals::Approval>,
) {
    let agent = (!agent.is_empty()).then_some(agent);
    let mut data = approval_data(session_id, pane_id, agent, approval, "menu");
    if let (Some(object), Some(fingerprint)) = (data.as_object_mut(), fingerprint) {
        object.insert("fingerprint".into(), json!(fingerprint));
    }
    // The same versioned envelope the endpoint answers with, so a client parses
    // an event and a response with one code path.
    let payload = content_envelope(data).to_string();
    let _ = state.approval_events.send(ApprovalEvent { name, payload });
}

/// The push a pending approval sends, before it is put into words.
///
/// Content-free by construction: the title carries the server the user named,
/// the body carries the agent's name, and the data carries ids and the option
/// *decisions* -- never the agent's own wording, which routinely quotes a
/// command or a path. Which language the words end up in is decided per device
/// at delivery; see [`AgentPushNotice`].
pub(crate) fn approval_notification(
    server_id: &str,
    server_label: &str,
    session_id: &str,
    pane_id: &str,
    agent: &str,
    approval: &approvals::Approval,
) -> AgentPushNotice {
    let mut data = serde_json::Map::new();
    data.insert("type".into(), json!("approval.pending"));
    data.insert("url".into(), json!(format!("/servers/{server_id}")));
    data.insert("server_id".into(), json!(server_id));
    data.insert("session_id".into(), json!(session_id));
    data.insert("pane_id".into(), json!(pane_id));
    // The notification category the client registered its approve/deny actions
    // under, plus which of those actions this particular menu offers.
    data.insert("categoryId".into(), json!("approval"));
    data.insert("fingerprint".into(), json!(approval.fingerprint));
    AgentPushNotice {
        notice: AgentNotice::ApprovalPending,
        server_label: server_label.trim().to_owned(),
        agent_name: (!agent.trim().is_empty()).then(|| agent.trim().to_owned()),
        data,
        // The answers travel as indices and decisions, and are worded at
        // delivery time. `options` is set on the payload there.
        choices: approval.push_choices(),
        // An approval push carries the fingerprint and the decisions, which is
        // enough to answer from the lock screen. The question itself is put on
        // the blocked push, and only when the owner asked for it.
        detail: None,
    }
}

/// What one session's activity hub broadcasts.
///
/// `Arc` because every subscriber gets a copy and the payload is a whole JSON
/// document; the point of the hub is that the work is done once.
#[derive(Clone, Debug)]
pub(crate) enum SessionActivity {
    Event(Arc<BackendActivity>),
    /// The underlying stream failed. Subscribers close their connection and
    /// the client reconnects; the hub rebuilds the stream on its own.
    Failed,
}

/// How long the hub waits before rebuilding a stream that failed.
pub(crate) const ACTIVITY_REBUILD_DELAY: Duration = Duration::from_secs(2);

/// Subscribe to a session's activity, starting the one stream that feeds it if
/// nobody had asked yet.
///
/// This exists because `activity_stream()` used to be called once per
/// subscriber: once by the notification watcher, and once more by every SSE
/// connection. For Herdr that is one socket subscription per phone. For tmux --
/// whose adapter polls by design -- it was a whole independent poll per phone,
/// and the cost is processes: three `tmux` invocations every 500ms, times the
/// number of people looking. Five devices watching one session spawned
/// thirty-six processes a second, forever.
///
/// One stream per session, fanned out, is what "publish changes" was always
/// meant to be: the conversion happens once and the result is shared. The
/// gateway already does exactly this for approvals; activity was the odd one
/// out.
pub(crate) fn subscribe_activity(
    state: &AppState,
    session: &SessionConfig,
) -> tokio::sync::broadcast::Receiver<SessionActivity> {
    let mut hubs = match state.activity.lock() {
        Ok(hubs) => hubs,
        // A poisoned map must not take the stream down with it: fall back to
        // a private hub for this one subscriber, which is the old behaviour.
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(sender) = hubs.get(&session.id) {
        return sender.subscribe();
    }
    let (sender, receiver) = tokio::sync::broadcast::channel(ACTIVITY_EVENT_CAPACITY);
    hubs.insert(session.id.clone(), sender.clone());
    tokio::spawn(run_activity_hub(state.clone(), session.clone(), sender));
    receiver
}

/// Whether a backend failure is worth a log line, or is the one already on
/// screen saying itself again.
///
/// `run_activity_hub` rebuilds every `ACTIVITY_REBUILD_DELAY` forever, so a
/// backend that is down for good -- an uninstalled tmux, a herdr that is not
/// running -- wrote the same line about thirty-four thousand times a day. That
/// is not a log, it is a denial of one: the lines that matter are the ones
/// around it, and they were unreadable. Only a *change* is news.
///
/// A stream that produces an event has recovered, and `recovered()` makes the
/// next failure news again even if it is the same failure -- so the log tells
/// "down, up, down" rather than falling silent after the first word.
#[derive(Default)]
pub(crate) struct FailureNotes {
    pub(crate) reported: Option<String>,
}

impl FailureNotes {
    pub(crate) fn is_news(&mut self, failure: &str) -> bool {
        if self.reported.as_deref() == Some(failure) {
            return false;
        }
        self.reported = Some(failure.to_owned());
        true
    }

    pub(crate) fn recovered(&mut self) {
        self.reported = None;
    }
}

/// Feed one session's hub for as long as anyone is listening.
///
/// In practice that is the life of the process, and the retirement path below
/// is a safety net rather than the normal case: `watch_agent_notifications`
/// subscribes at startup, one per configured session, and holds it for as long
/// as the gateway runs so that a phone which is not connected still gets push
/// notifications. So `receiver_count()` never reaches zero and
/// `retire_activity_hub` never fires. That is deliberate -- the alternative is
/// no notifications until somebody opens the app -- but it does mean a
/// configured session is polled from startup whether or not anyone is looking,
/// and the comment that used to sit here promised the opposite.
pub(crate) async fn run_activity_hub(
    state: AppState,
    session: SessionConfig,
    sender: tokio::sync::broadcast::Sender<SessionActivity>,
) {
    let mut notes = FailureNotes::default();
    loop {
        match terminal_backend(&session).activity_stream().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(activity) => {
                            notes.recovered();
                            // `send` fails only when nobody is listening, which
                            // is the condition this loop ends on anyway.
                            let _ = sender.send(SessionActivity::Event(Arc::new(activity)));
                        }
                        Err(err) => {
                            let failure = format!(
                                "terminal activity failed for session {} (backend={}, endpoint={}): {err}",
                                session.id,
                                session.backend.as_str(),
                                backend_endpoint(&session),
                            );
                            if notes.is_news(&failure) {
                                tracing::warn!("{failure}");
                            }
                            let _ = sender.send(SessionActivity::Failed);
                            break;
                        }
                    }
                    if sender.receiver_count() == 0 {
                        break;
                    }
                }
            }
            Err(err) => {
                let failure = format!(
                    "terminal activity stream could not be opened for session {} (backend={}): {err}",
                    session.id,
                    session.backend.as_str(),
                );
                if notes.is_news(&failure) {
                    tracing::warn!("{failure}");
                }
                let _ = sender.send(SessionActivity::Failed);
            }
        }
        if retire_activity_hub(&state, &session.id) {
            return;
        }
        tokio::time::sleep(ACTIVITY_REBUILD_DELAY).await;
    }
}

/// Drop a session's hub if nothing is listening any more, under the same lock
/// `subscribe_activity` takes.
///
/// The lock is the whole point: checking the count and removing the entry have
/// to be one step, or a subscriber arriving in between gets a receiver on a
/// hub whose producer has already decided to leave.
pub(crate) fn retire_activity_hub(state: &AppState, session_id: &str) -> bool {
    let mut hubs = match state.activity.lock() {
        Ok(hubs) => hubs,
        Err(poisoned) => poisoned.into_inner(),
    };
    match hubs.get(session_id) {
        Some(sender) if sender.receiver_count() == 0 => {
            hubs.remove(session_id);
            true
        }
        // Someone else replaced the entry; that hub owns itself now.
        None => true,
        _ => false,
    }
}

pub(crate) fn spawn_agent_notification_watchers(state: AppState) {
    for session in state.config.sessions.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            watch_agent_notifications(state, session).await;
        });
    }
}

pub(crate) fn spawn_agent_permission_watchers(state: AppState) {
    let mut rx = state.agent_runtime.subscribe_events();
    let state = state.clone();

    tokio::spawn(async move {
        let mut gates = AgentPushGates::default();
        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                // Falling behind loses the skipped events, not the watcher.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            match gates.admit(&event) {
                Some(AgentPendingPush::Approval { asid, request }) => {
                    let tokens = match state.push_tokens.lock() {
                        Ok(guard) => guard.clone(),
                        Err(_) => Vec::new(),
                    };
                    if !tokens.is_empty() {
                        let _ = send_expo_push_notifications(
                            &tokens,
                            "Approval Required".to_string(),
                            request.prompt.clone(),
                            agent_permission_push_data(&state.config.server_id, asid, &request.id),
                        )
                        .await;
                    }
                }
                Some(AgentPendingPush::Form { asid, request }) => {
                    // Naming the agent and the session can take a round trip
                    // to the agent; the event loop does not wait for it.
                    let state = state.clone();
                    let asid = asid.to_owned();
                    let request = request.clone();
                    tokio::spawn(async move {
                        deliver_agent_form_notice(&state, &asid, &request).await;
                    });
                }
                None => {}
            }
        }
    });
}

/// A pending approval or question that has earned a push.
#[derive(Debug, PartialEq)]
pub(crate) enum AgentPendingPush<'a> {
    Approval {
        asid: &'a str,
        request: &'a agents::PermissionRequest,
    },
    Form {
        asid: &'a str,
        request: &'a agents::FormRequest,
    },
}

/// One push per pending request. T3 re-announces every open approval and
/// question on each thread snapshot, so the same request id arrives as a
/// fresh `*Pending` event many times; only the first is news. A resolve
/// forgets the id. Events are per session, a child session's included --
/// nothing here looks at `parent_id`.
#[derive(Debug, Default)]
pub(crate) struct AgentPushGates {
    approvals: std::collections::HashSet<(String, String)>,
    forms: std::collections::HashSet<(String, String)>,
}

/// Requests that are never resolved (an agent that went away) must not grow
/// the gates forever; forgetting them all costs at most a repeat push.
const AGENT_PUSH_GATE_CAPACITY: usize = 1024;

impl AgentPushGates {
    pub(crate) fn admit<'a>(
        &mut self,
        event: &'a agents::AgentDomainEvent,
    ) -> Option<AgentPendingPush<'a>> {
        use agents::AgentDomainEvent as E;
        match event {
            E::PermissionPending { asid, request, .. } => {
                first_sighting(&mut self.approvals, &asid.0, &request.id).then_some(
                    AgentPendingPush::Approval {
                        asid: &asid.0,
                        request,
                    },
                )
            }
            E::FormPending { asid, request, .. } => {
                first_sighting(&mut self.forms, &asid.0, &request.id).then_some(
                    AgentPendingPush::Form {
                        asid: &asid.0,
                        request,
                    },
                )
            }
            E::PermissionResolved {
                asid, request_id, ..
            } => {
                self.approvals.remove(&(asid.0.clone(), request_id.clone()));
                None
            }
            E::FormResolved { asid, form_id, .. } => {
                self.forms.remove(&(asid.0.clone(), form_id.clone()));
                None
            }
            _ => None,
        }
    }
}

fn first_sighting(
    open: &mut std::collections::HashSet<(String, String)>,
    asid: &str,
    id: &str,
) -> bool {
    if open.len() >= AGENT_PUSH_GATE_CAPACITY {
        open.clear();
    }
    open.insert((asid.to_owned(), id.to_owned()))
}

/// The data of a structured agent's approval push; `session_id` is the agent
/// session the request belongs to.
fn agent_permission_push_data(
    server_id: &str,
    asid: &str,
    request_id: &str,
) -> serde_json::Map<String, Value> {
    let mut data = serde_json::Map::new();
    data.insert("type".to_string(), json!("approval"));
    data.insert("category".to_string(), json!("approval"));
    data.insert("server_id".to_string(), json!(server_id));
    data.insert("session_id".to_string(), json!(asid));
    data.insert("asid".to_string(), json!(asid));
    data.insert("approval_id".to_string(), json!(request_id));
    data.insert("fingerprint".to_string(), json!(request_id));
    data
}

/// The data of a structured agent's question push: an agent session -- a
/// child's as much as a top-level one's -- asking the user to fill a form.
fn agent_form_push_data(
    server_id: &str,
    agent_id: &str,
    asid: &str,
    form_id: &str,
) -> serde_json::Map<String, Value> {
    let mut data = serde_json::Map::new();
    data.insert("type".to_string(), json!("question"));
    data.insert("category".to_string(), json!("question"));
    data.insert("server_id".to_string(), json!(server_id));
    data.insert("agent_id".to_string(), json!(agent_id));
    data.insert("session_id".to_string(), json!(asid));
    data.insert("asid".to_string(), json!(asid));
    data.insert("form_id".to_string(), json!(form_id));
    data.insert("fingerprint".to_string(), json!(form_id));
    data
}

/// An agent's own name, for a push about one of its sessions.
fn agent_display_name(agent_id: &str) -> Option<&'static str> {
    match agent_id {
        "opencode" => Some("OpenCode"),
        "deepseek" => Some("DeepSeek"),
        "t3" => Some("T3 Code"),
        _ => None,
    }
}

/// The push a pending question sends, before it is put into words: "{name}
/// needs your input", where the name is the session's title when there is one
/// and the agent's otherwise. The form's own title is the body only when the
/// owner turned `rich_agent_pushes` on.
fn form_notification(
    state: &AppState,
    agent_id: &str,
    session_title: Option<&str>,
    asid: &str,
    request: &agents::FormRequest,
) -> AgentPushNotice {
    let agent_name = session_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| agent_display_name(agent_id))
        .map(str::to_owned);
    AgentPushNotice {
        notice: AgentNotice::AgentBlocked,
        server_label: current_server_label(&state.config.label).trim().to_owned(),
        agent_name,
        data: agent_form_push_data(&state.config.server_id, agent_id, asid, &request.id),
        choices: Vec::new(),
        detail: state
            .config
            .rich_agent_pushes
            .then(|| PushDetail::from_question(&request.title))
            .flatten(),
    }
}

async fn deliver_agent_form_notice(state: &AppState, asid: &str, request: &agents::FormRequest) {
    let (agent_id, title) = match state.agent_runtime.manager_for_session(asid).await {
        Some(manager) => {
            let title =
                tokio::time::timeout(Duration::from_secs(3), manager.sessions().get_session(asid))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|info| info.title);
            (manager.agent().kind().to_owned(), title)
        }
        None => (String::new(), None),
    };
    let notice = form_notification(state, &agent_id, title.as_deref(), asid, request);
    deliver_agent_notification(state, notice).await;
}

pub(crate) async fn watch_agent_notifications(state: AppState, session: SessionConfig) {
    let mut statuses = seed_agent_statuses(&session).await;
    let mut completions = CompletionGate::default();
    // One subscription to the session's shared hub, held for the life of the
    // process -- which is what lets a phone that is not connected still get a
    // push. It used to build a stream of its own, which on tmux meant this
    // watcher polled independently of every phone that was also polling.
    //
    // The "only a change is news" dedup this comment used to describe went
    // with the logging, into `FailureNotes` in the hub.
    let mut activity = subscribe_activity(&state, &session);
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        {
            tokio::select! {
                _ = poll.tick() => {
                    if let Some(events) = poll_agent_statuses(&session).await {
                        let present: std::collections::HashSet<String> = events.iter()
                            .filter_map(|event| event.pointer("/data/pane_id").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect();
                        for event in events {
                            if let Some(mut notification) = observe_agent_notification(
                                &state, &session.id, &event, &mut statuses, &mut completions,
                            ) {
                                enrich_blocked_notification(&state, &session, &mut notification).await;
                                deliver_agent_notification(&state, notification).await;
                            }
                        }
                        for notification in completions.ready(&statuses, &present, Instant::now()) {
                            deliver_agent_notification(&state, notification).await;
                        }
                    }
                }
                next = activity.recv() => match next {
                    Ok(SessionActivity::Event(event)) if event.name == "pane_agent_status_changed" => {
                        if let Some(mut notification) = observe_agent_notification(
                            &state,
                            &session.id,
                            &event.payload,
                            &mut statuses,
                            &mut completions,
                        ) {
                            enrich_blocked_notification(&state, &session, &mut notification).await;
                            deliver_agent_notification(&state, notification).await;
                        }
                    }
                    // The hub logs the failure and rebuilds the stream. This
                    // watcher keeps its two-second poll going meanwhile, which
                    // is what actually drives notifications on a backend whose
                    // activity carries no agent status at all.
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // The hub retired while this watcher was the last holder.
                    // Take a new subscription, which starts it again.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        activity = subscribe_activity(&state, &session);
                    }
                }
            }
        }
    }
}

pub(crate) async fn poll_agent_statuses(session: &SessionConfig) -> Option<Vec<Value>> {
    let Ok(value) = backend_agent_list(session).await else {
        return None;
    };
    Some(
        value
            .pointer("/result/agents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|agent| {
                json!({
                    "event": "pane.agent_status_changed",
                    "data": agent
                })
            })
            .collect(),
    )
}

pub(crate) const COMPLETION_GRACE: Duration = Duration::from_secs(30);

pub(crate) const COMPLETION_COOLDOWN: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(crate) struct CompletionGate {
    pub(crate) pending: HashMap<String, (Instant, AgentPushNotice)>,
    pub(crate) last_sent: HashMap<String, Instant>,
}

impl CompletionGate {
    pub(crate) fn observe(
        &mut self,
        pane_id: &str,
        status: &str,
        notice: Option<AgentPushNotice>,
        now: Instant,
    ) -> Option<AgentPushNotice> {
        if status != "idle" {
            self.pending.remove(pane_id);
        }
        let notice = notice?;
        if notice.notice != AgentNotice::AgentCompleted {
            return Some(notice);
        }
        if status == "idle" {
            self.pending.insert(pane_id.to_owned(), (now, notice));
            return None;
        }
        if self.in_cooldown(pane_id, now) {
            return None;
        }
        self.last_sent.insert(pane_id.to_owned(), now);
        Some(notice)
    }

    pub(crate) fn ready(
        &mut self,
        statuses: &HashMap<String, String>,
        present: &std::collections::HashSet<String>,
        now: Instant,
    ) -> Vec<AgentPushNotice> {
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(pane_id, (since, _))| {
                present.contains(*pane_id)
                    && statuses
                        .get(*pane_id)
                        .is_some_and(|status| status == "idle")
                    && now.saturating_duration_since(*since) >= COMPLETION_GRACE
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        let mut notices = Vec::new();
        for pane_id in ready {
            if let Some((_, notice)) = self.pending.remove(&pane_id) {
                if !self.in_cooldown(&pane_id, now) {
                    self.last_sent.insert(pane_id, now);
                    notices.push(notice);
                }
            }
        }
        self.pending.retain(|pane_id, _| {
            present.contains(pane_id)
                && statuses.get(pane_id).is_some_and(|status| status == "idle")
        });
        notices
    }

    pub(crate) fn in_cooldown(&self, pane_id: &str, now: Instant) -> bool {
        self.last_sent
            .get(pane_id)
            .is_some_and(|sent| now.saturating_duration_since(*sent) < COMPLETION_COOLDOWN)
    }
}

pub(crate) fn observe_agent_notification(
    state: &AppState,
    session_id: &str,
    event: &Value,
    statuses: &mut HashMap<String, String>,
    completions: &mut CompletionGate,
) -> Option<AgentPushNotice> {
    let data = event.get("data").unwrap_or(event);
    let pane_id = data.get("pane_id")?.as_str()?;
    let status = data.get("agent_status")?.as_str()?.to_ascii_lowercase();
    let notice = absorb_agent_status_event(state, session_id, event, statuses);
    completions.observe(pane_id, &status, notice, Instant::now())
}

/// Send one notice to every registered device, each in its own language.
///
/// Devices are grouped by the locale they registered with and the notice is
/// worded once per group, so a household with an English phone and a Chinese
/// phone gets one Expo batch each rather than one batch in whichever language
/// happened to be asked for last. In the ordinary case every device shares a
/// locale and this is exactly the single request it always was.
pub(crate) async fn deliver_agent_notification(state: &AppState, notice: AgentPushNotice) {
    let tokens = match state.push_tokens.lock() {
        Ok(tokens) => tokens.clone(),
        Err(_) => {
            tracing::warn!("agent notification skipped: push token lock failed");
            return;
        }
    };
    let mut by_locale: BTreeMap<Locale, Vec<PushTokenRecord>> = BTreeMap::new();
    for token in tokens {
        by_locale.entry(token.locale()).or_default().push(token);
    }
    for (locale, tokens) in by_locale {
        let notification = notice.render(locale);
        if let Err(err) = send_expo_push_notifications(
            &tokens,
            notification.title,
            notification.body,
            notification.data,
        )
        .await
        {
            tracing::warn!("agent notification failed: {err:#}");
        }
    }
}

pub(crate) async fn seed_agent_statuses(session: &SessionConfig) -> HashMap<String, String> {
    let Ok(value) = backend_agent_list(session).await else {
        return HashMap::new();
    };
    value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            Some((
                agent.get("pane_id")?.as_str()?.to_owned(),
                agent.get("agent_status")?.as_str()?.to_ascii_lowercase(),
            ))
        })
        .collect()
}

pub(crate) async fn backend_agent_list(session: &SessionConfig) -> Result<Value, BackendError> {
    Ok(backend::compat::agent_list(&backend_agents(session).await?))
}

/// Every agent the backend reports, with a status inferred for the tmux ones
/// the backend could not name.
///
/// The one source for both `GET .../agents` and the `agents` array inside
/// `GET .../snapshot`: two spellings of the same list is how the snapshot's
/// copy came to be missing `instance_id` and `target`.
pub(crate) async fn backend_agents(session: &SessionConfig) -> Result<Vec<Agent>, BackendError> {
    let terminal = terminal_backend(session);
    let mut agents = terminal.list_agents().await?;
    for agent in agents
        .iter_mut()
        .filter(|agent| agent.kind.is_some() && agent.status == BackendAgentStatus::Unknown)
    {
        let output = terminal
            .read_pane(&BackendReadPane {
                pane_id: agent.pane_id.clone(),
                source: BackendOutputSource::Visible,
                format: BackendOutputFormat::Text,
                lines: APPROVAL_READ_LINES,
                start: None,
                end: None,
            })
            .await;
        if let Ok(output) = output {
            agent.status = infer_tmux_agent_status(agent.kind.as_deref(), &output.text);
        }
    }
    Ok(agents)
}

pub(crate) fn infer_tmux_agent_status(agent: Option<&str>, visible: &str) -> backend::AgentStatus {
    if approvals::detect(visible).is_some() {
        return backend::AgentStatus::Blocked;
    }
    let Some(dictionary) = parts::dictionary_for(agent) else {
        return backend::AgentStatus::Unknown;
    };
    let normalized = parts::normalize_json(visible, Some(dictionary));
    match normalized
        .last()
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
    {
        Some("prompt") => backend::AgentStatus::Idle,
        Some("status") | Some("tool-block") => backend::AgentStatus::Working,
        _ => backend::AgentStatus::Unknown,
    }
}

/// One agent status change, after the bookkeeping and before anyone decides
/// what to do about it. Two things want it: the ring, which wants every one,
/// and the pushes, which want the two that are worth waking a phone for.
pub(crate) struct AgentTransition {
    pub(crate) pane_id: String,
    /// The agent's own name, when Herdr reported one.
    pub(crate) agent: Option<String>,
    pub(crate) from: Option<String>,
    pub(crate) to: String,
}

/// Read one status event and consume the transition it represents.
///
/// This is the only place `statuses` is written, which is what makes "exactly
/// once per event" a property of the code rather than a rule to remember: a
/// caller that wants both a ring entry and a push calls this once and hands the
/// answer to both.
pub(crate) fn agent_status_transition(
    event: &Value,
    statuses: &mut HashMap<String, String>,
) -> Option<AgentTransition> {
    let data = event.get("data").unwrap_or(event);
    let event_type = event
        .get("event")
        .or_else(|| data.get("type"))
        .and_then(Value::as_str)?;
    if event_type != "pane.agent_status_changed" {
        return None;
    }

    let pane_id = data.get("pane_id")?.as_str()?;
    let status = data.get("agent_status")?.as_str()?.to_ascii_lowercase();
    let previous = statuses.insert(pane_id.to_owned(), status.clone());
    if previous.as_deref() == Some(status.as_str()) {
        return None;
    }
    Some(AgentTransition {
        pane_id: pane_id.to_owned(),
        agent: ["display_agent", "agent", "title"]
            .into_iter()
            .find_map(|key| data.get(key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        from: previous,
        to: status,
    })
}

/// The push one transition raises, if it raises one at all.
///
/// Most transitions raise none -- an agent starting to work is not news to
/// someone who just asked it to. The two that do are the two a person is
/// waiting on: it needs them, or it is finished. It returns an unworded
/// [`AgentPushNotice`] rather than finished text because the same change may
/// have to be said in two languages.
pub(crate) fn notification_for_transition(
    transition: &AgentTransition,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Option<AgentPushNotice> {
    let pane_id = transition.pane_id.as_str();
    let (event_type, notice) = match (transition.to.as_str(), transition.from.as_deref()) {
        ("blocked", _) => ("agent.blocked", AgentNotice::AgentBlocked),
        ("idle", Some("working")) | ("done" | "completed", Some("working" | "idle")) => {
            ("agent.completed", AgentNotice::AgentCompleted)
        }
        _ => return None,
    };
    let agent_name = transition.agent.clone();
    let mut notification_data = serde_json::Map::new();
    notification_data.insert("type".into(), json!(event_type));
    notification_data.insert("url".into(), json!(format!("/servers/{server_id}")));
    notification_data.insert("server_id".into(), json!(server_id));
    notification_data.insert("session_id".into(), json!(session_id));
    notification_data.insert("pane_id".into(), json!(pane_id));

    Some(AgentPushNotice {
        notice,
        server_label: server_label.trim().to_owned(),
        agent_name,
        data: notification_data,
        choices: Vec::new(),
        // Filled in afterwards, and only for a blocked pane on a gateway whose
        // owner turned `rich_agent_pushes` on.
        detail: None,
    })
}

/// Everything one status event causes, in one call: it is remembered, and then
/// it may raise a push.
///
/// The ring records every transition and the push only ever names two of them,
/// which is the point -- a phone that missed the doorbell can still be told
/// that the agent worked for twenty minutes and then went idle.
pub(crate) fn absorb_agent_status_event(
    state: &AppState,
    session_id: &str,
    event: &Value,
    statuses: &mut HashMap<String, String>,
) -> Option<AgentPushNotice> {
    let transition = agent_status_transition(event, statuses)?;
    match state.agent_events.lock() {
        Ok(mut log) => {
            log.record(
                session_id,
                &transition.pane_id,
                transition.agent.as_deref(),
                transition.from.as_deref(),
                &transition.to,
                now_unix_ms(),
            );
        }
        // Losing one line of a digest must not cost the push that goes with it.
        Err(_) => tracing::warn!("agent event ring lock failed for session {session_id}"),
    }
    notification_for_transition(
        &transition,
        &state.config.server_id,
        &current_server_label(&state.config.label),
        session_id,
    )
}

/// Put the agent's own question on a blocked push, if this gateway's owner
/// asked for that.
///
/// The whole of what `rich_agent_pushes` does, in one place and behind one
/// check, so that "off" is a property of the code path and not a habit. Off, it
/// costs nothing: no pane is read, and the push is byte-for-byte the one this
/// gateway has always sent.
///
/// On, it reads the pane the way the approvals endpoint does and quotes what it
/// finds. A pane with no menu on it -- an agent blocked on something the
/// gateway cannot read -- is left as the content-free push it already was,
/// which is the right degradation: an empty question is worse than the generic
/// sentence, not better.
pub(crate) async fn enrich_blocked_notification(
    state: &AppState,
    session: &SessionConfig,
    notice: &mut AgentPushNotice,
) {
    if !state.config.rich_agent_pushes || notice.notice != AgentNotice::AgentBlocked {
        return;
    }
    let Some(pane_id) = notice
        .data
        .get("pane_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if let Ok((_, Some(approval))) = read_pane_approval(session, &pane_id).await {
        notice.detail = Some(PushDetail::from_approval(&approval));
    }
}

/// The two halves in one call, for tests that are about what a push says rather
/// than about what the ring holds.
#[cfg(test)]
pub(crate) fn notification_for_agent_status_event(
    event: &Value,
    statuses: &mut HashMap<String, String>,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Option<AgentPushNotice> {
    let transition = agent_status_transition(event, statuses)?;
    notification_for_transition(&transition, server_id, server_label, session_id)
}

pub(crate) async fn create_workspace(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    let workspace = backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: body.cwd.map(PathBuf::from),
            label: body.label,
            focus: body.focus.unwrap_or(false),
        })
        .await
        .map_err(backend_api_error)?;
    let tab = backend
        .list_tabs()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|tab| tab.workspace_id == workspace.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created tab")))?;
    let root_pane = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created pane")))?;
    Ok(Json(backend::compat::workspace_created(
        workspace, tab, root_pane,
    )))
}

pub(crate) async fn focus_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_workspace(&BackendWorkspaceId::new(workspace_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_focused")))
}

pub(crate) async fn rename_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_workspace(&BackendWorkspaceId::new(workspace_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_renamed")))
}

pub(crate) async fn close_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_workspace(&BackendWorkspaceId::new(workspace_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_closed")))
}

pub(crate) async fn tabs(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let tabs = terminal_backend(session)
        .list_tabs()
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::tab_list(tabs)))
}

pub(crate) async fn create_tab(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    let tab = backend
        .create_tab(&BackendCreateTab {
            workspace_id: body.workspace_id.map(BackendWorkspaceId::new),
            cwd: body.cwd.map(PathBuf::from),
            label: body.label,
            focus: body.focus.unwrap_or(false),
        })
        .await
        .map_err(backend_api_error)?;
    let root_pane = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created pane")))?;
    Ok(Json(backend::compat::tab_created(tab, root_pane)))
}

pub(crate) async fn focus_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_tab(&BackendTabId::new(tab_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_focused")))
}

pub(crate) async fn rename_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_tab(&BackendTabId::new(tab_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_renamed")))
}

pub(crate) async fn close_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_tab(&BackendTabId::new(tab_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_closed")))
}

pub(crate) async fn pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let answer = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
        .map(backend::compat::pane_get)
        .map_err(backend_api_error)?;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer)))
}

pub(crate) async fn focus_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_focused")))
}

pub(crate) async fn rename_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenamePaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_pane(&BackendPaneId::new(pane_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_renamed")))
}

pub(crate) async fn close_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_closed")))
}

pub(crate) async fn split_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SplitPaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    if !matches!(body.direction.as_str(), "right" | "down") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_direction",
            "direction must be right or down",
        ));
    }
    if body
        .ratio
        .is_some_and(|ratio| !(0.05..=0.95).contains(&ratio))
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_ratio",
            "ratio must be between 0.05 and 0.95",
        ));
    }
    let session = find_session(&state.config, &session_id)?;
    let command = body
        .command
        .map(|parts| parts.join(" "))
        .filter(|text| !text.trim().is_empty());
    if let Some(text) = command.as_deref() {
        validate_text(text)?;
    }
    let backend = terminal_backend(session);
    let pane = backend
        .split_pane(&BackendSplitPane {
            pane_id: BackendPaneId::new(pane_id),
            direction: match body.direction.as_str() {
                "right" => BackendSplitDirection::Right,
                "down" => BackendSplitDirection::Down,
                _ => unreachable!("direction was validated above"),
            },
            ratio: body.ratio,
            cwd: body.cwd.map(PathBuf::from),
            env: body.env,
        })
        .await
        .map_err(backend_api_error)?;
    if let Some(text) = command {
        backend
            .send_text(&pane.id, &text, BackendSendTextMode::Paste)
            .await
            .map_err(backend_api_error)?;
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .map_err(backend_api_error)?;
    }
    Ok(Json(backend::compat::pane_created(pane)))
}

pub(crate) async fn zoom_pane(
    State(state): State<AppState>,
    Path((session_id, _pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ZoomPaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mode = body.mode.unwrap_or_else(|| "on".into());
    if !matches!(mode.as_str(), "on" | "off" | "toggle") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_zoom_mode",
            "mode must be on, off, or toggle",
        ));
    }
    find_session(&state.config, &session_id)?;
    // Released Muqun builds POST `zoom:on` whenever a terminal view mounts.
    // That request is view setup, not an explicit user command. Propagating it
    // changed the user's tmux/Herdr layout merely by opening the app and raced
    // stale snapshots into misleading target-not-found errors. Keep the route
    // and response for wire compatibility, but observation is side-effect free.
    Ok(Json(backend::compat::command_ok("pane_zoomed")))
}

/// The directories this session is already working in, for a spawn picker.
///
/// Deliberately not a directory browser. It answers with the distinct working
/// directories of the panes Herdr reports right now and nothing else, so the
/// list a phone can pick from is exactly the list `cwd` will accept -- and a
/// phone cannot use it to walk the host's filesystem. `git` says which of them
/// is a checkout, because "start an agent here" usually means a repo.
pub(crate) async fn recent_cwds(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let roots = session_asset_roots(&state, &session, None).await;

    let cwds = tokio::task::spawn_blocking(move || {
        roots
            .into_iter()
            .map(|root| {
                json!({
                    "path": root.path.to_string_lossy(),
                    "name": root.path.file_name().unwrap_or_default().to_string_lossy(),
                    "pane_id": root.pane_id,
                    "workspace_id": root.workspace_id,
                    "git": tasks::is_git_checkout(&root.path),
                })
            })
            .collect::<Vec<Value>>()
    })
    .await
    .unwrap_or_default();

    Ok(Json(json!({ "session_id": session_id, "cwds": cwds })))
}

/// Stop whatever the agent in this pane is doing.
///
/// Sugar over send-keys, and the reason it is worth an endpoint is that the key
/// is not the same on every agent: `ctrl+c` at a shell, `esc` in every agent
/// this gateway has a profile for. A Stop button that guesses is wrong on most
/// panes, and the gateway is the piece that already knows which agent is in
/// this one.
///
/// It sends a keystroke and nothing else -- no signal, no kill. Whatever the
/// agent does with `esc` is the agent's business.
pub(crate) async fn interrupt_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let pane = pane_get(&session, &pane_id).await?;
    let agent = pane
        .get("agent")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let title = pane
        .get("terminal_title_stripped")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let key = shortcuts::interrupt_key(agent, title);

    send_pane_keys(&session, &pane_id, std::slice::from_ref(&key)).await?;

    Ok(Json(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "agent": agent,
        // Which key was actually sent, so a client can say what it did rather
        // than claim a stop it cannot see the effect of.
        "key": key,
        "sent": true,
    })))
}

/// A requested range, clamped to what one response may carry.
///
/// Out-of-bounds clamps instead of failing: a reader paging toward the top will
/// always eventually ask for more than the pane holds, and that is how reaching
/// the top looks from outside, not a mistake worth an error for.
pub(crate) fn validate_output_range(
    start: Option<u32>,
    end: Option<u32>,
) -> ApiResult<Option<(u32, u32)>> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if start < end => Ok(Some((
            start,
            end.min(start.saturating_add(MAX_OUTPUT_LINES)),
        ))),
        (Some(_), Some(_)) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start must be less than end",
        )),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start and end must be given together",
        )),
    }
}

pub(crate) async fn pane_output(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<OutputQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let range = validate_output_range(query.start, query.end)?;
    let source = query.source.unwrap_or_else(|| "recent-unwrapped".into());
    if !matches!(
        source.as_str(),
        "visible" | "recent" | "recent-unwrapped" | "detection"
    ) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_source",
            "source must be visible, recent, recent-unwrapped, or detection",
        ));
    }
    let lines = query.lines.unwrap_or(200).min(MAX_OUTPUT_LINES);
    let format = query.format.unwrap_or_else(|| "text".into());
    if !matches!(format.as_str(), "text" | "ansi") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_format",
            "format must be text or ansi",
        ));
    }
    let herdr_source = if source == "recent-unwrapped" {
        "recent_unwrapped"
    } else {
        source.as_str()
    };
    let session = find_session(&state.config, &session_id)?;
    let request = BackendReadPane {
        pane_id: BackendPaneId::new(pane_id.clone()),
        source: match source.as_str() {
            "visible" => BackendOutputSource::Visible,
            "recent" => BackendOutputSource::Recent,
            "recent-unwrapped" => BackendOutputSource::RecentUnwrapped,
            "detection" => BackendOutputSource::Detection,
            _ => unreachable!("source was validated above"),
        },
        format: match format.as_str() {
            "text" => BackendOutputFormat::Text,
            "ansi" => BackendOutputFormat::Ansi,
            _ => unreachable!("format was validated above"),
        },
        lines,
        start: range.map(|(start, _)| start),
        end: range.map(|(_, end)| end),
    };
    let output = terminal_backend(session)
        .read_pane(&request)
        .await
        .map_err(backend_api_error)?;
    let mut answer = backend::compat::pane_read(output);

    // Herdr answered with everything it has. For a pane it keeps nothing above
    // the viewport for, everything it has is one screen -- and this is the read
    // that both feeds what the gateway kept and hands it back.
    //
    // Scoped to the tail path only (`range.is_none()`): stitching windows the
    // local buffer by `lines`, which has no relationship to a requested
    // `[start, end)`, and it only ever rewrites the text pointer, never
    // `range`. A range-addressed read already got the backend's own answer
    // for that exact slice; substituting a differently-windowed text under an
    // unchanged `range` would make the response lie about which lines it
    // holds, which is worse than the fabricated `start: 0` Task 3 already
    // ruled out.
    if range.is_none() {
        if let (Some(text), Some(mut store)) = (pane_read_text(&answer), lock_scrollback(&state)) {
            let served = store.serve_read(
                &session_id,
                &pane_id,
                herdr_source,
                &format,
                &text,
                lines as usize,
            );
            if served != text {
                scrollback::replace_read_text(&mut answer, &served);
            }
        }
    }
    Ok(Json(answer))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PartsQuery {
    #[serde(default)]
    pub(crate) lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FileSearchQuery {
    /// What the user typed after the `@`. Absent or empty is not an error: it
    /// answers with the shallowest files in the workspace, which is what a
    /// picker should show before anything has been typed.
    #[serde(default)]
    pub(crate) query: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

/// The normalized transcript: the same text the raw output endpoint serves, read
/// through the marker dictionary of whichever agent is in the pane.
///
/// Additive on purpose. The raw ANSI endpoints stay forever, so a client that
/// dislikes what a dictionary made of a pane is one tap from the terminal view,
/// and a pane running no agent -- or one no dictionary covers yet -- is answered
/// with text parts rather than with an error.
pub(crate) async fn pane_parts(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<PartsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let lines = query
        .lines
        .unwrap_or(PARTS_DEFAULT_LINES)
        .clamp(1, MAX_OUTPUT_LINES);

    // Which agent is in the pane decides which source reads it, and the pane's
    // own working directory is the fence every workspace read is under.
    let (agent, root) = pane_agent_and_root(&session, &pane_id).await;

    // `recent_unwrapped` is the only source worth normalizing: the dictionaries
    // key off line starts, and it is the one source where a long line is one
    // line rather than however many the pane happens to be wide.
    let output = terminal_backend(&session)
        .read_pane(&BackendReadPane {
            pane_id: BackendPaneId::new(&pane_id),
            source: BackendOutputSource::RecentUnwrapped,
            format: BackendOutputFormat::Text,
            lines,
            start: None,
            end: None,
        })
        .await
        .map_err(backend_api_error)?;
    let (backend_text, revision) = (output.text, output.revision);
    // The transcript reads the same pane through a different endpoint, so it
    // has to be given the same rows: two views that disagree about where
    // history ends is the bug this whole thing is trying not to introduce.
    let text = match lock_scrollback(&state) {
        Some(mut store) => store.serve_read(
            &session_id,
            &pane_id,
            "recent_unwrapped",
            "text",
            &backend_text,
            lines as usize,
        ),
        None => backend_text,
    };

    let dictionary = parts::dictionary_for(agent.as_deref());

    // A native protocol, where the agent runs one and the operator pointed the
    // gateway at it, is a better source than the screen: it carries exit codes,
    // patches, checklists and pending permissions as data rather than as glyphs
    // a table has to guess at. It is also never required -- every failure below
    // falls through to the dictionary, so an adapter can add structure to a
    // pane and can never take one away.
    let native = match native::adapter_for(agent.as_deref()) {
        Some(adapter) => {
            native::read(
                adapter,
                root.as_deref(),
                native::DEFAULT_MESSAGE_LIMIT,
                i18n::current(),
            )
            .await
        }
        None => None,
    };
    let normalized = match &native {
        Some(read) => read.parts.clone(),
        None => parts::normalize_json(&text, dictionary),
    };

    // Reading a workspace's own skills and commands is blocking filesystem
    // work, so it runs off the async runtime like every other scan here.
    let composer = {
        let agent = agent.clone();
        tokio::task::spawn_blocking(move || composer::descriptor(agent.as_deref(), root.as_deref()))
            .await
            .unwrap_or_default()
    };

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        // Which source answered. `recent-unwrapped` is the pane's own text;
        // an adapter's id says the parts came from that agent's protocol and
        // that `range` spans the adapter's rendering rather than terminal rows.
        "source": match &native {
            Some(_) => "native",
            None => "recent-unwrapped",
        },
        "lines": lines,
        "revision": revision,
        "pane": pane_capabilities(
            &pane_id,
            agent.as_deref(),
            dictionary,
            native.as_ref(),
            composer,
        ),
        "parts": normalized,
    }))))
}

/// What Herdr says is running in a pane, and the workspace root that pane is
/// fenced to.
///
/// A `pane.get` that fails is not fatal to any caller: no agent means text
/// parts and no native source, which is exactly what an unreachable `pane.get`
/// should degrade to. The root is canonicalized and held to the same
/// `is_scannable_root` rule the assets and file-search APIs use, so a pane
/// sitting at `/` or in a home directory names no root at all.
pub(crate) async fn pane_agent_and_root(
    session: &SessionConfig,
    pane_id: &str,
) -> (Option<String>, Option<PathBuf>) {
    let Ok(pane) = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
    else {
        return (None, None);
    };
    let root = pane
        .cwd
        .filter(|path| is_scannable_root(path))
        .and_then(|path| std::fs::canonicalize(path).ok());
    (pane.agent, root)
}

/// The per-pane capability descriptor, because agent detection varies pane to
/// pane: `native` means the agent's own protocol answered, `dictionary` means
/// the parts were read off the screen, and `text` means this pane fell back to
/// prose and a client should not wait for tool blocks.
///
/// `native` is a third value of an existing enum, not a new concept: the parts
/// under it are the same closed set in the same envelope. What it does tell a
/// client is which coordinate system `range` is in -- terminal rows for a
/// dictionary read, rows of the adapter's own rendering for a native one, which
/// a client rebuilds by joining the parts' `fallback_text`.
///
/// `composer` is absent rather than null for an agent with no command table, so
/// a client can tell "this gateway knows nothing about this agent" from "this
/// agent understands no slash commands".
pub(crate) fn pane_capabilities(
    pane_id: &str,
    agent: Option<&str>,
    dictionary: Option<&'static parts::Dictionary>,
    native: Option<&native::NativeRead>,
    composer: Option<Value>,
) -> Value {
    let mut capabilities = json!({
        "pane_id": pane_id,
        "agent": agent,
        "parts": match (native.is_some(), dictionary.is_some()) {
            (true, _) => "native",
            (false, true) => "dictionary",
            (false, false) => "text",
        },
        "dictionary": dictionary.map(|dictionary| dictionary.id),
        // Absent unless a protocol actually answered, the same discipline
        // `composer` is under: a client must not have to tell "the adapter
        // could have read this pane" from "the adapter did".
        "native": native.map(|read| json!({
            "protocol": native::adapter_for(agent).map(|adapter| adapter.protocol),
            "version": read.version,
            "session": read.session,
        })),
        "image_input": "file-path",
    });
    if let (Some(object), Some(composer)) = (capabilities.as_object_mut(), composer) {
        object.insert("composer".to_owned(), composer);
    }
    capabilities
}

/// Fuzzy path search inside one pane's workspace, for the composer's `@` file
/// mentions.
///
/// Fenced exactly like the asset API: the only directory this can look in is
/// the pane's own working directory as Herdr reports it, canonicalized, and
/// every answer is a path relative to it. A pane whose cwd is not a workspace
/// -- the filesystem root, the home directory, a pane Herdr does not report --
/// is a miss rather than an error, the same way a fenced-out asset path is: it
/// must not be usable to probe the host.
///
/// Paths only. No contents, no sizes, no absolute paths. Reading a file is what
/// `GET /api/assets/{id}/content` is for, and it has its own fence.
pub(crate) async fn pane_files(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<FileSearchQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let limit = query
        .limit
        .unwrap_or(composer::FILE_SEARCH_DEFAULT_LIMIT)
        .clamp(1, composer::FILE_SEARCH_MAX_LIMIT);
    let needle = query.query.clone().unwrap_or_default();

    // The roots come from the same place the asset listing's do -- the pane
    // cwds Herdr reports -- so this endpoint cannot reach a directory the
    // assets API would refuse. Whole-session here (not workspace-scoped): the
    // narrowing below is to one exact pane, which is already narrower than one
    // workspace, so there is nothing for a workspace filter to add.
    let roots = session_asset_roots(&state, &session, None).await;
    let root = roots
        .iter()
        .find(|root| root.pane_id.as_deref() == Some(pane_id.as_str()))
        .and_then(|root| std::fs::canonicalize(&root.path).ok());

    let files = match root.clone() {
        Some(root) => {
            let needle = needle.clone();
            tokio::task::spawn_blocking(move || composer::search_files(&root, &needle, limit))
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "query": needle,
        "limit": limit,
        // The directory every path below is relative to, or null when this pane
        // has no workspace the gateway will look in.
        "root": root.map(|root| root.to_string_lossy().to_string()),
        "files": files
            .into_iter()
            .map(|hit| json!({ "path": hit.path, "name": hit.name, "kind": hit.kind }))
            .collect::<Vec<Value>>(),
    }))))
}

/// The pane's working directory as the fence sees it: the root the asset
/// listing reports for exactly this pane, canonicalized. `None` is "no
/// workspace the gateway will look in", the same answer `pane_files` gives.
pub(crate) async fn pane_fenced_cwd(
    state: &AppState,
    session: &SessionConfig,
    pane_id: &str,
) -> Option<PathBuf> {
    // The pane's own directory, read from the pane list, and only then the
    // fence. It used to be looked up in the asset roots by pane id -- but the
    // roots are one entry per *directory*, carrying the id of the first pane
    // found there. Every other pane in the same checkout matched nothing and
    // was told it was not in a repository: four panes in one repo, one answer
    // and three `repo: null`, while the pane-context badge (which asks by
    // directory) went on counting changes for all four.
    let listed = terminal_backend(session)
        .list_panes()
        .await
        .map(backend::compat::pane_list)
        .ok()
        .and_then(|response| pane_cwd_in_list(&response, pane_id));
    if let Some(path) = listed {
        return std::fs::canonicalize(&path).ok();
    }
    // A backend that could not list its panes just now: what was known before.
    let roots = session_asset_roots(state, session, None).await;
    roots
        .iter()
        .find(|root| root.pane_id.as_deref() == Some(pane_id))
        .and_then(|root| std::fs::canonicalize(&root.path).ok())
}

/// The directory one pane runs in, when it is inside the fence.
///
/// The same two fields and the same fence as `pane_list_roots`, without its
/// de-duplication: that list answers "which directories are worth scanning",
/// this answers "where is *this* pane".
pub(crate) fn pane_cwd_in_list(response: &Value, pane_id: &str) -> Option<PathBuf> {
    let panes = response
        .pointer("/result/panes")
        .and_then(Value::as_array)?;
    let pane = panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))?;
    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| pane.get("foreground_cwd").and_then(Value::as_str))?;
    let path = PathBuf::from(cwd);
    is_scannable_root(&path).then_some(path)
}

/// The checkout a fenced directory belongs to, when that checkout is itself
/// inside the fence. A home directory that is a dotfiles repository has a
/// toplevel `is_scannable_root` refuses, and so answers "not a checkout".
pub(crate) async fn checkout_of(cwd: &FsPath) -> Option<PathBuf> {
    let toplevel = git::toplevel(cwd).await?;
    is_scannable_root(&toplevel).then_some(toplevel)
}

pub(crate) fn git_error(err: git::GitError) -> (StatusCode, Json<Value>) {
    // The detail goes to the log; the client gets a bounded, generic sentence,
    // never git's stderr.
    tracing::warn!("git: {err}");
    match err {
        git::GitError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "git_timeout",
            "git took too long to answer",
        ),
        git::GitError::NotInstalled => api_error(
            StatusCode::NOT_IMPLEMENTED,
            "git_missing",
            "git is not installed on this host",
        ),
        git::GitError::Failed(_) => api_error(StatusCode::BAD_GATEWAY, "git_failed", "git failed"),
    }
}

/// Where this pane is and what runs in it, in one answer.
///
/// The join the app used to make itself out of the pane, `recent-cwds` and
/// the shortcuts: the directory, whether it is inside the fence, the checkout
/// it belongs to with its branch line and changed-file count, and the agent
/// with its declared profile. Capabilities stay on `/api/health`; these are
/// facts about one pane, read on demand and never pushed.
///
/// `git` is `null` for a pane outside any checkout, outside the fence, or
/// whose cwd the backend does not report; `agent` is `null` for a plain
/// shell. Neither is an error: a phone asks this to decide which icons to
/// show, and "nothing to show" is an ordinary answer.
pub(crate) async fn pane_context(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    let pane = terminal_backend(&session)
        .get_pane(&BackendPaneId::new(&pane_id))
        .await
        .ok();
    let fenced = pane_fenced_cwd(&state, &session, &pane_id).await;
    let cwd = pane
        .as_ref()
        .and_then(|pane| pane.cwd.clone())
        .or_else(|| fenced.clone());

    let git = match &fenced {
        Some(dir) => match checkout_of(dir).await {
            Some(toplevel) => git::summary(&toplevel)
                .await
                .map(|summary| summary.to_json())
                .unwrap_or(Value::Null),
            None => Value::Null,
        },
        None => Value::Null,
    };

    let agent = pane
        .filter(|pane| pane.agent.is_some())
        .map(backend::compat::pane_get)
        .map(|pane| {
            let kind = pane["agent"].as_str().unwrap_or("").to_owned();
            json!({
                "kind": kind,
                "status": pane["agent_status"],
                "foreground_command": pane["foreground_command"],
                "profile": shortcuts::is_known_agent(&kind),
            })
        })
        .unwrap_or(Value::Null);

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "cwd": cwd.map(|path| path.to_string_lossy().to_string()),
        "cwd_in_fence": fenced.is_some(),
        "git": git,
        "agent": agent,
    }))))
}

/// What changed in the pane's checkout: the branch line and one entry per
/// file, with line totals, working tree against `HEAD`.
pub(crate) async fn pane_git_status(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    let checkout = match pane_fenced_cwd(&state, &session, &pane_id).await {
        Some(cwd) => checkout_of(&cwd).await,
        None => None,
    };
    let Some(toplevel) = checkout else {
        return Ok(Json(content_envelope(json!({
            "session_id": session_id,
            "pane_id": pane_id,
            "repo": Value::Null,
            "truncated": false,
            "files": [],
        }))));
    };

    let status = git::status(&toplevel).await.map_err(git_error)?;
    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "repo": status.summary.to_json(),
        "truncated": status.truncated,
        "files": status.files.iter().map(git::FileChange::to_json).collect::<Vec<Value>>(),
    }))))
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitDiffQuery {
    pub(crate) path: Option<String>,
    /// The path before a rename or copy, so git sees both sides.
    pub(crate) old_path: Option<String>,
    /// Absent: working tree against `HEAD`. `true`: the index against `HEAD`.
    /// `false`: the working tree against the index.
    pub(crate) staged: Option<bool>,
    pub(crate) context: Option<u32>,
    pub(crate) from: Option<usize>,
    pub(crate) lines: Option<usize>,
}

/// One file's unified patch, one page at a time.
///
/// The path is the only client-supplied value that reaches git, validated
/// first and placed after `--`; see `git::file_patch`. Every number is
/// clamped. A page is cut on a hunk boundary so the next one parses alone.
pub(crate) async fn pane_git_diff(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<GitDiffQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    let path = query.path.unwrap_or_default();
    let old_path = query.old_path.filter(|old| !old.is_empty());
    let valid = git::validate_relative_path(&path).is_some()
        && old_path
            .as_deref()
            .is_none_or(|old| git::validate_relative_path(old).is_some());
    if !valid {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must be relative to the checkout",
        ));
    }
    let side = match query.staged {
        None => git::PatchSide::WorkingTreeVsHead,
        Some(true) => git::PatchSide::Staged,
        Some(false) => git::PatchSide::Unstaged,
    };
    let context = query
        .context
        .unwrap_or(git::DEFAULT_CONTEXT_LINES)
        .min(git::MAX_CONTEXT_LINES);
    let from = query.from.unwrap_or(0);
    let lines = query
        .lines
        .unwrap_or(git::FILE_PATCH_MAX_LINES)
        .clamp(1, git::FILE_PATCH_MAX_LINES);

    let checkout = match pane_fenced_cwd(&state, &session, &pane_id).await {
        Some(cwd) => checkout_of(&cwd).await,
        None => None,
    };
    let Some(toplevel) = checkout else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "no_repository",
            "this pane is not inside a git checkout",
        ));
    };

    let patch = git::file_patch(
        &toplevel,
        &path,
        old_path.as_deref(),
        side,
        context,
        from,
        lines,
    )
    .await
    .map_err(git_error)?;
    let Some(patch) = patch else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "no_such_path",
            "nothing in the checkout has that path",
        ));
    };

    let mut data = patch.to_json();
    data["session_id"] = json!(session_id);
    data["pane_id"] = json!(pane_id);
    Ok(Json(content_envelope(data)))
}

/// Which agents have a key row and command list, and where to add one. Lets a
/// client tell "this agent has no profile yet" from "the gateway is old".
pub(crate) async fn keymaps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(shortcuts::catalog()))
}

/// One pane as Herdr describes it, already unwrapped from the response
/// envelope. A pane the socket cannot answer for is a 502 rather than a guess:
/// every caller here is about to act on what this pane is running.
pub(crate) async fn pane_get(session: &SessionConfig, pane_id: &str) -> ApiResult<Value> {
    let pane = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(backend::compat::pane_get(pane)
        .pointer("/result/pane")
        .cloned()
        .unwrap_or_default())
}

/// The key row and slash commands for whatever this pane is running.
///
/// Resolving this here rather than in the client means a client picks up a new
/// agent when the developer updates the gateway, without shipping a new build.
pub(crate) async fn pane_shortcuts(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let pane = pane_get(&session, &pane_id).await?;
    let pane = &pane;

    // Herdr reports the agent on the pane itself when one is attached; the
    // stripped title is what is left of the terminal title, which is how a
    // full-screen program like an editor announces itself.
    let agent = pane
        .get("agent")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let title = pane
        .get("terminal_title_stripped")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    // The working directory scopes project-local commands, e.g. a repo's own
    // `.claude/commands`.
    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    Ok(agents::routes::json_etag_response(
        &headers,
        shortcuts::resolve(agent, title, cwd),
    ))
}

pub(crate) async fn send_pane_keys(
    session: &SessionConfig,
    pane_id: &str,
    keys: &[String],
) -> ApiResult<Value> {
    terminal_backend(session)
        .send_keys(&BackendPaneId::new(pane_id), keys)
        .await
        .map_err(backend_api_error)?;
    Ok(backend::compat::command_ok("pane_keys_sent"))
}

/// What a pane is drawing right now, as plain text.
///
/// `visible` rather than the transcript source the parts endpoint reads: both
/// callers care about the current screen, and the scrollback of an answered menu
/// or an already-submitted prompt would only mislead them.
pub(crate) async fn read_pane_visible_text(
    session: &SessionConfig,
    pane_id: &str,
) -> ApiResult<String> {
    terminal_backend(session)
        .read_pane(&BackendReadPane {
            pane_id: BackendPaneId::new(pane_id),
            source: BackendOutputSource::Visible,
            format: BackendOutputFormat::Text,
            lines: APPROVAL_READ_LINES,
            start: None,
            end: None,
        })
        .await
        .map(|output| output.text)
        .map_err(backend_api_error)
}

/// Read a pane's agent and whatever menu it is drawing.
pub(crate) async fn read_pane_approval(
    session: &SessionConfig,
    pane_id: &str,
) -> ApiResult<(Option<String>, Option<approvals::Approval>)> {
    let agent = pane_get(session, pane_id)
        .await
        .ok()
        .and_then(|pane| pane.get("agent").and_then(Value::as_str).map(str::to_owned))
        .filter(|agent| !agent.is_empty());
    let text = read_pane_visible_text(session, pane_id).await?;
    Ok((agent, approvals::detect(&text)))
}

/// The payload both the endpoint and the SSE events carry.
pub(crate) fn approval_data(
    session_id: &str,
    pane_id: &str,
    agent: Option<&str>,
    approval: Option<&approvals::Approval>,
    source: &str,
) -> Value {
    json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "state": if approval.is_some() { "pending" } else { "idle" },
        "approval": approval.map(approvals::Approval::to_json),
        // Per-pane capability, in the same shape the parts endpoint answers
        // with. "menu" means the approval was read off what the agent drew, and
        // a client that dislikes the reading still has raw send-keys.
        // "protocol" means the agent reported it and was answered by name, so
        // there is no cursor to race and no keystroke was sent.
        "pane": {
            "pane_id": pane_id,
            "agent": agent,
            "approvals": source,
        },
    })
}

pub(crate) async fn send_text(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendTextBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .send_text(&BackendPaneId::new(pane_id), &body.text, body.mode.into())
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_text_sent")))
}

pub(crate) async fn send_keys(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendKeysBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    if body.keys.is_empty() || body.keys.len() > MAX_SEND_KEYS {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_keys",
            "keys must contain 1 to 32 entries",
        ));
    }
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .send_keys(&BackendPaneId::new(pane_id), &body.keys)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_keys_sent")))
}

/// The scrollback store, or nothing if a previous holder panicked while it was
/// locked.
///
/// A poisoned buffer is not worth failing a read over: the pane's own answer
/// from Herdr is still correct, only shorter. Every caller treats `None` as
/// "this gateway keeps no history", which is exactly what release/0.5.0 did.
pub(crate) fn lock_scrollback(
    state: &AppState,
) -> Option<std::sync::MutexGuard<'_, scrollback::ScrollbackStore>> {
    state.scrollback.lock().ok()
}

#[derive(Deserialize)]
pub(crate) struct OutputQuery {
    pub(crate) source: Option<String>,
    pub(crate) lines: Option<u32>,
    pub(crate) format: Option<String>,
    /// Absolute half-open range. Both or neither; the range wins over `lines`.
    pub(crate) start: Option<u32>,
    pub(crate) end: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct SendTextBody {
    pub(crate) text: String,
    /// Whether this is a paste or a keyboard.
    ///
    /// Absent means paste, which is what `send-text` has always done, so an
    /// app built against any earlier gateway keeps exactly the behaviour it
    /// was written for. An unknown value is also a paste rather than a 400:
    /// a client sending a mode this gateway has never heard of is a client
    /// running ahead of it, and the two ship independently.
    #[serde(default)]
    pub(crate) mode: SendTextRequestMode,
}

/// The wire spelling of [`BackendSendTextMode`].
///
/// Its own type rather than the backend enum with serde on it, because this
/// one has to absorb a value it does not recognise and the backend enum must
/// not have a variant meaning "something else".
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SendTextRequestMode {
    #[default]
    Paste,
    Keys,
    /// Anything this gateway does not know, taken as the default.
    #[serde(other)]
    Unknown,
}

impl From<SendTextRequestMode> for BackendSendTextMode {
    fn from(mode: SendTextRequestMode) -> Self {
        match mode {
            SendTextRequestMode::Keys => Self::Keys,
            SendTextRequestMode::Paste | SendTextRequestMode::Unknown => Self::Paste,
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct SendKeysBody {
    pub(crate) keys: Vec<String>,
}

/// One published approval transition, fanned out to open event streams.
#[derive(Clone, Debug)]
pub(crate) struct ApprovalEvent {
    pub(crate) name: &'static str,
    pub(crate) payload: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateWorkspaceBody {
    pub(crate) cwd: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) focus: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct RenameWorkspaceBody {
    pub(crate) label: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateTabBody {
    pub(crate) workspace_id: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) focus: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct RenameTabBody {
    pub(crate) label: String,
}

#[derive(Deserialize)]
pub(crate) struct RenamePaneBody {
    pub(crate) label: String,
}

#[derive(Deserialize)]
pub(crate) struct SplitPaneBody {
    pub(crate) direction: String,
    pub(crate) ratio: Option<f64>,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) cwd: Option<String>,
    pub(crate) env: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
pub(crate) struct ZoomPaneBody {
    pub(crate) mode: Option<String>,
}

#[cfg(test)]
mod tests {
    use crate::agents::session_routes::native_approval_data;
    use crate::connectivity::routes::revoke_paired_device;
    use crate::*;

    /// A structured agent's approval push names the agent session it is for,
    /// not a placeholder.
    #[test]
    fn an_agent_permission_push_carries_its_agent_session_id() {
        let data = super::agent_permission_push_data("server-1", "ses_42", "perm_1");
        assert_eq!(data["server_id"], "server-1");
        assert_eq!(data["session_id"], "ses_42");
        assert_eq!(data["asid"], "ses_42");
        assert_eq!(data["approval_id"], "perm_1");
        assert_eq!(data["fingerprint"], "perm_1");
        assert_eq!(data["type"], "approval");
    }

    /// A structured agent's question push names the server, the agent and the
    /// agent session it is for, and the form to answer.
    #[test]
    fn an_agent_form_push_carries_its_agent_session_and_form_id() {
        let data = super::agent_form_push_data("server-1", "t3", "ses_child", "form_1");
        assert_eq!(data["type"], "question");
        assert_eq!(data["category"], "question");
        assert_eq!(data["server_id"], "server-1");
        assert_eq!(data["agent_id"], "t3");
        assert_eq!(data["session_id"], "ses_child");
        assert_eq!(data["asid"], "ses_child");
        assert_eq!(data["form_id"], "form_1");
        assert_eq!(data["fingerprint"], "form_1");
    }

    /// One question is one push: T3 re-announces open forms on every thread
    /// snapshot, so a repeat of the same id is not news until it is resolved.
    /// A child session's form is admitted like any other.
    #[test]
    fn a_pending_form_pushes_once_until_it_is_resolved() {
        use crate::agents::{AgentDomainEvent, AgentSessionId, FormRequest};
        let pending = |asid: &str, id: &str| AgentDomainEvent::FormPending {
            asid: AgentSessionId(asid.into()),
            request: FormRequest {
                id: id.into(),
                asid: AgentSessionId(asid.into()),
                title: "Which branch?".into(),
                fields: Vec::new(),
            },
            seq: 1,
        };
        let mut gates = super::AgentPushGates::default();
        let first = pending("ses_child", "form_1");
        assert!(matches!(
            gates.admit(&first),
            Some(super::AgentPendingPush::Form { asid: "ses_child", request }) if request.id == "form_1"
        ));
        assert_eq!(gates.admit(&pending("ses_child", "form_1")), None);
        assert!(gates.admit(&pending("ses_child", "form_2")).is_some());
        assert!(gates.admit(&pending("ses_other", "form_1")).is_some());

        let resolved = AgentDomainEvent::FormResolved {
            asid: AgentSessionId("ses_child".into()),
            form_id: "form_1".into(),
            seq: 2,
        };
        assert_eq!(gates.admit(&resolved), None);
        assert!(gates.admit(&pending("ses_child", "form_1")).is_some());
    }

    /// The question push says "{name} needs your input", naming the session
    /// when it has a title and the agent when it does not.
    #[test]
    fn a_form_push_names_the_session_else_the_agent() {
        use crate::agents::{AgentSessionId, FormRequest};
        let state = test_state("admin", Vec::new());
        let request = FormRequest {
            id: "form_1".into(),
            asid: AgentSessionId("ses_1".into()),
            title: "Which branch?".into(),
            fields: Vec::new(),
        };
        let titled = super::form_notification(&state, "t3", Some("Fix login"), "ses_1", &request);
        assert_eq!(titled.notice, AgentNotice::AgentBlocked);
        assert_eq!(titled.agent_name.as_deref(), Some("Fix login"));
        assert_eq!(titled.detail, None);
        let rendered = titled.render(Locale::default());
        assert_eq!(rendered.body, "Fix login needs your input.");
        assert_eq!(rendered.data["form_id"], "form_1");

        let untitled = super::form_notification(&state, "deepseek", Some("  "), "ses_1", &request);
        assert_eq!(untitled.agent_name.as_deref(), Some("DeepSeek"));
    }

    /// The defect this hub exists for: `activity_stream()` used to be built
    /// once per subscriber, so N phones watching one tmux session meant N
    /// independent polls -- and a tmux poll costs processes, not just sockets.
    #[tokio::test]
    async fn many_subscribers_share_one_activity_stream() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();

        let a = subscribe_activity(&state, &session);
        let b = subscribe_activity(&state, &session);
        let c = subscribe_activity(&state, &session);

        let hubs = state.activity.lock().unwrap();
        assert_eq!(hubs.len(), 1, "three subscribers must not make three hubs");
        assert_eq!(hubs.get(&session.id).unwrap().receiver_count(), 3);
        drop(hubs);
        drop((a, b, c));
    }

    /// `DELETE /api/pairings/{id}` exists to cut off a device somebody no
    /// longer controls. Until this, it cut off everything except the one
    /// channel that actually carries the terminal: the event stream the
    /// device already had open, which was authorised once at connect and then
    /// ran for as long as the phone kept it.
    #[test]
    fn revoking_a_device_makes_the_stream_recheck_fail() {
        let state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        assert!(still_paired(&state, "phone-1"));

        state
            .devices
            .lock()
            .unwrap()
            .retain(|device| device.id != "phone-1");
        assert!(
            !still_paired(&state, "phone-1"),
            "a revoked device must not keep a stream it already had"
        );
    }

    /// End to end: a real event stream, over a live tmux so the stream has a
    /// working backend and cannot end for any other reason, closed by the
    /// revoke route itself.
    ///
    /// `to_bytes` finishes exactly when the body ends, so it is the assertion:
    /// before this change it ran until the timeout, because nothing in the
    /// stream ever asked again whether the device was still allowed to hold
    /// it.
    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn revoking_a_device_closes_the_event_stream_it_already_had() {
        use tower::ServiceExt as _;

        let socket = std::path::PathBuf::from(format!(
            "/tmp/gw-revoke-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        let tmux = backend::TmuxBackend::new(Some(socket.clone()));
        let workspace = tmux
            .create_workspace(&BackendCreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-revoke".into()),
                focus: true,
            })
            .await
            .unwrap();

        let token = "device-token";
        let mut state = test_state("admin", vec![test_device("phone-1", token)]);
        state.config.sessions = vec![SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path: socket.to_string_lossy().into_owned(),
            backend: BackendKind::Tmux,
        }];

        let app = Router::new()
            .route("/api/sessions/{session_id}/events", get(events))
            .with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/default/events")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let revoking = tokio::spawn({
            let state = state.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                revoke_paired_device(
                    State(state),
                    Path("phone-1".to_owned()),
                    bearer_headers(token),
                )
                .await
                .expect("the revoke route should accept a paired device")
            }
        });

        let ended = tokio::time::timeout(
            STREAM_DEVICE_RECHECK_INTERVAL * 3,
            axum::body::to_bytes(response.into_body(), 1 << 20),
        )
        .await;
        drop(revoking.await.unwrap());
        tmux.close_workspace(&workspace.id).await.ok();
        assert!(
            ended.is_ok(),
            "the stream outlived the revocation of the device holding it"
        );
    }

    /// A backend that is down for good rebuilds every `ACTIVITY_REBUILD_DELAY`
    /// forever. Logging each attempt is about thirty-four thousand identical
    /// lines a day, which buries every line that matters.
    #[test]
    fn a_failure_that_is_still_the_same_failure_is_not_news_again() {
        let mut notes = FailureNotes::default();
        let down = "terminal activity failed for session default: no server running";

        assert!(notes.is_news(down), "the first time is always news");
        for _ in 0..10_000 {
            assert!(!notes.is_news(down));
        }

        // A different failure is a different thing to know.
        let other = "terminal activity failed for session default: connection refused";
        assert!(notes.is_news(other));
        assert!(!notes.is_news(other));
        assert!(notes.is_news(down), "and back again");

        // Recovered: the same failure recurring is news, or the log says
        // "down" once and never mentions the next three outages.
        notes.recovered();
        assert!(notes.is_news(down));
    }

    /// Two sessions are two streams; the hub is per session, not global.
    #[tokio::test]
    async fn each_session_gets_its_own_hub() {
        let mut state = test_state("admin", Vec::new());
        let mut second = state.config.sessions[0].clone();
        second.id = "second".into();
        state.config.sessions.push(second.clone());
        let first = state.config.sessions[0].clone();

        let a = subscribe_activity(&state, &first);
        let b = subscribe_activity(&state, &second);
        assert_eq!(state.activity.lock().unwrap().len(), 2);
        drop((a, b));
    }

    /// And the other half of sharing: when the last subscriber goes, the hub
    /// is retired, so a gateway nobody is talking to stops polling.
    #[tokio::test]
    async fn the_hub_retires_when_the_last_subscriber_leaves() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();

        let subscriber = subscribe_activity(&state, &session);
        assert_eq!(state.activity.lock().unwrap().len(), 1);
        drop(subscriber);

        // `retire_activity_hub` is what the producer calls between streams;
        // with nothing listening it removes the entry and reports that it
        // should stop.
        assert!(retire_activity_hub(&state, &session.id));
        assert!(state.activity.lock().unwrap().is_empty());
    }

    /// A subscriber arriving while the producer is deciding to leave must not
    /// be handed a receiver on a hub that then exits.
    #[tokio::test]
    async fn a_hub_with_a_live_subscriber_is_not_retired() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();
        let subscriber = subscribe_activity(&state, &session);

        assert!(!retire_activity_hub(&state, &session.id));
        assert_eq!(state.activity.lock().unwrap().len(), 1);
        drop(subscriber);
    }

    #[test]
    fn an_output_range_needs_both_ends_or_neither() {
        assert!(validate_output_range(None, None).unwrap().is_none());
        assert_eq!(
            validate_output_range(Some(10), Some(20)).unwrap(),
            Some((10, 20))
        );
        assert!(validate_output_range(Some(10), None).is_err());
        assert!(validate_output_range(None, Some(20)).is_err());
    }

    #[test]
    fn an_output_range_must_run_forwards() {
        assert!(validate_output_range(Some(20), Some(20)).is_err());
        assert!(validate_output_range(Some(21), Some(20)).is_err());
    }

    #[test]
    fn an_oversized_output_range_is_trimmed_from_its_start_rather_than_refused() {
        // A reader scrolling toward the top always eventually overreaches. That is
        // an arrival at the top, not a client mistake, so it clamps.
        let (start, end) = validate_output_range(Some(0), Some(50_000))
            .unwrap()
            .unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, MAX_OUTPUT_LINES);
    }

    #[tokio::test]
    async fn app_mount_zoom_is_side_effect_free_for_both_backends() {
        for backend in [BackendKind::Herdr, BackendKind::Tmux] {
            let mut state = test_state("secret", vec![test_device("device-1", "device-token")]);
            state.config.sessions[0].backend = backend;
            state.config.sessions[0].socket_path = String::from("/definitely/not/a/socket");
            let response = zoom_pane(
                State(state),
                Path((String::from("default"), String::from("missing-pane"))),
                bearer_headers("device-token"),
                Json(ZoomPaneBody {
                    mode: Some(String::from("on")),
                }),
            )
            .await
            .unwrap();
            assert_eq!(response.0["result"]["type"], "pane_zoomed");
        }
    }

    #[test]
    fn stream_pane_read_becomes_inline_update() {
        let frame = StreamPaneFrame {
            revision: 42,
            output: "hello\n".into(),
        };
        let encoded = stream_pane_update_payload(&frame, "w1:p2").unwrap();
        let payload: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(frame.revision, 42);
        assert_eq!(payload["event"], "pane_updated");
        assert_eq!(payload["data"]["pane"]["pane_id"], "w1:p2");
        assert_eq!(payload["data"]["pane"]["revision"], 42);
        assert!(payload["data"]["pane"].get("source_revision").is_none());
        assert_eq!(payload["data"]["output"], "hello\n");
    }

    /// A native adapter answers `parts: "native"`, and only when it actually
    /// answered. The distinction matters: an operator who has not pointed the
    /// gateway at an opencode server still gets the dictionary, and a client
    /// must be able to tell "could have" from "did".
    #[test]
    fn a_pane_says_native_only_when_a_protocol_actually_answered() {
        let read = native::NativeRead {
            parts: Vec::new(),
            session: Some("ses_1".into()),
            version: Some("1.18.0".into()),
        };
        let native_pane = pane_capabilities(
            "wA:p1",
            Some("opencode"),
            parts::dictionary_for(Some("opencode")),
            Some(&read),
            None,
        );
        assert_eq!(native_pane["parts"], "native");
        assert_eq!(native_pane["native"]["protocol"], "opencode-server");
        assert_eq!(native_pane["native"]["version"], "1.18.0");
        assert_eq!(native_pane["native"]["session"], "ses_1");
        // The dictionary is still named, because it is still what answers when
        // the server is not up.
        assert_eq!(native_pane["dictionary"], "opencode");

        // Same agent, no endpoint reached: the pane falls back and says so.
        let fallback = pane_capabilities(
            "wA:p1",
            Some("opencode"),
            parts::dictionary_for(Some("opencode")),
            None,
            None,
        );
        assert_eq!(fallback["parts"], "dictionary");
        assert_eq!(fallback["native"], Value::Null);
    }

    /// One shape for both sources: the approval endpoints answer the same keys
    /// whether the request was read off a menu or reported by a protocol, and
    /// `pane.approvals` is the only thing that says which.
    #[test]
    fn a_reported_approval_answers_in_the_same_shape_a_drawn_one_does() {
        let pending = native::NativeApproval {
            adapter: &native::OPENCODE,
            base: "http://127.0.0.1:1".into(),
            session: "ses_1".into(),
            request: parts::ApprovalRequest {
                id: "per_1".into(),
                prompt: "Allow bash?".into(),
                tool: Some("bash".into()),
                context: vec!["echo hi".into()],
                options: vec![parts::ApprovalChoice {
                    index: 1,
                    label: "Approve".into(),
                    decision: "allow",
                }],
            },
        };
        let data = native_approval_data("default", "wM:p1", Some("opencode"), Some(&pending));
        assert_eq!(data["state"], "pending");
        assert_eq!(data["approval"]["approval_id"], "per_1");
        assert_eq!(data["approval"]["options"][0]["decision"], "allow");
        // The label is the gateway's own, so the command in `context` is the
        // only agent-authored text on this payload.
        assert_eq!(data["approval"]["options"][0]["label"], "Approve");
        assert_eq!(data["pane"]["approvals"], "protocol");

        let idle = native_approval_data("default", "wM:p1", Some("opencode"), None);
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["approval"], Value::Null);
        assert_eq!(idle["pane"]["approvals"], "protocol");
    }

    #[test]
    fn a_pane_carries_a_composer_descriptor_only_for_an_agent_with_a_table() {
        let known = pane_capabilities(
            "wA:p1",
            Some("Claude Code"),
            parts::dictionary_for(Some("claude")),
            None,
            composer::descriptor(Some("Claude Code"), None),
        );
        assert_eq!(known["parts"], "dictionary");
        assert_eq!(known["composer"]["table"], "claude");
        assert_eq!(known["composer"]["file_mentions"], true);
        assert!(known["composer"]["slash_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["source"] == "catalog"));

        // An agent with no table carries no key at all -- not a null, which a
        // client would have to tell apart from "no commands".
        let unknown = pane_capabilities(
            "wA:p2",
            Some("aider"),
            parts::dictionary_for(Some("aider")),
            None,
            composer::descriptor(Some("aider"), None),
        );
        assert_eq!(unknown["parts"], "text");
        assert!(unknown.as_object().unwrap().get("composer").is_none());
    }

    /// The file search can only ever look in a root the asset API would also
    /// serve, because it takes its root from the same place: the pane cwds
    /// Herdr reports, filtered by the same "this is not the whole machine"
    /// rule. A pane id that is not in that list has no root to search.
    #[test]
    fn file_search_takes_its_root_from_the_panes_the_session_actually_has() {
        let roots = pane_list_roots(
            "default",
            &json!({ "result": { "panes": [
                { "pane_id": "wA:p1", "cwd": "/Users/dev/src/project", "workspace_id": "wA" },
                { "pane_id": "wA:p2", "cwd": "/" }
            ] } }),
        );
        let root_for = |pane: &str| {
            roots
                .iter()
                .find(|root| root.pane_id.as_deref() == Some(pane))
                .map(|root| root.path.clone())
        };
        assert_eq!(
            root_for("wA:p1"),
            Some(PathBuf::from("/Users/dev/src/project"))
        );
        // The pane sitting at the filesystem root never became a root, so the
        // search has nothing to look in rather than the whole machine.
        assert_eq!(root_for("wA:p2"), None);
        assert_eq!(root_for("wB:p9"), None);
    }

    #[test]
    fn a_file_search_limit_is_clamped_whatever_the_client_asks_for() {
        let clamp = |limit: Option<usize>| {
            limit
                .unwrap_or(composer::FILE_SEARCH_DEFAULT_LIMIT)
                .clamp(1, composer::FILE_SEARCH_MAX_LIMIT)
        };
        assert_eq!(clamp(None), 20);
        assert_eq!(clamp(Some(0)), 1);
        assert_eq!(clamp(Some(5)), 5);
        assert_eq!(clamp(Some(10_000)), 50);
    }

    /// The compatibility contract this field ships under: the app that sends
    /// it and the gateway that understands it are released separately, so both
    /// directions of the mismatch have to be harmless.
    #[test]
    fn the_send_text_mode_defaults_to_paste_and_tolerates_what_it_does_not_know() {
        let parse = |body: &str| serde_json::from_str::<SendTextBody>(body).unwrap();

        // An app built against any earlier gateway sends no mode at all.
        assert_eq!(parse(r#"{"text":"hi"}"#).mode, SendTextRequestMode::Paste);
        assert_eq!(
            BackendSendTextMode::from(parse(r#"{"text":"hi"}"#).mode),
            BackendSendTextMode::Paste
        );

        assert_eq!(
            parse(r#"{"text":"i","mode":"keys"}"#).mode,
            SendTextRequestMode::Keys
        );
        assert_eq!(
            BackendSendTextMode::from(parse(r#"{"text":"i","mode":"keys"}"#).mode),
            BackendSendTextMode::Keys
        );
        assert_eq!(
            parse(r#"{"text":"hi","mode":"paste"}"#).mode,
            SendTextRequestMode::Paste
        );

        // A newer app naming a mode this gateway has never heard of is a
        // client running ahead of it, not a bad request. It gets the old
        // behaviour rather than a 400.
        for unknown in [
            r#"{"text":"hi","mode":"literal"}"#,
            r#"{"text":"hi","mode":"KEYS"}"#,
            r#"{"text":"hi","mode":""}"#,
        ] {
            assert_eq!(
                BackendSendTextMode::from(parse(unknown).mode),
                BackendSendTextMode::Paste,
                "{unknown} should fall back to a paste"
            );
        }

        // A mode of the wrong shape is still a malformed body.
        assert!(serde_json::from_str::<SendTextBody>(r#"{"text":"hi","mode":5}"#).is_err());
    }

    #[tokio::test]
    async fn recent_cwds_lists_the_panes_directories_and_is_not_a_directory_browser() {
        // The picker for spawn. It answers with what the panes are already in
        // and nothing around it: a phone must not be able to walk the host from
        // here, and the list it can pick from is exactly the list `cwd` takes.
        let root = asset_test_dir("recent-cwds");
        let repo = root.join("repo");
        let plain = root.join("notes");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();

        let state = unreachable_state();
        state.assets.lock().unwrap().remember_roots(
            "default",
            None,
            vec![
                AssetRoot {
                    path: repo.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wA".into()),
                    tab_id: Some("wA:t1".into()),
                    pane_id: Some("wA:p1".into()),
                },
                AssetRoot {
                    path: plain.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wB".into()),
                    tab_id: Some("wB:t1".into()),
                    pane_id: Some("wB:p1".into()),
                },
            ],
        );

        let answer = recent_cwds(
            State(state.clone()),
            Path("default".into()),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        let cwds = answer["cwds"].as_array().unwrap();
        assert_eq!(cwds.len(), 2);
        assert_eq!(cwds[0]["path"], repo.to_string_lossy().as_ref());
        assert_eq!(cwds[0]["name"], "repo");
        assert_eq!(cwds[0]["pane_id"], "wA:p1");
        assert_eq!(cwds[0]["workspace_id"], "wA");
        // Which of them is a checkout, because "start an agent here" usually
        // means a repo.
        assert_eq!(cwds[0]["git"], true);
        assert_eq!(cwds[1]["git"], false);
        // Nothing above or below what the panes are in.
        assert!(!cwds
            .iter()
            .any(|entry| entry["path"] == root.to_string_lossy().as_ref()));

        assert_eq!(
            recent_cwds(
                State(state),
                Path("default".into()),
                bearer_headers("not-a-token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The whole point, end to end: Herdr keeps one screen, the gateway watched
    /// four, and the reader can ask for all four.
    #[tokio::test]
    async fn a_zero_backlog_pane_hands_back_more_than_herdr_kept() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } }),
        );

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        // Herdr's own answer is the last screen alone.
        assert_eq!(screens.last().unwrap(), "row 3\nrow 4\nrow 5\nrow 6");
        assert_eq!(served, "row 0\nrow 1\nrow 2\nrow 3\nrow 4\nrow 5\nrow 6");
    }

    /// The bug this pins: a pane the scrollback store is keeping rows for is
    /// exactly the condition the tail-path stitching above exists for, and
    /// `keeps()` is decided from session/pane identity and Herdr's own scroll
    /// telemetry alone -- nothing about it depends on whether the *current*
    /// request happens to be range-addressed. Without gating on that, this
    /// same "kept" pane, read with an explicit `[start, end)`, would come back
    /// windowed by the plain `lines` default (200) rather than sliced to the
    /// requested range, while nothing about the response said so.
    ///
    /// Reuses the exact setup `a_zero_backlog_pane_hands_back_more_than_herdr_kept`
    /// uses to prove stitching *does* widen a tail read for this pane, then
    /// shows a range-addressed read of the same pane is answered with exactly
    /// what Herdr served -- the last screen alone, not the seven-row window.
    #[tokio::test]
    async fn a_range_addressed_read_is_never_widened_by_local_scrollback() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } }),
        );

        // Feed the store the same four repaints that, in the tail-path test,
        // make a fifth plain read come back as all seven kept rows.
        for _ in 0..4 {
            read_output(&state, 240).await;
        }

        let served = read_output_range(&state, 0, 240).await;

        // Herdr's own answer for this read is the last screen alone -- the
        // range-addressed request must get exactly that, not the stitched span.
        assert_eq!(screens.last().unwrap(), "row 3\nrow 4\nrow 5\nrow 6");
        assert_eq!(served, "row 3\nrow 4\nrow 5\nrow 6");
    }

    /// And having kept them, it says so where the reader's affordance looks --
    /// on the pane, not on the output.
    #[tokio::test]
    async fn the_pane_listing_reports_what_was_kept() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        let pane = json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } });
        state.scrollback.lock().unwrap().observe("default", &pane);

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let listing =
            note_and_amend_panes(&state, "default", json!({ "result": { "panes": [pane] } }));

        // Seven rows kept, four of them on screen: three to reach back for.
        assert_eq!(
            listing.pointer("/result/panes/0/scroll/max_offset_from_bottom"),
            Some(&json!(3))
        );
    }

    /// The panes that already worked have to keep working exactly as they did.
    #[tokio::test]
    async fn a_pane_with_scrollback_is_answered_as_herdr_answered_it() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 908, "viewport_rows": 4 } }),
        );

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        assert_eq!(served, screens.last().unwrap().as_str());
    }

    /// And so does a pane nobody has reported on: not knowing is a reason to
    /// stay out of the way.
    #[tokio::test]
    async fn an_unreported_pane_is_never_buffered() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        assert_eq!(served, screens.last().unwrap().as_str());
    }
}
