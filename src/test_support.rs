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
        t3: agents::T3Config::default(),
    }
}

pub(crate) fn test_device(id: &str, token: &str) -> DeviceRecord {
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

pub(crate) fn test_state(admin_token: &str, devices: Vec<DeviceRecord>) -> AppState {
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
        ws_connections: Arc::new(agents::ws_routes::WsRegistry::default()),
        generation: new_generation(),
        message_images: Arc::default(),
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
        name: id.into(),
        token_hash: authority::hash_token(id),
        transport_key: None,
        paired_unix_ms: 1,
        last_seen_unix_ms: 1,
        install_id: None,
    }
}

pub(crate) fn test_pending_pairing(created_unix_ms: u128) -> Option<PendingPairing> {
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
    pub(crate) calls: Arc<Mutex<Vec<Value>>>,
}

impl FakeHerdr {
    /// `advance_after` is the number of Enters it takes for the agent's
    /// state sequence to move; `None` makes `agent.list` come back empty,
    /// which is a pane running no agent Herdr knows.
    pub(crate) fn start(screens: Vec<&str>, advance_after: Option<usize>) -> Self {
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
                    "pane.get" => json!({
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
        Self { socket_path, calls }
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
}

impl FakePaneListHerdr {
    pub(crate) fn start(panes: Value) -> Self {
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
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// An agent that answers from canned data, for route and service
/// tests. `failing` makes every read error.
pub(crate) struct FakeAgent {
    pub kind: &'static str,
    pub sessions: Vec<agents::domain::AgentSessionInfo>,
    pub failing: bool,
    pub catalog: agents::domain::AgentCatalog,
}

impl FakeAgent {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            sessions: Vec::new(),
            failing: false,
            catalog: Default::default(),
        }
    }

    pub fn with_sessions(mut self, sessions: &[(&str, u64)]) -> Self {
        self.sessions = sessions
            .iter()
            .map(|(asid, updated_ms)| fake_session(asid, *updated_ms))
            .collect();
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
        _query: &'a agents::domain::SessionQuery,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::AgentSessionInfo>> {
        Box::pin(async move {
            if self.failing {
                return self.fail();
            }
            Ok(self.sessions.clone())
        })
    }
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        _model: Option<&'a agents::domain::ModelRef>,
        _mode: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentSessionInfo> {
        Box::pin(async move {
            if self.failing {
                return self.fail();
            }
            let mut session = fake_session(&format!("{}_new", self.kind), 1);
            session.directory = directory.map(str::to_string);
            Ok(session)
        })
    }
    fn get_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, agents::domain::AgentSessionInfo> {
        Box::pin(async move {
            self.sessions
                .iter()
                .find(|s| s.asid.0 == session_id)
                .cloned()
                .ok_or_else(|| agents::ports::agent::AgentError::SessionNotFound(session_id.into()))
        })
    }
    fn send_prompt<'a>(
        &'a self,
        _session_id: &'a str,
        _text: &'a str,
        _attachments: &'a [String],
        _delivery: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, ()> {
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
        _directory: Option<&'a str>,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Ok(Vec::new()) })
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
        _session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::PermissionRequest>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_pending_forms<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::FormRequest>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_timeline<'a>(
        &'a self,
        _session_id: &'a str,
        _limit: usize,
    ) -> agents::ports::agent::AgentFuture<'a, Vec<agents::domain::TimelineItem>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}
