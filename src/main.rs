#![recursion_limit = "256"]

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::path::Path as FsPath;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use std::time::SystemTime;

use anyhow::Context as _;
#[cfg(test)]
use axum::body::Body;
#[cfg(test)]
use axum::extract::State;
#[cfg(test)]
use axum::extract::{Path, Query};
#[cfg(test)]
use axum::http::HeaderMap;
#[cfg(test)]
use axum::http::Request;
use axum::http::StatusCode;
#[cfg(test)]
use axum::middleware;
#[cfg(test)]
use axum::response::Response;
#[cfg(test)]
use axum::routing::{get, post};
use axum::Json;
#[cfg(test)]
use axum::Router;
#[cfg(test)]
use base64::Engine as _;
use clap::Parser;
#[cfg(test)]
use qrcode::{EcLevel, QrCode};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
#[cfg(test)]
use tower_http::compression::predicate::{DefaultPredicate, SizeAbove};

pub(crate) mod agents;
pub(crate) mod cli;
pub(crate) mod connectivity;
pub(crate) mod platform;
pub(crate) mod terminal;

// Backward-compatible re-exports at crate root
pub(crate) use agents::{agent_events, approvals, tasks};
pub(crate) use cli::*;
pub(crate) use connectivity::push::*;
pub(crate) use connectivity::{authority, gateway_listener, transport};
pub(crate) use platform::assets::*;
pub(crate) use platform::config::*;
pub(crate) use platform::http::*;
pub(crate) use platform::manage::*;
pub(crate) use platform::metadata::*;
#[cfg(test)]
pub(crate) use platform::openapi_spec;
#[allow(unused_imports)]
pub(crate) use platform::routes::{
    api_capabilities, api_discovery, api_meta, api_set_label, docs, health, openapi_json,
};
pub(crate) use platform::server::*;
pub(crate) use platform::setup::*;
pub(crate) use platform::store::*;
pub(crate) use platform::uploads::*;
pub(crate) use platform::{discovery, git, i18n, parts, state_lock};
pub(crate) use terminal::factory::*;
pub(crate) use terminal::routes::*;
pub(crate) use terminal::{
    backend, backend_startup, command_catalog, composer, login_env, native, scrollback, shortcuts,
    supervision,
};

#[cfg(test)]
use crate::i18n::Locale;
use authority::{hash_token, identify_device, DeviceRecord, PendingPairing};
#[cfg(test)]
use backend::AgentStatus as BackendAgentStatus;
#[cfg(test)]
use backend::BackendKind;
#[cfg(test)]
use backend::TerminalBackend;
use backend::{
    BackendError, CreateTab as BackendCreateTab, CreateWorkspace as BackendCreateWorkspace,
    OutputFormat as BackendOutputFormat, OutputSource as BackendOutputSource, Pane,
    PaneId as BackendPaneId, ReadPane as BackendReadPane, SendTextMode as BackendSendTextMode,
    SplitDirection as BackendSplitDirection, SplitPane as BackendSplitPane,
    StartAgent as BackendStartAgent, TabId as BackendTabId, WorkspaceId as BackendWorkspaceId,
    WorktreeRequest as BackendWorktreeRequest,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config: Config,
    pub(crate) pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
    pub(crate) pairing_requests: Arc<Mutex<VecDeque<u128>>>,
    pub(crate) push_tokens: Arc<Mutex<Vec<PushTokenRecord>>>,
    pub(crate) devices: Arc<Mutex<Vec<DeviceRecord>>>,
    pub(crate) assets: Arc<Mutex<AssetIndex>>,
    /// What panes with no scrollback of their own showed while the gateway was
    /// watching. Memory only, and only for those panes; see `scrollback`.
    pub(crate) scrollback: Arc<Mutex<scrollback::ScrollbackStore>>,
    /// The agent status transitions this gateway saw, so a phone coming back
    /// after a while can be told what happened. Memory only; see
    /// `agent_events`.
    pub(crate) agent_events: Arc<Mutex<agent_events::AgentEventLog>>,
    pub(crate) approval_events: tokio::sync::broadcast::Sender<ApprovalEvent>,
    /// One activity stream per session, shared by everyone who wants it. See
    /// [`subscribe_activity`].
    pub(crate) activity:
        Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<SessionActivity>>>>,
    /// The last backend liveness ordering, reused briefly so a burst of
    /// clients asking at once is answered once. See [`SESSION_LIVENESS_TTL`].
    pub(crate) session_liveness: Arc<Mutex<SessionLivenessCache>>,
    /// The OpenCode engine, which comes and goes: it is discovered, adopted or
    /// started, and re-attached whenever it moves. Routes ask it for the
    /// current manager rather than holding one.
    pub(crate) agent_runtime: Arc<agents::AgentRuntime>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Before any subcommand, because every one of them either spawns a backend
    // program or writes down how to. An init system starts this process with an
    // environment that is not the user's -- and so does `ssh host muqun-gateway
    // setup`, and a `cron` line. See `login_env`.
    //
    // Before the runtime, deliberately. `adopt` writes to the process
    // environment, and setting an environment variable while another thread
    // may be reading one is the data race that made `set_var` unsafe in
    // edition 2024. Here nothing else exists yet: no worker threads, no tasks,
    // just this one thread and its arguments.
    //
    // On stderr rather than stdout: for `run` this is the gateway log, which is
    // where it is wanted, and for a command a human is watching it says nothing
    // at all, because a shell has already given the process everything.
    for note in login_env::adopt() {
        eprintln!("environment repaired from the login shell -- {note}");
    }
    init_tracing();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(dispatch(cli))
}

#[cfg(test)]
pub(crate) use crate::agents::session_routes::*;
#[cfg(test)]
pub(crate) use crate::connectivity::routes::*;

pub(crate) type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;

#[cfg(test)]
mod tests {
    use authority::PairingCodeError;

    #[test]
    fn every_pane_in_one_checkout_resolves_to_its_directory() {
        // Two panes in the same directory. The asset roots keep one entry for
        // it, under the first pane's id; the second pane must still be found.
        let list = json!({ "result": { "panes": [
            { "pane_id": "w1:p1", "cwd": "/work/team/app" },
            { "pane_id": "w1:p2", "cwd": "/work/team/app" },
            { "pane_id": "w1:p3", "foreground_cwd": "/work/team/api" },
            { "pane_id": "w1:p4", "cwd": "/" },
        ] } });
        assert_eq!(pane_list_roots("s", &list).len(), 2);
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p1"),
            Some(PathBuf::from("/work/team/app"))
        );
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p2"),
            Some(PathBuf::from("/work/team/app"))
        );
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p3"),
            Some(PathBuf::from("/work/team/api"))
        );
        // Outside the fence, and unknown: no directory, so no git is run.
        assert_eq!(pane_cwd_in_list(&list, "w1:p4"), None);
        assert_eq!(pane_cwd_in_list(&list, "w9:p9"), None);
    }

    use super::*;
    use axum::http::HeaderValue;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn agent_splits_preserve_a_readable_terminal() {
        for rows in [0, 5, 20, 40, 47] {
            assert!(!can_split_agent_pane(Some(rows)));
        }
        for rows in [48, 80, 120] {
            assert!(can_split_agent_pane(Some(rows)));
        }
        assert!(can_split_agent_pane(None)); // Preserve older backend behavior.
    }

    #[test]
    fn startup_refusals_keep_their_actionable_codes() {
        for code in [
            "agent_not_ready",
            "agent_start_failed",
            "agent_start_timeout",
        ] {
            let error = backend_call_error(
                "agent.start",
                BackendError::Refused {
                    code: Some(code.to_owned()),
                    message: "startup detail".to_owned(),
                },
            );
            assert_eq!(error.code(), code);
            assert!(!error.can_retry_prompt());
        }
    }

    #[test]
    fn prompt_retry_requires_proof_nothing_was_submitted() {
        assert!(!HerdrCallError::Unavailable("response lost".into()).can_retry_prompt());
        assert!(!HerdrCallError::Malformed("agent.prompt".into()).can_retry_prompt());
        for code in [
            "agent_blocked",
            "timeout",
            "agent_prompt_stalled",
            "unknown",
        ] {
            assert!(!HerdrCallError::Herdr {
                method: "agent.prompt".into(),
                error: json!({ "code": code }),
            }
            .can_retry_prompt());
        }
        assert!(HerdrCallError::Herdr {
            method: "agent.prompt".into(),
            error: json!({ "code": "agent_not_found" }),
        }
        .can_retry_prompt());
    }

    fn test_config(token: &str) -> Config {
        Config {
            server_id: "server-1".into(),
            label: "test".into(),
            autostart_backends: Vec::new(),
            listen: "127.0.0.1:23100".into(),
            public_url: "http://127.0.0.1:23100".into(),
            token_hash: hash_token(token),
            transport_encryption: TransportEncryptionMode::Required,
            dev_unauthenticated: false,
            sessions: vec![SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: "/tmp/herdr.sock".into(),
                backend: BackendKind::Herdr,
            }],
            agent_commands: BTreeMap::new(),
            rich_agent_pushes: false,
            opencode: agents::OpencodeConfig::default(),
            deepseek: agents::DeepseekConfig::default(),
        }
    }

    #[test]
    fn backend_autostart_is_explicit_and_old_configs_stay_off() {
        let mut config = test_config("test");
        let old = serde_json::to_value(&config).unwrap();
        assert!(old.get("autostart_backends").is_none());
        assert!(serde_json::from_value::<Config>(old)
            .unwrap()
            .autostart_backends
            .is_empty());
        config.autostart_backends.push("default".into());
        let loaded: Config =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(loaded.autostart_backends, ["default"]);
    }

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

    /// The proof header is the encrypted-transport middleware talking to the
    /// handlers below it, and nothing else may put words in its mouth.
    ///
    /// Before this was stripped on the way in, a client could send
    /// `x-muqun-internal-device-proof` itself over cleartext and be taken for
    /// the encrypted device whose transport key it named -- having just put
    /// that key on the wire in the clear to do it, which is precisely what the
    /// encrypted transport exists to prevent.
    #[tokio::test]
    async fn a_client_cannot_forge_the_transport_proof_header() {
        use axum::routing::get;
        use tower::ServiceExt;

        let state = test_state("admin", Vec::new());
        let app = Router::new()
            .route(
                "/probe",
                get(|headers: axum::http::HeaderMap| async move {
                    // What the handlers below the middleware would see.
                    headers
                        .get(TRANSPORT_PROOF_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("absent")
                        .to_string()
                }),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                encrypted_transport,
            ))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(TRANSPORT_PROOF_HEADER, "a-transport-key-i-do-not-own")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "absent",
            "a client-supplied device proof must never reach a handler"
        );
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

    /// A device that was never paired is not paired now either -- the recheck
    /// is an identity test, not a "did anything change" test.
    #[test]
    fn a_device_that_was_never_paired_is_not_still_paired() {
        let state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        assert!(!still_paired(&state, "phone-2"));
        assert!(!still_paired(&state, ""));
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

    fn test_device(id: &str, token: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: format!("device {id}"),
            token_hash: hash_token(token),
            transport_key: None,
            paired_unix_ms: 1_000,
            // Fresh enough that require_device will not flush to disk.
            last_seen_unix_ms: now_unix_ms(),
            install_id: None,
        }
    }

    fn test_state(admin_token: &str, devices: Vec<DeviceRecord>) -> AppState {
        AppState {
            config: test_config(admin_token),
            pending_pairing: Arc::new(Mutex::new(None)),
            pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
            push_tokens: Arc::new(Mutex::new(Vec::new())),
            devices: Arc::new(Mutex::new(devices)),
            assets: Arc::new(Mutex::new(AssetIndex::default())),
            scrollback: Arc::new(Mutex::new(scrollback::ScrollbackStore::default())),
            agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
            approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
            activity: Arc::new(Mutex::new(HashMap::new())),
            session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
            agent_runtime: agents::AgentRuntime::disabled(),
        }
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    fn encrypted_test_request(device_id: &str, material: &[u8], token: &str) -> Request<Body> {
        let payload = EncryptedRequestPayload {
            token: token.into(),
            content_type: Some("application/json".into()),
            body: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"ok":true}"#),
        };
        let envelope = transport::seal(
            material,
            transport::Direction::Request,
            b"POST /api/test",
            &serde_json::to_vec(&payload).unwrap(),
            now_unix_ms(),
        )
        .unwrap();
        Request::builder()
            .method("POST")
            .uri("/api/test")
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, device_id)
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap()
    }

    /// Seal a body of `size` bytes for `path` exactly the way the app does,
    /// and answer how many bytes that puts on the wire.
    fn sealed_wire_len(material: &[u8], token: &str, path: &str, size: usize) -> usize {
        let payload = EncryptedRequestPayload {
            token: token.into(),
            content_type: Some("multipart/form-data; boundary=x".into()),
            body: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vec![0_u8; size]),
        };
        let envelope = transport::seal(
            material,
            transport::Direction::Request,
            format!("POST {path}").as_bytes(),
            &serde_json::to_vec(&payload).unwrap(),
            now_unix_ms(),
        )
        .unwrap();
        serde_json::to_vec(&envelope).unwrap().len()
    }

    /// The gateway advertises a 25 MiB upload limit and answers 413 with "the
    /// upload must be at most 25 MiB". Over its own encrypted transport --
    /// which is on by default -- it could not actually accept one: the
    /// middleware buffered the sealed bytes against a ceiling sized for the
    /// plaintext, and a 25 MiB file is about 44 MiB sealed.
    #[test]
    fn a_maximum_upload_fits_through_the_encrypted_transport() {
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();

        // Measured at a size that is quick to seal rather than at the ceiling
        // itself. The expansion is affine -- base64, JSON, seal, base64 -- so
        // one sample fixes the slope, and `MAX_UPLOAD_BYTES` is exactly 25 of
        // these. The exact version is
        // `a_literal_maximum_upload_seals_within_the_ceiling`, kept ignored
        // because sealing 25 MiB in a debug build takes twelve seconds.
        const SAMPLE: usize = 1024 * 1024;
        let sampled = sealed_wire_len(&material, "device-token", UPLOADS_PATH, SAMPLE);
        assert!(sampled <= sealed_body_ceiling(SAMPLE));

        let at_maximum = sampled * (MAX_UPLOAD_BYTES / SAMPLE);
        let previous = MAX_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES;
        assert!(
            at_maximum > previous,
            "a {MAX_UPLOAD_BYTES}-byte upload seals to about {at_maximum} bytes, \
             which the old {previous}-byte ceiling should not have fit"
        );
        assert!(
            at_maximum <= sealed_body_ceiling(MAX_UPLOAD_BYTES),
            "sealed about {at_maximum} bytes against a ceiling of {}",
            sealed_body_ceiling(MAX_UPLOAD_BYTES)
        );
    }

    #[test]
    #[ignore = "seals 25 MiB; slow in a debug build"]
    fn a_literal_maximum_upload_seals_within_the_ceiling() {
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let wire = sealed_wire_len(&material, "device-token", UPLOADS_PATH, MAX_UPLOAD_BYTES);
        assert!(
            wire > MAX_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES,
            "sealed to {wire} bytes"
        );
        assert!(
            wire <= sealed_body_ceiling(MAX_UPLOAD_BYTES),
            "sealed {wire} bytes against a ceiling of {}",
            sealed_body_ceiling(MAX_UPLOAD_BYTES)
        );
    }

    /// And the other half: every route that is not the upload route is held to
    /// the same 128 KiB the router holds it to in the clear. The middleware
    /// runs outside those limits, so before this it let any encrypted request
    /// on any route buffer 25 MiB.
    #[test]
    fn every_other_route_is_held_to_the_small_body_limit() {
        assert_eq!(plaintext_body_limit(UPLOADS_PATH), MAX_UPLOAD_BYTES);
        for path in [
            "/api/sessions/default/panes/%1/send-text",
            "/api/pair/claim",
            "/api/uploads/",
            "/api/uploadsx",
            "/health",
        ] {
            assert_eq!(
                plaintext_body_limit(path),
                MAX_REQUEST_BODY_BYTES,
                "{path} should get the small limit"
            );
        }
        assert!(
            sealed_body_ceiling(MAX_REQUEST_BODY_BYTES) < MAX_UPLOAD_BYTES,
            "the non-upload ceiling must be far under what it used to be"
        );
    }

    /// An over-limit body says so, instead of arriving as a corrupt envelope.
    #[tokio::test]
    async fn an_oversized_encrypted_body_is_refused_as_too_large() {
        let token = "device-token";
        let transport_key = generate_token();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);

        let request = Request::builder()
            .method("POST")
            .uri("/api/sessions/default/panes/%1/send-text")
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, "phone-1")
            .body(Body::from(vec![
                b'x';
                sealed_body_ceiling(MAX_REQUEST_BODY_BYTES)
                    + 1
            ]))
            .unwrap();
        let (status, body) = decrypt_transport_request(&state, request)
            .await
            .expect_err("an over-limit body must be refused");
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body.0["error"]["code"], "body_too_large");
    }

    #[tokio::test]
    async fn a_stolen_bearer_token_is_not_a_device_transport_credential() {
        let token = "device-token";
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);
        assert!(require_device(&state, &bearer_headers(token)).is_err());

        let stolen_token_material = base64::engine::general_purpose::STANDARD
            .decode(hash_token(token))
            .unwrap();
        let rejected = decrypt_transport_request(
            &state,
            encrypted_test_request("phone-1", &stolen_token_material, token),
        )
        .await;
        assert!(rejected.is_err());

        let (request, _, _, _) =
            decrypt_transport_request(&state, encrypted_test_request("phone-1", &material, token))
                .await
                .unwrap();
        assert_eq!(bearer_token(request.headers()).unwrap(), token);
        assert_eq!(
            require_device(&state, request.headers()).unwrap(),
            "phone-1"
        );
    }

    /// The decrypted request carries the stream context a sealing handler
    /// needs, and records sealed under it open exactly the way the app's
    /// decryptor is specified to: derive from (device key, sid, request
    /// nonce), AAD of request AAD + sid + seq, nonce = seq.
    #[tokio::test]
    async fn an_encrypted_request_leaves_a_stream_context_the_sealer_honours() {
        let token = "device-token";
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);

        let (request, _, aad, nonce) =
            decrypt_transport_request(&state, encrypted_test_request("phone-1", &material, token))
                .await
                .unwrap();
        let context = request
            .extensions()
            .get::<EncryptedStreamContext>()
            .expect("stream context is injected for every encrypted request")
            .clone();
        assert_eq!(context.request_aad, aad);
        assert_eq!(context.request_nonce, nonce);
        assert_eq!(context.material, material);

        let mut sealer = EventStreamSealer::new(&context).unwrap();
        let first: Value =
            serde_json::from_str(&sealer.seal_record("herdr", "{\"n\":1}").unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&sealer.seal_record("approval.pending", "{}").unwrap()).unwrap();
        assert_eq!(first["v"], 1);
        assert_eq!(first["seq"], 0);
        assert_eq!(second["seq"], 1);
        let sid = first["sid"].as_str().unwrap();
        assert_eq!(second["sid"].as_str().unwrap(), sid);

        let key = transport::derive_stream_key(&material, sid, &nonce).unwrap();
        let open = |record: &Value| {
            let seq = record["seq"].as_u64().unwrap();
            let aad = format!("{}\n{}\n{}", aad, sid, seq);
            transport::open_stream_event(
                &key,
                seq,
                aad.as_bytes(),
                record["ciphertext"].as_str().unwrap(),
            )
        };
        let inner: Value = serde_json::from_slice(&open(&first).unwrap()).unwrap();
        assert_eq!(inner["event"], "herdr");
        assert_eq!(inner["data"], "{\"n\":1}");
        let inner: Value = serde_json::from_slice(&open(&second).unwrap()).unwrap();
        assert_eq!(inner["event"], "approval.pending");

        // A record moved to another slot in the stream never opens: the seq is
        // in both the nonce and the AAD, so reorder and replay both fail.
        let replayed = json!({
            "v": 1, "sid": sid, "seq": 1,
            "ciphertext": first["ciphertext"].as_str().unwrap(),
        });
        assert!(open(&replayed).is_err());
    }

    /// What compression will and will not touch.
    ///
    /// The two that matter are a stream and an upload. Compressing
    /// `text/event-stream` would buffer frames that exist to arrive one at a
    /// time, and an uploaded image is already compressed, so gzip spends CPU
    /// to make it slightly bigger.
    #[test]
    fn compression_leaves_streams_uploads_and_small_bodies_alone() {
        use tower_http::compression::predicate::Predicate;

        let predicate = DefaultPredicate::new().and(SizeAbove::new(COMPRESSION_MIN_BYTES));
        // A real body: `SizeAbove` reads the body's own size hint, not the
        // header, so an empty body with a large content-length is still small.
        let response = |content_type: &str, len: usize| {
            Response::builder()
                .header(axum::http::header::CONTENT_TYPE, content_type)
                .body(Body::from(vec![b'x'; len]))
                .unwrap()
        };
        let big = COMPRESSION_MIN_BYTES as usize * 40;

        assert!(
            !predicate.should_compress(&response("text/event-stream", big)),
            "an SSE stream is never compressed, however long"
        );
        for image in ["image/png", "image/jpeg", "image/webp"] {
            assert!(
                !predicate.should_compress(&response(image, big)),
                "{image} is already compressed"
            );
        }
        assert!(
            !predicate.should_compress(&response("application/json", 64)),
            "a body under the floor is not worth a gzip header"
        );
        assert!(
            predicate.should_compress(&response("application/json", big)),
            "a real JSON payload is exactly what this is for"
        );
        assert_eq!(COMPRESSION_MIN_BYTES, 512);
    }

    /// The sealed transport is left exactly as it was.
    ///
    /// Compression sits inside the envelope, so without this gate an encrypted
    /// response would be gzipped and then sealed -- and the client, which sees
    /// base64 and a `content-encoding: gzip` carried in the envelope's own
    /// headers, would try to inflate ciphertext. Compressing inside the
    /// envelope is worth doing, but only once the client says it understands
    /// the flag; until then an encrypted request is answered as it is today.
    #[tokio::test]
    async fn the_gate_keeps_compression_off_a_sealed_response() {
        use axum::routing::get;
        use tower::ServiceExt;

        // What a handler below the gate sees.
        let app = Router::new()
            .route(
                "/probe",
                get(|headers: HeaderMap| async move {
                    headers
                        .get(axum::http::header::ACCEPT_ENCODING)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("absent")
                        .to_string()
                }),
            )
            .layer(middleware::from_fn(envelope_compression_gate));

        // A cleartext request keeps its Accept-Encoding: it is compressed the
        // ordinary way and the client's HTTP stack inflates it.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(axum::http::header::ACCEPT_ENCODING, "gzip, br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::VARY)
                .and_then(|v| v.to_str().ok()),
            Some("accept-encoding"),
            "the answer varies by what was asked for, compressed or not"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "gzip, br");

        // A request that arrived sealed has it taken away, so the compression
        // layer below declines and the envelope seals plaintext.
        let mut request = Request::builder()
            .uri("/probe")
            .header(axum::http::header::ACCEPT_ENCODING, "gzip, br")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(EncryptedStreamContext {
            material: vec![0u8; 32],
            request_aad: "aad".to_string(),
            request_nonce: "nonce".to_string(),
        });
        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "absent",
            "a sealed request is answered uncompressed"
        );
    }

    /// Compression inside the envelope, and only for a client that asked.
    ///
    /// The envelope inflates what it seals by about 1.78x, and it seals before
    /// anything could compress -- so this is the one place that cost can be
    /// paid back. But the client is reading a base64 body whose real headers
    /// are sealed with it, so it cannot use `content-encoding` the ordinary
    /// way: it says what it can inflate with its own request header, and the
    /// payload answers with its own field.
    #[tokio::test]
    async fn the_envelope_compresses_only_for_a_client_that_asked_for_gzip() {
        use axum::routing::get;
        use tower::ServiceExt;

        let app = Router::new()
            .route(
                "/probe",
                get(|headers: HeaderMap| async move {
                    headers
                        .get(axum::http::header::ACCEPT_ENCODING)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("absent")
                        .to_string()
                }),
            )
            .layer(middleware::from_fn(envelope_compression_gate));

        let sealed_request = |accept: Option<&str>| {
            let mut request = Request::builder().uri("/probe");
            request = request.header(axum::http::header::ACCEPT_ENCODING, "gzip, br, zstd");
            if let Some(accept) = accept {
                request = request.header(ENVELOPE_ACCEPT_HEADER, accept);
            }
            let mut request = request.body(Body::empty()).unwrap();
            request.extensions_mut().insert(EncryptedStreamContext {
                material: vec![0u8; 32],
                request_aad: "aad".to_string(),
                request_nonce: "nonce".to_string(),
            });
            request
        };
        let seen = |app: Router, request: Request<Body>| async move {
            let response = app.oneshot(request).await.unwrap();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            String::from_utf8_lossy(&body).to_string()
        };

        // A client that says nothing is answered exactly as before.
        assert_eq!(seen(app.clone(), sealed_request(None)).await, "absent");

        // One that asks for gzip gets gzip -- and only gzip, though it also
        // sent br and zstd in the ordinary header: the app fails a response
        // encoded any other way, so the choice is pinned rather than passed on.
        assert_eq!(
            seen(app.clone(), sealed_request(Some("gzip"))).await,
            "gzip"
        );
        assert_eq!(
            seen(app.clone(), sealed_request(Some(" GZIP , br"))).await,
            "gzip",
            "the name is matched without case or spacing mattering"
        );

        // Something else entirely is not an opt-in.
        assert_eq!(seen(app, sealed_request(Some("br"))).await, "absent");
    }

    /// Where the flag goes, and what it must not do to the headers map.
    #[test]
    fn the_sealed_payload_carries_its_encoding_beside_the_headers() {
        let payload = EncryptedResponsePayload {
            status: 200,
            headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
            body: "…".to_string(),
            content_encoding: Some("gzip".to_string()),
        };
        let value = serde_json::to_value(&payload).expect("serializes");
        assert_eq!(
            value["content_encoding"], "gzip",
            "a top-level field, so a client finds it without walking the header map"
        );
        assert!(
            value["headers"].get("content-encoding").is_none(),
            "and never left in the headers, or the client would inflate twice"
        );

        // An uncompressed body says nothing at all, so an old client sees the
        // payload it has always seen.
        let plain = EncryptedResponsePayload {
            status: 200,
            headers: BTreeMap::new(),
            body: "…".to_string(),
            content_encoding: None,
        };
        let value = serde_json::to_value(&plain).expect("serializes");
        assert!(value.get("content_encoding").is_none());
    }

    /// The agent stream is sealed on an encrypted deployment, and plain on a
    /// cleartext one -- the same rule, and the same record shape, as the
    /// terminal stream.
    ///
    /// It used to be neither: the agent stream went out in the clear whatever
    /// the deployment, so a gateway configured `transport_encryption:
    /// required` put the device token and every agent event on the wire
    /// unprotected. `stream_event` is the single decision point for both
    /// streams, so this asks it both ways.
    #[tokio::test]
    async fn the_agent_stream_is_sealed_exactly_when_the_device_is_encrypted() {
        let token = "device-token";
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);

        let (request, _, aad, nonce) =
            decrypt_transport_request(&state, encrypted_test_request("phone-1", &material, token))
                .await
                .unwrap();
        let context = request
            .extensions()
            .get::<EncryptedStreamContext>()
            .expect("an encrypted request carries a stream context")
            .clone();

        // What the agent stream emits: a `connected` hello, then a domain
        // event under its own name.
        let mut sealed = Some(EventStreamSealer::new(&context).unwrap());
        let hello = sealed
            .as_mut()
            .unwrap()
            .seal_record("connected", r#"{"asid":"ses_1"}"#)
            .unwrap();
        let record: Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(record["seq"], 0);
        let sid = record["sid"].as_str().unwrap().to_string();

        let upsert = sealed
            .as_mut()
            .unwrap()
            .seal_record("agent.timeline.upsert", r#"{"items":[]}"#)
            .unwrap();
        let record: Value = serde_json::from_str(&upsert).unwrap();
        assert_eq!(record["seq"], 1, "one stream, one counter");
        assert_eq!(record["sid"].as_str().unwrap(), sid);

        // And it opens to exactly what the plaintext stream would have sent.
        let key = transport::derive_stream_key(&material, &sid, &nonce).unwrap();
        let opened = transport::open_stream_event(
            &key,
            1,
            format!("{}\n{}\n{}", aad, sid, 1).as_bytes(),
            record["ciphertext"].as_str().unwrap(),
        )
        .unwrap();
        let inner: Value = serde_json::from_slice(&opened).unwrap();
        assert_eq!(inner["event"], "agent.timeline.upsert");
        assert_eq!(inner["data"], r#"{"items":[]}"#);

        // A device paired without a transport key keeps the plaintext stream,
        // byte for byte: no sealer, no envelope, the event under its own name.
        let mut plain: Option<EventStreamSealer> = None;
        let event = stream_event(&mut plain, "agent.timeline.upsert", r#"{"items":[]}"#)
            .expect("a cleartext stream still emits");
        let wire = format!("{event:?}");
        assert!(
            wire.contains("agent.timeline.upsert"),
            "the event keeps its own name on a cleartext deployment: {wire}"
        );
        assert!(
            !wire.contains(ENCRYPTED_SSE_EVENT),
            "and is not wrapped in the sealed envelope: {wire}"
        );
    }

    /// A paired device's headers with the app's locale header on them, which is
    /// what every real request from the app carries.
    fn locale_headers(token: &str, locale: &str) -> HeaderMap {
        let mut headers = bearer_headers(token);
        headers.insert(
            axum::http::HeaderName::from_static(i18n::LOCALE_HEADER),
            HeaderValue::from_str(locale).unwrap(),
        );
        headers
    }

    fn error_body(refusal: &(StatusCode, Json<Value>)) -> Value {
        refusal.1 .0.clone()
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

    /// The refusal a request reads is in the language it asked for, and the
    /// `code` beside it is not.
    ///
    /// This goes through `require_device` rather than through `api_error`
    /// directly because the interesting part is the *ambient* locale: no handler
    /// and no helper on this path takes a `Locale` argument, and the answer
    /// still changes language. That is the whole mechanism, asserted end to end.
    #[tokio::test]
    async fn a_refusal_is_written_in_the_language_the_request_asked_for() {
        let state = test_state("admin-token", vec![test_device("device-1", "token-1")]);

        let english = i18n::scope(Locale::En, async {
            require_device(&state, &locale_headers("wrong", "en")).unwrap_err()
        })
        .await;
        let chinese = i18n::scope(Locale::ZhTw, async {
            require_device(&state, &locale_headers("wrong", "zh-TW")).unwrap_err()
        })
        .await;

        assert_eq!(english.0, chinese.0, "the status is not prose");
        let english = error_body(&english);
        let chinese = error_body(&chinese);
        assert_eq!(english["error"]["code"], "invalid_token");
        assert_eq!(
            english["error"]["code"], chinese["error"]["code"],
            "a client dispatches on the code, so it has no language"
        );
        assert_eq!(english["error"]["message"], "invalid token");
        assert_eq!(chinese["error"]["message"], "token 無效");
    }

    /// Outside a request there is no locale to read, and English is the answer
    /// -- never a panic and never a missing message.
    #[test]
    fn a_refusal_built_outside_a_request_is_english() {
        let state = test_state("admin-token", vec![test_device("device-1", "token-1")]);
        let refusal = require_device(&state, &bearer_headers("wrong")).unwrap_err();
        assert_eq!(error_body(&refusal)["error"]["message"], "invalid token");
    }

    /// The wire vocabulary is the contract; the prose is not.
    #[test]
    fn error_codes_are_byte_identical_across_locales() {
        // One of each shape: a validation refusal, an auth refusal, a
        // not-found, and one whose message quotes API vocabulary that must
        // survive translation intact.
        let cases = [
            ("session_not_found", "session not found"),
            ("invalid_platform", "platform must be ios or android"),
            (
                "invalid_decision",
                "decision must be allow, allow_always, or deny",
            ),
            (
                "invalid_source",
                "source must be visible, recent, recent-unwrapped, or detection",
            ),
            (
                "unknown_agent",
                "agent is not one this gateway offers; see GET /api/agents/catalog",
            ),
        ];
        for (code, message) in cases {
            let english = api_error_in(Locale::En, StatusCode::BAD_REQUEST, code, message);
            let chinese = api_error_in(Locale::ZhTw, StatusCode::BAD_REQUEST, code, message);
            let english = error_body(&english);
            let chinese = error_body(&chinese);
            assert_eq!(english["error"]["code"], code);
            assert_eq!(chinese["error"]["code"], code);
            assert_eq!(english["error"]["message"], message);
            assert_ne!(
                chinese["error"]["message"], message,
                "{code} has no translation"
            );
        }

        // The literals inside a message are API vocabulary a client sends back,
        // so only the sentence around them moves.
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "invalid_decision",
            "decision must be allow, allow_always, or deny",
        ));
        let message = chinese["error"]["message"].as_str().unwrap();
        for literal in ["decision", "allow", "allow_always", "deny"] {
            assert!(message.contains(literal), "{literal} was translated away");
        }
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "invalid_source",
            "source must be visible, recent, recent-unwrapped, or detection",
        ));
        let message = chinese["error"]["message"].as_str().unwrap();
        for literal in ["visible", "recent", "recent-unwrapped", "detection"] {
            assert!(message.contains(literal), "{literal} was translated away");
        }
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
        assert!(chinese["error"]["message"]
            .as_str()
            .unwrap()
            .contains("GET /api/agents/catalog"));
    }

    /// A message nobody has translated is still a message.
    #[test]
    fn an_untranslated_refusal_falls_back_to_its_english() {
        let refusal = api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_GATEWAY,
            "herdr_error",
            "pane.read: Herdr refused the request",
        );
        assert_eq!(
            error_body(&refusal)["error"]["message"],
            "pane.read: Herdr refused the request"
        );
        assert_eq!(error_body(&refusal)["error"]["code"], "herdr_error");
    }

    /// A `push-tokens.json` written before the locale field existed still
    /// loads, and the device it describes is notified in English.
    #[test]
    fn a_push_token_registered_before_locales_existed_still_loads() {
        let old: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "device_name": "Phone", "updated_unix_ms": 1 }]"#,
        )
        .expect("the field is optional, so an older file is still a valid one");
        assert_eq!(old[0].locale, None);
        assert_eq!(old[0].locale(), Locale::En);

        let current: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "device_name": "Phone", "locale": "zh-TW", "updated_unix_ms": 1 }]"#,
        )
        .unwrap();
        assert_eq!(current[0].locale(), Locale::ZhTw);

        // And a value that is not a locale this gateway serves is not an error
        // either -- the device simply gets English.
        let odd: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "locale": "tlh", "updated_unix_ms": 1 }]"#,
        )
        .unwrap();
        assert_eq!(odd[0].locale(), Locale::En);
    }

    #[test]
    fn a_secret_directory_is_marked_never_to_be_committed() {
        let dir = std::env::temp_dir().join(format!("herdr-gitignore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("config.json");
        write_secret_file(&secret, b"{}").unwrap();

        let ignore = dir.join(".gitignore");
        assert!(
            ignore.exists(),
            "a secret directory must carry a .gitignore"
        );
        assert!(std::fs::read_to_string(&ignore).unwrap().contains('*'));

        // An existing file is left alone: the developer may have written it.
        std::fs::write(&ignore, "mine\n").unwrap();
        write_secret_file(&secret, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "mine\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The files have been 0600 for a while. The directory around them was
    /// left at the umask, and a world-listable directory still says which
    /// devices' record file is there and that this account runs a gateway.
    #[cfg(unix)]
    #[test]
    fn a_secret_directory_is_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("herdr-secret-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let secret = dir.join("devices.json");
        write_secret_file(&secret, b"[]").unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the directory is still readable");
        let file = std::fs::metadata(&secret).unwrap().permissions().mode();
        assert_eq!(file & 0o777, 0o600, "the secret is still readable");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn device_fixture(id: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: id.into(),
            token_hash: authority::hash_token(id),
            transport_key: None,
            paired_unix_ms: 1,
            last_seen_unix_ms: 1,
            install_id: None,
        }
    }

    /// The shape of the loss this file was written for: a device file that a
    /// process could not read became an empty list, and the next pairing
    /// wrote that empty list back over the records that were still there.
    #[test]
    fn an_unreadable_device_file_is_an_error_not_an_empty_list() {
        let dir = std::env::temp_dir().join(format!("gateway-devices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEVICES_FILE);

        // A file that was never written is genuinely empty.
        assert!(read_devices_at(&path).unwrap().is_empty());

        // A write cut short leaves nothing to parse. That is not "no devices".
        std::fs::write(&path, b"").unwrap();
        assert!(
            read_devices_at(&path).is_err(),
            "a zero-byte device file read as an empty device list"
        );

        // Nor is a half-written one.
        std::fs::write(&path, b"[{\"id\":\"phone\",\"na").unwrap();
        assert!(
            read_devices_at(&path).is_err(),
            "a truncated device file read as an empty device list"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Replacing the list must never put the file through a state in which it
    /// holds less than a whole generation, because every reader of it treats
    /// what it finds as the complete set of pairings.
    #[cfg(unix)]
    #[test]
    fn replacing_a_secret_file_never_leaves_it_short() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("gateway-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEVICES_FILE);

        write_secret_file(&path, b"[\"first\"]").unwrap();
        let first_inode = std::fs::metadata(&path).unwrap().ino();

        write_secret_file(&path, b"[\"second\"]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[\"second\"]");
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            first_inode,
            "the file was rewritten in place, so it was empty for part of the write"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the replacement lost the owner-only mode"
        );

        // The temporary the rename came from must not be left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "left a temporary behind: {leftovers:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Editing the device list on disk while a gateway owns the directory is
    /// the same loss from the other side: the gateway is holding the whole
    /// pre-edit list in memory and writes it back at the next pairing, so an
    /// edit made underneath it is undone without anyone being told. Refusing
    /// is the only honest answer -- the caller's own fallback is to ask the
    /// running gateway to do it instead.
    #[test]
    fn revoking_on_disk_refuses_while_a_gateway_owns_the_directory() {
        let dir = std::env::temp_dir().join(format!("gateway-revoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();

        let owner = state_lock::acquire_within(&dir, state_lock::RELEASE_VISIBLE_WITHIN).unwrap();
        // Exact, not waited on: while the owner holds it this must be refused
        // every time.
        let refused = revoke_device_at(&dir, "phone");
        assert!(
            refused.is_err(),
            "a device was revoked on disk behind a running gateway's back"
        );
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"],
            "the refused revoke still rewrote the device list"
        );

        // With no gateway running there is no in-memory list to contradict,
        // and the same call goes through. Waited on rather than asserted on
        // the next instruction -- see `state_lock::acquire_within`.
        drop(owner);
        assert!(
            state_lock::retry_while_directory_is_busy(|| revoke_device_at(&dir, "phone")).unwrap()
        );
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tablet"]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lock has to span the read *and* the write, not just the write.
    ///
    /// A change that locked only its write would still lose records, just
    /// through a smaller window: it reads the whole list, another writer's
    /// change lands in the gap, and then it stores a list that never
    /// contained it. This forces exactly that interleaving -- one change is
    /// held open between its read and its write while a second one tries to
    /// go -- and the second must be refused rather than allowed to slip in
    /// and be overwritten a moment later.
    #[test]
    fn a_device_list_change_owns_the_directory_from_its_read_to_its_write() {
        let dir =
            std::env::temp_dir().join(format!("gateway-devices-rmw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();

        let slow_dir = dir.clone();
        let slow = std::thread::spawn(move || {
            state_lock::retry_while_directory_is_busy(|| {
                update_devices_at(&slow_dir, |devices| {
                    devices.retain(|device| device.id != "phone");
                    // Sitting between the read and the write is the entire
                    // point: this is the window a write-only lock would leave
                    // open, and it is far longer than the pre-`exec` window
                    // that makes an unrelated refusal possible.
                    std::thread::sleep(Duration::from_millis(400));
                    Some(())
                })
            })
        });

        std::thread::sleep(Duration::from_millis(100));
        // Exact, not waited on: the slow change is provably mid-flight.
        let competing = update_devices_at(&dir, |devices| {
            devices.retain(|device| device.id != "tablet");
            Some(())
        });
        assert!(
            competing.is_err(),
            "a second change ran while another was between its read and its write, so the \
             slower one was about to store a list that never had this change in it"
        );

        assert!(slow.join().unwrap().unwrap().is_some());
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tablet"],
            "the in-flight change did not land intact"
        );

        // Once the directory is free again the same change goes through.
        assert!(
            state_lock::retry_while_directory_is_busy(|| revoke_device_at(&dir, "tablet")).unwrap()
        );
        assert!(read_devices_at(&dir.join(DEVICES_FILE)).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bad list is survivable as long as the one it replaced is still there.
    #[test]
    fn writing_the_device_list_keeps_the_generation_it_replaced() {
        let dir =
            std::env::temp_dir().join(format!("gateway-devices-bak-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();
        // Nothing was replaced by the first write, so there is nothing to keep.
        assert!(!dir.join(DEVICES_BACKUP_FILE).exists());

        write_devices_at(&dir, &[]).unwrap();
        assert!(read_devices_at(&dir.join(DEVICES_FILE)).unwrap().is_empty());

        let kept = read_devices_at(&dir.join(DEVICES_BACKUP_FILE)).unwrap();
        assert_eq!(
            kept.iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"],
            "the replaced pairings were not recoverable"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn event_filter_matches_dot_and_underscore() {
        assert_eq!(normalize_event_name("pane.updated"), "pane_updated");
        assert_eq!(normalize_event_name(" pane_updated "), "pane_updated");
    }

    /// The rebinding case, written as the header it arrives in. A page served
    /// from a name the attacker owns keeps sending that name in `Host` even
    /// after the name has been re-pointed at this machine, which is exactly
    /// what makes the header worth reading.
    #[test]
    fn a_name_this_gateway_was_never_told_about_is_refused() {
        let mut config = test_config("secret");
        config.listen = "100.99.165.54:23847".into();
        config.public_url = "http://mac-mini.example-tailnet.ts.net:23847".into();
        let known = known_hosts(&config);

        for good in [
            "100.99.165.54:23847",
            "100.99.165.54",
            "mac-mini.example-tailnet.ts.net:23847",
            "MAC-MINI.example-tailnet.TS.NET",
            "localhost:23847",
            "127.0.0.1:23847",
            "[::1]:23847",
            // A trailing dot is the same name spelled absolutely.
            "mac-mini.example-tailnet.ts.net.",
        ] {
            assert!(host_is_known(good, &known), "{good} should be answered");
        }

        for bad in [
            "rebind.attacker.example:23847",
            "attacker.example",
            "gateway.attacker.example",
            // The suffix rules are suffixes of a label, not of a string.
            "evil-ts.net",
            "notlocalhost",
            "ts.net.attacker.example",
        ] {
            assert!(!host_is_known(bad, &known), "{bad} should be refused");
        }
    }

    /// Ellen's gateway answers on a bare Tailscale address, and a great many
    /// installs will. An address literal has to pass, because rebinding needs a
    /// name whose resolution can be flipped and an address has none.
    #[test]
    fn an_address_is_always_a_host_this_gateway_answers_to() {
        let known = known_hosts(&test_config("secret"));
        for address in [
            "100.99.165.54:23847",
            "192.168.1.20:23847",
            "10.0.0.1",
            "[fd7a:115c:a1e0::1]:23847",
            "[::1]",
        ] {
            assert!(
                host_is_known(address, &known),
                "{address} should be answered"
            );
        }
    }

    #[test]
    fn the_known_hosts_are_the_public_url_and_the_listen_address() {
        let mut config = test_config("secret");
        config.listen = "0.0.0.0:23847".into();
        config.public_url = "https://desk.example-tailnet.ts.net".into();
        assert_eq!(
            known_hosts(&config),
            vec![
                String::from("0.0.0.0"),
                String::from("desk.example-tailnet.ts.net"),
                String::from("localhost"),
            ]
        );
    }

    #[test]
    fn a_host_header_is_read_down_to_its_name() {
        assert_eq!(host_name("Example.COM:23847"), "example.com");
        assert_eq!(host_name("example.com"), "example.com");
        assert_eq!(host_name("[::1]:23847"), "::1");
        assert_eq!(host_name("::1"), "::1");
        assert_eq!(host_name("example.com."), "example.com");
        // Not a port, so not cut off.
        assert_eq!(host_name("example.com:notaport"), "example.com:notaport");
    }

    #[test]
    fn token_hash_is_stable_and_not_plaintext() {
        let hash = hash_token("secret");
        assert_eq!(hash, hash_token("secret"));
        assert_ne!(hash, "secret");
    }

    #[test]
    fn control_routes_accept_a_device_token_and_report_which_device() {
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        assert_eq!(
            require_device(&state, &bearer_headers("device-token")).unwrap(),
            "device-1"
        );
    }

    #[test]
    fn control_routes_reject_the_admin_token() {
        // The admin token sits in plaintext on disk for the manage UI. Control
        // routes can run commands on the host, so it must not reach them.
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        let err = require_device(&state, &bearer_headers("secret")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
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
    fn pending_pairing_accepts_only_the_admin_token() {
        let config = test_config("secret");
        assert!(require_admin(&config, &bearer_headers("secret")).is_ok());
        let err = require_admin(&config, &bearer_headers("device-token")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn pairing_revocation_accepts_manager_or_device_but_control_stays_device_only() {
        let state = test_state("admin-token", vec![test_device("device-1", "device-token")]);
        assert!(require_pairing_manager(&state, &bearer_headers("admin-token")).is_ok());
        assert!(require_pairing_manager(&state, &bearer_headers("device-token")).is_ok());
        assert!(require_pairing_manager(&state, &bearer_headers("wrong")).is_err());
        assert!(require_device(&state, &bearer_headers("admin-token")).is_err());
    }

    #[test]
    fn auth_rejects_invalid_bearer_token() {
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        let err = require_device(&state, &bearer_headers("wrong")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn auth_rejects_overlong_token() {
        let state = test_state("secret", Vec::new());
        let err = require_device(&state, &bearer_headers(&"x".repeat(257))).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        let config = test_config("secret");
        let err = require_admin(&config, &bearer_headers(&"x".repeat(257))).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn revoking_a_device_token_stops_it_authenticating() {
        let devices = vec![
            test_device("device-1", "token-1"),
            test_device("device-2", "token-2"),
        ];
        assert_eq!(
            identify_device(&devices, "token-1"),
            Some("device-1".into())
        );

        let remaining = devices
            .into_iter()
            .filter(|device| device.id != "device-1")
            .collect::<Vec<_>>();
        assert_eq!(identify_device(&remaining, "token-1"), None);
        // Revoking one device must leave the others working.
        assert_eq!(
            identify_device(&remaining, "token-2"),
            Some("device-2".into())
        );
    }

    #[test]
    fn device_names_reject_terminal_escape_injection() {
        assert!(valid_device_name("Ellen's iPhone"));
        assert!(valid_device_name("Pixel 9 Pro"));
        assert!(!valid_device_name(""));
        // The manage UI draws this into a terminal box.
        assert!(!valid_device_name("evil\x1b[2J\x1b[Hcode: AAAA-BBBB"));
        assert!(!valid_device_name("two\nlines"));
        assert!(!valid_device_name("tab\there"));
        assert!(!valid_device_name(&"x".repeat(MAX_DEVICE_NAME_CHARS + 1)));
    }

    #[test]
    fn configured_port_is_read_from_the_listen_address() {
        let mut config = test_config("secret");
        config.listen = "127.0.0.1:23847".into();
        assert_eq!(config.port(), 23847);
        config.listen = "0.0.0.0:9000".into();
        assert_eq!(config.port(), 9000);
        // A malformed listen address must not silently target another service.
        config.listen = "not-an-address".into();
        assert_eq!(config.port(), DEFAULT_PORT);
    }

    #[test]
    fn validate_text_enforces_size_limit() {
        assert!(validate_text("ok").is_ok());
        let too_large = "x".repeat(MAX_SEND_TEXT_BYTES + 1);
        let err = validate_text(&too_large).unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The only field on this API that becomes argv for a process on the host.
    /// A device token can already type into the pane, so this is a bound and
    /// not a fence -- but it is the same bound every other client string is
    /// under, and an argument list is not the field to leave unbounded.
    #[test]
    fn agent_args_are_bounded_like_every_other_client_string() {
        assert!(validate_agent_args(&[]).is_ok());
        assert!(validate_agent_args(&["--model".into(), "opus".into()]).is_ok());

        let too_many = vec![String::from("-v"); MAX_AGENT_ARGS + 1];
        assert_eq!(
            validate_agent_args(&too_many).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        let too_long = vec!["x".repeat(MAX_AGENT_ARG_CHARS + 1)];
        assert_eq!(
            validate_agent_args(&too_long).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        // A newline in an argument forges a line of the step log it is echoed
        // into, which is the same reason a device name cannot carry one.
        assert_eq!(
            validate_agent_args(&["--flag\nvalue".into()])
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert!(validate_agent_args(&["--flag\u{0}".into()]).is_err());
    }

    #[test]
    fn pairing_code_uses_unambiguous_characters() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), authority::PAIRING_CODE_LENGTH);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(authority::valid_pairing_code(&code));
        assert!(!authority::valid_pairing_code("ABCD2345"));
        assert!(!authority::valid_pairing_code("abcD-2345"));
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
        assert!(!code.contains('L'));
    }

    /// What the code is worth is what every position can hold and how evenly it
    /// holds it. Both halves are asserted, because both were wrong: one
    /// position could only reach sixteen of the thirty-one glyphs because it
    /// was reading a UUID's version nibble, and every position leaned on the
    /// first eight because a byte was folded with `%`.
    ///
    /// The bands are wide on purpose. Twenty thousand draws puts about 645 of
    /// each glyph in each position and about 5161 overall; a tenth of that is
    /// seven standard deviations, so the old bias (a ninth over, five of them)
    /// fails and a fair generator does not flake.
    #[test]
    fn every_glyph_can_land_in_every_position_and_none_is_favoured() {
        const DRAWS: usize = 20_000;
        let mut counts =
            vec![vec![0_usize; PAIRING_CODE_ALPHABET.len()]; PAIRING_CODE_CHARACTER_COUNT];
        for _ in 0..DRAWS {
            let code = generate_pairing_code();
            assert!(
                authority::valid_pairing_code(&code),
                "{code} is not a pairing code"
            );
            let glyphs: Vec<u8> = code.bytes().filter(|byte| *byte != b'-').collect();
            for (position, glyph) in glyphs.iter().enumerate() {
                let index = PAIRING_CODE_ALPHABET
                    .iter()
                    .position(|candidate| candidate == glyph)
                    .expect("a code is drawn from the alphabet");
                counts[position][index] += 1;
            }
        }

        for (position, row) in counts.iter().enumerate() {
            for (index, count) in row.iter().enumerate() {
                assert!(
                    *count > 0,
                    "position {position} never produced {}",
                    PAIRING_CODE_ALPHABET[index] as char
                );
            }
        }

        let total = DRAWS * PAIRING_CODE_CHARACTER_COUNT;
        let expected = total / PAIRING_CODE_ALPHABET.len();
        for index in 0..PAIRING_CODE_ALPHABET.len() {
            let seen: usize = counts.iter().map(|row| row[index]).sum();
            assert!(
                seen * 10 > expected * 9 && seen * 10 < expected * 11,
                "{} came up {seen} times against {expected} expected",
                PAIRING_CODE_ALPHABET[index] as char
            );
        }
    }

    #[test]
    fn request_id_validation_is_restrictive() {
        assert!(valid_request_id("iphone-15.req_1"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("has space"));
        assert!(!valid_request_id(&"x".repeat(81)));
    }

    #[test]
    fn pairing_requests_are_rate_limited_per_window() {
        let state = test_state("secret", Vec::new());
        for _ in 0..MAX_PAIRING_REQUESTS_PER_WINDOW {
            assert!(record_pairing_request(&state, 1_000).is_ok());
        }
        let error = record_pairing_request(&state, 1_001).unwrap_err();
        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(record_pairing_request(&state, 1_000 + PAIRING_RATE_LIMIT_WINDOW_MS).is_ok());
    }

    #[test]
    fn tailscale_serve_proxy_matching_uses_the_exact_port() {
        assert!(proxy_targets_port("http://127.0.0.1:23100", 23100));
        assert!(proxy_targets_port("http://localhost:23100/path", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:123100", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:23100.example", 23100));
    }

    #[test]
    fn management_connection_uses_the_actual_safe_listener() {
        assert_eq!(
            local_management_addr("0.0.0.0:23100".parse().unwrap()),
            "127.0.0.1:23100".parse().unwrap()
        );
        assert_eq!(
            local_management_addr("100.100.100.100:23100".parse().unwrap()),
            "100.100.100.100:23100".parse().unwrap()
        );
    }

    #[test]
    fn public_url_validation_allows_http_without_allowing_url_injection() {
        assert_eq!(
            validate_public_url("http://100.100.100.100:23100/").unwrap(),
            "http://100.100.100.100:23100"
        );
        assert!(validate_public_url("ftp://100.100.100.100/file").is_err());
        assert!(validate_public_url("http://user:secret@100.100.100.100:23100").is_err());
        assert!(validate_public_url("http://100.100.100.100:23100?token=secret").is_err());
    }

    fn test_pending_pairing(created_unix_ms: u128) -> Option<PendingPairing> {
        let code = "2345-6789".to_owned();
        Some(PendingPairing {
            request_id: "request-1".into(),
            device_name: "Muqun test".into(),
            install_id: None,
            code_hash: hash_token(&code),
            code,
            created_unix_ms,
            failed_attempts: 0,
        })
    }

    fn consume_test_pairing_code(
        pending: &mut Option<PendingPairing>,
        request_id: &str,
        code: &str,
        now_unix_ms: u128,
    ) -> Result<(), PairingCodeError> {
        authority::consume_pairing_code(
            pending,
            request_id,
            code,
            now_unix_ms,
            PAIRING_CODE_TTL_MS,
            MAX_PAIRING_CODE_ATTEMPTS,
        )
    }

    #[test]
    fn pairing_code_is_consumed_after_one_successful_claim() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_001),
            Ok(())
        );
        assert!(pending.is_none());
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_002),
            Err(PairingCodeError::Missing)
        );
    }

    #[test]
    fn expired_pairing_code_is_rejected_and_cleared() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(
                &mut pending,
                "request-1",
                "2345-6789",
                1_000 + PAIRING_CODE_TTL_MS
            ),
            Err(PairingCodeError::Expired)
        );
        assert!(pending.is_none());
    }

    #[test]
    fn repeated_invalid_pairing_attempts_invalidate_code() {
        let mut pending = test_pending_pairing(1_000);
        for _ in 0..MAX_PAIRING_CODE_ATTEMPTS {
            assert_eq!(
                consume_test_pairing_code(&mut pending, "request-1", "AAAA-AAAA", 1_001),
                Err(PairingCodeError::Invalid)
            );
        }
        assert!(pending.is_none());
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

    #[test]
    fn pairing_payload_contains_mobile_connection_fields() {
        let payload = PairingPayload {
            kind: "muqun-gateway".into(),
            server_id: "server-1".into(),
            label: "machine".into(),
            url: "http://100.1.2.3:23100".into(),
            token: "secret".into(),
            transport_key: "transport-secret".into(),
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["kind"], "muqun-gateway");
        assert_eq!(value["url"], "http://100.1.2.3:23100");
        assert_eq!(value["token"], "secret");
    }

    #[test]
    fn manager_qr_uses_the_current_config_fields() {
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1", Some("key_1")),
            "muqun://pair?u=http%3A%2F%2F100.1.2.3%3A23847&s=server-1&k=key_1"
        );
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1", None),
            "muqun://pair?u=http%3A%2F%2F100.1.2.3%3A23847&s=server-1"
        );
    }

    #[test]
    fn pairing_transport_policy_rejects_only_a_stale_encrypted_request() {
        // `Required` no longer forces an encrypted request through this gate:
        // `pair_claim` authenticates an unencrypted one with the one-time
        // code instead (see `code_pairing_response`), and `pair_request`
        // never carried anything worth protecting.
        assert!(require_pairing_transport(TransportEncryptionMode::Required, true).is_ok());
        assert!(require_pairing_transport(TransportEncryptionMode::Required, false).is_ok());
        assert!(require_pairing_transport(TransportEncryptionMode::Disabled, false).is_ok());
        // The one case still worth rejecting: a request sealed with a key
        // from before encryption was turned off.
        assert!(require_pairing_transport(TransportEncryptionMode::Disabled, true).is_err());
    }

    /// Reads the code `manage` would show a reader on the machine's screen,
    /// after a device with no QR key has called `pair_request`.
    fn pending_code(state: &AppState) -> String {
        state
            .pending_pairing
            .lock()
            .unwrap()
            .as_ref()
            .expect("pair_request left a pending code")
            .code
            .clone()
    }

    #[tokio::test]
    async fn manual_pairing_ends_up_with_the_same_transport_key_a_qr_pairing_would() {
        let state = test_state("admin-token", vec![]);
        assert_eq!(
            state.config.transport_encryption,
            TransportEncryptionMode::Required
        );
        // No `k`: exactly what a device that typed the address, rather than
        // scanning a QR, sends.
        let request_response = pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-1", "device_name": "Readers phone" })),
        )
        .await
        .unwrap();
        assert_eq!(request_response.status(), StatusCode::OK);

        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-1", "code": code })),
        )
        .await
        .unwrap();
        assert_eq!(claim_response.status(), StatusCode::OK);

        // The body is a sealed envelope, not a plain pairing payload: it was
        // never sent in the clear even though the request that earned it
        // carried no pre-shared key.
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: transport::Envelope = serde_json::from_slice(&body).unwrap();
        let material = code_pairing_material(&code, "manual-1").unwrap();
        let plaintext = transport::open(
            &material,
            transport::Direction::PairingResponse,
            b"POST /api/pair/claim\ncode-pairing\n",
            &envelope,
            now_unix_ms(),
        )
        .expect("a reader who typed the correct code can open the response");
        let payload: Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(payload["kind"], "muqun-gateway");
        // Same shape a QR pairing gets from this gateway: a device transport
        // key, not just a bearer token. No silent downgrade.
        assert_eq!(payload["transport"], "muqun-aes-256-gcm-v1");
        assert!(payload["device_id"].as_str().is_some());
        let transport_key = payload["transport_key"].as_str().unwrap();
        assert!(transport::decode_key(transport_key).is_ok_and(|key| key.len() == 32));
    }

    #[tokio::test]
    async fn manual_pairing_response_cannot_be_opened_without_the_code() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-eaves", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-eaves", "code": code })),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: transport::Envelope = serde_json::from_slice(&body).unwrap();

        // A passive observer who saw the wire traffic but not the code typed
        // into the phone has to brute force the code before this opens --
        // guessing wrong does not open it.
        let wrong_material = code_pairing_material("AAAA-AAAA", "manual-eaves").unwrap();
        assert!(transport::open(
            &wrong_material,
            transport::Direction::PairingResponse,
            b"POST /api/pair/claim\ncode-pairing\n",
            &envelope,
            now_unix_ms(),
        )
        .is_err());
    }

    /// Cleartext mode drops the envelope, not the door.
    ///
    /// `transport_encryption: disabled` used to make `require_device` answer
    /// `Ok` for *every* request, including one with no `Authorization` header
    /// at all -- so on a gateway configured that way the entire device API,
    /// uploads and agent routes included, answered anyone who could reach the
    /// port. The mode is about the envelope around a request; it was never
    /// meant to be about whether the request is authenticated, and the setup
    /// warning it prints ("a leaked bearer token can call the API") says as
    /// much.
    #[test]
    fn cleartext_mode_still_asks_for_a_token() {
        let token = "device-token";
        let mut state = test_state("admin-token", vec![test_device("phone-1", token)]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;

        // No header at all: 401, the same answer the encrypted mode gives.
        let (status, body) = require_device(&state, &HeaderMap::new()).unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0["error"]["code"], "missing_authorization");

        // A token that is not a device's and not the admin's: 403.
        let (status, body) = require_device(&state, &bearer_headers("guessed")).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.0["error"]["code"], "invalid_token");

        // A malformed header is not a way past either.
        let mut malformed = HeaderMap::new();
        malformed.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("device-token"),
        );
        assert_eq!(
            require_device(&state, &malformed).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );

        // The paired device still gets in, and is still identified as itself.
        assert_eq!(
            require_device(&state, &bearer_headers(token)).unwrap(),
            "phone-1"
        );
        // As does the admin token, which this mode has always accepted here.
        assert_eq!(
            require_device(&state, &bearer_headers("admin-token")).unwrap(),
            "admin"
        );

        // First-run pairing has no token to send and must stay reachable. It
        // does not come through here at all -- `/api/pair/request` and
        // `/api/pair/claim` go through `require_pairing_transport` -- and
        // `manual_pairing_omits_the_transport_key_when_encryption_is_disabled`
        // drives both with no headers whatsoever in this very mode.
    }

    /// A device paired while encryption was on keeps a `transport_key` it will
    /// never prove over cleartext. The proof is the part cleartext skips --
    /// tightening the token check must not quietly start demanding it.
    #[test]
    fn cleartext_mode_admits_a_device_that_was_paired_with_a_transport_key() {
        let mut device = test_device("phone-1", "device-token");
        device.transport_key = Some("a-key-from-when-encryption-was-on".into());
        let mut state = test_state("admin-token", vec![device]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;

        assert_eq!(
            require_device(&state, &bearer_headers("device-token")).unwrap(),
            "phone-1",
            "cleartext drops the envelope and the proof, never the token"
        );
    }

    /// The old behaviour survives only as a thing the owner writes down.
    #[test]
    fn only_an_explicit_opt_in_answers_without_a_token() {
        let mut state = test_state("admin-token", vec![test_device("phone-1", "device-token")]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        state.config.dev_unauthenticated = true;

        assert_eq!(
            require_device(&state, &HeaderMap::new()).unwrap(),
            DEV_UNAUTHENTICATED_DEVICE
        );
        // A real device is still identified as itself, not as the stand-in:
        // the opt-in is a fallback, not a replacement for the token check.
        assert_eq!(
            require_device(&state, &bearer_headers("device-token")).unwrap(),
            "phone-1"
        );

        // It is off unless written, and writing nothing writes nothing.
        assert!(!test_config("admin-token").dev_unauthenticated);
        let round_tripped = serde_json::to_value(test_config("admin-token")).unwrap();
        assert!(
            round_tripped.get("dev_unauthenticated").is_none(),
            "an existing config.json must round-trip untouched"
        );

        // And it grants nothing in the encrypted mode, where there is no
        // cleartext story to tell in the first place.
        state.config.transport_encryption = TransportEncryptionMode::Required;
        assert_eq!(
            require_device(&state, &HeaderMap::new()).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn manual_pairing_omits_the_transport_key_when_encryption_is_disabled() {
        let mut state = test_state("admin-token", vec![]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-2", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-2", "code": code })),
        )
        .await
        .unwrap();
        assert_eq!(claim_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        // Plain JSON, not a sealed envelope: `Disabled` never sealed a
        // response before this card and still does not.
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["kind"], "muqun-gateway");
        assert!(payload.get("transport_key").is_none());
        assert!(payload.get("transport").is_none());
    }

    #[tokio::test]
    async fn manual_pairing_code_is_single_use() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "code": code.clone() })),
        )
        .await
        .unwrap();

        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "code": code })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "pairing_not_requested");
    }

    #[tokio::test]
    async fn manual_pairing_code_expires_server_side() {
        let state = test_state("admin-token", vec![]);
        let code = "2345-6789".to_owned();
        *state.pending_pairing.lock().unwrap() = Some(PendingPairing {
            request_id: "manual-4".into(),
            device_name: "Phone".into(),
            install_id: None,
            code_hash: hash_token(&code),
            code,
            created_unix_ms: 0,
            failed_attempts: 0,
        });

        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-4", "code": "2345-6789" })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body["error"]["code"], "pairing_code_expired");
    }

    #[tokio::test]
    async fn manual_pairing_wrong_codes_are_rate_limited_and_burn_the_pairing() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-5", "device_name": "Phone" })),
        )
        .await
        .unwrap();

        for _ in 0..MAX_PAIRING_CODE_ATTEMPTS {
            let (status, Json(body)) = pair_claim(
                State(state.clone()),
                Json(json!({ "request_id": "manual-5", "code": "AAAA-AAAA" })),
            )
            .await
            .unwrap_err();
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"]["code"], "invalid_pairing_code");
        }

        // The correct code no longer works either: the pairing burned after
        // `MAX_PAIRING_CODE_ATTEMPTS` wrong guesses, same as it always did.
        let real_code = "2345-6789";
        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-5", "code": real_code })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "pairing_not_requested");
    }

    #[test]
    fn terminal_qr_has_explicit_standard_colors_and_measurable_width() {
        let code = QrCode::with_error_correction_level(
            b"muqun://pair?u=http%3A%2F%2Fhost&s=id",
            EcLevel::L,
        )
        .unwrap();
        let image = render_qr(&code);
        let expected_width = code.width() + 8;
        assert!(image
            .lines()
            .all(|line| line.starts_with("\x1b[30;47m") && line.ends_with("\x1b[0m")));
        assert!(image
            .lines()
            .all(|line| display_width(line) == expected_width));
        assert_eq!(display_width("\x1b[30;47m█▀ \x1b[0m"), 3);
    }

    #[test]
    fn blocked_agent_event_creates_one_notification() {
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "type": "pane.agent_status_changed",
                "pane_id": "w1:p2",
                "workspace_id": "w1",
                "display_agent": "Codex",
                "agent_status": "blocked"
            }
        });
        let mut statuses = HashMap::new();
        let notice = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Agent blocked · Studio");
        assert_eq!(notification.body, "Codex needs your input.");
        assert_eq!(notification.data["type"], "agent.blocked");
        assert_eq!(notification.data["url"], "/servers/server-1");
        // The same transition, said to a phone that reads Chinese. The name the
        // agent goes by is not translated -- it is a name.
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "代理程式等待中 · Studio");
        assert_eq!(chinese.body, "Codex 需要你的輸入。");
        assert_eq!(chinese.data["type"], "agent.blocked");
        assert_eq!(chinese.data["url"], "/servers/server-1");
        assert!(notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .is_none());
    }

    #[test]
    fn working_to_idle_creates_completion_notification() {
        let mut statuses = HashMap::from([("w1:p2".into(), "working".into())]);
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": "w1:p2",
                "agent": "codex",
                "agent_status": "idle"
            }
        });
        let notice = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Agent done · Studio");
        assert_eq!(notification.body, "codex finished running.");
        assert_eq!(notification.data["type"], "agent.completed");
        assert_eq!(notification.data["pane_id"], "w1:p2");
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "代理程式已完成 · Studio");
        assert_eq!(chinese.body, "codex 已執行完畢。");
    }

    #[test]
    fn a_temporary_idle_does_not_send_a_completion_push() {
        let pane = "w1:p2";
        let now = Instant::now();
        let mut gate = CompletionGate::default();
        let mut statuses = HashMap::from([(pane.to_owned(), "idle".to_owned())]);
        let present = std::collections::HashSet::from([pane.to_owned()]);
        let notice = notification_for_transition(
            &AgentTransition {
                pane_id: pane.to_owned(),
                agent: Some("codex".to_owned()),
                from: Some("working".to_owned()),
                to: "idle".to_owned(),
            },
            "server-1",
            "Studio",
            "default",
        );
        assert!(gate.observe(pane, "idle", notice, now).is_none());
        assert!(gate
            .ready(&statuses, &present, now + COMPLETION_GRACE / 2)
            .is_empty());

        statuses.insert(pane.to_owned(), "working".to_owned());
        gate.observe(pane, "working", None, now + COMPLETION_GRACE / 2);
        assert!(gate
            .ready(&statuses, &present, now + COMPLETION_GRACE)
            .is_empty());
    }

    #[test]
    fn stable_idle_is_confirmed_once_and_a_quick_second_cycle_is_suppressed() {
        let pane = "w1:p2";
        let now = Instant::now();
        let mut gate = CompletionGate::default();
        let mut statuses = HashMap::from([(pane.to_owned(), "idle".to_owned())]);
        let present = std::collections::HashSet::from([pane.to_owned()]);
        let transition = AgentTransition {
            pane_id: pane.to_owned(),
            agent: Some("codex".to_owned()),
            from: Some("working".to_owned()),
            to: "idle".to_owned(),
        };
        let notice = || notification_for_transition(&transition, "server-1", "Studio", "default");
        gate.observe(pane, "idle", notice(), now);
        assert!(gate
            .ready(
                &statuses,
                &present,
                now + COMPLETION_GRACE - Duration::from_secs(1)
            )
            .is_empty());
        assert_eq!(
            gate.ready(&statuses, &present, now + COMPLETION_GRACE)
                .len(),
            1
        );
        assert!(gate
            .ready(&statuses, &present, now + COMPLETION_GRACE)
            .is_empty());

        statuses.insert(pane.to_owned(), "working".to_owned());
        gate.observe(
            pane,
            "working",
            None,
            now + COMPLETION_GRACE + Duration::from_secs(1),
        );
        statuses.insert(pane.to_owned(), "idle".to_owned());
        gate.observe(
            pane,
            "idle",
            notice(),
            now + COMPLETION_GRACE + Duration::from_secs(2),
        );
        assert!(gate
            .ready(
                &statuses,
                &present,
                now + COMPLETION_GRACE * 2 + Duration::from_secs(2)
            )
            .is_empty());
    }

    #[test]
    fn missing_pane_cancels_a_pending_completion() {
        let pane = "w1:p2";
        let now = Instant::now();
        let mut gate = CompletionGate::default();
        let statuses = HashMap::from([(pane.to_owned(), "idle".to_owned())]);
        let notice = notification_for_transition(
            &AgentTransition {
                pane_id: pane.to_owned(),
                agent: None,
                from: Some("working".to_owned()),
                to: "idle".to_owned(),
            },
            "server-1",
            "Studio",
            "default",
        );
        gate.observe(pane, "idle", notice, now);
        assert!(gate
            .ready(
                &statuses,
                &std::collections::HashSet::new(),
                now + COMPLETION_GRACE
            )
            .is_empty());
        assert!(gate.pending.is_empty());
    }

    /// The agent's name goes where the sentence wants it, not where the English
    /// happened to put it.
    ///
    /// The old body was `format!("{name} {tail}")` over fragments like "needs
    /// your input.", which fixes the name to the front of the sentence in every
    /// language there will ever be. A whole format string per locale is what
    /// makes the slot movable, and an agent that reports no name at all gets the
    /// reader's own word for one rather than the English "Agent".
    #[test]
    fn a_push_names_the_agent_from_a_slot_and_not_from_a_concatenation() {
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p9", "agent": "   ", "agent_status": "blocked" }
        });
        let mut statuses = HashMap::new();
        let notice =
            notification_for_agent_status_event(&event, &mut statuses, "s", "", "default").unwrap();
        assert_eq!(notice.agent_name, None);
        // No server label, so the title is the heading on its own.
        assert_eq!(notice.render(Locale::En).title, "Agent blocked");
        assert_eq!(notice.render(Locale::En).body, "Agent needs your input.");
        assert_eq!(notice.render(Locale::ZhTw).title, "代理程式等待中");
        assert!(notice.render(Locale::ZhTw).body.ends_with("需要你的輸入。"));
        assert!(notice.render(Locale::ZhTw).body.starts_with("代理程式"));
    }

    #[test]
    fn an_approval_push_says_that_something_needs_answering_and_never_what() {
        // The whole privacy rule for notifications, asserted end to end on a
        // real menu: the agent quoted the command in its own option label, and
        // none of it may reach Expo.
        let approval =
            approvals::detect(include_str!("../tests/fixtures/approval-claude-bash.txt"))
                .expect("the fixture is a pending approval");
        let notice = approval_notification(
            "server-1", "Studio", "default", "wM:p1", "claude", &approval,
        );
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Approval needed · Studio");
        assert_eq!(notification.body, "claude is waiting for your approval.");
        assert_eq!(notification.data["type"], "approval.pending");
        assert_eq!(notification.data["pane_id"], "wM:p1");
        // The category the client registered its approve/deny actions under,
        // and which of them this menu offers.
        assert_eq!(notification.data["categoryId"], "approval");
        assert_eq!(notification.data["options"][0]["decision"], "allow");
        assert_eq!(notification.data["options"][2]["decision"], "deny");
        assert_eq!(notification.data["fingerprint"], approval.fingerprint);
        let rendered = Value::Object(notification.data).to_string();
        assert!(!rendered.contains("npm"), "the command must not travel");
        assert!(!rendered.contains("Do you want"), "nor the question");

        // Translating the four labels the gateway wrote for itself cannot
        // weaken any of that: the words changed, whose words they are did not.
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "需要核准 · Studio");
        assert_eq!(chinese.body, "claude 正在等待你的核准。");
        assert_eq!(chinese.data["options"][0]["label"], "核准");
        assert_eq!(chinese.data["options"][2]["label"], "拒絕");
        assert_eq!(
            chinese.data["options"][0]["decision"], "allow",
            "the decision is wire vocabulary and has no language"
        );
        let rendered = Value::Object(chinese.data).to_string();
        assert!(!rendered.contains("npm"), "the command must not travel");
        assert!(!rendered.contains("Do you want"), "nor the question");
    }

    #[test]
    fn the_approval_payload_carries_the_pane_and_answers_in_the_content_envelope() {
        let approval =
            approvals::detect(include_str!("../tests/fixtures/approval-claude-bash.txt")).unwrap();
        let pending = content_envelope(approval_data_menu(
            "default",
            "wM:p1",
            Some("claude"),
            Some(&approval),
        ));
        assert_eq!(pending["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(pending["data"]["state"], "pending");
        assert_eq!(pending["data"]["pane"]["approvals"], "menu");
        assert_eq!(
            pending["data"]["approval"]["options"][2]["decision"],
            "deny"
        );

        // An idle pane is answered with the same shape and a null approval, so
        // a client has one code path rather than two.
        let idle = content_envelope(approval_data_menu("default", "wM:p1", Some("claude"), None));
        assert_eq!(idle["data"]["state"], "idle");
        assert!(idle["data"]["approval"].is_null());
        assert_eq!(idle["data"]["pane"]["approvals"], "menu");
    }

    #[test]
    fn approvals_are_announced_as_a_capability_and_documented() {
        // Additive: the routes and events are new, so a client gates on the
        // capability rather than probing for a 404.
        assert!(API_CAPABILITIES.contains(&"pane_approvals"));
        let spec = openapi_spec();
        let approval = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/approval"];
        assert!(approval["get"].is_object());
        assert!(approval["post"]["requestBody"].is_object());
        assert!(approval["post"]["responses"]["409"].is_object());
    }

    fn status_event(pane_id: &str, agent: &str, status: &str) -> Value {
        json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": pane_id, "agent": agent, "agent_status": status }
        })
    }

    #[test]
    fn the_ring_records_every_transition_and_not_only_the_ones_worth_a_push() {
        // The digest is the reason: "it worked for twenty minutes and then went
        // idle" is the sentence a returning user wants, and only the last half
        // of it ever rang a doorbell.
        let state = test_state("admin", vec![test_device("d1", "token")]);
        let mut statuses = HashMap::new();

        let started = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "working"),
            &mut statuses,
        );
        let finished = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "idle"),
            &mut statuses,
        );
        // A repeat of the status the pane is already in is not a transition and
        // must not appear twice in a digest.
        let repeated = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "idle"),
            &mut statuses,
        );

        assert!(started.is_none(), "starting work wakes nobody");
        assert!(finished.is_some(), "finishing does");
        assert!(repeated.is_none());

        let log = state.agent_events.lock().unwrap();
        let events = log.since("default", None);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from, None);
        assert_eq!(events[0].to, "working");
        assert_eq!(events[1].from.as_deref(), Some("working"));
        assert_eq!(events[1].to, "idle");
        assert_eq!(events[1].agent.as_deref(), Some("claude"));
        assert_eq!(log.latest_seq("default"), 2);
    }

    #[tokio::test]
    async fn the_digest_endpoint_answers_what_is_new_and_where_to_resume_from() {
        let state = test_state("admin", vec![test_device("d1", "token")]);
        let mut statuses = HashMap::new();
        for status in ["working", "blocked", "idle"] {
            absorb_agent_status_event(
                &state,
                "default",
                &status_event("w1:p1", "claude", status),
                &mut statuses,
            );
        }

        let answer = session_agent_events(
            State(state.clone()),
            Path("default".into()),
            Query(AgentEventsQuery { since: None }),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(answer["events"].as_array().unwrap().len(), 3);
        assert_eq!(answer["events"][0]["to"], "working");
        assert_eq!(answer["events"][0]["from"], Value::Null);
        assert_eq!(answer["next_since"], 3);
        assert_eq!(answer["missed"], false);
        assert_eq!(answer["capacity"], agent_events::RING_CAPACITY);

        // Polling from where the last answer left off returns nothing and still
        // says where to resume from, so an idle session does not walk backwards.
        let resumed = session_agent_events(
            State(state.clone()),
            Path("default".into()),
            Query(AgentEventsQuery { since: Some(3) }),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(resumed["events"].as_array().unwrap().len(), 0);
        assert_eq!(resumed["next_since"], 3);
        assert_eq!(resumed["missed"], false);

        // Nothing here is readable without a paired device, and a session this
        // gateway does not have is a 404 rather than an empty digest.
        assert_eq!(
            session_agent_events(
                State(state.clone()),
                Path("default".into()),
                Query(AgentEventsQuery { since: None }),
                bearer_headers("not-a-token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            session_agent_events(
                State(state),
                Path("other".into()),
                Query(AgentEventsQuery { since: None }),
                bearer_headers("token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn first_idle_event_does_not_create_false_completion() {
        let mut statuses = HashMap::new();
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p2", "agent_status": "idle" }
        });
        assert!(notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .is_none());
        assert_eq!(statuses.get("w1:p2").map(String::as_str), Some("idle"));
    }

    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"IHDR and the rest of a real file");
        bytes
    }

    fn test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let local_offset = bytes.len() as u32;
            bytes.extend_from_slice(ZIP_LOCAL_HEADER);
            bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
            bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
            bytes.extend_from_slice(&0u16.to_le_bytes()); // stored
            bytes.extend_from_slice(&0u16.to_le_bytes()); // time
            bytes.extend_from_slice(&0u16.to_le_bytes()); // date
            bytes.extend_from_slice(&0u32.to_le_bytes()); // crc (not read by the probe)
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes()); // extra
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(body);

            central.extend_from_slice(ZIP_CENTRAL_HEADER);
            central.extend_from_slice(&20u16.to_le_bytes()); // made by
            central.extend_from_slice(&20u16.to_le_bytes()); // needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // stored
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&0u32.to_le_bytes()); // crc
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(ZIP_END_HEADER);
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk
        bytes.extend_from_slice(&0u16.to_le_bytes()); // central disk
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&central_offset.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment
        bytes
    }

    #[test]
    fn uploads_are_typed_by_content_not_by_name() {
        assert_eq!(sniff_upload_kind(&png_bytes()).unwrap().mime, "image/png");
        assert_eq!(
            sniff_upload_kind(b"\xff\xd8\xff\xe0\x00\x10JFIF")
                .unwrap()
                .mime,
            "image/jpeg"
        );
        assert_eq!(sniff_upload_kind(b"GIF89a....").unwrap().extension, "gif");
        assert_eq!(sniff_upload_kind(b"GIF87a....").unwrap().extension, "gif");
        assert_eq!(
            sniff_upload_kind(b"RIFF\x24\x00\x00\x00WEBPVP8 ")
                .unwrap()
                .mime,
            "image/webp"
        );
        assert_eq!(
            sniff_upload_kind(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00")
                .unwrap()
                .mime,
            "image/heic"
        );
        assert_eq!(
            sniff_upload_kind(b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00")
                .unwrap()
                .extension,
            "heic"
        );
    }

    /// Every way of asking for something other than one plain file inside the
    /// upload directory. The app only ever sends back a name this gateway
    /// minted, so anything else is an attempt.
    #[test]
    fn an_upload_name_that_is_not_one_plain_component_is_refused() {
        for attempt in [
            "../config.json",
            "..",
            ".",
            "../../.local/share/muqun-gateway/devices.json",
            "sub/dir.webp",
            "sub\\dir.webp",
            "/etc/passwd",
            "a/../b.webp",
            ".hidden.webp",
            "with space.webp",
            "semi;colon.webp",
            "quote\".webp",
            "nul\0.webp",
            "unicode\u{2215}.webp",
            "",
        ] {
            assert!(
                safe_upload_component(attempt).is_none(),
                "{attempt:?} must not resolve to an upload"
            );
        }
        // A percent-encoded separator is decoded before the handler sees it,
        // so it arrives as the separator and fails on the same rule.
        assert!(safe_upload_component("..%2fconfig.json").is_none());

        // What the gateway actually generates passes, unchanged.
        let minted = stored_upload_name(UploadKind {
            extension: "webp",
            mime: "image/webp",
        });
        assert_eq!(safe_upload_component(&minted).as_deref(), Some(&*minted));
        assert_eq!(upload_url(&minted), format!("/api/uploads/{minted}"));
    }

    /// The round trip the app needs: it posts a file, gets back the host path
    /// for the agent *and* a URL for itself, and reads the same bytes back
    /// under the type the content earned rather than the one a name claimed.
    #[tokio::test]
    async fn an_upload_answers_with_a_url_the_app_can_read_the_same_bytes_from() {
        use tower::ServiceExt as _;

        let token = "device-token";
        let state = test_state("admin", vec![test_device("phone-1", token)]);
        let app = Router::new()
            .route(UPLOADS_PATH, post(upload_file))
            .route("/api/uploads/{file_name}", get(upload_content))
            .with_state(state);

        // A png announced as a `.txt`: the stored type must come from the
        // bytes at write time and be re-derived from the bytes at read time.
        let boundary = "muqun-upload-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"screenshot.txt\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&png_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(UPLOADS_PATH)
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let stored: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(stored["mime"], "image/png");
        // The client name is echoed for the label only; it never became a path.
        assert_eq!(stored["name"], "screenshot.txt");
        let host_path = stored["path"].as_str().unwrap().to_string();
        let url = stored["url"].as_str().unwrap().to_string();
        let file_name = FsPath::new(&host_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(file_name.ends_with(".png"), "got {file_name}");
        assert_eq!(url, format!("/api/uploads/{file_name}"));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&url)
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(
            response.headers()["cache-control"],
            "private, no-store, max-age=0"
        );
        let served = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(served.as_ref(), png_bytes().as_slice());

        // A name that was never minted, and a traversal spelled out in full,
        // are the same miss -- neither says whether the target exists.
        for miss in [
            "/api/uploads/deadbeef-0000-0000-0000-000000000000.png",
            "/api/uploads/..%2f..%2fconfig.json",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(miss)
                        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{miss}");
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1 << 16)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["error"]["code"], "upload_not_found");
        }

        // And an unpaired caller gets nothing at all.
        let response = app
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The blanket `cache-control` is a floor. Every handler that says nothing
    /// still gets `no-store`; the one that says something says more, not less,
    /// and must reach the client as it wrote it.
    #[tokio::test]
    async fn the_blanket_cache_control_is_a_floor_a_handler_can_only_tighten() {
        use axum::routing::get;
        use tower::ServiceExt as _;

        let app = Router::new()
            .route("/quiet", get(|| async { "body" }))
            .route(
                "/specific",
                get(|| async {
                    Response::builder()
                        .header("cache-control", "private, no-store, max-age=0")
                        .body(Body::from("body"))
                        .unwrap()
                }),
            )
            .layer(middleware::from_fn(security_headers));

        let quiet = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/quiet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(quiet.headers()["cache-control"], "no-store, max-age=0");

        let specific = app
            .oneshot(
                Request::builder()
                    .uri("/specific")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            specific.headers()["cache-control"],
            "private, no-store, max-age=0",
            "the middleware must not overwrite a handler's own, stricter value"
        );
        // The rest of the blanket still applies either way.
        assert_eq!(specific.headers()["x-content-type-options"], "nosniff");
        assert_eq!(specific.headers()["pragma"], "no-cache");
    }

    /// Retention is a property of the file's age, not of whether the hourly
    /// sweep happened to have run: a file the sweeper would take reads as gone.
    #[test]
    fn an_upload_past_its_retention_reads_as_gone_before_the_sweep_takes_it() {
        let dir = asset_test_dir("upload-retention");
        let path = dir.join("expired.png");
        std::fs::write(&path, png_bytes()).unwrap();
        let now = SystemTime::now();
        assert!(!upload_expired(now, now));
        assert!(upload_expired(now - UPLOAD_RETENTION, now));
        // Still typed from its bytes while it lives.
        assert_eq!(
            sniff_stored_upload(&path, "expired.png").unwrap().mime,
            "image/png"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The read path types a stored file the way the write path did: magic
    /// numbers, then the office container, then text with the earned
    /// extension deciding the flavour -- never the extension on its own.
    #[test]
    fn a_stored_upload_is_typed_by_its_bytes_on_the_way_out_too() {
        let dir = asset_test_dir("upload-readback-types");

        let png = dir.join("a.png");
        std::fs::write(&png, png_bytes()).unwrap();
        assert_eq!(
            sniff_stored_upload(&png, "a.png").unwrap().mime,
            "image/png"
        );

        // The extension lies; the bytes do not.
        let mislabelled = dir.join("b.md");
        std::fs::write(&mislabelled, png_bytes()).unwrap();
        assert_eq!(
            sniff_stored_upload(&mislabelled, "b.md").unwrap().mime,
            "image/png"
        );

        let docx = dir.join("c.docx");
        std::fs::write(
            &docx,
            test_zip(&[
                ("[Content_Types].xml", b"<Types/>"),
                ("_rels/.rels", b"<Relationships/>"),
                ("word/document.xml", b"<document/>"),
            ]),
        )
        .unwrap();
        assert_eq!(
            sniff_stored_upload(&docx, "c.docx").unwrap().extension,
            "docx"
        );

        let markdown = dir.join("d.md");
        std::fs::write(&markdown, b"# notes\n\nplain\n").unwrap();
        assert_eq!(
            sniff_stored_upload(&markdown, "d.md").unwrap().mime,
            "text/markdown; charset=utf-8"
        );

        // Nothing the gateway would have refused at upload time is served.
        let elf = dir.join("e.png");
        std::fs::write(&elf, b"\x7fELF\x02\x01\x01\x00and the rest").unwrap();
        assert!(sniff_stored_upload(&elf, "e.png").is_none());

        let empty = dir.join("f.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(sniff_stored_upload(&empty, "f.png").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safe_documents_are_typed_by_content_before_name() {
        let pdf = sniff_document_upload_kind(b"%PDF-1.7\n1 0 obj", "notes.txt").unwrap();
        assert_eq!(pdf.extension, "pdf");
        assert_eq!(pdf.mime, "application/pdf");

        let markdown = sniff_document_upload_kind(b"# notes\n\nplain\n", "notes.md").unwrap();
        assert_eq!(markdown.extension, "md");
        assert_eq!(markdown.mime, "text/markdown; charset=utf-8");

        let source = sniff_document_upload_kind(b"const answer = 42;\n", "answer.ts").unwrap();
        assert_eq!(source.extension, "ts");
        assert_eq!(source.mime, "text/plain; charset=utf-8");

        let unknown = sniff_document_upload_kind(b"plain UTF-8\n", "README.weird").unwrap();
        assert_eq!(unknown.extension, "txt");
    }

    #[test]
    fn modern_office_packages_are_recognised_from_their_zip_structure() {
        let docx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("word/document.xml", b"document"),
        ]);
        assert_eq!(sniff_office_upload_kind(&docx).unwrap().extension, "docx");

        let xlsx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("xl/workbook.xml", b"workbook"),
        ]);
        assert_eq!(sniff_office_upload_kind(&xlsx).unwrap().extension, "xlsx");

        let pptx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("ppt/presentation.xml", b"presentation"),
        ]);
        assert_eq!(sniff_office_upload_kind(&pptx).unwrap().extension, "pptx");
    }

    #[test]
    fn open_document_packages_require_the_stored_mimetype_first() {
        let odt = test_zip(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("content.xml", b"content"),
            ("META-INF/manifest.xml", b"manifest"),
        ]);
        assert_eq!(sniff_office_upload_kind(&odt).unwrap().extension, "odt");

        let mimetype_late = test_zip(&[
            ("content.xml", b"content"),
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("META-INF/manifest.xml", b"manifest"),
        ]);
        assert!(sniff_office_upload_kind(&mimetype_late).is_none());
    }

    #[test]
    fn binary_documents_and_archives_are_refused() {
        let ordinary_zip = test_zip(&[("hello.txt", b"hello")]);
        assert!(sniff_office_upload_kind(&ordinary_zip).is_none());
        assert!(sniff_document_upload_kind(&ordinary_zip, "archive.zip").is_none());
        let ambiguous = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("word/document.xml", b"document"),
            ("xl/workbook.xml", b"workbook"),
        ]);
        assert!(sniff_office_upload_kind(&ambiguous).is_none());
        let traversal = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("../word/document.xml", b"document"),
        ]);
        assert!(sniff_office_upload_kind(&traversal).is_none());
        assert!(sniff_document_upload_kind(b"hello\0world", "notes.txt").is_none());
        assert!(sniff_document_upload_kind(b"\xff\xfe\x00x", "notes.txt").is_none());
        // A filename can preserve a useful extension only after the bytes have
        // passed the text probe; it cannot disguise an archive as source.
        assert!(sniff_document_upload_kind(b"PK\x03\x04", "archive.ts").is_none());
    }

    #[test]
    fn a_truncated_or_binary_upload_is_not_mistaken_for_a_known_type() {
        // Half a signature is not a match.
        assert!(sniff_upload_kind(b"\x89PN").is_none());
        assert!(sniff_upload_kind(b"\x89PNG\r\n\x1a").is_none());
        assert!(sniff_upload_kind(b"RIFF\x24\x00\x00\x00WEB").is_none());
        assert!(sniff_upload_kind(b"\x00\x00\x00\x18ftyp").is_none());
        // An ISO base media file that is not a HEIC flavour.
        assert!(sniff_upload_kind(b"\x00\x00\x00\x18ftypqt  \x00\x00\x00\x00").is_none());
        assert!(sniff_upload_kind(b"\x1f\x8b\x08\x00\x00\x00\x00\x00").is_none());
        assert!(sniff_upload_kind(b"").is_none());
    }

    #[test]
    fn executables_and_scripts_are_refused_whatever_they_are_called() {
        assert!(looks_executable(b"MZ\x90\x00\x03"));
        assert!(looks_executable(b"\x7fELF\x02\x01\x01"));
        assert!(looks_executable(b"\xfe\xed\xfa\xce\x00"));
        assert!(looks_executable(b"\xfe\xed\xfa\xcf\x00"));
        assert!(looks_executable(b"\xce\xfa\xed\xfe\x00"));
        assert!(looks_executable(b"\xcf\xfa\xed\xfe\x00"));
        assert!(looks_executable(b"\xca\xfe\xba\xbe\x00"));
        assert!(looks_executable(b"\xbe\xba\xfe\xca\x00"));
        assert!(looks_executable(b"#!/bin/sh\nrm -rf /\n"));
        assert!(looks_executable(b"#!"));

        // A script is refused twice over: it is executable, and it carries no
        // image magic number either.
        assert!(sniff_upload_kind(b"#!/bin/sh\nrm -rf /\n").is_none());

        assert!(!looks_executable(&png_bytes()));
        assert!(!looks_executable(b"%PDF-1.7"));
        assert!(!looks_executable(b"# a markdown file\n"));
        assert!(!looks_executable(b"M"));
    }

    #[test]
    fn a_client_file_name_is_only_ever_echoed_back_after_scrubbing() {
        assert_eq!(sanitize_upload_name("../../evil.png"), "evil.png");
        assert_eq!(sanitize_upload_name("..\\..\\evil.png"), "evil.png");
        assert_eq!(sanitize_upload_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_upload_name("shot\r\n.png"), "shot.png");
        assert_eq!(sanitize_upload_name("bell\x07.txt"), "bell.txt");
        assert_eq!(sanitize_upload_name("  spaced.png  "), "spaced.png");
        assert_eq!(sanitize_upload_name(""), "upload");
        assert_eq!(sanitize_upload_name("   "), "upload");
        assert_eq!(sanitize_upload_name(".."), "upload");
        assert_eq!(sanitize_upload_name("../.."), "upload");
        assert_eq!(sanitize_upload_name("photo.png"), "photo.png");

        let long = format!("{}.png", "n".repeat(400));
        assert_eq!(
            sanitize_upload_name(&long).chars().count(),
            MAX_UPLOAD_NAME_CHARS
        );

        // A multi-byte name must not be cut mid-character.
        let wide = "截图".repeat(200);
        assert!(sanitize_upload_name(&wide).chars().count() <= MAX_UPLOAD_NAME_CHARS);
    }

    #[test]
    fn the_stored_name_comes_from_the_sniffed_type_and_nothing_else() {
        let kind = sniff_upload_kind(&png_bytes()).unwrap();
        let first = stored_upload_name(kind);
        let second = stored_upload_name(kind);
        assert!(first.ends_with(".png"));
        assert_ne!(first, second, "each upload gets its own name");
        assert!(!first.contains('/') && !first.contains('\\') && !first.contains(".."));
        assert_eq!(
            first.len(),
            "00000000-0000-0000-0000-000000000000.png".len()
        );
    }

    #[test]
    fn uploads_expire_after_the_retention_window_but_survive_a_clock_jump() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!upload_expired(now - Duration::from_secs(47 * 3600), now));
        assert!(upload_expired(now - UPLOAD_RETENTION, now));
        assert!(upload_expired(now - Duration::from_secs(49 * 3600), now));
        // A timestamp in the future means the clock moved, not that the file is
        // old; deleting it would lose an upload the user just made.
        assert!(!upload_expired(now + Duration::from_secs(3600), now));
    }

    #[test]
    fn the_sweep_removes_only_files_past_the_retention_window() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-uploads-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();

        let fresh = dir.join("fresh.png");
        std::fs::write(&fresh, b"fresh").unwrap();
        let stale = dir.join("stale.png");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(now - UPLOAD_RETENTION - Duration::from_secs(60))
            .unwrap();

        assert_eq!(purge_expired_uploads(&dir, now).unwrap(), 1);
        assert!(fresh.exists());
        assert!(!stale.exists());

        // A missing directory is not an error: nothing has been uploaded yet.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(purge_expired_uploads(&dir, now).unwrap(), 0);
    }

    fn asset_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-assets-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The temp dir is itself a symlink on macOS, so the fixture is
        // canonicalized once here: every check downstream compares canonical
        // paths, and a test must not be the one place that does not.
        std::fs::canonicalize(&dir).unwrap()
    }

    fn test_asset_entry(path: &FsPath, root: &FsPath, modified_unix_ms: u128) -> AssetEntry {
        AssetEntry {
            id: asset_id(path),
            path: path.to_path_buf(),
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            size: 1,
            modified_unix_ms,
            root: root.to_path_buf(),
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: Some("wA:t1".into()),
            pane_id: Some("wA:p1".into()),
        }
    }

    #[test]
    fn asset_reads_are_fenced_inside_the_workspace_roots() {
        let root = asset_test_dir("fence");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("docs/report.md"), b"# report\n").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let roots = vec![workspace.clone()];

        assert!(resolve_asset_path(&workspace.join("docs/report.md"), &roots).is_some());

        // Traversal out of the root, whatever shape it arrives in.
        assert!(
            resolve_asset_path(&workspace.join("docs/../../outside/secret.txt"), &roots).is_none()
        );
        assert!(resolve_asset_path(&outside.join("secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(FsPath::new("/etc/hosts"), &roots).is_none());

        // A sibling whose name merely starts with the root's is not inside it.
        let neighbour = root.join("workspace-notes");
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("note.txt"), b"note\n").unwrap();
        assert!(resolve_asset_path(&neighbour.join("note.txt"), &roots).is_none());

        // A directory is not an asset, and neither is the root itself.
        assert!(resolve_asset_path(&workspace, &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("docs"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("missing.md"), &roots).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_indexed_asset_outlives_the_workspace_that_made_it() {
        // The worktree an agent wrote into was removed hours later. The roots
        // resolve to nothing now, which is the whole of what changed: the file
        // is still there and still the thing the user tapped.
        let root = asset_test_dir("provenance");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();

        // While the workspace is a root, nothing about the read changed.
        let live = vec![workspace.clone()];
        assert_eq!(
            resolve_indexed_asset_path(&report, &live),
            Some(report.clone())
        );

        // With no roots at all -- the workspace closed -- the stored path is
        // replayed and answers the same bytes.
        assert_eq!(
            resolve_indexed_asset_path(&report, &[]),
            Some(report.clone())
        );

        // A file that is gone is gone, roots or no roots.
        let deleted = workspace.join("gone.md");
        std::fs::write(&deleted, b"bye\n").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        assert!(resolve_indexed_asset_path(&deleted, &[]).is_none());

        // A directory left where the file was is not a file.
        std::fs::create_dir_all(workspace.join("was-a-file")).unwrap();
        assert!(resolve_indexed_asset_path(&workspace.join("was-a-file"), &[]).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_into_a_stored_asset_path_is_not_that_asset() {
        // The attack the equality guard exists for: the workspace closes, the
        // real file is replaced by a link to somewhere the gateway would never
        // have indexed, and the old id is presented again.
        let root = asset_test_dir("provenance-symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let stored = workspace.join("report.md");
        std::fs::write(&stored, b"# report\n").unwrap();
        assert!(resolve_indexed_asset_path(&stored, &[]).is_some());

        std::fs::remove_file(&stored).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), &stored).unwrap();

        // The path canonicalizes to the link's target, which is not the path
        // that was stored, so the replay refuses it -- and refuses it the same
        // way an unknown id is refused.
        assert!(resolve_indexed_asset_path(&stored, &[]).is_none());
        assert!(resolve_indexed_asset_path(&stored, std::slice::from_ref(&workspace)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_that_was_never_indexed_has_no_entry_to_replay() {
        // The fallback is reached through an index lookup and nowhere else, so
        // provenance is what it serves. A file the gateway never scanned has no
        // entry, and the read stops at the lookup with the same 404 as a bad id.
        let root = asset_test_dir("provenance-unindexed");
        std::fs::create_dir_all(&root).unwrap();
        let indexed = root.join("indexed.md");
        let never = root.join("never-scanned.md");
        std::fs::write(&indexed, b"# indexed\n").unwrap();
        std::fs::write(&never, b"# private\n").unwrap();

        let mut index = AssetIndex::default();
        index.upsert(test_asset_entry(&indexed, &root, 1));

        assert!(index.get(&asset_id(&indexed)).is_some());
        assert!(index.get(&asset_id(&never)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_uploads_directory_ingests_like_any_other_root_and_survives_a_cold_start() {
        // The bug this guards against: an upload lives in the gateway's own
        // uploads directory, which is not under any pane's cwd, so a rebuild
        // that only walks pane roots never finds it again once the in-memory
        // index is gone (e.g. after a restart). The cold-start path in
        // `asset_content` now adds the uploads directory as one more root per
        // session -- this proves that root ingests the same way a pane root
        // does, and that the resulting entry answers a lookup with none of the
        // session's live roots present, exactly the state a fresh process is
        // in right after startup.
        let uploads = asset_test_dir("uploads-cold-start");
        let upload = uploads.join("5969c1e3.webp");
        std::fs::write(&upload, b"fake webp bytes").unwrap();

        let root = AssetRoot {
            path: uploads.clone(),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        };
        let index: Mutex<AssetIndex> = Mutex::new(AssetIndex::default());
        let created = ingest_root(&index, &root);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].path, upload);
        assert_eq!(created[0].session_id, "default");

        // Simulate the state right after a restart answering `asset_content`:
        // the entry is in the index (just rebuilt), but the session's live
        // roots -- the pane cwds -- do not include the uploads directory and
        // never will. `resolve_indexed_asset_path`'s own-path fallback is what
        // actually serves it; this proves the entry it is given exists to
        // fall back on at all.
        let id = asset_id(&upload);
        let entry = index.lock().unwrap().get(&id).unwrap();
        assert_eq!(entry.path, upload);
        let no_live_pane_roots: Vec<PathBuf> = Vec::new();
        assert_eq!(
            resolve_indexed_asset_path(&entry.path, &no_live_pane_roots),
            Some(upload.clone())
        );

        std::fs::remove_dir_all(&uploads).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_workspace_root_cannot_be_read_or_scanned() {
        let root = asset_test_dir("symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("escape.txt"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape-dir")).unwrap();
        std::fs::write(workspace.join("own.txt"), b"mine\n").unwrap();
        let roots = vec![workspace.clone()];

        // The link resolves to where it points, which is outside the root.
        assert!(resolve_asset_path(&workspace.join("escape.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("escape-dir/secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("own.txt"), &roots).is_some());

        // The scan never offers such a path in the first place.
        let names: Vec<String> = scan_workspace_root(&workspace, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert_eq!(names, vec![String::from("own.txt")]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn asset_kind_comes_from_the_bytes_and_the_extension_only_splits_the_text_kinds() {
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.png").kind,
            AssetKind::Image
        );
        // A name that lies about the content does not change what it is.
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.md").kind,
            AssetKind::Image
        );
        assert_eq!(
            sniff_asset_type(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n", "report.pdf").kind,
            AssetKind::Pdf
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.md").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.MARKDOWN").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.txt").kind,
            AssetKind::Text
        );
        assert_eq!(
            sniff_asset_type("日本語とemoji 🎈\n".as_bytes(), "notes").kind,
            AssetKind::Text
        );
        // A markdown extension over bytes that are not text is still binary.
        assert_eq!(
            sniff_asset_type(b"\x00\x01\x02binary\x00", "notes.md").kind,
            AssetKind::Binary
        );
        assert_eq!(
            sniff_asset_type(&[0xff, 0xfe, 0xfd, 0xfc], "blob.bin").kind,
            AssetKind::Binary
        );
        assert_eq!(sniff_asset_type(b"", "empty.txt").kind, AssetKind::Text);

        assert!(AssetKind::Markdown.previewable());
        assert!(AssetKind::Image.previewable());
        assert!(AssetKind::Pdf.previewable());
        assert!(!AssetKind::Binary.previewable());

        // A UTF-8 character cut in half by the sniff window is a truncation,
        // not a binary file.
        let mut truncated = "héllo".as_bytes().to_vec();
        truncated.pop();
        assert!(looks_textual(&truncated));
        assert!(!looks_textual(b"text\x00text"));
    }

    #[test]
    fn a_scan_stays_shallow_and_skips_heavy_directories() {
        let root = asset_test_dir("scan");
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
        std::fs::write(root.join("report.md"), b"# report\n").unwrap();
        std::fs::write(root.join(".hidden"), b"hidden\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), b"module\n").unwrap();
        std::fs::write(root.join("target/debug/binary"), b"binary\n").unwrap();
        std::fs::write(root.join(".git/objects/blob"), b"blob\n").unwrap();
        std::fs::write(root.join("a/b/c/deep.txt"), b"deep\n").unwrap();
        std::fs::write(root.join("a/b/c/d/e/too-deep.txt"), b"too deep\n").unwrap();

        let mut names: Vec<String> = scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![String::from("deep.txt"), String::from("report.md")]
        );

        // The file budget is a hard stop, not a suggestion.
        assert_eq!(scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 1).len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_index_dedupes_by_path_and_answers_newest_first() {
        let root = asset_test_dir("index");
        let nested = root.join("nested");
        let mut index = AssetIndex::default();

        let old = root.join("old.txt");
        let new = root.join("new.txt");
        assert!(index.upsert(test_asset_entry(&old, &root, 1_000)));
        assert!(index.upsert(test_asset_entry(&new, &root, 2_000)));
        // The same file seen again by a later scan updates it in place.
        let mut rescanned = test_asset_entry(&old, &root, 3_000);
        rescanned.size = 4_096;
        assert!(!index.upsert(rescanned));
        assert_eq!(index.entries.len(), 2);

        // `test_asset_entry` puts everything in workspace "wA".
        let scope_a = AssetScope::Workspace("wA".into());
        let scope_b = AssetScope::Workspace("wB".into());
        let listed = index.session_assets("default", &scope_a, None, 10);
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>(),
            vec![String::from("old.txt"), String::from("new.txt")]
        );
        assert_eq!(listed[0].size, 4_096);
        assert_eq!(listed[0].modified_unix_ms, 3_000);

        // `since` is exclusive, and `limit` cuts the newest page.
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(2_000), 10)
                .len(),
            1
        );
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(3_000), 10)
                .len(),
            0
        );
        assert_eq!(index.session_assets("default", &scope_a, None, 1).len(), 1);
        assert_eq!(index.session_assets("other", &scope_a, None, 10).len(), 0);
        // Same session, a different workspace: none of "wA"'s files leak into
        // it. This is the defect the workspace scope closes -- everything
        // above proves the index still works exactly as it did, this proves
        // it no longer answers wider than the workspace asked for.
        assert_eq!(index.session_assets("default", &scope_b, None, 10).len(), 0);

        // Nested roots see the same file; the deeper one owns it.
        let shared = nested.join("shared.txt");
        assert!(index.upsert(test_asset_entry(&shared, &root, 4_000)));
        let mut deeper = test_asset_entry(&shared, &nested, 4_000);
        deeper.workspace_id = Some("wB".into());
        assert!(!index.upsert(deeper));
        let owned = index.get(&asset_id(&shared)).unwrap();
        assert_eq!(owned.root, nested);
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));
        // A shallower root does not take it back.
        let mut shallower = test_asset_entry(&shared, &root, 5_000);
        shallower.workspace_id = Some("wA".into());
        assert!(!index.upsert(shallower));
        assert_eq!(index.get(&asset_id(&shared)).unwrap().root, nested);

        // A removed worktree takes its files with it.
        index.forget_under(&nested);
        assert!(index.get(&asset_id(&shared)).is_none());
        assert_eq!(index.entries.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_kind_allow_list_is_normalized_and_an_absent_one_filters_nothing() {
        assert!(asset_kind_filter(None).is_empty());
        assert!(asset_kind_filter(Some("")).is_empty());
        // A trailing comma is a client's join, not a kind.
        assert!(asset_kind_filter(Some(",, ,")).is_empty());
        assert_eq!(
            asset_kind_filter(Some("image")),
            vec![String::from("image")]
        );
        assert_eq!(
            asset_kind_filter(Some(" Markdown , PDF ")),
            vec![String::from("markdown"), String::from("pdf")]
        );
        // A kind outside the taxonomy is carried as asked and simply matches
        // nothing, the way an unknown name in the events allow-list does.
        assert_eq!(
            asset_kind_filter(Some("document")),
            vec![String::from("document")]
        );
    }

    /// The listing's own state, pointed at a socket that is not there: no roots
    /// come back and none are remembered, so nothing rescans and the entries
    /// under test stay as they were put in. Their mtimes are what the ordering
    /// is asserted on, which a run of files written in the same millisecond
    /// could not give.
    fn asset_listing_state(root: &FsPath, entries: Vec<AssetEntry>) -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path =
            root.join("herdr.sock").to_string_lossy().to_string();
        {
            let mut index = state.assets.lock().unwrap();
            for entry in entries {
                index.upsert(entry);
            }
        }
        state
    }

    fn asset_listing_query(kind: Option<&str>, limit: usize) -> AssetsQuery {
        AssetsQuery {
            since: None,
            limit: Some(limit),
            kind: kind.map(str::to_owned),
            path: None,
        }
    }

    async fn listed_asset_names(state: &AppState, kind: Option<&str>, limit: usize) -> Vec<String> {
        // `test_asset_entry` puts every fixture in workspace "wA".
        let response = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(kind, limit)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        response.0["data"]["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_kind_filter_answers_the_newest_of_that_kind_not_the_kind_among_the_newest() {
        // The shape that made the filter necessary: an agent editing source code
        // writes files faster than it writes artifacts, so the image and the
        // documents sit well behind the newest page.
        let root = asset_test_dir("kind-listing");
        std::fs::write(root.join("chart.png"), png_bytes()).unwrap();
        std::fs::write(root.join("notes.md"), b"# notes\n").unwrap();
        std::fs::write(root.join("report.pdf"), b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n").unwrap();
        for index in 0..3 {
            std::fs::write(root.join(format!("mod{index}.rs")), b"fn main() {}\n").unwrap();
        }
        let state = asset_listing_state(
            &root,
            vec![
                test_asset_entry(&root.join("chart.png"), &root, 1_000),
                test_asset_entry(&root.join("notes.md"), &root, 2_000),
                test_asset_entry(&root.join("report.pdf"), &root, 3_000),
                test_asset_entry(&root.join("mod0.rs"), &root, 4_000),
                test_asset_entry(&root.join("mod1.rs"), &root, 5_000),
                test_asset_entry(&root.join("mod2.rs"), &root, 6_000),
            ],
        );

        // No kind is the old answer exactly: the newest files, whatever they are.
        assert_eq!(
            listed_asset_names(&state, None, 3).await,
            vec![
                String::from("mod2.rs"),
                String::from("mod1.rs"),
                String::from("mod0.rs")
            ]
        );
        assert_eq!(listed_asset_names(&state, Some(""), 3).await.len(), 3);

        // The image is the fourth-oldest file of six, so a page of three would
        // never have shown it. Filtering during the scan is what finds it.
        assert_eq!(
            listed_asset_names(&state, Some("image"), 3).await,
            vec![String::from("chart.png")]
        );
        // One request for a client filter that spans two kinds, still newest
        // first across both.
        assert_eq!(
            listed_asset_names(&state, Some("markdown,pdf"), 3).await,
            vec![String::from("report.pdf"), String::from("notes.md")]
        );
        // The page is still cut to the limit, and cut from the matches.
        assert_eq!(
            listed_asset_names(&state, Some("text"), 2).await,
            vec![String::from("mod2.rs"), String::from("mod1.rs")]
        );
        // A kind the gateway does not have matches nothing rather than erroring
        // or quietly widening back to everything.
        assert!(listed_asset_names(&state, Some("document"), 3)
            .await
            .is_empty());

        // The applied allow-list comes back, so a client can tell this gateway
        // from one old enough to have ignored the parameter.
        let response = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(Some("Image"), 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(response.0["data"]["kind"], json!(["image"]));
        assert_eq!(response.0["data"]["assets"][0]["kind"], "image");
        let unfiltered = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(unfiltered.0["data"]["kind"], json!([]));

        // A different workspace in the same session sees none of it: the
        // scope this fix adds is the workspace, not the session, and this is
        // the reported defect in miniature -- another workspace's files never
        // showing up in this one's listing.
        let other_workspace = session_assets(
            State(state.clone()),
            Path(("default".into(), "wB".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert!(other_workspace.0["data"]["assets"]
            .as_array()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_page_is_filled_from_the_matches_and_sniffs_no_further_than_it_has_to() {
        let root = asset_test_dir("kind-page");
        std::fs::write(root.join("a.png"), png_bytes()).unwrap();
        std::fs::write(root.join("b.png"), png_bytes()).unwrap();
        std::fs::write(root.join("c.txt"), b"plain\n").unwrap();
        let ordered = vec![
            test_asset_entry(&root.join("a.png"), &root, 3_000),
            test_asset_entry(&root.join("b.png"), &root, 2_000),
            test_asset_entry(&root.join("c.txt"), &root, 1_000),
        ];

        let names = |page: Vec<Value>| -> Vec<String> {
            page.iter()
                .map(|asset| asset["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            names(asset_page(ordered.clone(), &[], 2)),
            vec![String::from("a.png"), String::from("b.png")]
        );
        assert_eq!(
            names(asset_page(ordered.clone(), &[String::from("image")], 1)),
            vec![String::from("a.png")]
        );
        // A name that lies is not what the filter goes on: the kind is the one
        // sniffed from the bytes, the same one the asset carries on the wire.
        std::fs::write(root.join("a.png"), b"not an image at all\n").unwrap();
        assert_eq!(
            names(asset_page(ordered, &[String::from("image")], 2)),
            vec![String::from("b.png")]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_exact_path_lookup_reaches_an_upload_only_through_the_uploads_root() {
        // The path a client is guaranteed to hold for an upload is the one the
        // upload response returned -- the gateway's own directory, never under
        // a pane's cwd. Without the fold-in, that exact path was the one
        // lookup that could never answer.
        let base = asset_test_dir("uploads-by-path");
        let uploads = base.join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        let stored = uploads.join("9126cf50.webp");
        std::fs::write(&stored, b"fake webp bytes").unwrap();
        let pane_roots = vec![AssetRoot {
            path: base.join("workspace"),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        }];

        // Pane roots alone miss it; the composed lookup roots answer it.
        assert!(asset_entry_for_path(&stored.to_string_lossy(), &pane_roots).is_none());
        let composed = with_uploads_root(pane_roots.clone(), "default", Some(uploads.clone()));
        let found = asset_entry_for_path(&stored.to_string_lossy(), &composed).unwrap();
        assert_eq!(found.path, stored);
        assert_eq!(found.session_id, "default");

        // No uploads directory resolved leaves the roots untouched.
        assert_eq!(
            with_uploads_root(pane_roots.clone(), "default", None).len(),
            1
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_preview_accepts_home_files_without_widening_scan_roots() {
        let base = asset_test_dir("home-preview");
        let home = base.join("home");
        let workspace = home.join("app");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(home.join("docs")).unwrap();
        std::fs::create_dir_all(home.join(".config")).unwrap();
        let report = home.join("docs/report.md");
        let config = home.join(".config/example.txt");
        let outside = base.join("outside.txt");
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&config, b"configuration").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        let scan_roots = vec![AssetRoot {
            path: workspace,
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: None,
            pane_id: None,
        }];
        let roots = preview_lookup_roots(
            scan_roots.clone(),
            "default",
            Some(&home),
            [base.clone(), home.clone(), PathBuf::from("/")],
        );
        assert_eq!(scan_roots.len(), 1);
        assert!(asset_entry_for_path(&report.to_string_lossy(), &scan_roots).is_none());
        assert_eq!(roots.len(), 2);
        for path in [&report, &config] {
            let entry = asset_entry_for_path(&path.to_string_lossy(), &roots).unwrap();
            assert_eq!(entry.session_id, "default");
            assert_eq!(
                resolve_indexed_asset_path(&entry.path, &[]),
                Some(entry.path)
            );
        }
        assert!(asset_entry_for_path(&outside.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(&home.to_string_lossy(), &roots).is_none());
        #[cfg(unix)]
        {
            let link = home.join("escape.txt");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(asset_entry_for_path(&link.to_string_lossy(), &roots).is_none());
        }
        assert!(preview_lookup_roots(vec![], "default", Some(FsPath::new("/")), []).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_exact_path_lookup_answers_one_asset_or_none_and_never_leaves_the_roots() {
        let base = asset_test_dir("lookup");
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(workspace.join("a/b/c/d/e/f")).unwrap();
        std::fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();
        // Deeper than the scan goes, and inside a directory the scan skips: the
        // user pointed at these, so an exact lookup still resolves them.
        let deep = workspace.join("a/b/c/d/e/f/deep.txt");
        std::fs::write(&deep, b"deep\n").unwrap();
        let skipped = workspace.join("node_modules/pkg/index.js");
        std::fs::write(&skipped, b"module\n").unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"secret\n").unwrap();
        let roots = vec![AssetRoot {
            path: workspace.clone(),
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: Some("wA:t1".into()),
            pane_id: Some("wA:p1".into()),
        }];

        let found = asset_entry_for_path(&report.to_string_lossy(), &roots).unwrap();
        assert_eq!(found.path, report);
        assert_eq!(found.id, asset_id(&report));
        assert_eq!(found.name, "report.md");
        assert_eq!(found.workspace_id.as_deref(), Some("wA"));
        assert!(asset_entry_for_path(&deep.to_string_lossy(), &roots).is_some());
        assert!(asset_entry_for_path(&skipped.to_string_lossy(), &roots).is_some());

        // The fence still holds, and a fenced-out path is simply a miss.
        assert!(asset_entry_for_path(&secret.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(
            &workspace.join("../outside/secret.txt").to_string_lossy(),
            &roots
        )
        .is_none());
        assert!(asset_entry_for_path("/etc/hosts", &roots).is_none());
        assert!(asset_entry_for_path(&workspace.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(&workspace.join("a").to_string_lossy(), &roots).is_none());
        assert!(
            asset_entry_for_path(&workspace.join("gone.md").to_string_lossy(), &roots).is_none()
        );
        assert!(asset_entry_for_path("report.md", &roots).is_none());
        assert!(asset_entry_for_path(&report.to_string_lossy(), &[]).is_none());

        // Nested roots: the deepest one owns the file it contains.
        let nested = AssetRoot {
            path: workspace.join("a"),
            session_id: "default".into(),
            workspace_id: Some("wB".into()),
            tab_id: Some("wB:t1".into()),
            pane_id: Some("wB:p1".into()),
        };
        let mut both = roots.clone();
        both.push(nested);
        let owned = asset_entry_for_path(&deep.to_string_lossy(), &both).unwrap();
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn asset_ids_are_stable_per_path_and_carry_no_path() {
        let first = asset_id(FsPath::new("/tmp/workspace/report.md"));
        assert_eq!(first, asset_id(FsPath::new("/tmp/workspace/report.md")));
        assert_ne!(first, asset_id(FsPath::new("/tmp/workspace/report2.md")));
        assert!(first.starts_with("as_"));
        assert!(!first.contains("report"));
        assert!(first[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn workspace_roots_come_from_pane_cwds_and_never_widen_to_the_whole_machine() {
        let home = dirs::home_dir().unwrap();
        let response = json!({
            "result": { "panes": [
                { "pane_id": "wA:p1", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                // A second pane in the same directory is the same root.
                { "pane_id": "wA:p2", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                { "pane_id": "wB:p1", "workspace_id": "wB", "tab_id": "wB:t1", "foreground_cwd": "/Users/okk/.ws/api" },
                { "pane_id": "wC:p1", "workspace_id": "wC", "cwd": "/" },
                { "pane_id": "wD:p1", "workspace_id": "wD", "cwd": home.to_string_lossy() },
                { "pane_id": "wE:p1", "workspace_id": "wE" }
            ] }
        });
        let roots = pane_list_roots("default", &response);
        assert_eq!(
            roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![
                String::from("/Users/okk/.repos/muqun"),
                String::from("/Users/okk/.ws/api")
            ]
        );
        assert_eq!(roots[0].pane_id.as_deref(), Some("wA:p1"));
        assert_eq!(roots[0].tab_id.as_deref(), Some("wA:t1"));
        assert_eq!(roots[1].workspace_id.as_deref(), Some("wB"));
        assert_eq!(roots[1].tab_id.as_deref(), Some("wB:t1"));
        assert!(pane_list_roots("default", &json!({ "result": {} })).is_empty());
    }

    #[test]
    fn asset_scope_narrows_to_a_tab_or_to_a_workspace_and_never_to_the_other() {
        // The invariant card #802's tab-scoping stands on: a `Tab` scope only
        // ever matches its own tmux window, and a `Workspace` scope (what a
        // herdr session resolves its tab to) matches every tab inside that
        // workspace -- so a herdr workspace with more than one tab keeps
        // seeing all of them, exactly as it did before tabs existed here.
        let root_in_tab_a = AssetRoot {
            path: PathBuf::from("/work/a"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t1".into()),
            pane_id: Some("wM:t1:p1".into()),
        };
        let root_in_tab_b = AssetRoot {
            path: PathBuf::from("/work/b"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t2".into()),
            pane_id: Some("wM:t2:p1".into()),
        };
        let root_elsewhere = AssetRoot {
            path: PathBuf::from("/work/c"),
            session_id: "default".into(),
            workspace_id: Some("wN".into()),
            tab_id: Some("wN:t1".into()),
            pane_id: Some("wN:t1:p1".into()),
        };

        let tab_scope = AssetScope::Tab("wM:t1".into());
        assert!(tab_scope.matches_root(&root_in_tab_a));
        assert!(!tab_scope.matches_root(&root_in_tab_b));
        assert!(!tab_scope.matches_root(&root_elsewhere));

        // A herdr session's tab id resolves to its workspace before scoping,
        // so both of that workspace's tabs match -- this is the "herdr must
        // not narrow" guarantee, proven at the layer that actually filters.
        let workspace_scope = AssetScope::Workspace("wM".into());
        assert!(workspace_scope.matches_root(&root_in_tab_a));
        assert!(workspace_scope.matches_root(&root_in_tab_b));
        assert!(!workspace_scope.matches_root(&root_elsewhere));
    }

    #[tokio::test]
    async fn a_tmux_session_scopes_by_tab_directly_and_a_herdr_session_resolves_it_to_a_workspace()
    {
        // tmux: the tab id is used verbatim, no lookup involved.
        let mut session = test_config("token").sessions[0].clone();
        session.backend = BackendKind::Tmux;
        assert_eq!(
            resolve_asset_scope(&session, "@3").await,
            AssetScope::Tab("@3".into())
        );

        // herdr: with no live socket to ask (as in every other test in this
        // file), the tab id can't be translated to its owning workspace, so
        // it is kept as-is rather than silently widened to "no scope at all".
        // This is also exactly what makes every pre-existing
        // `session_assets(Path(("default", "wA")))` test call in this file
        // keep behaving as a workspace-scoped call after this change: they
        // never had a live socket either.
        let mut herdr_session = test_config("token").sessions[0].clone();
        herdr_session.backend = BackendKind::Herdr;
        herdr_session.socket_path = "/tmp/herdr-does-not-exist.sock".into();
        assert_eq!(
            resolve_asset_scope(&herdr_session, "wA").await,
            AssetScope::Workspace("wA".into())
        );
    }

    #[test]
    fn worktree_events_name_a_root_to_scan_and_never_a_file() {
        // The payload Herdr actually sends on protocol 17: the worktree's
        // checkout path and its workspace, with no file information at all.
        let created = json!({
            "event": "worktree_created",
            "data": {
                "type": "worktree_created",
                "workspace": { "workspace_id": "wM", "number": 3, "label": "muqun" },
                "worktree": {
                    "path": "/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463",
                    "branch": "wip/live-activity",
                    "is_bare": false,
                    "is_detached": false,
                    "is_prunable": false,
                    "is_linked_worktree": true,
                    "label": "muqun"
                }
            }
        })
        .to_string();
        let root = worktree_event_root("default", &created).unwrap();
        assert_eq!(
            root.path,
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463")
        );
        assert_eq!(root.workspace_id.as_deref(), Some("wM"));
        assert_eq!(root.session_id, "default");

        let opened = json!({
            "event": "worktree_opened",
            "data": { "type": "worktree_opened", "already_open": false,
                "workspace": { "workspace_id": "wZ" },
                "worktree": { "path": "/Users/okk/.repos/muqun", "label": "muqun" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_root("default", &opened).unwrap().path,
            PathBuf::from("/Users/okk/.repos/muqun")
        );

        let removed = json!({
            "event": "worktree_removed",
            "data": { "type": "worktree_removed", "forced": false, "workspace_id": "wZ",
                "worktree": { "path": "/Users/okk/.repos/muqun/.claude/worktrees/gone" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_removed_root(&removed).unwrap(),
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/gone")
        );
        assert!(worktree_event_root("default", &removed).is_none());

        // Everything else on the stream leaves the index alone.
        assert!(worktree_event_root("default", r#"{"event":"pane_updated","data":{}}"#).is_none());
        assert!(worktree_event_root("default", "not json").is_none());
        assert!(worktree_event_removed_root(r#"{"event":"pane_updated","data":{}}"#).is_none());
    }

    #[test]
    fn an_asset_envelope_is_versioned_and_declares_capabilities() {
        let root = PathBuf::from("/tmp/workspace");
        let entry = test_asset_entry(&root.join("report.md"), &root, 1_785_100_000_000);
        let envelope: Value = serde_json::from_str(&asset_created_payload(
            &entry,
            sniff_asset_type(b"# report\n", "report.md"),
        ))
        .unwrap();
        assert_eq!(envelope["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(envelope["capabilities"]["assets"], true);
        let asset = &envelope["data"]["asset"];
        assert_eq!(asset["id"], entry.id);
        assert_eq!(asset["kind"], "markdown");
        assert_eq!(asset["mime"], "text/markdown; charset=utf-8");
        assert_eq!(asset["previewable"], true);
        assert_eq!(asset["origin"]["session_id"], "default");
        assert_eq!(asset["origin"]["workspace_id"], "wA");
        assert_eq!(asset["origin"]["pane_id"], "wA:p1");
        assert_eq!(asset["modified_unix_ms"], 1_785_100_000_000_u64);
    }

    #[test]
    fn the_content_envelope_declares_parts_at_the_version_that_added_them() {
        // One envelope and one version across the content model: a client reads
        // the version once and knows both endpoints answer it.
        let envelope = content_envelope(json!({}));
        assert_eq!(envelope["schema_version"], "1.5.0");
        assert_eq!(envelope["capabilities"]["parts"], true);
        assert_eq!(envelope["capabilities"]["assets"], true);
        assert_eq!(envelope["capabilities"]["image_upload"], true);
        assert_eq!(envelope["capabilities"]["composer"], true);
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

    /// The drawn-menu spelling, which is what every existing assertion means.
    fn approval_data_menu(
        session_id: &str,
        pane_id: &str,
        agent: Option<&str>,
        approval: Option<&approvals::Approval>,
    ) -> Value {
        approval_data(session_id, pane_id, agent, approval, "menu")
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

    #[test]
    fn an_asset_file_name_cannot_break_out_of_a_response_header() {
        assert_eq!(header_safe_name("report.md"), "report.md");
        assert_eq!(
            header_safe_name("re\"port\r\nX-Evil: 1.md"),
            "reportX-Evil 1.md"
        );
        assert_eq!(header_safe_name("../../etc/passwd"), "....etcpasswd");
        assert_eq!(header_safe_name("图片.png"), ".png");
        assert_eq!(header_safe_name("\u{202e}"), "asset");
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

    #[test]
    fn openapi_spec_contains_docs_routes_and_auth() {
        let spec = openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        let output = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"];
        assert!(output.is_object());
        assert_eq!(output["parameters"][4]["name"], "start");
        assert_eq!(output["parameters"][5]["name"], "end");
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/zoom"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/events"].is_object());
        assert!(spec["paths"]["/api/pair/request"].is_object());
        assert!(spec["paths"]["/api/pair/claim"].is_object());
        assert!(spec["paths"]["/api/meta"].is_object());
        assert!(spec["paths"]["/api/devices/push-token"]["delete"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/workspaces/{workspaceId}"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/agents/{target}/send"].is_object());
        // The cursor and the geometry have to be in the spec, and so does
        // which backend leaves which of them null: a client that assumes
        // `width` is always there measures a herdr pane wrong.
        for route in [
            "/api/sessions/{sessionId}/panes",
            "/api/sessions/{sessionId}/panes/{paneId}",
        ] {
            let pane = &spec["paths"][route]["get"];
            let described = pane["description"].as_str().unwrap_or_default();
            assert!(described.contains("cursor_x"), "{route} omits the cursor");
            assert!(
                described.contains("herdr"),
                "{route} does not say which backend leaves these null"
            );
            let item = &pane["responses"]["200"]["content"]["application/json"]["schema"]
                ["properties"]["result"]["properties"]["panes"]["items"]["properties"];
            for field in ["width", "height", "cursor_x", "cursor_y"] {
                assert_eq!(
                    item[field]["type"],
                    json!(["integer", "null"]),
                    "{route}.{field} must be documented as nullable"
                );
            }
            assert_eq!(
                item["scroll"]["properties"]["alternate_on"]["type"],
                json!(["boolean", "null"])
            );
        }

        // The mode has to be in the spec or a client has no way to learn the
        // field exists, and no way to know that leaving it out is a paste.
        let send_text =
            &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/send-text"]["post"];
        let mode = &send_text["requestBody"]["content"]["application/json"]["schema"]["properties"]
            ["mode"];
        assert_eq!(mode["enum"], json!(["paste", "keys"]));
        assert_eq!(mode["default"], "paste");
        // Required stays exactly `text`: adding the field must not make every
        // existing client's body invalid.
        assert_eq!(
            send_text["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["text"])
        );
        assert!(
            spec["paths"]["/api/uploads"]["post"]["requestBody"]["content"]["multipart/form-data"]
                .is_object()
        );
        assert!(spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"].is_object());
        let parts = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/parts"]["get"];
        assert!(parts.is_object());
        assert_eq!(parts["parameters"][2]["name"], "lines");
        let part = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["parts"]["items"];
        // A client dispatches on `type` and falls back on `fallback_text`, so
        // the spec has to require exactly those two of every part.
        assert_eq!(part["required"], json!(["type", "fallback_text"]));
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("tool-block")));
        // v2's one addition to the closed set. It has to be in the spec's enum
        // or a client has no way to learn the type exists without meeting one.
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("approval")));
        assert!(part["properties"]["approval_id"].is_object());
        assert_eq!(
            part["properties"]["options"]["items"]["required"],
            json!(["index", "label", "decision"])
        );
        // Which source answered is a value of an enum that already had two.
        let pane = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"];
        assert_eq!(
            pane["parts"]["enum"],
            json!(["native", "dictionary", "text"])
        );
        assert!(pane["native"].is_object());
        assert_eq!(
            parts["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["data"]["properties"]["source"]["enum"],
            json!(["recent-unwrapped", "native"])
        );
        // The composer descriptor rides on the pane, and its source vocabulary
        // is closed the same way the part types are.
        let composer = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"]["composer"];
        assert_eq!(
            composer["properties"]["slash_commands"]["items"]["properties"]["source"]["enum"],
            json!(["builtin", "workspace"])
        );
        let files = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/files"]["get"];
        assert!(files.is_object());
        assert_eq!(files["parameters"][2]["name"], "query");
        assert_eq!(files["parameters"][3]["name"], "limit");
        assert_eq!(
            files["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["schema_version"]["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"]["responses"]
                ["200"]["content"]["application/json"]["schema"]["properties"]["schema_version"]
                ["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["415"].is_object()
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["413"].is_object()
        );
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["413"].is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["415"].is_object());
        // The upload answers with both ways of reaching the file, and both are
        // required: a client that only got `path` could not draw the
        // attachment it just sent.
        let stored = &spec["paths"]["/api/uploads"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            stored["required"],
            json!(["path", "url", "name", "size", "mime"])
        );
        assert!(stored["properties"]["url"].is_object());
        let upload_read = &spec["paths"]["/api/uploads/{fileName}"]["get"];
        assert!(upload_read.is_object());
        assert_eq!(upload_read["parameters"][0]["name"], "fileName");
        assert!(upload_read["responses"]["404"].is_object());
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"]["parameters"]
                [6]["name"],
            "format"
        );

        // Task dispatch, including the partial answer, which a client that only
        // handles 200 and "error" would silently mishandle.
        let tasks = &spec["paths"]["/api/sessions/{sessionId}/tasks"]["post"];
        assert!(tasks.is_object());
        assert!(tasks["responses"]["207"].is_object());
        assert!(tasks["responses"]["403"].is_object());
        assert_eq!(
            tasks["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["repo_path", "agent"])
        );
        assert!(spec["paths"]["/api/agents/catalog"]["get"].is_object());
        let cap = &spec["paths"]["/api/capabilities"]["get"];
        assert!(cap.is_object());
        assert_eq!(
            cap["responses"]["200"]["content"]["application/json"]["schema"]["required"],
            json!(["serverVersion", "protocolVersion", "planes", "capabilities"])
        );
        assert!(spec["paths"]["/api/agent-engine"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-catalog"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-sessions"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-sessions"]["post"].is_object());
        assert!(spec["paths"]["/api/agent-sessions/{asid}/prompt"]["post"].is_object());
        assert!(spec["paths"]["/api/agent-sessions/{asid}/events"]["get"].is_object());
    }

    #[test]
    fn the_api_version_and_capabilities_announce_task_dispatch() {
        // A minor bump: the routes are additive, so an older client keeps
        // working, and a newer one can gate on the capability rather than on
        // probing for a 404.
        assert!(GATEWAY_API_VERSION.starts_with("1.8."));
        assert_eq!(GATEWAY_API_MAJOR, 1);
        assert!(API_CAPABILITIES.contains(&"tasks"));
        assert!(API_CAPABILITIES.contains(&"agent_catalog"));
        assert!(API_CAPABILITIES.contains(&"terminal_backends"));
        assert!(API_CAPABILITIES.contains(&"multiple_terminal_backends"));
        assert!(API_CAPABILITIES.contains(&"capabilities_discovery"));
        assert!(API_CAPABILITIES.contains(&"harness_discovery"));
    }

    #[tokio::test]
    async fn capabilities_discovery_endpoint_and_health_expose_dual_planes() {
        use tower::ServiceExt;
        let mut state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        let app = Router::new()
            .route("/health", axum::routing::get(health))
            .route("/api/capabilities", axum::routing::get(api_capabilities))
            .with_state(state);

        // 1. GET /api/capabilities
        let req = Request::builder()
            .uri("/api/capabilities")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["planes"]["terminal"].is_object());
        assert!(json["planes"]["harness"].is_object());
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("capabilities_discovery")));
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("harness_discovery")));

        // 2. GET /health contains planes
        let req = Request::builder()
            .uri("/health")
            .method("GET")
            .header("authorization", "Bearer device-token")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["planes"]["terminal"].is_object());
        assert!(json["planes"]["harness"].is_object());
    }

    #[tokio::test]
    async fn headless_gateway_exposes_capabilities_without_crashing() {
        use tower::ServiceExt;
        let mut state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        state.config.sessions.clear();

        let app = Router::new()
            .route("/health", axum::routing::get(health))
            .route("/api/capabilities", axum::routing::get(api_capabilities))
            .with_state(state);

        // 1. GET /api/capabilities
        let req = Request::builder()
            .uri("/api/capabilities")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["planes"]["terminal"]["supported"], false);
        assert_eq!(
            json["planes"]["terminal"]["degradedReason"],
            "no_terminal_backend_configured"
        );

        // 2. GET /health
        let req = Request::builder()
            .uri("/health")
            .method("GET")
            .header("authorization", "Bearer device-token")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["planes"]["terminal"]["supported"], false);
        assert_eq!(json["backend"], Value::Null);
    }

    /// The snapshot is one call where the app used to make four, and its
    /// `agents` array is now the real agent list -- so a client can prewarm a
    /// session from it and drop the separate `/agents` call. It is announced
    /// because the alternative is the app probing for a 404 and then guessing
    /// whether the `agents` it got back carry `instance_id` and `target`.
    #[test]
    fn the_session_snapshot_is_announced_as_a_capability() {
        assert!(API_CAPABILITIES.contains(&"session_snapshot"));
        assert!(
            gateway_capabilities(false).contains(&"session_snapshot"),
            "it is a property of this build, not of a session's backend"
        );
        assert!(gateway_capabilities(true).contains(&"session_snapshot"));
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
    fn the_pane_context_and_git_routes_are_documented_and_announced() {
        let spec = openapi_spec();
        for path in [
            "/api/sessions/{sessionId}/panes/{paneId}/context",
            "/api/sessions/{sessionId}/panes/{paneId}/git/status",
            "/api/sessions/{sessionId}/panes/{paneId}/git/diff",
        ] {
            assert!(
                spec["paths"][path]["get"].is_object(),
                "{path} is not documented"
            );
        }
        let names: Vec<&str> = spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/git/diff"]
            ["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "sessionId",
                "paneId",
                "path",
                "old_path",
                "staged",
                "context",
                "from",
                "lines"
            ]
        );
        for capability in ["pane_context", "git_diff"] {
            assert!(
                API_CAPABILITIES.contains(&capability),
                "{capability} is not announced"
            );
        }
        assert!(CONTENT_SCHEMA_VERSION.starts_with("1.5."));
    }

    fn git_test_repo(name: &str) -> (PathBuf, PathBuf) {
        let root = asset_test_dir(name);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        for args in [
            vec!["init", "--initial-branch", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let output = ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        std::fs::write(repo.join("src/a.ts"), "const a = 1;\nconst b = 2;\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
            let output = ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        std::fs::write(repo.join("src/a.ts"), "const a = 1;\nconst B = 2;\n").unwrap();
        std::fs::write(repo.join("notes.md"), "new\n").unwrap();
        (root, repo)
    }

    fn remember_pane_root(state: &AppState, pane_id: &str, path: PathBuf) {
        state.assets.lock().unwrap().remember_roots(
            "default",
            None,
            vec![AssetRoot {
                path,
                session_id: "default".into(),
                workspace_id: Some("wA".into()),
                tab_id: Some("wA:t1".into()),
                pane_id: Some(pane_id.into()),
            }],
        );
    }

    #[tokio::test]
    async fn git_status_lists_the_checkout_of_the_panes_fenced_directory() {
        let (root, repo) = git_test_repo("git-status");
        let state = unreachable_state();
        // The pane sits in a subdirectory; the checkout is found above it.
        remember_pane_root(&state, "wA:p1", repo.join("src"));

        let answer = pane_git_status(
            State(state.clone()),
            Path(("default".into(), "wA:p1".into())),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(answer["schema_version"], CONTENT_SCHEMA_VERSION);
        let data = &answer["data"];
        assert_eq!(data["repo"]["branch"], "main");
        assert_eq!(data["repo"]["changed_files"], 2);
        assert_eq!(data["truncated"], false);
        let files = data["files"].as_array().unwrap();
        let modified = files
            .iter()
            .find(|file| file["path"] == "src/a.ts")
            .unwrap();
        assert_eq!(modified["status"], "modified");
        assert_eq!(modified["added"], 1);
        assert_eq!(modified["removed"], 1);
        let untracked = files
            .iter()
            .find(|file| file["path"] == "notes.md")
            .unwrap();
        assert_eq!(untracked["status"], "untracked");
        assert_eq!(untracked["added"], 1);

        // A wrong token is refused before anything runs.
        assert_eq!(
            pane_git_status(
                State(state.clone()),
                Path(("default".into(), "wA:p1".into())),
                bearer_headers("not-a-token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );

        // A pane the fence knows nothing about is "no repository", not an error.
        let none = pane_git_status(
            State(state),
            Path(("default".into(), "wB:p9".into())),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert!(none["data"]["repo"].is_null());
        assert_eq!(none["data"]["files"].as_array().unwrap().len(), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn git_diff_answers_one_file_and_refuses_what_is_not_a_path() {
        let (root, repo) = git_test_repo("git-diff");
        let state = unreachable_state();
        remember_pane_root(&state, "wA:p1", repo.clone());

        let query = |path: &str| GitDiffQuery {
            path: Some(path.into()),
            old_path: None,
            staged: None,
            context: Some(3),
            from: None,
            lines: None,
        };
        let answer = pane_git_diff(
            State(state.clone()),
            Path(("default".into(), "wA:p1".into())),
            Query(query("src/a.ts")),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        let data = &answer["data"];
        assert_eq!(data["path"], "src/a.ts");
        assert_eq!(data["binary"], false);
        assert_eq!(data["truncated"], false);
        assert!(data["patch"]
            .as_str()
            .unwrap()
            .contains("\n-const b = 2;\n+const B = 2;\n"));

        for bad in ["--cached", "../repo/src/a.ts", "/etc/passwd", ""] {
            let status = pane_git_diff(
                State(state.clone()),
                Path(("default".into(), "wA:p1".into())),
                Query(query(bad)),
                bearer_headers("token"),
            )
            .await
            .unwrap_err()
            .0;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        }

        let missing = pane_git_diff(
            State(state.clone()),
            Path(("default".into(), "wA:p1".into())),
            Query(query("nope.txt")),
            bearer_headers("token"),
        )
        .await
        .unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(missing.1["error"]["code"], "no_such_path");

        let no_repo = pane_git_diff(
            State(state),
            Path(("default".into(), "wB:p9".into())),
            Query(query("src/a.ts")),
            bearer_headers("token"),
        )
        .await
        .unwrap_err();
        assert_eq!(no_repo.0, StatusCode::NOT_FOUND);
        assert_eq!(no_repo.1["error"]["code"], "no_repository");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pane_context_answers_git_and_cwd_without_a_backend() {
        let (root, repo) = git_test_repo("pane-context");
        let state = unreachable_state();
        remember_pane_root(&state, "wA:p1", repo.clone());

        let answer = pane_context(
            State(state.clone()),
            Path(("default".into(), "wA:p1".into())),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        let data = &answer["data"];
        assert_eq!(data["cwd_in_fence"], true);
        assert_eq!(
            data["cwd"],
            std::fs::canonicalize(&repo)
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(data["git"]["branch"], "main");
        assert_eq!(data["git"]["changed_files"], 2);
        // The backend is unreachable, so nothing is known about an agent --
        // and nothing is guessed.
        assert!(data["agent"].is_null());

        let unknown = pane_context(
            State(state),
            Path(("default".into(), "wB:p9".into())),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert!(unknown["data"]["cwd"].is_null());
        assert_eq!(unknown["data"]["cwd_in_fence"], false);
        assert!(unknown["data"]["git"].is_null());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_digest_endpoint_is_documented_and_announced() {
        let spec = openapi_spec();
        let events = &spec["paths"]["/api/sessions/{sessionId}/agent-events"]["get"];
        assert!(events.is_object());
        let names: Vec<&str> = events["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sessionId", "since"]);
        // A client must be able to tell a gateway that keeps this from one old
        // enough to answer 404, without probing for the 404.
        assert!(API_CAPABILITIES.contains(&"agent_events"));
    }

    #[test]
    fn a_config_without_agent_commands_still_loads_and_round_trips_unchanged() {
        // Every gateway already in the field has a config.json written before
        // this field existed. Reading one must not fail, and rewriting one must
        // not add noise to it.
        let existing = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }]
        });
        let config: Config = serde_json::from_value(existing.clone()).unwrap();
        assert!(config.agent_commands.is_empty());
        assert_eq!(serde_json::to_value(&config).unwrap(), existing);

        let with_override = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }],
            "agent_commands": { "claude": "claude-canary" }
        });
        let config: Config = serde_json::from_value(with_override).unwrap();
        assert_eq!(
            config.agent_commands.get("claude").map(String::as_str),
            Some("claude-canary")
        );
    }

    #[test]
    fn a_tmux_session_is_explicit_while_old_sessions_stay_herdr() {
        let old: SessionConfig = serde_json::from_value(json!({
            "id": "default", "label": "Default", "socket_path": "/tmp/herdr.sock"
        }))
        .unwrap();
        assert_eq!(old.backend, BackendKind::Herdr);
        assert!(serde_json::to_value(&old).unwrap().get("backend").is_none());

        let tmux = SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        assert_eq!(serde_json::to_value(tmux).unwrap()["backend"], "tmux");
    }

    /// Config position is what breaks a liveness tie in `session_order_key`,
    /// so position 0 is the session the app opens. Adding a backend must
    /// therefore leave the existing order alone: it appends.
    #[test]
    fn adding_a_backend_appends_and_never_reorders_the_existing_ones() {
        let mut config = test_config("token");
        config.sessions.clear();
        upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
        upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
        assert_eq!(config.sessions[0].backend, BackendKind::Herdr);
        assert_eq!(config.sessions[1].backend, BackendKind::Tmux);
    }

    /// The bug this closes: `backend default` moves an entry to position 0,
    /// and the next `backend add` used to sort tmux back in front of it --
    /// silently changing which backend the reader's phone opens.
    #[test]
    fn a_chosen_default_survives_adding_another_backend() {
        let mut config = test_config("token");
        config.sessions.clear();
        let herdr =
            upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
        upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
        make_backend_default(&mut config, &herdr).unwrap();
        assert_eq!(config.sessions[0].id, herdr);

        upsert_backend_session(
            &mut config,
            BackendKind::Tmux,
            Some("tmux-late".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            config.sessions[0].id, herdr,
            "adding a backend must not overrule `backend default`"
        );
    }

    fn liveness_session(id: &str, backend: BackendKind) -> SessionConfig {
        SessionConfig {
            id: id.into(),
            label: id.into(),
            socket_path: String::new(),
            backend,
        }
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

    #[test]
    fn adding_a_backend_preserves_the_primary_session_and_is_idempotent() {
        let mut config = test_config("token");
        config.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };

        let id = upsert_backend_session(
            &mut config,
            BackendKind::Herdr,
            None,
            None,
            Some("/tmp/herdr.sock".into()),
        )
        .unwrap();
        assert_eq!(id, "herdr");
        assert_eq!(config.sessions[0].id, "default");
        assert_eq!(config.sessions[1].backend, BackendKind::Herdr);

        let id = upsert_backend_session(
            &mut config,
            BackendKind::Herdr,
            None,
            Some("Herdr local".into()),
            Some("/tmp/new.sock".into()),
        )
        .unwrap();
        assert_eq!(id, "herdr");
        assert_eq!(config.sessions.len(), 2);
        assert_eq!(config.sessions[1].label, "Herdr local");
        assert_eq!(config.sessions[1].socket_path, "/tmp/new.sock");
    }

    #[test]
    fn choosing_a_default_backend_only_reorders_sessions() {
        let mut config = test_config("token");
        config.sessions.push(SessionConfig {
            id: "tmux".into(),
            label: "Local tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        });
        make_backend_default(&mut config, "tmux").unwrap();
        assert_eq!(config.sessions[0].id, "tmux");
        assert_eq!(config.sessions[1].id, "default");
        assert!(make_backend_default(&mut config, "missing").is_err());
    }

    #[test]
    fn a_fresh_install_with_no_backend_named_defaults_to_tmux() {
        assert_eq!(
            resolve_setup_backend(None, None),
            BackendKind::Tmux,
            "nothing configured yet, and nothing asked for -- tmux is primary"
        );
    }

    #[test]
    fn an_explicit_backend_always_wins_over_whatever_already_exists() {
        let herdr_only = test_config("token"); // sessions[0] is Herdr, see test_config
        assert_eq!(
            resolve_setup_backend(Some(BackendKind::Tmux), Some(&herdr_only)),
            BackendKind::Tmux
        );
    }

    #[test]
    fn an_existing_herdr_install_stays_herdr_when_backend_is_left_off() {
        // The exact case the old `default_value_t = SetupBackend::Herdr` was
        // protecting: a bare `setup` (e.g. the Herdr-plugin action, which has
        // no way to pass --backend) on a machine already running Herdr must
        // not start asking for tmux, which might not even be installed.
        let herdr_only = test_config("token");
        assert_eq!(
            resolve_setup_backend(None, Some(&herdr_only)),
            BackendKind::Herdr
        );
    }

    #[test]
    fn an_existing_tmux_install_stays_tmux_when_backend_is_left_off() {
        let mut tmux_only = test_config("token");
        tmux_only.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        assert_eq!(
            resolve_setup_backend(None, Some(&tmux_only)),
            BackendKind::Tmux
        );
    }

    #[test]
    fn manager_fields_wrap_without_losing_url_or_message_text() {
        let value = "http://osk.taila90692.ts.net:23847/a-long-path";
        let mut lines = Vec::new();
        push_wrapped_field(&mut lines, "url", value, 24);
        let reconstructed = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    line.strip_prefix("url: ").unwrap()
                } else {
                    line.trim_start()
                }
            })
            .collect::<String>();
        assert_eq!(reconstructed, value);
        assert!(lines.iter().all(|line| display_width(line) <= 24));
    }

    #[test]
    fn a_renamed_standalone_dir_migrates_once_and_never_again() {
        let parent = std::env::temp_dir().join(format!("gateway-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
        std::fs::create_dir_all(&old_dir).unwrap();
        // A real, parseable config so the pre-migration liveness check has a
        // port to look at -- this is the ordinary case: an install whose old
        // gateway has already been stopped, so nothing is listening on that
        // port under the old name and the migration proceeds. (The refusal
        // path -- a process actually found listening -- is not exercised
        // here: this file does not spawn real OS processes to unit-test
        // process introspection anywhere else either, and that path was
        // verified operationally when this fix was written, against a
        // gateway genuinely still running under the pre-rename name.)
        std::fs::write(
            old_dir.join(CONFIG_FILE),
            serde_json::to_vec(&test_config("migration-token")).unwrap(),
        )
        .unwrap();

        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!old_dir.exists());
        assert!(migrated.join(CONFIG_FILE).exists());

        // A directory recreated under the old name afterward is left alone:
        // once the new name exists, migration never looks at the old one again.
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("marker"), b"should not move").unwrap();
        let migrated_again = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated_again, migrated);
        assert!(old_dir.join("marker").exists());
        assert!(!migrated.join("marker").exists());

        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn no_old_dir_and_no_new_dir_migrates_nothing() {
        let parent =
            std::env::temp_dir().join(format!("gateway-rename-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!migrated.exists());
        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn an_unparseable_old_config_does_not_block_migration() {
        // No port can be read from this, so the liveness check has nothing to
        // ask about and degrades to "proceed" rather than "refuse forever" --
        // an install should not be permanently stuck migrating because one
        // file did not parse.
        let parent = std::env::temp_dir().join(format!(
            "gateway-rename-unparseable-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join(CONFIG_FILE), b"{}").unwrap();

        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!old_dir.exists());

        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn backend_ids_and_labels_reject_terminal_control_input() {
        assert!(validate_session_id("tmux-2").is_ok());
        assert!(validate_session_id("../tmux").is_err());
        assert!(validate_session_id("tmux\nforged").is_err());
        assert!(validate_label("Local tmux").is_ok());
        assert!(validate_label("tmux\x1b[2J").is_err());
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

    #[test]
    fn an_explicit_loopback_url_never_opens_the_listener_to_the_lan() {
        assert_eq!(
            listen_for_explicit_public_url("http://localhost:23847", 23847),
            "127.0.0.1:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("http://127.0.0.1:23847", 23847),
            "127.0.0.1:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("http://[::1]:23847", 23847),
            "[::1]:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("https://host.tailnet.ts.net", 23847),
            "0.0.0.0:23847"
        );
    }

    /// The shape that shipped: a tailnet name in the QR, a socket on loopback.
    ///
    /// It is reached without anyone choosing it -- install before Tailscale is
    /// up, and the next start rewrites the URL and leaves the socket behind --
    /// so the gateway has to say so rather than come up looking healthy.
    #[test]
    fn a_loopback_socket_under_a_tailnet_name_is_warned_about() {
        let warning = unreachable_listen_warning("127.0.0.1:23847", "http://y.ts.net:23847")
            .expect("a loopback socket cannot serve a tailnet name");
        assert!(warning.contains("127.0.0.1:23847"));
        assert!(warning.contains("http://y.ts.net:23847"));
    }

    #[test]
    fn a_reachable_listener_is_not_warned_about() {
        assert!(unreachable_listen_warning("0.0.0.0:23847", "http://y.ts.net:23847").is_none());
        assert!(
            unreachable_listen_warning("100.99.165.54:23847", "http://y.ts.net:23847").is_none()
        );
    }

    /// Loopback is correct under a local URL, and correct under Tailscale
    /// Serve -- which terminates TLS outside and proxies in over 127.0.0.1.
    /// Warning about either would train people to ignore the warning.
    #[test]
    fn loopback_is_left_alone_where_loopback_is_the_answer() {
        assert!(unreachable_listen_warning("127.0.0.1:23847", "http://127.0.0.1:23847").is_none());
        assert!(unreachable_listen_warning("127.0.0.1:23847", "http://localhost:23847").is_none());
        assert!(unreachable_listen_warning("127.0.0.1:23847", "https://y.ts.net").is_none());
    }

    #[test]
    fn an_existing_install_requires_one_consistent_pairing_identity() {
        let dir =
            std::env::temp_dir().join(format!("gateway-existing-install-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config("admin-token");
        let pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: config.server_id.clone(),
                label: config.label.clone(),
                url: config.public_url.clone(),
                token: "admin-token".into(),
                transport_key: "transport-key".into(),
            },
        };
        std::fs::write(dir.join(CONFIG_FILE), serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::write(
            dir.join(PAIRING_FILE),
            serde_json::to_vec(&pairing).unwrap(),
        )
        .unwrap();
        assert!(load_existing_install(&dir.join(CONFIG_FILE), &dir.join(PAIRING_FILE)).is_some());

        let mut stale = pairing;
        stale.payload.token = "different-token".into();
        std::fs::write(dir.join(PAIRING_FILE), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(load_existing_install(&dir.join(CONFIG_FILE), &dir.join(PAIRING_FILE)).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn imported_state_is_merged_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("gateway-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        let target = dir.join("target.json");
        std::fs::write(&source, br#"["one","two"]"#).unwrap();
        std::fs::write(&target, br#"["two","three"]"#).unwrap();
        merge_plugin_state::<String, _>(&source, &target, |left, right| left == right).unwrap();
        let merged: Vec<String> = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(merged, vec!["one", "two", "three"]);
        assert!(target
            .with_file_name("target.json.before-herdr-import")
            .exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plugin_import_keeps_the_paired_identity_and_merges_tmux() {
        let root = std::env::temp_dir().join(format!("gateway-import-{}", uuid::Uuid::new_v4()));
        let source_config_dir = root.join("plugin-config");
        let source_state_dir = root.join("plugin-state");
        let target_config_dir = root.join("standalone-config");
        let target_state_dir = root.join("standalone-state");
        for dir in [
            &source_config_dir,
            &source_state_dir,
            &target_config_dir,
            &target_state_dir,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut plugin = test_config("plugin-token");
        plugin.server_id = "paired-herdr".into();
        plugin.listen = "127.0.0.1:31987".into();
        plugin.public_url = "http://127.0.0.1:31987".into();
        let plugin_pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: plugin.server_id.clone(),
                label: plugin.label.clone(),
                url: plugin.public_url.clone(),
                token: "plugin-token".into(),
                transport_key: "plugin-transport-key".into(),
            },
        };
        write_config(&source_config_dir.join(CONFIG_FILE), &plugin).unwrap();
        write_secret_file(
            &source_config_dir.join(PAIRING_FILE),
            &serde_json::to_vec(&plugin_pairing).unwrap(),
        )
        .unwrap();

        let mut standalone = test_config("tmux-token");
        standalone.server_id = "discarded-tmux-identity".into();
        standalone.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        let standalone_pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: standalone.server_id.clone(),
                label: standalone.label.clone(),
                url: standalone.public_url.clone(),
                token: "tmux-token".into(),
                transport_key: "tmux-transport-key".into(),
            },
        };
        write_config(&target_config_dir.join(CONFIG_FILE), &standalone).unwrap();
        write_secret_file(
            &target_config_dir.join(PAIRING_FILE),
            &serde_json::to_vec(&standalone_pairing).unwrap(),
        )
        .unwrap();

        import_herdr_plugin(
            Some(source_config_dir),
            Some(source_state_dir),
            Some(target_config_dir.clone()),
            Some(target_state_dir),
        )
        .unwrap();

        let merged: Config =
            serde_json::from_slice(&std::fs::read(target_config_dir.join(CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(merged.server_id, "paired-herdr");
        assert_eq!(merged.sessions.len(), 2);
        // The install being imported *into* keeps position 0, because that is
        // the session the reader's phone already opens (`session_order_key`
        // breaks a liveness tie by config position). Importing a plugin's
        // backend adds one; it does not re-point the app at it.
        assert_eq!(merged.sessions[0].backend, BackendKind::Herdr);
        assert_eq!(merged.sessions[1].backend, BackendKind::Tmux);
        assert!(target_config_dir.join(HERDR_PLUGIN_IMPORT_MARKER).exists());
        std::fs::remove_dir_all(root).ok();
    }

    fn write_test_install(dir: &std::path::Path, server_id: &str, token: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut config = test_config(token);
        config.server_id = server_id.into();
        let pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: server_id.into(),
                label: config.label.clone(),
                url: config.public_url.clone(),
                token: token.into(),
                transport_key: format!("{token}-transport-key"),
            },
        };
        write_config(&dir.join(CONFIG_FILE), &config).unwrap();
        write_secret_file(
            &dir.join(PAIRING_FILE),
            &serde_json::to_vec(&pairing).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn the_installer_never_swaps_the_identity_of_a_standalone_install_with_devices() {
        let root = std::env::temp_dir().join(format!("gateway-import-{}", uuid::Uuid::new_v4()));
        let plugin = root.join("plugin-config");
        let standalone = root.join("standalone-config");
        let state = root.join("standalone-state");
        std::fs::create_dir_all(&state).unwrap();
        write_test_install(&plugin, "stale-plugin", "plugin-token");

        // Nothing standalone yet: adopting the plugin is the migration.
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());

        // A standalone identity nothing is paired to loses nothing.
        write_test_install(&standalone, "standalone", "standalone-token");
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());

        // ...unless a gateway is using it: stopping that is not the installer's call.
        {
            let _running = state_lock::StateLock::acquire(&state).unwrap();
            let reason = auto_import_skip_reason(&plugin, &standalone, &state)
                .expect("a running standalone gateway must be left alone");
            assert!(reason.contains("running"), "{reason}");
        }

        // Once a device is paired to it, the installer must leave it alone.
        write_devices_at(&state, &[test_device("phone", "phone-token")]).unwrap();
        let reason = auto_import_skip_reason(&plugin, &standalone, &state)
            .expect("paired standalone identity must be kept");
        assert!(reason.contains("1 paired device"), "{reason}");

        // An unreadable device file is a reason to skip, never to abort the install.
        std::fs::write(state.join(DEVICES_FILE), b"not json").unwrap();
        let reason = auto_import_skip_reason(&plugin, &standalone, &state)
            .expect("unreadable device file must be kept");
        assert!(reason.contains("could not be read"), "{reason}");

        // Unless it already is the plugin identity, where import changes nothing.
        write_test_install(&standalone, "stale-plugin", "plugin-token");
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_run_that_got_part_of_the_way_answers_207_with_the_same_body() {
        let payload = json!({ "workspace_id": "ws-1", "pane_id": "pane-1" });

        let mut steps = tasks::StepLog::new();
        steps.ok("worktree", json!({ "path": "/tmp/wt" }));
        steps.ok("workspace", json!({ "workspace_id": "ws-1" }));
        steps.ok("agent", json!({ "kind": "claude" }));
        steps.skipped("prompt", "no prompt was given");
        assert_eq!(
            task_partial(payload.clone(), &steps).status(),
            StatusCode::OK,
            "a skipped step is not a failure"
        );

        steps.failed("prompt", "herdr_error", "pane vanished");
        assert_eq!(
            task_partial(payload, &steps).status(),
            StatusCode::MULTI_STATUS
        );
    }

    #[test]
    fn nothing_created_is_an_error_whose_status_says_whose_fault_it_was() {
        let steps = tasks::StepLog::new();
        // Herdr refusing is the request being wrong.
        let refused = HerdrCallError::Herdr {
            method: "worktree.create".into(),
            error: json!({ "code": "not_a_repo", "message": "not a git repository" }),
        };
        assert_eq!(refused.code(), "not_a_repo");
        assert!(refused.message().contains("worktree.create"));
        assert_eq!(
            task_failure(refused, &steps).status(),
            StatusCode::BAD_REQUEST
        );

        // The socket being down, or Herdr answering off-schema, is not.
        assert_eq!(
            task_failure(
                HerdrCallError::Unavailable("Herdr is unavailable".into()),
                &steps
            )
            .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            task_failure(HerdrCallError::malformed("workspace.create"), &steps).status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            HerdrCallError::malformed("workspace.create").code(),
            "invalid_herdr_response"
        );
    }

    #[test]
    fn repo_roots_come_from_the_repos_this_session_has_and_never_widen_to_the_machine() {
        // The gathering half of task_repo_roots, which is the part that decides
        // what the fence lets through. A workspace names its repo root as well
        // as its checkout, which is why a pane sitting deep inside a repo still
        // lets the repo's top level be branched from.
        let workspaces = json!({
            "id": "1",
            "result": { "type": "workspace_list", "workspaces": [
                { "workspace_id": "ws-1", "worktree": {
                    "repo_key": "k", "repo_name": "muqun",
                    "repo_root": "/Users/dev/code/muqun",
                    "checkout_path": "/Users/dev/code/muqun-task",
                    "is_linked_worktree": true } },
                { "workspace_id": "ws-2" },
                { "workspace_id": "ws-3", "worktree": {
                    "repo_key": "k2", "repo_name": "home", "repo_root": "/",
                    "checkout_path": "/", "is_linked_worktree": false } }
            ] }
        });
        let mut found: Vec<String> = Vec::new();
        for workspace in workspaces["result"]["workspaces"].as_array().unwrap() {
            for key in ["repo_root", "checkout_path"] {
                if let Some(path) = workspace
                    .pointer(&format!("/worktree/{key}"))
                    .and_then(Value::as_str)
                {
                    if is_scannable_root(FsPath::new(path)) {
                        found.push(path.to_owned());
                    }
                }
            }
        }
        assert_eq!(
            found,
            vec!["/Users/dev/code/muqun", "/Users/dev/code/muqun-task"]
        );
        // A workspace with no worktree contributes nothing, and "/" is refused
        // by the same guard the asset roots use.
        assert!(!found.iter().any(|path| path == "/"));
    }

    /// A state whose Herdr socket cannot answer, which is how a test reaches
    /// the checks that happen before anything is created.
    fn unreachable_state() -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path = std::env::temp_dir()
            .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        state
    }

    /// A Herdr socket that answers `pane.list` with a fixed set of panes (or
    /// none), for driving the real `HerdrBackend` -> `list_panes` path that
    /// `sessions()` probes end to end, without needing Herdr installed or a
    /// live tmux server.
    struct FakePaneListHerdr {
        socket_path: PathBuf,
    }

    impl FakePaneListHerdr {
        fn start(panes: Value) -> Self {
            let socket_path = std::env::temp_dir().join(format!(
                "herdr-panelist-{}.sock",
                uuid::Uuid::new_v4().simple()
            ));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap_or_default();
                    let response = json!({
                        "id": request["id"],
                        "result": { "panes": panes.clone() }
                    })
                    .to_string();
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(b"\n").await;
                    let _ = stream.flush().await;
                }
            });
            Self { socket_path }
        }

        fn session(&self, id: &str) -> SessionConfig {
            SessionConfig {
                id: id.into(),
                label: id.into(),
                socket_path: self.socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Herdr,
            }
        }
    }

    fn session_ids(response: &Value) -> Vec<String> {
        response["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_owned())
            .collect()
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

    fn spawn_body(agent: &str, cwd: Option<&str>) -> SpawnBody {
        SpawnBody {
            agent: agent.to_owned(),
            cwd: cwd.map(str::to_owned),
            tab_id: None,
            prompt: None,
        }
    }

    #[tokio::test]
    async fn a_spawn_is_refused_before_anything_is_created() {
        let state = unreachable_state();

        // An agent this gateway does not offer, answered in the reader's own
        // language and pointing at the list that would have said so.
        let refusal = spawn_agent(
            State(state.clone()),
            Path("default".into()),
            locale_headers("token", "zh-TW"),
            Json(spawn_body("definitely-not-an-agent", None)),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(error_body(&refusal)["error"]["code"], "unknown_agent");
        assert!(error_body(&refusal)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("GET /api/agents/catalog"));

        // A real agent, but a directory this session does not work in. The
        // socket is unreachable here, so the session has no roots at all --
        // which is exactly the case that must refuse rather than fall open.
        let refusal = spawn_agent(
            State(state.clone()),
            Path("default".into()),
            bearer_headers("token"),
            Json(spawn_body("claude", Some("/etc"))),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);
        assert_eq!(error_body(&refusal)["error"]["code"], "cwd_not_allowed");

        // And none of it is reachable without a paired device.
        assert_eq!(
            spawn_agent(
                State(state),
                Path("default".into()),
                bearer_headers("not-a-token"),
                Json(spawn_body("claude", None)),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
    }

    /// A tmux tab id is a window id: it counts up for the life of the server
    /// and is never reused, so browsing assets across tabs used to add one
    /// permanent entry each, holding that tab's whole `Vec` of root paths.
    #[test]
    fn the_remembered_root_scopes_are_bounded_and_evict_the_oldest() {
        let mut index = AssetIndex::default();
        let root = |path: &str| AssetRoot {
            path: PathBuf::from(path),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        };
        let scope = |tab: usize| AssetScope::Tab(format!("@{tab}"));

        for tab in 0..MAX_REMEMBERED_ROOT_SCOPES * 3 {
            index.remember_roots("default", Some(&scope(tab)), vec![root("/tmp")]);
            assert!(index.roots.len() <= MAX_REMEMBERED_ROOT_SCOPES);
            assert_eq!(index.roots.len(), index.roots_order.len());
        }
        assert_eq!(index.roots.len(), MAX_REMEMBERED_ROOT_SCOPES);
        // The oldest went, the newest stayed.
        assert!(index.known_roots("default", Some(&scope(0))).is_empty());
        assert!(!index
            .known_roots("default", Some(&scope(MAX_REMEMBERED_ROOT_SCOPES * 3 - 1)))
            .is_empty());

        // Rewriting a scope already held replaces it rather than filling a
        // second slot, or the cap would evict live scopes on a busy session.
        let before = index.roots.len();
        for _ in 0..10 {
            index.remember_roots(
                "default",
                Some(&scope(MAX_REMEMBERED_ROOT_SCOPES * 3 - 1)),
                vec![root("/tmp/again")],
            );
        }
        assert_eq!(index.roots.len(), before);
        assert_eq!(index.roots.len(), index.roots_order.len());
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

    #[test]
    fn a_blocked_push_says_nothing_the_agent_wrote_until_the_owner_asks_it_to() {
        let approval = approvals::detect(concat!(
            "Bash command\n",
            "\n",
            "  rm -rf build/\n",
            "\n",
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. Yes, and don't ask again for rm commands\n",
            "  3. No, and tell Claude what to do differently (esc)\n",
        ))
        .expect("the fixture draws a menu");

        let mut statuses = HashMap::new();
        let notice = notification_for_agent_status_event(
            &status_event("w1:p1", "claude", "blocked"),
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();

        // The default, and what every gateway sends until someone changes it:
        // that something needs answering, and never what.
        let plain = notice.render(Locale::En);
        assert_eq!(plain.title, "Agent blocked · Studio");
        assert_eq!(plain.body, "claude needs your input.");
        assert!(plain.data.get("question").is_none());
        assert!(!plain.body.contains("rm -rf"));

        // Opted in: the agent's own question, verbatim, plus the answers it is
        // offering. Not translated, because it is a quotation.
        let mut rich = notice.clone();
        rich.detail = Some(PushDetail::from_approval(&approval));
        let opted_in = rich.render(Locale::ZhTw);
        assert_eq!(opted_in.title, "代理程式等待中 · Studio");
        assert_eq!(opted_in.body, "Do you want to proceed?");
        assert_eq!(opted_in.data["question"], "Do you want to proceed?");
        let labels = opted_in.data["option_labels"].as_array().unwrap();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "Yes");

        // A question longer than a glance is cut, and a menu with more answers
        // than a notification row shows is cut too.
        let long = approvals::Approval {
            prompt: "x".repeat(400),
            options: (1..=6)
                .map(|index| approvals::ApprovalOption {
                    index,
                    label: "y".repeat(80),
                    selected: false,
                    decision: approvals::Decision::Allow,
                })
                .collect(),
            ..approval
        };
        let detail = PushDetail::from_approval(&long);
        // Cut, and visibly cut: the ellipsis is how a reader knows there is
        // more rather than believing they have read the whole question.
        assert!(detail
            .question
            .starts_with(&"x".repeat(MAX_PUSH_QUESTION_CHARS)));
        assert!(detail.question.ends_with("..."));
        assert_eq!(detail.question.chars().count(), MAX_PUSH_QUESTION_CHARS + 3);
        assert_eq!(detail.option_labels.len(), MAX_PUSH_OPTIONS);
        assert_eq!(
            detail.option_labels[0].chars().count(),
            MAX_PUSH_OPTION_CHARS + 3
        );
    }

    #[test]
    fn rich_pushes_are_off_until_a_config_says_otherwise() {
        // The one switch that puts terminal text on a lock screen. A config
        // written before it existed must read as off, and a gateway that has
        // not been told otherwise must not start saying more than it did.
        let config = test_config("admin");
        assert!(!config.rich_agent_pushes);

        let existing = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }]
        });
        let parsed: Config = serde_json::from_value(existing.clone()).unwrap();
        assert!(!parsed.rich_agent_pushes);
        // And writing it back does not add the key, so an untouched config file
        // stays untouched.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), existing);

        let mut object = existing.as_object().cloned().unwrap();
        object.insert("rich_agent_pushes".into(), json!(true));
        let opted_in: Config = serde_json::from_value(Value::Object(object)).unwrap();
        assert!(opted_in.rich_agent_pushes);
        assert_eq!(
            serde_json::to_value(&opted_in).unwrap()["rich_agent_pushes"],
            json!(true)
        );
    }

    #[test]
    fn dispatch_and_stop_are_documented_and_announced() {
        let spec = openapi_spec();
        let spawn = &spec["paths"]["/api/sessions/{sessionId}/spawn"]["post"];
        assert!(spawn.is_object());
        assert_eq!(
            spawn["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["agent"])
        );
        assert!(spawn["responses"]["207"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/recent-cwds"]["get"].is_object());
        assert!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/interrupt"]["post"].is_object()
        );

        for capability in ["agent_spawn", "recent_cwds", "pane_interrupt"] {
            assert!(
                API_CAPABILITIES.contains(&capability),
                "{capability} is not announced"
            );
        }
    }

    /// A Herdr socket that answers from a script.
    ///
    /// Every gateway request is its own short-lived connection, so this accepts
    /// in a loop and answers one line per connection. `pane.read` walks the
    /// scripted screens and then repeats the last one forever, which is what
    /// lets a test say "and from here on the pane holds still".
    ///
    /// `agent.list` is scripted by how many Enters have arrived rather than by
    /// call count, because that is the real relationship: the agent's state
    /// sequence moves when a keystroke is finally taken, not after some number
    /// of polls.
    struct FakeHerdr {
        socket_path: PathBuf,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    /// Seq the fake agent sits on until it accepts an Enter. Arbitrary, but not
    /// zero, so "advanced past the baseline" cannot pass by accident.
    const FAKE_AGENT_SEQ: u64 = 100;

    impl FakeHerdr {
        /// `advance_after` is the number of Enters it takes for the agent's
        /// state sequence to move; `None` makes `agent.list` come back empty,
        /// which is a pane running no agent Herdr knows.
        fn start(screens: Vec<&str>, advance_after: Option<usize>) -> Self {
            let socket_path = std::env::temp_dir().join(format!(
                "herdr-submit-{}.sock",
                uuid::Uuid::new_v4().simple()
            ));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let screens: Vec<String> = screens.into_iter().map(str::to_owned).collect();
            tokio::spawn(async move {
                let mut reads = 0usize;
                let mut enters = 0usize;
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap_or_default().to_owned();
                    let result = match method.as_str() {
                        "pane.read" => {
                            let screen = screens
                                .get(reads)
                                .or_else(|| screens.last())
                                .cloned()
                                .unwrap_or_default();
                            reads += 1;
                            json!({ "read": { "text": screen, "revision": reads } })
                        }
                        "agent.list" => {
                            let agents = match advance_after {
                                None => json!([]),
                                Some(threshold) => {
                                    let seq = FAKE_AGENT_SEQ + u64::from(enters >= threshold);
                                    json!([
                                        { "pane_id": "wZ:p9", "state_change_seq": 1 },
                                        {
                                            "pane_id": "w1:p1",
                                            "agent": "claude",
                                            "agent_status": "idle",
                                            "state_change_seq": seq
                                        }
                                    ])
                                }
                            };
                            json!({ "agents": agents })
                        }
                        "pane.send_keys" => {
                            enters += 1;
                            json!({ "ok": true })
                        }
                        _ => json!({ "ok": true }),
                    };
                    recorded.lock().unwrap().push(request.clone());
                    let response = json!({ "id": request["id"], "result": result }).to_string();
                    let mut stream = reader.into_inner();
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(b"\n").await.unwrap();
                    stream.flush().await.unwrap();
                }
            });
            Self { socket_path, calls }
        }

        fn session(&self) -> SessionConfig {
            SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: self.socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Herdr,
            }
        }

        /// The call order with the state polling filtered out, so a test can say
        /// what happened to the pane without counting how many times the agent
        /// was asked about.
        fn pane_methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].as_str().unwrap_or_default().to_owned())
                .filter(|method| method != "agent.list")
                .collect()
        }

        fn enters(&self) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call["method"] == "pane.send_keys")
                .cloned()
                .collect()
        }
    }

    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    #[tokio::test]
    async fn an_enter_the_agent_took_is_not_repeated() {
        let herdr = FakeHerdr::start(
            // The screen keeps moving after the Enter and then keeps moving
            // again, which on its own proves nothing either way.
            vec![
                "> review this",
                "> review this",
                "reviewing...",
                "reviewing",
            ],
            Some(1),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        let enters = herdr.enters();
        assert_eq!(enters.len(), 1);
        assert_eq!(enters[0]["params"]["pane_id"], "w1:p1");
        assert_eq!(enters[0]["params"]["keys"], json!(["Enter"]));
    }

    #[tokio::test]
    async fn enter_waits_for_the_pane_to_stop_redrawing() {
        // The middle screens are Claude Code staging an image: the input line is
        // rewritten while the file is read, and an Enter in there is swallowed.
        let herdr = FakeHerdr::start(
            vec![
                "> look at /tmp/a.jpg",
                "> look at /tmp/a.jpg (reading)",
                "> look at [Image #1]",
                "> look at [Image #1]",
                "analyzing image...",
            ],
            Some(1),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 1);
        assert_eq!(
            herdr.pane_methods(),
            vec![
                "pane.read",
                "pane.read",
                "pane.read",
                "pane.read",
                "pane.send_keys"
            ]
        );
    }

    #[tokio::test]
    async fn a_screen_that_moved_without_submitting_does_not_pass_for_a_submission() {
        // The exact false positive that broke the first version of this: three
        // large images stage in bursts, so the pane looks still, then different,
        // then still again, while the prompt never leaves the input box. Only
        // the agent's state sequence knows, and here it moves on the third
        // Enter.
        let herdr = FakeHerdr::start(
            vec![
                "> look at [Image #1]",
                "> look at [Image #1]",
                "> look at [Image #1] [Image #2]",
                "> look at [Image #1] [Image #2] [Image #3]",
            ],
            Some(3),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 3);
    }

    #[tokio::test]
    async fn enters_stop_at_the_budget_when_the_agent_never_takes_one() {
        let herdr = FakeHerdr::start(vec!["> review this"], Some(usize::MAX));

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), SUBMIT_MAX_ATTEMPTS as usize);
    }

    /// A pane that repaints a four-row screen, scrolling one row per read: the
    /// shape of the pane in card #646, small enough to assert on exactly.
    fn repainting_screens() -> Vec<String> {
        (0..4)
            .map(|top| {
                (top..top + 4)
                    .map(|row| format!("row {row}"))
                    .collect::<Vec<String>>()
                    .join("\n")
            })
            .collect()
    }

    fn output_state(herdr: &FakeHerdr) -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path = herdr.session().socket_path;
        state
    }

    async fn read_output(state: &AppState, lines: u32) -> String {
        let response = pane_output(
            State(state.clone()),
            Path(("default".into(), "wM:p1".into())),
            Query(OutputQuery {
                source: Some("recent-unwrapped".into()),
                lines: Some(lines),
                format: Some("text".into()),
                start: None,
                end: None,
            }),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        pane_read_text(&response.0).unwrap_or_default()
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

    async fn read_output_range(state: &AppState, start: u32, end: u32) -> String {
        let response = pane_output(
            State(state.clone()),
            Path(("default".into(), "wM:p1".into())),
            Query(OutputQuery {
                source: Some("recent-unwrapped".into()),
                lines: None,
                format: Some("text".into()),
                start: Some(start),
                end: Some(end),
            }),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        pane_read_text(&response.0).unwrap_or_default()
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

    #[tokio::test]
    async fn a_pane_herdr_lists_no_agent_for_falls_back_to_watching_the_screen() {
        let herdr = FakeHerdr::start(
            vec!["$ ls", "$ ls", "Cargo.toml  src", "Cargo.toml  src"],
            None,
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 1);
        assert_eq!(
            herdr.pane_methods(),
            vec!["pane.read", "pane.read", "pane.send_keys", "pane.read"]
        );
    }

    #[tokio::test]
    async fn a_blind_submit_presses_enter_no_more_than_the_small_budget() {
        let herdr = FakeHerdr::start(vec!["$ ls"], None);

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), SUBMIT_BLIND_MAX_ATTEMPTS as usize);
    }
}
