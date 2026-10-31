//! Shared test fixtures and fakes, compiled only for test builds.

// The test prelude: names every test module sees through `use crate::*`.
pub(crate) use crate::backend::{AgentStatus as BackendAgentStatus, BackendKind, TerminalBackend};
pub(crate) use crate::i18n::Locale;
pub(crate) use axum::body::Body;
pub(crate) use axum::extract::{Path, Query, State};
pub(crate) use axum::http::{HeaderMap, Request};
pub(crate) use axum::middleware;
pub(crate) use axum::response::Response;
pub(crate) use axum::routing::{get, post};
pub(crate) use axum::Router;
pub(crate) use base64::Engine as _;
pub(crate) use qrcode::{EcLevel, QrCode};
pub(crate) use serde_json::json;
pub(crate) use std::collections::BTreeMap;
pub(crate) use std::path::{Path as FsPath, PathBuf};
pub(crate) use std::process::Command as ProcessCommand;
pub(crate) use std::time::{Duration, Instant, SystemTime};
pub(crate) use tower_http::compression::predicate::{DefaultPredicate, SizeAbove};

use crate::agents::session_routes::SpawnBody;
use crate::authority::PairingCodeError;
use crate::*;
use axum::http::HeaderValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) fn test_config(token: &str) -> Config {
    Config {
        history: Default::default(),
        server_id: "server-1".into(),
        label: "test".into(),
        autostart_backends: Vec::new(),
        listen: "127.0.0.1:23100".into(),
        public_url: "http://127.0.0.1:23100".into(),
        token_hash: hash_token(token),
        transport_encryption: TransportEncryptionMode::Disabled,
        dev_unauthenticated: false,
        sessions: vec![SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path: "/tmp/herdr.sock".into(),
            backend: BackendKind::Herdr,
        }],
        agent_commands: BTreeMap::new(),
        agent_launch_args: BTreeMap::new(),
        agent_env: Default::default(),
        agent_trust_folders: true,
        rich_agent_pushes: false,
        opencode: agents::OpencodeConfig::default(),
        deepseek: agents::DeepseekConfig::default(),
        t3: agents::T3Config::default(),
        pi: agents::PiConfig::default(),
        codex: agents::runtime::CodexConfig {
            enabled: false,
            binary: None,
        },
        fx: agents::runtime::FxConfig {
            enabled: false,
            binary: None,
            ..Default::default()
        },
        qoder: agents::runtime::QoderConfig {
            enabled: false,
            binary: None,
            ..Default::default()
        },
        antigravity: agents::runtime::AntigravityConfig::disabled(),
        agent_runtime: crate::platform::config::AgentRuntimeConfig { enabled: false },
        store: crate::platform::config::StoreConfig::default(),
        mcp: crate::platform::config::McpConfig::default(),
        fleet: Default::default(),
    }
}

/// The transport key the test fixtures give a device whose token hashes to
/// `token_hash`: derived, so a header builder that knows only the token can
/// produce the proof the middleware would have set after opening a sealed
/// request (`sealed_bearer_headers`).
pub(crate) fn test_transport_key_for(token_hash: &str) -> String {
    use sha2::{Digest as _, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(token_hash.as_bytes()))
}

/// `device`, paired while encryption was on: it holds the fixture transport
/// key for its token and is served in `required` mode only with the proof
/// `sealed_bearer_headers` carries.
pub(crate) fn sealed(mut device: DeviceRecord) -> DeviceRecord {
    device.transport_key = Some(test_transport_key_for(&device.token_hash));
    device
}

/// A state in `transport_encryption: required`: `test_state` is token-only
/// (`disabled`), the mode a device without a transport key can still be
/// served in. Devices here need to be `sealed` and the requests to carry
/// `sealed_bearer_headers`.
pub(crate) fn required_test_state(admin_token: &str, devices: Vec<DeviceRecord>) -> AppState {
    let mut state = test_state(admin_token, devices);
    state.config.transport_encryption = TransportEncryptionMode::Required;
    state
}

pub(crate) fn test_device(id: &str, token: &str) -> DeviceRecord {
    DeviceRecord {
        id: id.into(),
        kind: DeviceKind::App,
        name: format!("device {id}"),
        token_hash: hash_token(token),
        transport_key: None,
        paired_unix_ms: 1_000,
        // Fresh enough that require_device will not flush to disk.
        last_seen_unix_ms: now_unix_ms(),
        install_id: None,
        server_id: None,
    }
}

/// A device record for another of the owner's gateways.
pub(crate) fn test_peer_device(id: &str, token: &str) -> DeviceRecord {
    DeviceRecord {
        kind: DeviceKind::Gateway,
        name: format!("gateway {id}"),
        ..test_device(id, token)
    }
}

/// `state` as the computer `id` named `label`.
pub(crate) fn named(mut state: AppState, id: &str, label: &str) -> AppState {
    state.config.server_id = id.into();
    state.config.label = label.into();
    state
}

/// The routes one gateway calls on another, behind the transport middleware
/// the gateway runs.
pub(crate) fn peer_router(state: AppState) -> Router {
    let app = crate::platform::routes::mount(Router::new());
    let app = crate::connectivity::routes::mount(app);
    let app = crate::agents::ws_routes::mount(app);
    let app = crate::fleet::routes::mount(app);
    let app = crate::fleet::peer_routes::mount(app);
    crate::tasks::routes::mount(app)
        .layer(axum::middleware::from_fn(
            crate::platform::server::envelope_compression_gate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::platform::server::encrypted_transport,
        ))
        .with_state(state)
}

/// `router` on a loopback port; its base URL.
pub(crate) async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

pub(crate) fn test_state(admin_token: &str, devices: Vec<DeviceRecord>) -> AppState {
    let scrollback = Arc::new(Mutex::new(scrollback::ScrollbackStore::default()));
    let db = platform::store::sqlite::SqliteStore::open(
        &test_state_dir().join(format!("{}.sqlite3", uuid::Uuid::new_v4())),
        None,
        platform::store::sqlite::ALL_MIGRATIONS,
    )
    .expect("the test store opens");
    let config = test_config(admin_token);
    let task_events = tokio::sync::broadcast::channel(crate::tasks::events::TASK_EVENT_CAPACITY).0;
    let presence = Arc::new(crate::fleet::peers::PeerPresence::default());
    let tasks = Arc::new(crate::tasks::service::TaskService::new(
        Arc::new(crate::tasks::store_sqlite::SqliteTaskStore::new(db.clone())),
        task_events.clone(),
        config.server_id.clone(),
        presence.clone(),
    ));
    let peers = Arc::new(crate::fleet::peers::PeerService::new(
        Arc::new(crate::fleet::peers_sqlite::SqlitePeerStore::new(db.clone())),
        &crate::platform::config::FleetConfig::default(),
        None,
        presence,
    ));
    AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(Vec::new())),
        devices: Arc::new(Mutex::new(devices)),
        assets: Arc::new(Mutex::new(AssetIndex::default())),
        history: Arc::new(terminal::history_memory::MemoryHistoryRepository::new(
            scrollback.clone(),
        )),
        scrollback,
        agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
        approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
        activity: Arc::new(Mutex::new(HashMap::new())),
        session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
        agent_runtime: agents::AgentRuntime::disabled(),
        ws_connections: Arc::new(agents::ws_routes::WsRegistry::default()),
        generation: new_generation(),
        message_images: Arc::default(),
        mcp_tokens: Arc::new(crate::mcp::tokens_sqlite::SqliteTokenStore::new(db.clone())),
        fleet: Arc::default(),
        peers,
        tasks,
        task_events,
        home_overview: Default::default(),
        db,
    }
}

pub(crate) fn bearer_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

/// What a handler sees for a `sealed` device's request once the transport
/// middleware has opened the envelope: the bearer token and the proof header
/// only the middleware may set.
pub(crate) fn sealed_bearer_headers(token: &str) -> HeaderMap {
    let mut headers = bearer_headers(token);
    headers.insert(
        axum::http::HeaderName::from_static(TRANSPORT_PROOF_HEADER),
        HeaderValue::from_str(&test_transport_key_for(&hash_token(token))).unwrap(),
    );
    headers
}

pub(crate) fn encrypted_test_request(
    device_id: &str,
    material: &[u8],
    token: &str,
) -> Request<Body> {
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
pub(crate) fn sealed_wire_len(material: &[u8], token: &str, path: &str, size: usize) -> usize {
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

/// A paired device's headers with the app's locale header on them, which is
/// what every real request from the app carries.
pub(crate) fn locale_headers(token: &str, locale: &str) -> HeaderMap {
    let mut headers = bearer_headers(token);
    headers.insert(
        axum::http::HeaderName::from_static(i18n::LOCALE_HEADER),
        HeaderValue::from_str(locale).unwrap(),
    );
    headers
}

pub(crate) fn error_body(refusal: &(StatusCode, Json<Value>)) -> Value {
    refusal.1 .0.clone()
}

pub(crate) fn device_fixture(id: &str) -> DeviceRecord {
    DeviceRecord {
        id: id.into(),
        kind: DeviceKind::App,
        name: id.into(),
        token_hash: authority::hash_token(id),
        transport_key: None,
        paired_unix_ms: 1,
        last_seen_unix_ms: 1,
        install_id: None,
        server_id: None,
    }
}

pub(crate) fn test_pending_pairing(created_unix_ms: u128) -> Option<PendingPairing> {
    let code = "2345-6789".to_owned();
    Some(PendingPairing {
        request_id: "request-1".into(),
        kind: Default::default(),
        device_name: "Muqun test".into(),
        install_id: None,
        server_id: None,
        code_hash: hash_token(&code),
        code,
        created_unix_ms,
        failed_attempts: 0,
        minted_by: None,
    })
}

pub(crate) fn consume_test_pairing_code(
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

/// Reads the code `manage` would show a reader on the machine's screen,
/// after a device with no QR key has called `pair_request`.
pub(crate) fn pending_code(state: &AppState) -> String {
    state
        .pending_pairing
        .lock()
        .unwrap()
        .as_ref()
        .expect("pair_request left a pending code")
        .code
        .clone()
}

pub(crate) fn png_bytes() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(b"IHDR and the rest of a real file");
    bytes
}

pub(crate) fn test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
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

pub(crate) fn asset_test_dir(name: &str) -> PathBuf {
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

/// An owned private fixture directory whose canonical socket path fits sun_path.
/// Only socket fixtures fall back to /tmp; asset fixtures keep their TMPDIR.
pub(crate) struct TestSocket {
    dir: PathBuf,
    pub(crate) path: PathBuf,
}

impl TestSocket {
    pub(crate) fn new() -> Self {
        use std::os::unix::{ffi::OsStrExt, fs::DirBuilderExt};

        // sun_path includes its terminating NUL: 104 bytes on macOS, 108 on Linux.
        let max_bytes = if cfg!(target_os = "macos") { 103 } else { 107 };
        let name = format!("s-{}", uuid::Uuid::new_v4().simple());
        for base in [std::env::temp_dir(), PathBuf::from("/tmp")] {
            let dir = base.canonicalize().unwrap().join(&name);
            let path = dir.join("h");
            if path.as_os_str().as_bytes().len() > max_bytes {
                continue;
            }
            // create, not create_dir_all: never adopt a directory owned by another fixture.
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            return Self { dir, path };
        }
        panic!("no temporary directory fits the test Unix socket path");
    }
}

impl Drop for TestSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn test_asset_entry(path: &FsPath, root: &FsPath, modified_unix_ms: u128) -> AssetEntry {
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

/// The listing's own state, pointed at a socket that is not there: no roots
/// come back and none are remembered, so nothing rescans and the entries
/// under test stay as they were put in. Their mtimes are what the ordering
/// is asserted on, which a run of files written in the same millisecond
/// could not give.
pub(crate) fn asset_listing_state(root: &FsPath, entries: Vec<AssetEntry>) -> AppState {
    let mut state = test_state("admin", vec![test_device("d1", "token")]);
    state.config.sessions[0].socket_path = root.join("herdr.sock").to_string_lossy().to_string();
    {
        let mut index = state.assets.lock().unwrap();
        for entry in entries {
            index.upsert(entry);
        }
    }
    state
}

pub(crate) fn asset_listing_query(kind: Option<&str>, limit: usize) -> AssetsQuery {
    AssetsQuery {
        cursor: None,
        query: None,
        since: None,
        limit: Some(limit),
        kind: kind.map(str::to_owned),
        path: None,
    }
}

pub(crate) async fn listed_asset_names(
    state: &AppState,
    kind: Option<&str>,
    limit: usize,
) -> Vec<String> {
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

pub(crate) fn git_test_repo(name: &str) -> (PathBuf, PathBuf) {
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

pub(crate) fn remember_pane_root(state: &AppState, pane_id: &str, path: PathBuf) {
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

pub(crate) fn output_state(herdr: &FakeHerdr) -> AppState {
    let mut state = test_state("admin", vec![test_device("d1", "token")]);
    state.config.sessions[0].socket_path = herdr.session().socket_path;
    state
}

pub(crate) async fn read_output(state: &AppState, lines: u32) -> String {
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

pub(crate) async fn read_output_range(state: &AppState, start: u32, end: u32) -> String {
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

pub(crate) fn spawn_body(agent: &str, cwd: Option<&str>) -> SpawnBody {
    SpawnBody {
        agent: agent.to_owned(),
        cwd: cwd.map(str::to_owned),
        tab_id: None,
        prompt: None,
    }
}

pub(crate) fn status_event(pane_id: &str, agent: &str, status: &str) -> Value {
    json!({
        "event": "pane.agent_status_changed",
        "data": { "pane_id": pane_id, "agent": agent, "agent_status": status }
    })
}

/// The drawn-menu spelling, which is what every existing assertion means.
pub(crate) fn approval_data_menu(
    session_id: &str,
    pane_id: &str,
    agent: Option<&str>,
    approval: Option<&approvals::Approval>,
) -> Value {
    approval_data(session_id, pane_id, agent, approval, "menu")
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
pub(crate) struct FakeHerdr {
    pub(crate) socket_path: PathBuf,
    socket: TestSocket,
    serving: tokio::task::JoinHandle<()>,
    pub(crate) calls: Arc<Mutex<Vec<Value>>>,
    /// What `pane.list` answers once set (entries may carry a test-only
    /// `shell_pid` that `pane.process_info` answers with); unset, both
    /// methods answer a bare `ok` as before.
    panes: Arc<Mutex<Option<Value>>>,
    agents: Arc<Mutex<Option<Value>>>,
    process_shells: Arc<Mutex<VecDeque<Option<u32>>>>,
    /// What `pane.get` and `agent.get` answer once set: one pane running an
    /// agent.
    pane: Arc<Mutex<Option<Value>>>,
}

impl FakeHerdr {
    /// `advance_after` is the number of Enters it takes for the agent's
    /// state sequence to move; `None` makes `agent.list` come back empty,
    /// which is a pane running no agent Herdr knows.
    pub(crate) fn start(screens: Vec<&str>, advance_after: Option<usize>) -> Self {
        let socket = TestSocket::new();
        let socket_path = socket.path.clone();
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let panes: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let listed = Arc::clone(&panes);
        let agents: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let listed_agents = agents.clone();
        let process_shells = Arc::new(Mutex::new(VecDeque::<Option<u32>>::new()));
        let shells = process_shells.clone();
        let pane: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let one_pane = pane.clone();
        let screens: Vec<String> = screens.into_iter().map(str::to_owned).collect();
        let serving = tokio::spawn(async move {
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
                let panes = listed.lock().unwrap().clone();
                let result = match (method.as_str(), panes) {
                    ("pane.list", Some(panes)) => json!({ "panes": panes }),
                    ("pane.process_info", Some(panes)) => {
                        let pane = panes.as_array().and_then(|all| {
                            all.iter()
                                .find(|p| p["pane_id"] == request["params"]["pane_id"])
                        });
                        let shell = shells
                            .lock()
                            .unwrap()
                            .pop_front()
                            .map(|pid| json!(pid))
                            .unwrap_or_else(|| {
                                pane.map(|p| p["shell_pid"].clone()).unwrap_or(Value::Null)
                            });
                        json!({ "process_info": { "shell_pid": shell } })
                    }
                    ("pane.read", _) => {
                        let screen = screens
                            .get(reads)
                            .or_else(|| screens.last())
                            .cloned()
                            .unwrap_or_default();
                        reads += 1;
                        json!({ "read": { "text": screen, "revision": reads } })
                    }
                    ("agent.list", _) if listed_agents.lock().unwrap().is_some() => {
                        json!({"agents": listed_agents.lock().unwrap().clone().unwrap()})
                    }
                    ("agent.list", _) => {
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
                    ("pane.send_keys", _) => {
                        enters += 1;
                        json!({ "ok": true })
                    }
                    // One new tab with one pane, for `exec.run`.
                    ("tab.create", _) => json!({
                        "tab": {
                            "tab_id": "w1:t9",
                            "workspace_id": "w1",
                            "active_pane_id": "w1:p9",
                        }
                    }),
                    ("pane.get" | "agent.get", _) if one_pane.lock().unwrap().is_some() => {
                        let pane = one_pane.lock().unwrap().clone().unwrap();
                        if method == "pane.get" {
                            json!({ "pane": pane })
                        } else {
                            json!({ "agent": pane })
                        }
                    }
                    ("pane.get", Some(panes)) => json!({
                        "pane": panes.as_array().and_then(|panes| panes.iter()
                            .find(|pane| pane["pane_id"] == request["params"]["pane_id"]))
                    }),
                    ("pane.get", _) => json!({
                        "pane": {
                            "pane_id": request["params"]["pane_id"],
                            "workspace_id": "w1",
                            "tab_id": "w1:t1",
                        }
                    }),
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
        Self {
            socket_path,
            socket,
            serving,
            calls,
            panes,
            agents,
            process_shells,
            pane,
        }
    }

    /// Script successive fresh shell identity proofs; fall back to `panes` afterwards.
    pub(crate) fn set_process_shells(&self, shells: impl IntoIterator<Item = Option<u32>>) {
        *self.process_shells.lock().unwrap() = shells.into_iter().collect();
    }

    /// Answer `pane.get` and `agent.get` with `pane` from now on.
    pub(crate) fn set_pane(&self, pane: Value) {
        *self.pane.lock().unwrap() = Some(pane);
    }

    pub(crate) fn set_agents(&self, agents: Value) {
        *self.agents.lock().unwrap() = Some(agents);
    }

    /// Answer `pane.list` (and `pane.process_info`) from `panes` from now on.
    pub(crate) fn set_panes(&self, panes: Value) {
        *self.panes.lock().unwrap() = Some(panes);
    }

    pub(crate) fn session(&self) -> SessionConfig {
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
    pub(crate) fn pane_methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call["method"].as_str().unwrap_or_default().to_owned())
            .filter(|method| method != "agent.list")
            .collect()
    }

    pub(crate) fn enters(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["method"] == "pane.send_keys")
            .cloned()
            .collect()
    }
}

/// A pane that repaints a four-row screen, scrolling one row per read: the
/// shape of the pane in card #646, small enough to assert on exactly.
pub(crate) fn repainting_screens() -> Vec<String> {
    (0..4)
        .map(|top| {
            (top..top + 4)
                .map(|row| format!("row {row}"))
                .collect::<Vec<String>>()
                .join("\n")
        })
        .collect()
}

/// A Herdr socket that answers `pane.list` with a fixed set of panes (or
/// none), for driving the real `HerdrBackend` -> `list_panes` path that
/// `sessions()` probes end to end, without needing Herdr installed or a
/// live tmux server.
pub(crate) struct FakePaneListHerdr {
    pub(crate) socket_path: PathBuf,
    panes: Arc<Mutex<Value>>,
}

impl FakePaneListHerdr {
    pub(crate) fn start(panes: Value) -> Self {
        let socket_path = std::env::temp_dir().join(format!(
            "herdr-panelist-{}.sock",
            uuid::Uuid::new_v4().simple()
        ));
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let panes = Arc::new(Mutex::new(panes));
        let live = Arc::clone(&panes);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap_or_default();
                let panes = live.lock().unwrap().clone();
                // `pane.process_info` answers with the `shell_pid` the pane
                // entry carries (a test-only field `pane.list` consumers ignore).
                let result = if request["method"] == "pane.process_info" {
                    let pane = panes.as_array().and_then(|all| {
                        all.iter()
                            .find(|p| p["pane_id"] == request["params"]["pane_id"])
                    });
                    json!({ "process_info": { "shell_pid": pane.map(|p| p["shell_pid"].clone()) } })
                } else {
                    json!({ "panes": panes })
                };
                let response = json!({ "id": request["id"], "result": result }).to_string();
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(b"\n").await;
                let _ = stream.flush().await;
            }
        });
        Self { socket_path, panes }
    }

    /// Replace what `pane.list` answers from now on (a pane closing).
    pub(crate) fn set_panes(&self, panes: Value) {
        *self.panes.lock().unwrap() = panes;
    }

    pub(crate) fn session(&self, id: &str) -> SessionConfig {
        SessionConfig {
            id: id.into(),
            label: id.into(),
            socket_path: self.socket_path.to_string_lossy().into_owned(),
            backend: BackendKind::Herdr,
        }
    }
}

/// A state whose Herdr socket cannot answer, which is how a test reaches
/// the checks that happen before anything is created.
pub(crate) fn unreachable_state() -> AppState {
    let mut state = test_state("admin", vec![test_device("d1", "token")]);
    state.config.sessions[0].socket_path = std::env::temp_dir()
        .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned();
    state
}

pub(crate) fn session_ids(response: &Value) -> Vec<String> {
    response["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect()
}

pub(crate) fn liveness_session(id: &str, backend: BackendKind) -> SessionConfig {
    SessionConfig {
        id: id.into(),
        label: id.into(),
        socket_path: String::new(),
        backend,
    }
}

/// `pairing.json` in the test config directory, written once per process:
/// what the pairing routes seal a QR-style claim with. Answers its current
/// transport key (the routes rotate it after a sealed claim).
pub(crate) fn test_pairing_transport_key() -> String {
    let path = config_dir().unwrap().join(PAIRING_FILE);
    if !path.exists() {
        let pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: "server-1".into(),
                label: "test".into(),
                url: "http://127.0.0.1:23100".into(),
                token: "admin".into(),
                transport_key: generate_token(),
            },
        };
        write_secret_file(&path, &serde_json::to_vec(&pairing).unwrap()).unwrap();
    }
    read_pairing_file().unwrap().payload.transport_key
}

pub(crate) fn write_test_install(dir: &std::path::Path, server_id: &str, token: &str) {
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

/// Seq the fake agent sits on until it accepts an Enter. Arbitrary, but not
/// zero, so "advanced past the baseline" cannot pass by accident.
pub(crate) const FAKE_AGENT_SEQ: u64 = 100;

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

    #[tokio::test]
    async fn socket_fixtures_bind_and_clean_up_with_a_long_tmpdir() {
        const CHILD: &str = "MUQUN_TEST_LONG_SOCKET_TMPDIR";
        if std::env::var_os(CHILD).is_some() {
            let temp = std::env::temp_dir().canonicalize().unwrap();
            assert!(temp.as_os_str().as_bytes().len() > 108);
            let socket = TestSocket::new();
            let max_bytes = if cfg!(target_os = "macos") { 103 } else { 107 };
            assert!(socket.path.as_os_str().as_bytes().len() <= max_bytes);
            assert_eq!(socket.dir.canonicalize().unwrap(), socket.dir);
            assert_eq!(
                std::fs::metadata(&socket.dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let second = TestSocket::new();
            assert_ne!(socket.dir, second.dir);
            let listener = tokio::net::UnixListener::bind(&socket.path).unwrap();
            let client = tokio::net::UnixStream::connect(&socket.path).await.unwrap();
            let (accepted, _) = listener.accept().await.unwrap();
            drop((client, accepted, listener));
            let dir = socket.dir.clone();
            drop(socket);
            assert!(!dir.exists());
            assert!(
                second.dir.exists(),
                "cleanup must not remove another fixture"
            );
            let fake = FakeHerdr::start(vec!["synthetic screen"], None);
            let fake_dir = fake.socket.dir.clone();
            assert!(fake.socket_path.as_os_str().as_bytes().len() <= max_bytes);
            assert_eq!(
                read_output(&output_state(&fake), 1).await,
                "synthetic screen"
            );
            drop(fake);
            assert!(!fake_dir.exists());
            return;
        }

        let root = asset_test_dir("long-socket-tmpdir");
        // A symlink and multibyte component also exercise canonical byte length.
        let long = root.join("é".repeat(60));
        let alias = root.join("alias");
        std::fs::create_dir(&long).unwrap();
        std::os::unix::fs::symlink(&long, &alias).unwrap();
        let outputs: Vec<_> = [
            "test_support::tests::socket_fixtures_bind_and_clean_up_with_a_long_tmpdir",
            "tasks::service::tests::terminal_start_retries_only_before_a_typed_goal_can_be_delivered",
        ].into_iter().map(|test| {
            ProcessCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(CHILD, "1")
                .env("TMPDIR", &alias)
                .output().unwrap()
        }).collect();
        std::fs::remove_dir_all(root).unwrap();
        for output in outputs {
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }
}

/// An agent that answers from canned data, for route and service
/// tests. `failing` makes every read error. `calls` records the writes it
/// was asked for (`create_session`, `send_prompt`, `interrupt`), and is shared
/// so a test can read it after the agent moved into a manager.
type PreparationLog = Arc<Mutex<Vec<(bool, String)>>>;

pub(crate) struct FakeAgent {
    pub kind: &'static str,
    pub sessions: Vec<agents::domain::AgentSessionInfo>,
    pub failing: bool,
    pub catalog: agents::domain::AgentCatalog,
    /// Hold catalog reads to exercise callers that must not wait on providers.
    pub catalog_gate: Option<Arc<tokio::sync::Notify>>,
    pub timeline: Vec<agents::domain::TimelineItem>,
    /// A session's own timeline, in place of `timeline`, by asid.
    pub session_timelines: HashMap<String, Vec<agents::domain::TimelineItem>>,
    /// Permission requests `get_pending_permissions` reports, per their asid.
    /// Shared, so a test can answer them after the agent moved.
    pub permissions: Arc<Mutex<Vec<agents::domain::PermissionRequest>>>,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub preparation_log: Option<PreparationLog>,
    /// The model each `create_session` was asked for.
    pub created_with: Arc<Mutex<Vec<Option<agents::domain::ModelRef>>>>,
    /// The text of every prompt sent.
    pub prompts: Arc<Mutex<Vec<String>>>,
    /// The attachments every prompt carried.
    pub attached: Arc<Mutex<Vec<Vec<String>>>>,
    pub deliveries: Arc<Mutex<Vec<Option<String>>>>,
    /// Sessions answered the way OpenCode answers a deleted one: a plain
    /// HTTP 404 rather than `SessionNotFound`.
    pub http_404: Vec<String>,
    pub session_deletion: bool,
    /// An agent whose service takes MCP servers at runtime.
    pub mcp: Option<Arc<Mutex<FakeMcp>>>,
}

/// A fake agent service's MCP registration.
#[derive(Default)]
pub(crate) struct FakeMcp {
    pub connected: bool,
    /// Refuse the next registrations.
    pub refuse: bool,
    /// How many registrations it took.
    pub added: usize,
}

impl FakeAgent {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            sessions: Vec::new(),
            failing: false,
            catalog: Default::default(),
            catalog_gate: None,
            timeline: Vec::new(),
            session_timelines: HashMap::new(),
            permissions: Arc::default(),
            calls: Arc::default(),
            preparation_log: None,
            created_with: Arc::default(),
            prompts: Arc::default(),
            attached: Arc::default(),
            deliveries: Arc::default(),
            http_404: Vec::new(),
            session_deletion: false,
            mcp: None,
        }
    }

    pub fn with_timeline(mut self, timeline: Vec<agents::domain::TimelineItem>) -> Self {
        self.timeline = timeline;
        self
    }

    fn record(&self, call: &str) {
        self.calls.lock().unwrap().push(call.to_string());
    }

    pub fn with_sessions(mut self, sessions: &[(&str, u64)]) -> Self {
        self.sessions = sessions
            .iter()
            .map(|(asid, updated_ms)| fake_session(asid, *updated_ms))
            .collect();
        self
    }

    /// Sessions with parents: `(asid, parent, updated_ms)`. `list_sessions`
    /// honours the parent filter as every real adapter does.
    pub fn with_session_tree(mut self, sessions: &[(&str, Option<&str>, u64)]) -> Self {
        self.sessions = sessions
            .iter()
            .map(|(asid, parent, updated_ms)| {
                let mut session = fake_session(asid, *updated_ms);
                session.parent_id = parent.map(str::to_owned);
                session
            })
            .collect();
        self
    }

    pub fn with_session_deletion(mut self) -> Self {
        self.session_deletion = true;
        self
    }

    pub fn failing(mut self) -> Self {
        self.failing = true;
        self
    }

    pub fn manager(self) -> Arc<agents::manager::AgentManager> {
        Arc::new(agents::manager::AgentManager::for_test(Arc::new(self)))
    }

    fn fail<T>(&self) -> Result<T, agents::ports::agent::AgentError> {
        Err(agents::ports::agent::AgentError::Network(
            "fake agent is down".into(),
        ))
    }
}

/// A session as an adapter returns it: no `agent_id`, which the manager adds.
pub(crate) fn fake_session(asid: &str, updated_ms: u64) -> agents::domain::AgentSessionInfo {
    serde_json::from_value(json!({
        "asid": asid,
        "title": asid,
        "status": "idle",
        "updated_ms": updated_ms,
    }))
    .expect("a minimal session parses")
}

impl agents::ports::agent::AgentPort for FakeAgent {
    fn refresh_session_mcp<'a>(
        &'a self,
        asid: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, bool> {
        self.record(&format!("refresh_mcp {asid}"));
        Box::pin(async { Ok(false) })
    }

    fn adds_mcp_servers(&self) -> bool {
        self.mcp.is_some()
    }

    fn mcp_server_connected<'a>(
        &'a self,
        _name: &'a str,
        _directory: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, bool> {
        let connected = self
            .mcp
            .as_ref()
            .is_some_and(|mcp| mcp.lock().unwrap().connected);
        Box::pin(async move { Ok(connected) })
    }

    fn remove_session_instruction<'a>(
        &'a self,
        session_id: &'a str,
        key: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        self.record(&format!("remove_instruction {session_id} {key}"));
        Box::pin(async { Ok(()) })
    }

    fn add_mcp_server<'a>(
        &'a self,
        _name: &'a str,
        _config: &'a serde_json::Value,
        _directory: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        let outcome = match &self.mcp {
            Some(mcp) => {
                let mut mcp = mcp.lock().unwrap();
                if mcp.refuse {
                    Err(agents::ports::agent::AgentError::RequestFailed(
                        "HTTP 400: Authorization Bearer secret-token".into(),
                    ))
                } else {
                    mcp.connected = true;
                    mcp.added += 1;
                    Ok(())
                }
            }
            None => self.fail(),
        };
        Box::pin(async move { outcome })
    }

    fn kind(&self) -> &'static str {
        self.kind
    }
    fn probe(&self) -> agents::ports::agent::AgentFuture<'_, bool> {
        Box::pin(async move { Ok(!self.failing) })
    }
    fn list_projects(
        &self,
    ) -> agents::ports::agent::AgentFuture<'_, Vec<agents::domain::AgentProject>> {
        Box::pin(async move {
            if self.failing {
                return self.fail();
            }
            Ok(vec![agents::domain::AgentProject {
                id: format!("{}-project", self.kind),
                canonical: "/work".into(),
                name: self.kind.into(),
                vcs: None,
                sandboxes: Vec::new(),
                missing: false,
            }])
        })
    }
    fn list_sessions<'a>(
        &'a self,
        query: &'a agents::domain::SessionQuery,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::AgentSessionInfo>> {
        Box::pin(async move {
            if self.failing {
                return self.fail();
            }
            self.record(&format!(
                "list_sessions parent={}",
                query.parent_id.as_deref().unwrap_or("-")
            ));
            Ok(self
                .sessions
                .iter()
                .filter(|session| query.keeps_parent(session.parent_id.as_deref()))
                .cloned()
                .collect())
        })
    }
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a agents::domain::ModelRef>,
        _mode: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentSessionInfo> {
        Box::pin(async move {
            if self.failing {
                return self.fail();
            }
            self.record("create_session");
            self.created_with.lock().unwrap().push(model.cloned());
            let mut session = fake_session(&format!("{}_new", self.kind), 1);
            session.directory = directory.map(str::to_string);
            Ok(session)
        })
    }
    fn fork_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentSessionInfo> {
        Box::pin(async move {
            let mut info = self.get_session(session_id).await?;
            if self.failing {
                return self.fail();
            }
            self.record(&format!(
                "fork_session {session_id} {}",
                message_id.unwrap_or("whole")
            ));
            info.asid = agents::domain::AgentSessionId(format!("{}_fork", self.kind));
            Ok(info)
        })
    }
    fn get_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentSessionInfo> {
        Box::pin(async move {
            if self.http_404.iter().any(|asid| asid == session_id) {
                return Err(agents::ports::agent::AgentError::RequestFailed(
                    "HTTP 404 Not Found: no such session".into(),
                ));
            }
            self.sessions
                .iter()
                .find(|s| s.asid.0 == session_id)
                .cloned()
                .ok_or_else(|| agents::ports::agent::AgentError::SessionNotFound(session_id.into()))
        })
    }
    fn prepare_selected_session<'a>(
        &'a self,
        id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(log) = &self.preparation_log {
                log.lock().unwrap().push((true, id.into()));
            }
            Ok(())
        })
    }
    fn prepare_observed_session<'a>(
        &'a self,
        id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(log) = &self.preparation_log {
                log.lock().unwrap().push((false, id.into()));
            }
            Ok(())
        })
    }
    fn delete_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async move {
            if !self.session_deletion {
                return Err(agents::ports::agent::AgentError::Unsupported(
                    "delete_session".into(),
                ));
            }
            self.record(&format!("delete_session {session_id}"));
            Ok(())
        })
    }

    fn send_prompt<'a>(
        &'a self,
        _session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        self.record("send_prompt");
        self.deliveries
            .lock()
            .unwrap()
            .push(delivery.map(str::to_owned));
        self.prompts.lock().unwrap().push(text.to_owned());
        self.attached.lock().unwrap().push(attachments.to_vec());
        Box::pin(async { Ok(()) })
    }
    fn revert_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn interrupt<'a>(&'a self, _session_id: &'a str) -> agents::ports::agent::AgentFuture<'a, ()> {
        self.record("interrupt");
        Box::pin(async { Ok(()) })
    }
    fn switch_model<'a>(
        &'a self,
        _session_id: &'a str,
        _model: &'a agents::domain::ModelRef,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn switch_mode<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        directory: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async move {
            self.record(&format!("find_files:{}", directory.unwrap_or("")));
            Ok(Vec::new())
        })
    }
    fn reply_permission<'a>(
        &'a self,
        _session_id: &'a str,
        _request_id: &'a str,
        _decision: agents::domain::PermissionDecision,
        _message: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn reply_form<'a>(
        &'a self,
        _session_id: &'a str,
        _form_id: &'a str,
        _answers: serde_json::Value,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn get_catalog<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentCatalog> {
        Box::pin(async move {
            if let Some(gate) = &self.catalog_gate {
                gate.notified().await;
            }
            if self.failing {
                return self.fail();
            }
            Ok(self.catalog.clone())
        })
    }
    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::ports::agent::FileDiffItem>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::PermissionRequest>> {
        Box::pin(async move {
            Ok(self
                .permissions
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.asid.0 == session_id)
                .cloned()
                .collect())
        })
    }
    fn get_pending_forms<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::FormRequest>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        _limit: usize,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::TimelineItem>> {
        Box::pin(async move {
            Ok(self
                .session_timelines
                .get(session_id)
                .unwrap_or(&self.timeline)
                .clone())
        })
    }
}
