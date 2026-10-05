//! Gateway and session metadata: capabilities, health facts, and liveness.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::discovery;
use crate::backend::{self, BackendError, BackendFuture, BackendKind, Pane, TerminalBackend};
use crate::{
    backend_endpoint, terminal_backend, ApiResult, AppState, Config, SessionConfig,
    HERDR_PROTOCOL_MIN,
};

/// `metadata()` answers per session, reused for [`SESSION_LIVENESS_TTL`].
///
/// `/health` and `/api/meta` both describe every configured backend, and both
/// asked each one live on every request -- an RPC per backend per call, for an
/// answer that changes when a backend restarts and not otherwise. Every phone
/// polls on its own schedule, so the cost multiplied by devices.
///
/// Held behind the same one-second window liveness already uses, for the same
/// reason: short enough that a backend coming or going shows up inside a
/// second, long enough that a burst of clients asking at once is answered
/// once.
/// One session's answer and when it was taken.
type CachedMetadata = (std::time::Instant, (Value, Value));

pub(crate) const GATEWAY_API_VERSION: &str = "1.9.0";
pub(crate) const GATEWAY_API_MAJOR: u64 = 1;

/// What this gateway *build* can do, independent of which terminal it is
/// attached to. Every entry here is announced unconditionally: the endpoint
/// exists, the code path is compiled in, and no backend can take it away.
///
/// A feature whose availability depends on the terminal on the other side does
/// not belong in this list -- see [`AGENT_COLLABORATION_CAPABILITY`].
pub(crate) const API_CAPABILITIES: &[&str] = &[
    "agent_catalog",
    "agent_events",
    "multi_agent",
    "agent_forms",
    "agent_lifecycle_notifications",
    "agent_mcp",
    "agent_models",
    "agent_permissions",
    "agent_sessions",
    "capabilities_discovery",
    "agent_discovery",
    "agent_spawn",
    "agent_timeline",
    "agent_vcs",
    // `vcs/files`, `vcs/file` and `vcs/discard` on an agent session: git in
    // the session's directory, for every agent.
    "agent_vcs_files",
    "assets",
    "device_revocation",
    "file_uploads",
    "git_diff",
    "one_time_pairing_codes",
    "pane_context",
    "pane_approvals",
    "pane_composer",
    "pane_file_search",
    "pane_interrupt",
    "pane_output_ansi",
    "pane_captured_history",
    "pane_parts",
    "pane_parts_native",
    "pane_shortcuts",
    // The same three Changes routes on a terminal pane, in its cwd.
    "pane_vcs_files",
    "configurable_agent_profiles",
    "per_device_tokens",
    "push_notifications",
    "push_token_revocation",
    "recent_cwds",
    // `GET /api/sessions/{id}/snapshot`, whose `agents` array is the same one
    // `GET .../agents` answers with -- `instance_id` and `target` included --
    // so a client can prewarm a whole session in one call. Announced so the
    // app can ask rather than probe for a 404 and guess at the shape.
    "session_snapshot",
    "tasks",
    "terminal_backends",
    "multiple_terminal_backends",
    "terminal_session_liveness",
    "terminal_input",
    // `planes.terminal.backends[].keyboard` in `/api/discovery`: the key
    // names and chords `send-keys` delivers per backend, and `400
    // key_unsupported` for the rest. Lets the app feature-detect without
    // parsing the planes.
    "terminal_keyboard",
    // `GET /api/ws`: one WebSocket per device carrying the agent events of
    // any number of sessions. Also announced as `transports.websocket` in
    // `/api/discovery`. The per-session SSE stream stays the fallback.
    "ws_events",
];

/// Handing a task to an agent the app did not start, and following it home.
///
/// Not in [`API_CAPABILITIES`], because this gateway build implementing the
/// endpoints is not what decides whether the feature works. Two things the
/// gateway does not control decide it:
///
///  1. **The backend must be Herdr.** An assignment is bound to an agent
///     *instance* -- the identity of a process and its conversation -- and only
///     the Herdr adapter has one to bind to. `TmuxBackend::start_agent` returns
///     `instance_id: None`, and a pane id is not a substitute: panes are reused
///     and renumbered, so a task bound to one can be delivered to whoever took
///     the pane over. tmux sessions keep every ordinary terminal capability;
///     they simply never see this one.
///  2. **That Herdr must be 0.9.0 or newer.** Below it there is no instance
///     identity on the wire at all.
///
/// Announcing it anyway -- which is what the static list did -- told a phone
/// attached to a tmux session that collaboration was available, and left the
/// only honest refusal to happen after the reader had written the task.
///
/// It is answered in two places, and they mean different things:
///
///  * `backends[].capabilities` in `/health` is the precise answer, per
///    session. A client that has chosen a session should read that one.
///  * the gateway-wide `capabilities` array carries it when *any* configured
///    session qualifies. That is what an app too old to read the per-session
///    list sees, and it is the weaker claim on purpose: "somewhere on this
///    machine", not "on the session you are looking at".
pub(crate) const AGENT_COLLABORATION_CAPABILITY: &str = "agent_collaboration";

/// The first Herdr that puts an opaque agent instance id on the wire.
///
/// The same floor `herdr_owns_prompt_submission` uses, and not a coincidence:
/// 0.9.0 is the release that made an agent addressable as something other than
/// the pane it happens to occupy.
pub(crate) const HERDR_COLLABORATION_MIN: (u64, u64, u64) = (0, 9, 0);

/// What one session offers beyond what the build offers, from the three facts
/// `/health` already has about it.
///
/// Pure, so every combination -- including the ones that need a real Herdr or a
/// real tmux server to reach -- is a unit test rather than a manual check.
pub(crate) fn session_capabilities(
    kind: BackendKind,
    connected: bool,
    version: Option<&str>,
) -> Vec<&'static str> {
    if connected
        && kind == BackendKind::Herdr
        && backend::version_at_least(version, HERDR_COLLABORATION_MIN)
    {
        vec![AGENT_COLLABORATION_CAPABILITY]
    } else {
        Vec::new()
    }
}

/// The gateway-wide list: the build's own capabilities, plus collaboration if
/// at least one configured session can actually deliver it.
///
/// Nothing else in the list is conditional, and nothing else should become
/// conditional without the same justification: a capability that comes and goes
/// with a socket is a capability a client has to re-check, and every entry in
/// [`API_CAPABILITIES`] is true for as long as this binary is running.
pub(crate) fn gateway_capabilities(collaboration_somewhere: bool) -> Vec<&'static str> {
    let mut capabilities: Vec<&'static str> = API_CAPABILITIES.to_vec();
    if collaboration_somewhere {
        capabilities.push(AGENT_COLLABORATION_CAPABILITY);
    }
    capabilities
}

/// Whether this device's requests are sealed by the application-layer
/// transport.
///
/// It is a property of the device, not of the gateway: a device paired while
/// `transport_encryption` was `disabled` holds no transport key and is
/// authorised on its bearer token alone, and stays that way after the setting
/// changes -- `require_device` demands the proof header from exactly the
/// devices that have a key. So the honest answer to "is this connection
/// encrypted at the application layer" is about the device asking.
pub(crate) fn device_seals_its_transport(state: &AppState, device_id: &str) -> bool {
    state.devices.lock().is_ok_and(|devices| {
        devices
            .iter()
            .any(|device| device.id == device_id && device.transport_key.is_some())
    })
}

pub(crate) async fn gateway_metadata(
    state: &AppState,
    application_layer_encryption: bool,
) -> ApiResult<Value> {
    // The primary session described below must be the one `GET /api/sessions`
    // leads with, not merely the first configured one: with tmux dead or
    // empty and herdr live, the app connects to whichever session
    // `sessions[0]` names, and `assertSupportedHerdr` has to validate *that*
    // session's metadata, not a stored-order tmux entry that always reports
    // `compatible: true`. See `ordered_sessions`.
    let ordered = ordered_sessions(state).await;
    let primary = ordered.first().copied();
    let mut backends = Vec::with_capacity(state.config.sessions.len());
    let mut primary_metadata = None;
    let mut legacy_herdr = None;
    // Set by any session that can deliver a collaboration task, which is what
    // the gateway-wide list gets to claim. See `AGENT_COLLABORATION_CAPABILITY`
    // for why that claim is weaker than the per-session one beside it.
    let mut collaboration_somewhere = false;
    for session in &state.config.sessions {
        let (metadata, compatibility) = session_metadata(session).await;
        let capabilities = session_capabilities(
            session.backend,
            metadata
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            metadata.get("version").and_then(Value::as_str),
        );
        collaboration_somewhere |= !capabilities.is_empty();
        let metadata = json!({
            "sessionId": session.id,
            "label": session.label,
            "kind": session.backend,
            "connected": metadata.get("connected").cloned().unwrap_or(json!(false)),
            "version": metadata.get("version").cloned().unwrap_or(Value::Null),
            "protocol": metadata.get("protocol").cloned().unwrap_or(Value::Null),
            // Always present, empty included: an app that finds the key knows
            // this gateway answers per session, and can stop guessing at the
            // backend's version itself. An app that does not find it is talking
            // to a gateway that predates the field and falls back to the
            // gateway-wide list.
            "capabilities": capabilities,
        });
        if let Some(primary_session) = primary {
            if session.id == primary_session.id {
                primary_metadata = Some(metadata.clone());
                legacy_herdr = Some(compatibility);
            }
        }
        backends.push(metadata);
    }
    let planes = discovery::build_discovery_planes(state).await;
    Ok(json!({
        "ok": true,
        "gatewayVersion": env!("CARGO_PKG_VERSION"),
        "apiVersion": GATEWAY_API_VERSION,
        "apiMajor": GATEWAY_API_MAJOR,
        "generation": &*state.generation,
        "minimumCompatibleApiVersion": "1.0.0",
        "legacyUnversionedApi": true,
        "capabilities": gateway_capabilities(collaboration_somewhere),
        "planes": planes,
        "serverId": state.config.server_id,
        "label": state.config.label,
        "transportSecurity": {
            "protection": transport_protection(&state.config),
            // Was hardcoded `false`, on a gateway whose default is
            // `transport_encryption: required` and which had just decrypted
            // the request asking. A client that reads this field to decide
            // whether it needs to seal would have read a gateway that does
            // seal as one that does not, and downgraded itself.
            "applicationLayerEncryption": application_layer_encryption,
            "httpsRecommended": !state.config.public_url.starts_with("https://")
        },
        "backend": primary_metadata.unwrap_or(Value::Null),
        "backends": backends,
        "herdr": legacy_herdr.unwrap_or_else(|| json!({ "connected": false }))
    }))
}

pub(crate) fn transport_protection(config: &Config) -> &'static str {
    if config.public_url.starts_with("https://") {
        return "https";
    }
    let Ok(listen) = config.listen.parse::<SocketAddr>() else {
        return "unknown";
    };
    if listen.ip().is_loopback() {
        return "local-only";
    }
    match listen.ip() {
        std::net::IpAddr::V4(ip) if is_tailscale_ipv4(ip) => "tailscale-wireguard",
        _ => "unencrypted-http",
    }
}

pub(crate) fn is_tailscale_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// Whether a Herdr socket protocol is one this gateway will serve.
///
/// Open-ended above [`HERDR_PROTOCOL_MIN`]; see that constant for why.
pub(crate) fn herdr_protocol_supported(protocol: u64) -> bool {
    protocol >= HERDR_PROTOCOL_MIN
}

pub(crate) static SESSION_METADATA_CACHE: std::sync::OnceLock<
    Mutex<HashMap<String, CachedMetadata>>,
> = std::sync::OnceLock::new();

pub(crate) async fn session_metadata(session: &SessionConfig) -> (Value, Value) {
    let cache = SESSION_METADATA_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some((seen, answer)) = cache.get(&session.id) {
            if seen.elapsed() < SESSION_LIVENESS_TTL {
                return answer.clone();
            }
        }
    }
    let answer = session_metadata_uncached(session).await;
    if let Ok(mut cache) = cache.lock() {
        // Bounded by the configured sessions, which is a handful; the sweep is
        // only here so a config reload cannot leave an entry behind for ever.
        cache.retain(|_, (seen, _)| seen.elapsed() < SESSION_LIVENESS_TTL);
        cache.insert(
            session.id.clone(),
            (std::time::Instant::now(), answer.clone()),
        );
    }
    answer
}

pub(crate) async fn session_metadata_uncached(session: &SessionConfig) -> (Value, Value) {
    let backend = terminal_backend(session);
    backend_metadata(session, backend.as_ref()).await
}

/// `connected` is the live answer: the backend's server is up and answers.
///
/// `metadata()` alone is not that on tmux -- it is `tmux -V`, which only says
/// the binary is installed, so a Mac with no tmux server running reported
/// `connected: true` and the app sat on a loader instead of showing that the
/// terminal is unavailable. `probe_reachable` asks the server itself
/// (`list-sessions`, which never starts one) and is what decides. Herdr's
/// `metadata()` is already a socket `ping`, and its probe is the trait default.
/// The answer rides `session_metadata`'s one-second cache, so a server started
/// later reads as connected on the next poll, with no gateway restart.
pub(crate) async fn backend_metadata(
    session: &SessionConfig,
    backend: &dyn TerminalBackend,
) -> (Value, Value) {
    match backend.metadata().await {
        Ok(metadata)
            if !tokio::time::timeout(SESSION_PROBE_TIMEOUT, backend.probe_reachable())
                .await
                .is_ok_and(|reachable| reachable.unwrap_or(true)) =>
        {
            // An ordinary state, not a failure worth a warning on every poll:
            // nobody has started a server yet.
            tracing::debug!(
                "terminal backend for session {} has no running server",
                session.id
            );
            (
                json!({ "kind": metadata.kind, "connected": false, "version": metadata.version }),
                json!({ "connected": false, "error": "Terminal backend is unavailable" }),
            )
        }
        Ok(metadata) => {
            if backend_came_back(&session.id) {
                tracing::info!("terminal backend for session {} answers again", session.id);
            }
            // Asked here so it rides the same short-lived cache as the
            // version: discovery is polled, and on tmux this is a process.
            let keyboard = backend.keyboard(&metadata).await.ok().flatten();
            // A backend that does not report a protocol at all is taken at the
            // floor rather than refused: absent is not the same as too old.
            let compatibility_protocol = metadata.protocol.unwrap_or(HERDR_PROTOCOL_MIN);
            let compatible = metadata.kind == BackendKind::Tmux
                || herdr_protocol_supported(compatibility_protocol);
            let mut compatibility = json!({
                "connected": true,
                "version": metadata.version,
                "protocol": compatibility_protocol,
                "compatible": compatible,
                "supportedProtocolMin": HERDR_PROTOCOL_MIN,
                // `null` is the ceiling: explicitly open-ended, which a client
                // can tell apart from a gateway too old to send the field.
                "supportedProtocolMax": Value::Null,
            });
            if let (Some(object), Some(response)) = (
                compatibility.as_object_mut(),
                metadata.compatibility_response,
            ) {
                object.insert("response".into(), response);
            }
            (
                json!({
                    "kind": metadata.kind,
                    "connected": true,
                    "version": metadata.version,
                    "protocol": metadata.protocol,
                    "keyboard": keyboard,
                }),
                compatibility,
            )
        }
        Err(err) => {
            // Discovery polls this about once a second per client, so a backend
            // that stays down would write the same warning every second for as
            // long as it is down. Warned once when it goes down; debug after.
            if backend_went_down(&session.id) {
                tracing::warn!(
                    "terminal metadata request failed for session {} (backend={}, endpoint={}): {err}; \
                     further failures are logged at debug until it answers again",
                    session.id,
                    session.backend.as_str(),
                    backend_endpoint(session),
                );
            } else {
                tracing::debug!(
                    "terminal metadata request failed for session {}: {err}",
                    session.id
                );
            }
            (
                json!({ "kind": session.backend, "connected": false }),
                json!({ "connected": false, "error": "Terminal backend is unavailable" }),
            )
        }
    }
}

/// Sessions whose backend failed its last metadata request, so a backend that
/// stays down is warned about once rather than on every discovery poll.
static DOWN_SESSIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether this failure is the first since the session's backend last answered.
fn backend_went_down(session_id: &str) -> bool {
    let Ok(mut down) = DOWN_SESSIONS.lock() else {
        return true;
    };
    if down.iter().any(|seen| seen == session_id) {
        return false;
    }
    down.push(session_id.to_owned());
    true
}

/// Whether this answer ends a run of failures for the session's backend.
fn backend_came_back(session_id: &str) -> bool {
    let Ok(mut down) = DOWN_SESSIONS.lock() else {
        return false;
    };
    let before = down.len();
    down.retain(|seen| seen != session_id);
    down.len() != before
}

/// How much of "there's something to actually look at" a configured session
/// offers, most useful first. `GET /api/sessions` orders by this instead of a
/// fixed backend rank, because the app reads `sessions[0]` and shows no
/// picker -- whichever backend actually has something in it is the one that
/// needs to be first, and only the gateway is in a position to know which
/// that is on any given request.
///
/// Declared top-to-bottom in the order it should sort, so deriving `Ord`
/// gives exactly the "most significant first" comparison the endpoint wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SessionLiveness {
    HasPanes,
    Empty,
    Unreachable,
}

/// How long `GET /api/sessions` waits on a single backend before counting it
/// as unreachable. A dead socket that refuses the connection resolves this
/// fast on its own; this bound exists for the backend that accepts a
/// connection and then never answers, which would otherwise hang the whole
/// endpoint on one dead session.
pub(crate) const SESSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Turn a `list_panes` call into a liveness bucket, bounded by `timeout`.
///
/// Takes the future rather than a backend or a `SessionConfig` so it can be
/// exercised with a canned outcome -- immediate success with or without
/// panes, immediate failure, or a future that never resolves -- without
/// standing up a real Herdr or tmux backend.
pub(crate) async fn session_liveness(
    list_panes: BackendFuture<'_, Vec<Pane>>,
    timeout: Duration,
) -> SessionLiveness {
    match tokio::time::timeout(timeout, list_panes).await {
        Ok(Ok(panes)) if !panes.is_empty() => SessionLiveness::HasPanes,
        Ok(Ok(_)) => SessionLiveness::Empty,
        Ok(Err(_)) | Err(_) => SessionLiveness::Unreachable,
    }
}

/// `GET /api/sessions` ordering key: liveness first (has panes, then
/// empty-but-reachable, then unreachable), and the order they appear in the
/// config as the tiebreaker between otherwise-equal entries.
///
/// The tiebreaker used to be the backend kind, tmux always ahead of herdr, and
/// that made `muqun-gateway backend default` a lie: it moves an entry to the
/// front of `config.sessions`, which nothing then read. On a machine where
/// both backends are live -- which is what fixing tmux made ordinary -- the
/// reader had no way at all to say which one their phone should open, and the
/// answer moved on them whenever a probe was slow.
///
/// Config order is the reader's own statement of preference, so it is what
/// breaks the tie. Liveness still outranks it: a session that is down does not
/// get to be first just because somebody once preferred it.
///
/// Pure and independent of any backend, so every bucket -- including "every
/// configured session is unreachable" -- gets a fast, deterministic unit
/// test instead of depending on a live tmux or Herdr server.
pub(crate) fn session_order_key(
    index: usize,
    liveness: SessionLiveness,
) -> (SessionLiveness, usize) {
    (liveness, index)
}

/// How long a liveness verdict is reused before the backends are probed again.
///
/// `GET /api/sessions` and `/health` both order by liveness, every phone asks
/// on its own schedule, and every ask costs each configured backend two
/// commands -- which on tmux are two processes. Five devices polling twelve
/// seconds apart therefore probed roughly twice a second between them, for an
/// answer that is the same answer.
///
/// Short enough that a backend going down is reflected within a second, which
/// is far inside the window any client would notice; long enough that a burst
/// of clients asking at once is answered once.
pub(crate) const SESSION_LIVENESS_TTL: Duration = Duration::from_millis(1000);

/// Every configured session, ordered exactly as `GET /api/sessions` presents
/// them: liveness first, config position as the tiebreaker.
///
/// Shared with `gateway_metadata` so the "primary" session it describes for
/// `/health` and `/api/meta` is always the same one a client that reads
/// `sessions[0]` from `/api/sessions` actually connects to. Two copies of
/// this ordering agreeing by construction is the only way to keep them
/// agreeing at all -- a second, independently-written rank is a second place
/// for the two endpoints to drift apart.
///
/// Orders, never filters: every configured session stays in the result, even
/// when nothing is reachable, so a client that reads only the first entry
/// never sees `undefined` where it used to see a session.
pub(crate) async fn ordered_sessions(state: &AppState) -> Vec<&SessionConfig> {
    if let Some(order) = state
        .session_liveness
        .lock()
        .ok()
        .and_then(|cache| cache.fresh(Instant::now(), SESSION_LIVENESS_TTL))
    {
        // Recorded as indices rather than ids so a config reload cannot make a
        // stale entry name a session that is no longer there.
        if order.len() == state.config.sessions.len() {
            return order
                .into_iter()
                .filter_map(|index| state.config.sessions.get(index))
                .collect();
        }
    }
    let liveness =
        futures::future::join_all(state.config.sessions.iter().map(|session| async move {
            let backend = terminal_backend(session);
            // `probe_reachable` catches the one case `list_panes` cannot tell
            // apart on its own: tmux's `list_output` deliberately maps "no
            // server running" onto an empty pane list for every other
            // caller, which would otherwise make a dead tmux backend
            // indistinguishable from a live one that holds nothing. Both
            // calls run inside the one probe `session_liveness` bounds, so a
            // dead backend is still discovered by a single timeout, not two.
            let probe = async {
                match backend.probe_reachable().await {
                    Ok(false) => Err(BackendError::Unavailable),
                    _ => backend.list_panes().await,
                }
            };
            session_liveness(Box::pin(probe), SESSION_PROBE_TIMEOUT).await
        }))
        .await;
    let mut order: Vec<usize> = (0..state.config.sessions.len()).collect();
    order.sort_by_key(|&index| session_order_key(index, liveness[index]));
    if let Ok(mut cache) = state.session_liveness.lock() {
        cache.reachable = state
            .config
            .sessions
            .iter()
            .zip(&liveness)
            .map(|(session, live)| (session.id.clone(), *live != SessionLiveness::Unreachable))
            .collect();
        cache.record(order.clone(), Instant::now());
    }
    order
        .into_iter()
        .map(|index| &state.config.sessions[index])
        .collect()
}

/// The last liveness ordering and when it was taken. See
/// [`SESSION_LIVENESS_TTL`].
#[derive(Default)]
pub(crate) struct SessionLivenessCache {
    pub(crate) taken: Option<(Instant, Vec<usize>)>,
    pub(crate) reachable: HashMap<String, bool>,
}

impl SessionLivenessCache {
    pub(crate) fn fresh(&self, now: Instant, ttl: Duration) -> Option<Vec<usize>> {
        self.taken
            .as_ref()
            .filter(|(at, _)| now.duration_since(*at) < ttl)
            .map(|(_, order)| order.clone())
    }

    pub(crate) fn record(&mut self, order: Vec<usize>, now: Instant) {
        self.taken = Some((now, order));
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn a_liveness_verdict_is_reused_only_inside_its_window() {
        // Five phones polling on their own schedules asked every backend the
        // same question about twice a second between them, and on tmux each
        // ask is two processes.
        let mut cache = SessionLivenessCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.fresh(t0, SESSION_LIVENESS_TTL), None);

        cache.record(vec![1, 0], t0);
        assert_eq!(
            cache.fresh(t0 + Duration::from_millis(500), SESSION_LIVENESS_TTL),
            Some(vec![1, 0])
        );
        // Past the window the backends are asked again, so a session that
        // actually went down is reflected rather than remembered.
        assert_eq!(
            cache.fresh(t0 + SESSION_LIVENESS_TTL, SESSION_LIVENESS_TTL),
            None
        );
    }

    #[test]
    fn liveness_outranks_config_position_in_the_sessions_ordering_key() {
        use SessionLiveness::{Empty, HasPanes, Unreachable};
        // Liveness is the significant digit and config position is only the
        // tiebreaker: index 0 is the reader's preferred entry, and even so,
        // being down loses to a live session further down the list.
        assert!(session_order_key(1, HasPanes) < session_order_key(0, Empty));
        assert!(session_order_key(1, HasPanes) < session_order_key(0, Unreachable));
        assert!(session_order_key(1, Empty) < session_order_key(0, Unreachable));
    }

    /// The behaviour `muqun-gateway backend default` promises, and did not
    /// have: whichever entry the reader put first wins a tie, whatever kind of
    /// backend it is. Replaces a test that asserted tmux always won, which is
    /// what made the command a no-op.
    #[test]
    fn the_configured_order_breaks_ties_at_every_liveness_level() {
        for liveness in [
            SessionLiveness::HasPanes,
            SessionLiveness::Empty,
            SessionLiveness::Unreachable,
        ] {
            assert!(
                session_order_key(0, liveness) < session_order_key(1, liveness),
                "the first configured session should win a tie when both are {liveness:?}"
            );
        }
    }

    /// Sorting a full session list by the key, including every configured
    /// session staying present when none of them are reachable -- ordering
    /// must never turn into filtering.
    #[test]
    fn sorting_by_the_key_orders_without_dropping_anyone() {
        let sessions = [
            liveness_session("herdr-empty", BackendKind::Herdr),
            liveness_session("tmux-dead", BackendKind::Tmux),
            liveness_session("herdr-live", BackendKind::Herdr),
            liveness_session("tmux-empty", BackendKind::Tmux),
        ];
        let liveness = [
            SessionLiveness::Empty,
            SessionLiveness::Unreachable,
            SessionLiveness::HasPanes,
            SessionLiveness::Empty,
        ];
        let mut order: Vec<usize> = (0..sessions.len()).collect();
        order.sort_by_key(|&index| session_order_key(index, liveness[index]));
        let ids: Vec<&str> = order
            .iter()
            .map(|&index| sessions[index].id.as_str())
            .collect();
        // Liveness first; among the two equally-empty entries the one
        // configured earlier wins, which is the reader's stated preference
        // rather than a rule about backend kinds.
        assert_eq!(
            ids,
            vec!["herdr-live", "herdr-empty", "tmux-empty", "tmux-dead"]
        );
        assert_eq!(
            order.len(),
            sessions.len(),
            "every session stays in the list"
        );
    }

    /// All-unreachable is the case that matters most: a client reading only
    /// `sessions[0]` must still get a session back, not `undefined`.
    #[test]
    fn every_session_survives_when_all_are_unreachable() {
        let sessions = [
            liveness_session("a", BackendKind::Herdr),
            liveness_session("b", BackendKind::Tmux),
            liveness_session("c", BackendKind::Herdr),
        ];
        let mut order: Vec<usize> = (0..sessions.len()).collect();
        order.sort_by_key(|&index| session_order_key(index, SessionLiveness::Unreachable));
        assert_eq!(order.len(), 3);
        // All equally unreachable, so the configured order stands -- ordering
        // must never turn into filtering, and it must not reshuffle either.
        let ids: Vec<&str> = order
            .iter()
            .map(|&index| sessions[index].id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn session_liveness_reports_panes_present() {
        let panes = vec![Pane {
            id: BackendPaneId::new("p1"),
            terminal_id: None,
            workspace_id: BackendWorkspaceId::new("w1"),
            tab_id: BackendTabId::new("t1"),
            label: None,
            terminal_title: None,
            cwd: None,
            focused: false,
            width: None,
            height: None,
            revision: None,
            foreground_command: None,
            agent: None,
            agent_status: BackendAgentStatus::Unknown,
            max_offset_from_bottom: None,
            viewport_rows: None,
            alternate_on: None,
            cursor_x: None,
            cursor_y: None,
        }];
        let outcome = session_liveness(
            Box::pin(async move { Ok(panes) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::HasPanes);
    }

    #[tokio::test]
    async fn session_liveness_reports_reachable_but_empty() {
        let outcome = session_liveness(
            Box::pin(async { Ok(Vec::new()) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Empty);
    }

    #[tokio::test]
    async fn session_liveness_reports_unreachable_on_a_backend_error() {
        let outcome = session_liveness(
            Box::pin(async { Err(BackendError::Unavailable) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Unreachable);
    }

    /// The case a failed connect does not cover: a backend that accepts and
    /// then never answers must not hang `GET /api/sessions` -- it has to be
    /// discovered by timeout.
    #[tokio::test]
    async fn session_liveness_reports_unreachable_on_timeout_without_waiting_for_the_probe() {
        let outcome = session_liveness(
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(Vec::new())
            }),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Unreachable);
    }

    /// End to end through the real handler: a herdr session that actually has
    /// a pane outranks a tmux session configured but not running -- the
    /// motivating regression for this card, where the app reads
    /// `sessions[0]` and the old static tmux-first order pointed it at the
    /// dead backend.
    #[tokio::test]
    async fn sessions_endpoint_puts_a_live_backend_ahead_of_a_configured_but_dead_one() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            herdr.session("herdr-live"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["herdr-live", "tmux-dead"]);
        assert_eq!(response.0["sessions"][0]["connected"], true);
        assert_eq!(response.0["sessions"][1]["connected"], false);
    }

    /// The dual-backend defect the final review caught: with tmux dead (or
    /// merely empty) and herdr live, the app connects to whichever session
    /// `GET /api/sessions` leads with -- but it validates that connection
    /// against the metadata `/health`/`/api/meta` describe as "primary".
    /// Before this fix `gateway_metadata` always read
    /// `config.sessions.first()`, which is stored order (tmux-first) and
    /// ignores liveness entirely, so it kept describing the dead tmux
    /// session -- whose `session_metadata` hardcodes `compatible: true` --
    /// while the app was actually talking to herdr. The version fence
    /// `assertSupportedHerdr` exists to enforce was defeated for exactly this
    /// configuration. `gateway_metadata`'s primary must agree with
    /// `sessions()`'s first entry.
    #[tokio::test]
    async fn gateway_metadata_primary_agrees_with_the_sessions_endpoint() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            herdr.session("herdr-live"),
        ];

        let metadata = gateway_metadata(&state, false).await.unwrap();
        assert_eq!(metadata["backend"]["sessionId"], "herdr-live");
        assert_eq!(metadata["backend"]["kind"], json!(BackendKind::Herdr));
    }

    /// Ordering must never turn into filtering: every configured session is
    /// still in the response when none of them are reachable, so a client
    /// reading `sessions[0]` finds a session object instead of `undefined`.
    #[tokio::test]
    async fn sessions_endpoint_keeps_every_session_when_nothing_is_reachable() {
        let mut state = unreachable_state();
        state.config.sessions.push(SessionConfig {
            id: "tmux-also-dead".into(),
            label: "tmux".into(),
            socket_path: std::env::temp_dir()
                .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            backend: BackendKind::Tmux,
        });
        let configured = state.config.sessions.len();
        let preferred = state.config.sessions[0].id.clone();

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        let ids = session_ids(&response.0);
        assert_eq!(ids.len(), configured, "no session drops out of the list");
        // Both are genuinely SessionLiveness::Unreachable here -- the herdr
        // entry via a refused connection, the tmux entry via
        // `probe_reachable` catching the same "no such file or directory"
        // that `list_output` would otherwise fold into an empty topology
        // (see `probe_reachable_tells_no_server_apart_from_list_panes_reporting_empty`
        // in `backend/tmux.rs`). So this genuinely exercises the tiebreak
        // between two unreachable entries, not an accident of tmux
        // misreporting as merely empty -- and the tiebreak is now the order
        // the reader configured, so the first entry stays first.
        assert_eq!(ids[0], preferred);
    }

    /// The regression `sessions_endpoint_keeps_every_session_when_nothing_is_reachable`
    /// could not have caught on its own: before `probe_reachable`, a tmux
    /// session pointed at a socket nothing is listening on classified as
    /// `SessionLiveness::Empty` (via `list_output`'s "no server is an empty
    /// topology" masking), not `Unreachable`. That happened to still sort
    /// tmux first against an `Unreachable` herdr entry -- Empty outranks
    /// Unreachable regardless of the tiebreak -- so the bug was invisible
    /// there. Pairing the dead tmux session with a *reachable-but-empty*
    /// herdr session instead exposes it directly: a herdr session that is
    /// genuinely `Empty` must outrank a tmux session that is genuinely
    /// `Unreachable`, which only holds if tmux's "no server" case is actually
    /// classified as `Unreachable` and not conflated with `Empty`.
    #[tokio::test]
    async fn sessions_endpoint_ranks_a_reachable_empty_backend_ahead_of_a_dead_tmux_one() {
        let empty_herdr = FakePaneListHerdr::start(json!([]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            empty_herdr.session("herdr-empty"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["herdr-empty", "tmux-dead"]);
    }

    /// A reachable session with no panes open still outranks an unreachable
    /// one, and still trails a session that actually has something in it.
    #[tokio::test]
    async fn sessions_endpoint_ranks_reachable_empty_between_live_and_dead() {
        let empty_herdr = FakePaneListHerdr::start(json!([]));
        let busy_herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            empty_herdr.session("empty"),
            SessionConfig {
                id: "dead".into(),
                label: "dead".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Herdr,
            },
            busy_herdr.session("busy"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["busy", "empty", "dead"]);
    }

    /// Same regression as `sessions_endpoint_puts_a_live_backend_ahead_of_a_configured_but_dead_one`,
    /// but against a genuinely live tmux server instead of a fake -- on a
    /// private socket this test creates and owns, never the developer's
    /// default tmux server. Requires `tmux` on `PATH` and permission to
    /// create a Unix socket, so it is `--ignored` like the other isolated
    /// tmux contract tests.
    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn sessions_endpoint_puts_a_live_isolated_tmux_session_ahead_of_a_dead_herdr_one() {
        if tokio::process::Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .is_err()
        {
            eprintln!("skipping: no tmux on PATH");
            return;
        }
        let socket_path = short_test_socket("gw-live");
        let tmux = backend::TmuxBackend::new(Some(socket_path.clone()));
        let workspace = tmux
            .create_workspace(&BackendCreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-sessions-live".into()),
                focus: true,
            })
            .await
            .unwrap();

        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "herdr-dead".into(),
                label: "herdr".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Herdr,
            },
            SessionConfig {
                id: "tmux-live".into(),
                label: "tmux".into(),
                socket_path: socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Tmux,
            },
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["tmux-live", "herdr-dead"]);

        tmux.close_workspace(&workspace.id).await.unwrap();
    }

    /// `transportSecurity.applicationLayerEncryption` was hardcoded `false`,
    /// on a gateway whose default is `transport_encryption: required` and
    /// which had just decrypted the request asking the question. A client
    /// reading this field to decide whether it needs to seal would have read
    /// a gateway that does seal as one that does not.
    ///
    /// It is a property of the *device*: one paired while encryption was
    /// disabled holds no transport key, is authorised on its bearer token
    /// alone, and stays that way after the setting changes.
    #[tokio::test]
    async fn the_metadata_says_whether_this_device_actually_seals_its_requests() {
        let sealed_token = "sealed-device";
        let plain_token = "plain-device";
        let mut sealed = test_device("phone-sealed", sealed_token);
        sealed.transport_key = Some(generate_token());
        let plain = test_device("phone-plain", plain_token);
        let state = test_state("admin", vec![sealed, plain]);

        assert!(device_seals_its_transport(&state, "phone-sealed"));
        assert!(!device_seals_its_transport(&state, "phone-plain"));
        assert!(!device_seals_its_transport(&state, "phone-gone"));

        for encrypted in [true, false] {
            let metadata = gateway_metadata(&state, encrypted).await.unwrap();
            assert_eq!(
                metadata["transportSecurity"]["applicationLayerEncryption"],
                json!(encrypted)
            );
        }
    }

    #[test]
    fn transport_metadata_distinguishes_tls_tailscale_and_plain_http() {
        let mut config = test_config("token");
        config.public_url = "https://host.tailnet.ts.net".into();
        config.listen = "127.0.0.1:23847".into();
        assert_eq!(transport_protection(&config), "https");

        config.public_url = "http://host.tailnet.ts.net:23847".into();
        config.listen = "100.118.124.50:23847".into();
        assert_eq!(transport_protection(&config), "tailscale-wireguard");

        config.listen = "0.0.0.0:23847".into();
        assert_eq!(transport_protection(&config), "unencrypted-http");
    }

    /// Collaboration is the one capability that is not a property of this
    /// build, so it is the one capability that has to be earned per session.
    #[test]
    fn collaboration_is_announced_only_for_a_connected_modern_herdr() {
        assert_eq!(
            session_capabilities(BackendKind::Herdr, true, Some("0.9.0")),
            vec![AGENT_COLLABORATION_CAPABILITY]
        );
        for version in ["v0.9.1", "0.10.0", "1.0.0", "0.9.0+build"] {
            assert_eq!(
                session_capabilities(BackendKind::Herdr, true, Some(version)),
                vec![AGENT_COLLABORATION_CAPABILITY],
                "{version} should carry collaboration"
            );
        }

        // A Herdr too old to put an instance id on the wire, and a version
        // string nobody can read, are both refused rather than guessed at.
        for version in [
            Some("0.8.9"),
            Some("0.9.0-rc.1"),
            Some("0.9"),
            Some("x"),
            None,
        ] {
            assert!(
                session_capabilities(BackendKind::Herdr, true, version).is_empty(),
                "{version:?} should not carry collaboration"
            );
        }

        // tmux keeps every other capability and never gains this one: it has no
        // agent instance identity to bind an assignment to.
        for version in [Some("3.6"), Some("99.0.0"), None] {
            assert!(
                session_capabilities(BackendKind::Tmux, true, version).is_empty(),
                "tmux {version:?} should not carry collaboration"
            );
        }

        // A backend that is not answering cannot deliver anything, whatever
        // version it reported the last time it did.
        assert!(session_capabilities(BackendKind::Herdr, false, Some("0.9.0")).is_empty());
    }

    /// The gateway-wide list is the weaker, older-app-facing claim: it says
    /// "somewhere on this machine", and it must not disturb anything else.
    #[test]
    fn the_gateway_wide_list_adds_collaboration_and_changes_nothing_else() {
        let without = gateway_capabilities(false);
        let with = gateway_capabilities(true);

        assert!(!without.contains(&AGENT_COLLABORATION_CAPABILITY));
        assert!(with.contains(&AGENT_COLLABORATION_CAPABILITY));
        assert!(
            !API_CAPABILITIES.contains(&AGENT_COLLABORATION_CAPABILITY),
            "collaboration must not be static: a tmux-only gateway would announce it"
        );

        // Every other capability is unconditional, and a tmux-only machine must
        // lose exactly one thing by being tmux-only.
        for capability in API_CAPABILITIES {
            assert!(without.contains(capability), "{capability} went missing");
            assert!(with.contains(capability), "{capability} went missing");
        }
        assert_eq!(without.len(), API_CAPABILITIES.len());
        assert_eq!(with.len(), API_CAPABILITIES.len() + 1);

        // Spawning is not collaboration. It runs on tmux -- `start_agent` there
        // types the command and waits for the pane to show the agent -- so it
        // stays in the static list and a tmux-only gateway still offers it.
        assert!(without.contains(&"agent_spawn"));
    }

    #[test]
    fn herdr_compatibility_has_a_floor_and_no_ceiling() {
        // The floor is the actual contract: below it the JSON API is a
        // different shape, so it stays frozen here on purpose.
        assert_eq!(HERDR_PROTOCOL_MIN, 17);
        assert!(!herdr_protocol_supported(1));
        assert!(!herdr_protocol_supported(16));

        // Both Herdr releases in play today.
        assert!(herdr_protocol_supported(17)); // 0.7.5
        assert!(herdr_protocol_supported(19)); // 0.8.0

        // And everything above them. A Herdr newer than this build has ever
        // seen is still served -- the protocol number tracks TUI wire changes
        // the gateway never speaks, so a bump is not evidence of a break.
        assert!(herdr_protocol_supported(20));
        assert!(herdr_protocol_supported(99));
        assert!(herdr_protocol_supported(u64::MAX));
    }

    /// A stand-in tmux whose "server" is a marker file: `list-sessions` and
    /// `show-options` fail with tmux's own no-server message until something
    /// creates it, and only `new-session`/`start-server` would. Every call is
    /// logged so a test can tell exactly what the gateway ran.
    struct FakeTmux {
        dir: std::path::PathBuf,
    }

    impl FakeTmux {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("fake-tmux-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let script = format!(
                "#!/bin/sh\ndir='{}'\necho \"$*\" >> \"$dir/log\"\n\
                 case \"$1\" in\n  -V) echo 'tmux 3.5a'; exit 0;;\n  \
                 new-session|start-server) : >> \"$dir/server\"; exit 0;;\nesac\n\
                 if [ ! -f \"$dir/server\" ]; then\n  \
                 echo 'no server running on /tmp/tmux-501/default' >&2; exit 1\nfi\n\
                 case \"$1\" in\n  list-sessions) cat \"$dir/sessions\" 2>/dev/null; exit 0;;\n  \
                 show-options) echo 'extended-keys on'; exit 0;;\nesac\nexit 1\n",
                dir.display()
            );
            let binary = dir.join("tmux");
            std::fs::write(&binary, script).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir }
        }

        fn start_server(&self, sessions: &str) {
            std::fs::write(self.dir.join("server"), "").unwrap();
            std::fs::write(self.dir.join("sessions"), sessions).unwrap();
        }

        fn server_running(&self) -> bool {
            self.dir.join("server").exists()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
        }

        async fn connected(&self) -> bool {
            let backend = crate::backend::TmuxBackend::with_binary(self.dir.join("tmux"), None);
            let session = SessionConfig {
                id: "tmux".into(),
                label: "tmux".into(),
                socket_path: String::new(),
                backend: BackendKind::Tmux,
            };
            let (metadata, compatibility) = backend_metadata(&session, &backend).await;
            assert_eq!(metadata["connected"], compatibility["connected"]);
            metadata["connected"].as_bool().unwrap()
        }
    }

    impl Drop for FakeTmux {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn tmux_without_a_server_is_not_connected_even_though_the_binary_answers() {
        let tmux = FakeTmux::new();
        assert!(!tmux.connected().await);
    }

    #[tokio::test]
    async fn tmux_with_a_server_and_no_sessions_is_connected() {
        let tmux = FakeTmux::new();
        tmux.start_server("");
        assert!(tmux.connected().await);
    }

    #[tokio::test]
    async fn tmux_with_a_server_and_sessions_is_connected() {
        let tmux = FakeTmux::new();
        tmux.start_server("main\nwork\n");
        assert!(tmux.connected().await);
    }

    #[tokio::test]
    async fn asking_whether_tmux_is_connected_never_starts_a_server() {
        let tmux = FakeTmux::new();
        assert!(!tmux.connected().await);
        assert!(!tmux.server_running(), "the probe started a server");
        let log = tmux.log();
        assert!(log.contains("list-sessions"), "{log}");
        for starts in ["new-session", "start-server", "new "] {
            assert!(!log.contains(starts), "ran {starts:?}: {log}");
        }
    }

    #[tokio::test]
    async fn a_tmux_server_started_later_reads_as_connected_without_a_restart() {
        let tmux = FakeTmux::new();
        assert!(!tmux.connected().await);
        tmux.start_server("main\n");
        assert!(tmux.connected().await);
    }

    /// The same against a real tmux on a private socket: not connected, and
    /// asking left no socket behind, so no server was started.
    #[tokio::test]
    async fn a_real_tmux_with_no_server_is_not_connected_and_stays_serverless() {
        if tokio::process::Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let socket = crate::short_test_socket("gw-conn");
        let session = SessionConfig {
            id: "tmux".into(),
            label: "tmux".into(),
            socket_path: socket.to_string_lossy().into_owned(),
            backend: BackendKind::Tmux,
        };
        let (metadata, _) = session_metadata_uncached(&session).await;
        assert_eq!(metadata["connected"], false);
        assert!(!socket.exists(), "discovery started a tmux server");
    }

    #[test]
    fn a_backend_that_stays_down_is_warned_about_once() {
        let session = "metadata-test-down-once";
        assert!(super::backend_went_down(session));
        assert!(!super::backend_went_down(session));
        assert!(!super::backend_went_down(session));
        assert!(super::backend_came_back(session));
        assert!(!super::backend_came_back(session));
        assert!(
            super::backend_went_down(session),
            "a second outage is warned about again"
        );
        assert!(super::backend_came_back(session));
    }
}
