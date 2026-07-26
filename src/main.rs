#![recursion_limit = "256"]

use std::collections::{HashMap, VecDeque};
use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::process::{Command as ProcessCommand, Stdio};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use base64::Engine as _;
use clap::{Parser, Subcommand};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    poll as poll_event, read as read_event, Event as TerminalEvent, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_stream::Stream;

mod shortcuts;

#[cfg(unix)]
use tokio::net::UnixStream;

const CONFIG_FILE: &str = "config.json";
const PAIRING_FILE: &str = "pairing.json";
const PUSH_TOKENS_FILE: &str = "push-tokens.json";
const DEVICES_FILE: &str = "devices.json";
const PID_FILE: &str = "gateway.pid";
const LOG_FILE: &str = "gateway.log";
// Deliberately outside the common-service range and outside the OS ephemeral
// ranges (Linux 32768-60999, macOS 49152-65535) so setup rarely collides.
const DEFAULT_PORT: u16 = 23847;
// Scrollback a phone can page back through. Herdr keeps far more; this is the
// ceiling on a single read, and 1000 ran out after a few screens of an agent
// transcript.
const MAX_OUTPUT_LINES: u32 = 5000;
const MAX_SEND_TEXT_BYTES: usize = 64 * 1024;
const PAIRING_CODE_CHARACTER_COUNT: usize = 8;
const PAIRING_CODE_LENGTH: usize = PAIRING_CODE_CHARACTER_COUNT + 1;
const PAIRING_CODE_TTL_MS: u128 = 5 * 60 * 1000;
const MAX_PAIRING_CODE_ATTEMPTS: u8 = 8;
const PAIRING_RATE_LIMIT_WINDOW_MS: u128 = 10 * 60 * 1000;
const MAX_PAIRING_REQUESTS_PER_WINDOW: usize = 6;
const MAX_SEND_KEYS: usize = 32;
const MAX_PUSH_TOKENS: usize = 64;
const MAX_DEVICES: usize = 32;
const MAX_DEVICE_NAME_CHARS: usize = 80;
/// Device `last_seen` is kept in memory and only flushed to disk past this
/// interval so that routine polling does not rewrite the file on every request.
const DEVICE_LAST_SEEN_FLUSH_MS: u128 = 5 * 60 * 1000;
const MANAGE_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// Herdr's general `pane.updated` subscription is intentionally coalesced and
/// can arrive several seconds after terminal output is already readable. While
/// a phone is actively viewing one pane, sample that pane locally and publish a
/// frame only when its actual content changes. Herdr's revision is coalesced
/// along with `pane.updated`, so it can remain stale even after `pane.read`
/// already exposes new text. This keeps the mobile SSE live without polling
/// every pane or sending unchanged terminal frames over the network.
const STREAM_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(150);
const STREAM_OUTPUT_READ_TIMEOUT: Duration = Duration::from_millis(100);
const GATEWAY_API_VERSION: &str = "1.2.0";
const GATEWAY_API_MAJOR: u64 = 1;
const HERDR_PROTOCOL_MIN: u64 = 17;
const HERDR_PROTOCOL_MAX: u64 = 17;
const MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;
const UPLOADS_DIR: &str = "uploads";
/// Ceiling on one upload body, enforced by the framework's body limit so an
/// oversized request is cut off mid-stream instead of being buffered first.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// The client's own file name is only ever echoed back, never used to build a
/// path, so this is a display cap rather than a safety one.
const MAX_UPLOAD_NAME_CHARS: usize = 120;
/// Uploads are a handoff to a local agent, not storage: whatever the agent was
/// going to do with a file, it has done long before this.
const UPLOAD_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const UPLOAD_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);
const API_CAPABILITIES: &[&str] = &[
    "agent_lifecycle_notifications",
    "device_revocation",
    "file_uploads",
    "one_time_pairing_codes",
    "pane_output_ansi",
    "pane_shortcuts",
    "configurable_agent_profiles",
    "per_device_tokens",
    "push_notifications",
    "push_token_revocation",
    "terminal_input",
];

#[derive(Parser)]
#[command(name = "gateway", about = "Mobile gateway for Herdr")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Setup {
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        socket_path: Option<String>,
    },
    Run {
        #[arg(long)]
        config: Option<String>,
    },
    Start,
    Stop,
    Status,
    Manage,
    /// List devices that hold a gateway token.
    Devices,
    /// Revoke one device's token, or every device token with --all.
    Revoke {
        /// Device id from `gateway devices`.
        device_id: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    server_id: String,
    label: String,
    listen: String,
    public_url: String,
    /// Hash of the admin token. The admin token lives in `pairing.json` and is
    /// used by the local `manage` UI; paired devices get their own tokens.
    token_hash: String,
    sessions: Vec<SessionConfig>,
}

impl Config {
    fn port(&self) -> u16 {
        self.listen
            .parse::<SocketAddr>()
            .map(|addr| addr.port())
            .unwrap_or(DEFAULT_PORT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionConfig {
    id: String,
    label: String,
    socket_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingPayload {
    kind: String,
    server_id: String,
    label: String,
    url: String,
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingFile {
    payload: PairingPayload,
}

struct PublicUrlSelection {
    url: String,
    source: String,
    listen_host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingPairing {
    request_id: String,
    device_name: String,
    #[serde(default)]
    install_id: Option<String>,
    code: String,
    code_hash: String,
    created_unix_ms: u128,
    #[serde(default)]
    failed_attempts: u8,
}

#[derive(Debug, PartialEq, Eq)]
enum PairingCodeError {
    Missing,
    Expired,
    Invalid,
}

#[derive(Deserialize)]
struct PairRequestBody {
    request_id: String,
    device_name: Option<String>,
    /// A stable per-install identifier from the client. When present, a new
    /// pairing replaces any earlier device with the same value, so re-pairing a
    /// device does not leave a duplicate record behind.
    #[serde(default)]
    install_id: Option<String>,
}

#[derive(Deserialize)]
struct PairClaimBody {
    request_id: String,
    code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenRecord {
    token: String,
    platform: String,
    device_name: Option<String>,
    updated_unix_ms: u128,
}

/// One paired device. Each successful pairing mints a token that only this
/// record can authenticate, so a single device can be revoked on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeviceRecord {
    id: String,
    name: String,
    token_hash: String,
    paired_unix_ms: u128,
    #[serde(default)]
    last_seen_unix_ms: u128,
    /// The client's stable install identifier, used to replace an earlier
    /// record for the same device instead of piling up duplicates.
    #[serde(default)]
    install_id: Option<String>,
}

#[derive(Deserialize)]
struct RegisterPushTokenBody {
    token: String,
    platform: String,
    device_name: Option<String>,
}

#[derive(Deserialize)]
struct UnregisterPushTokenBody {
    token: String,
}

#[derive(Deserialize)]
struct SendPushNotificationBody {
    title: Option<String>,
    body: Option<String>,
    data: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, PartialEq, Eq)]
struct AgentPushNotification {
    title: String,
    body: String,
    data: serde_json::Map<String, Value>,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
    pairing_requests: Arc<Mutex<VecDeque<u128>>>,
    push_tokens: Arc<Mutex<Vec<PushTokenRecord>>>,
    devices: Arc<Mutex<Vec<DeviceRecord>>>,
}

#[derive(Deserialize)]
struct OutputQuery {
    source: Option<String>,
    lines: Option<u32>,
    format: Option<String>,
}

#[derive(Deserialize)]
struct SendTextBody {
    text: String,
}

#[derive(Deserialize)]
struct SendKeysBody {
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct CreateWorkspaceBody {
    cwd: Option<String>,
    label: Option<String>,
    focus: Option<bool>,
}

#[derive(Deserialize)]
struct RenameWorkspaceBody {
    label: String,
}

#[derive(Deserialize)]
struct CreateTabBody {
    workspace_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    focus: Option<bool>,
}

#[derive(Deserialize)]
struct RenameTabBody {
    label: String,
}

#[derive(Deserialize)]
struct RenamePaneBody {
    label: String,
}

#[derive(Deserialize)]
struct SplitPaneBody {
    direction: String,
    ratio: Option<f64>,
    command: Option<Vec<String>>,
    cwd: Option<String>,
    env: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
struct ZoomPaneBody {
    mode: Option<String>,
}

#[derive(Deserialize)]
struct AgentSendBody {
    text: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup {
            public_url,
            port,
            socket_path,
        } => setup(public_url, port, socket_path)?,
        Command::Run { config } => run(config).await?,
        Command::Start => start_background()?,
        Command::Stop => stop_background()?,
        Command::Status => status()?,
        Command::Manage => manage()?,
        Command::Devices => list_devices()?,
        Command::Revoke { device_id, all } => revoke_device(device_id, all)?,
    }
    Ok(())
}

fn setup(public_url: Option<String>, port: u16, socket_path: Option<String>) -> anyhow::Result<()> {
    let config_dir = config_dir()?;
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create config dir {}", config_dir.display()))?;

    // Reuse an existing install's identity so re-running setup (after an update
    // or a retry) refreshes settings without minting a new server id or token --
    // that would orphan every already-paired device. Only a consistent config +
    // pairing pair is trusted; a half-written state falls back to a fresh mint.
    let existing = load_existing_identity(
        &config_dir.join(CONFIG_FILE),
        &config_dir.join(PAIRING_FILE),
    );

    let (server_id, token, token_hash) = match &existing {
        Some(id) => (
            id.server_id.clone(),
            id.token.clone(),
            id.token_hash.clone(),
        ),
        None => {
            let token = generate_token();
            let token_hash = hash_token(&token);
            (uuid::Uuid::new_v4().to_string(), token, token_hash)
        }
    };

    // An explicit --public-url always wins. Otherwise a returning install keeps
    // the URL and listen address it already has (including one set from the
    // manage panel); only a fresh install auto-detects.
    let (public_url, listen, url_source) = match (public_url, &existing) {
        (Some(url), _) => (
            validate_public_url(&url)?,
            format!("0.0.0.0:{port}"),
            String::from("manual --public-url"),
        ),
        (None, Some(id)) => (
            id.public_url.clone(),
            id.listen.clone(),
            String::from("existing config"),
        ),
        (None, None) => {
            let selection = auto_public_url(port);
            (
                selection.url,
                format!("{}:{port}", selection.listen_host),
                selection.source,
            )
        }
    };
    let socket_path = socket_path
        .or_else(|| std::env::var("HERDR_SOCKET_PATH").ok())
        .unwrap_or_else(default_socket_path);

    let config = Config {
        server_id,
        label: hostname_label(),
        listen,
        public_url: public_url.clone(),
        token_hash,
        sessions: vec![SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path,
        }],
    };

    let path = config_dir.join(CONFIG_FILE);
    write_secret_file(&path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", path.display()))?;

    let payload = PairingPayload {
        kind: "herdr-gateway".into(),
        server_id: config.server_id.clone(),
        label: config.label.clone(),
        url: public_url,
        token,
    };
    let pairing_path = config_dir.join(PAIRING_FILE);
    write_secret_file(
        &pairing_path,
        &serde_json::to_vec_pretty(&PairingFile {
            payload: payload.clone(),
        })?,
    )?;
    println!("wrote config: {}", path.display());
    println!("wrote pairing file: {}", pairing_path.display());
    println!("public URL: {} ({url_source})", payload.url);
    if payload.url.contains("127.0.0.1") || payload.url.contains("localhost") {
        println!("warning: pairing URL is local-only; rerun setup after starting Tailscale or pass --public-url");
    }
    println!("pairing identity is ready");
    println!("open the Gateway Manager pane to scan the QR code");
    Ok(())
}

/// The parts of a prior install that setup preserves so a rerun keeps devices
/// paired. Pulled from both files: the server id and admin token hash live in
/// the config, the raw admin token lives in the pairing file.
struct ExistingIdentity {
    server_id: String,
    token: String,
    token_hash: String,
    public_url: String,
    listen: String,
}

fn load_existing_identity(
    config_path: &std::path::Path,
    pairing_path: &std::path::Path,
) -> Option<ExistingIdentity> {
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).ok()?).ok()?;
    let pairing: PairingFile = serde_json::from_slice(&std::fs::read(pairing_path).ok()?).ok()?;
    // A config whose pairing file points at a different server is inconsistent;
    // treat it as no identity so setup mints a clean one rather than stitching
    // two mismatched halves together.
    if pairing.payload.server_id != config.server_id {
        return None;
    }
    Some(ExistingIdentity {
        server_id: config.server_id,
        token: pairing.payload.token,
        token_hash: config.token_hash,
        public_url: config.public_url,
        listen: config.listen,
    })
}

fn auto_public_url(port: u16) -> PublicUrlSelection {
    let Some(status) = tailscale_status_json() else {
        return PublicUrlSelection {
            url: format!("http://127.0.0.1:{port}"),
            source: String::from("localhost fallback"),
            listen_host: String::from("127.0.0.1"),
        };
    };

    if status.get("BackendState").and_then(Value::as_str) != Some("Running") {
        return PublicUrlSelection {
            url: format!("http://127.0.0.1:{port}"),
            source: String::from("tailscale not running"),
            listen_host: String::from("127.0.0.1"),
        };
    }

    if let Some(domain) = tailscale_serve_https_domain(port) {
        return PublicUrlSelection {
            url: format!("https://{domain}"),
            source: String::from("tailscale serve https"),
            listen_host: String::from("127.0.0.1"),
        };
    }

    if let Some(ip) = status
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .and_then(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .find(|ip| ip.contains('.'))
        })
    {
        // Prefer the MagicDNS name in the URL over the raw IP: it's stable across
        // IP changes and is the name the user gets HTTPS on the moment they point
        // Tailscale Serve at the gateway. The listener still binds to the IP, and
        // MagicDNS resolves the name to it, so the phone reaches it either way.
        let magic_dns = status
            .pointer("/Self/DNSName")
            .and_then(Value::as_str)
            .map(|name| name.trim_end_matches('.'))
            .filter(|name| !name.is_empty());
        return match magic_dns {
            Some(name) => PublicUrlSelection {
                url: format!("http://{name}:{port}"),
                source: String::from("tailscale magicdns (http; set up Serve for https)"),
                listen_host: ip.to_string(),
            },
            None => PublicUrlSelection {
                url: format!("http://{ip}:{port}"),
                source: String::from("tailscale ip"),
                listen_host: ip.to_string(),
            },
        };
    }

    PublicUrlSelection {
        url: format!("http://127.0.0.1:{port}"),
        source: String::from("localhost fallback"),
        listen_host: String::from("127.0.0.1"),
    }
}

fn tailscale_status_json() -> Option<Value> {
    let output = ProcessCommand::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn tailscale_serve_https_domain(port: u16) -> Option<String> {
    let output = ProcessCommand::new("tailscale")
        .args(["serve", "status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let status: Value = serde_json::from_slice(&output.stdout).ok()?;
    let web = status.get("Web")?.as_object()?;
    for (host_port, config) in web {
        let handlers = config.get("Handlers").and_then(Value::as_object)?;
        let serves_gateway = handlers.values().any(|handler| {
            handler
                .get("Proxy")
                .and_then(Value::as_str)
                .is_some_and(|proxy| proxy_targets_port(proxy, port))
        });
        if serves_gateway {
            let domain = host_port
                .strip_suffix(":443")
                .unwrap_or(host_port)
                .trim_end_matches('.');
            if !domain.is_empty() {
                return Some(domain.to_string());
            }
        }
    }
    None
}

fn proxy_targets_port(proxy: &str, port: u16) -> bool {
    let without_path = proxy
        .strip_prefix("http://")
        .or_else(|| proxy.strip_prefix("https://"))
        .unwrap_or(proxy)
        .split('/')
        .next()
        .unwrap_or_default();
    without_path
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        == Some(port)
}

fn start_background() -> anyhow::Result<()> {
    start_background_inner(true)
}

fn start_background_inner(verbose: bool) -> anyhow::Result<()> {
    let state_dir = state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create state dir {}", state_dir.display()))?;
    if let Some(pid) = read_pid()? {
        if process_running(pid) {
            if verbose {
                println!("gateway already running with pid {pid}");
            }
            return Ok(());
        }
    }

    let exe = std::env::current_exe().context("failed to find current executable")?;
    let config = config_dir()?.join(CONFIG_FILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(LOG_FILE))?;
    let err = log.try_clone()?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("run")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    detach_background_process(&mut command);
    let child = command.spawn().context("failed to start gateway")?;
    let pid = child.id();
    std::fs::write(state_dir.join(PID_FILE), pid.to_string())?;
    if verbose {
        println!("gateway started with pid {pid}");
    }
    Ok(())
}

#[cfg(unix)]
fn detach_background_process(command: &mut ProcessCommand) {
    // Herdr may tear down the popup/action process group when a panel closes.
    // Start the gateway in a new session so it survives the manager UI.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_background_process(_command: &mut ProcessCommand) {}

fn stop_background() -> anyhow::Result<()> {
    stop_background_inner(true)
}

/// The port this installation actually listens on, falling back to the default
/// when the config is missing or unreadable.
fn configured_port() -> u16 {
    load_config(None)
        .map(|config| config.port())
        .unwrap_or(DEFAULT_PORT)
}

fn stop_background_inner(verbose: bool) -> anyhow::Result<()> {
    let mut stopped = false;
    if let Some(pid) = read_pid()? {
        if process_running(pid) {
            stop_pid(pid)?;
            stopped = true;
            if verbose {
                println!("gateway stopped pid {pid}");
            }
        } else if verbose {
            println!("gateway pid file exists, but pid {pid} is not running");
        }
    }

    let port = configured_port();
    for pid in gateway_listener_pids(port)? {
        if process_running(pid) {
            stop_pid(pid)?;
            stopped = true;
            if verbose {
                println!("gateway stopped listener pid {pid}");
            }
        }
    }

    remove_pid_file()?;
    if verbose && !stopped {
        println!("gateway is not running");
    }
    Ok(())
}

async fn run(config_path: Option<String>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;

    let state = AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(read_push_tokens().unwrap_or_default())),
        devices: Arc::new(Mutex::new(read_devices().unwrap_or_default())),
    };
    spawn_agent_notification_watchers(state.clone());
    spawn_upload_gc();

    let app = Router::new()
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi_json))
        .route("/api/pair/request", post(pair_request))
        .route("/api/pair/claim", post(pair_claim))
        .route("/api/pair/pending", get(pair_pending))
        .route("/api/meta", get(api_meta).patch(api_set_label))
        .route("/api/pairings", get(list_paired_devices))
        .route(
            "/api/pairings/{device_id}",
            axum::routing::delete(revoke_paired_device),
        )
        .route(
            "/api/devices/push-token",
            post(register_push_token).delete(unregister_push_token),
        )
        .route("/api/notifications/test", post(send_test_notification))
        .route("/health", get(health))
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
        .route("/api/sessions/{session_id}/agents", get(agents))
        .route("/api/sessions/{session_id}/agents/{target}", get(agent))
        .route(
            "/api/sessions/{session_id}/agents/{target}/focus",
            post(focus_agent),
        )
        .route(
            "/api/sessions/{session_id}/agents/{target}/send",
            post(send_agent),
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
            "/api/sessions/{session_id}/panes/{pane_id}/send-text",
            post(send_text),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/send-keys",
            post(send_keys),
        )
        // A route-level limit is applied inside the router-wide one, so uploads
        // get their own ceiling while every JSON route keeps the small one.
        .route(
            "/api/uploads",
            post(upload_file).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);

    println!("herdr gateway listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "cache-control",
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

async fn docs() -> Html<&'static str> {
    Html(DOCS_HTML)
}

async fn openapi_json() -> Json<Value> {
    Json(openapi_spec())
}

async fn pair_request(
    State(state): State<AppState>,
    Json(body): Json<PairRequestBody>,
) -> ApiResult<Json<Value>> {
    if !valid_request_id(&body.request_id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_id",
            "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        ));
    }
    let device_name = body
        .device_name
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Muqun app".into());
    // The manage UI renders this straight into a terminal, so an unfiltered
    // name could inject ANSI escapes and forge the pairing prompt.
    if !valid_device_name(&device_name) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_device_name",
            "device_name must be at most 80 characters and contain no control characters",
        ));
    }

    let now = now_unix_ms();
    let mut pending_pairing = state.pending_pairing.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_lock_failed",
            "failed to lock pending pairing state",
        )
    })?;
    if let Some(pending) = pending_pairing.as_ref() {
        if !pairing_code_expired(pending, now) {
            if pending.request_id == body.request_id {
                return Ok(Json(pair_request_response(&state.config, &body.request_id)));
            }
            return Err(api_error(
                StatusCode::CONFLICT,
                "pairing_in_progress",
                "another pairing request is awaiting confirmation",
            ));
        }
    }

    record_pairing_request(&state, now)?;

    let code = generate_pairing_code();
    let install_id = body
        .install_id
        .as_deref()
        .filter(|value| valid_install_id(value))
        .map(str::to_owned);
    let pending = PendingPairing {
        request_id: body.request_id.clone(),
        device_name,
        install_id,
        code: code.clone(),
        code_hash: hash_token(&code),
        created_unix_ms: now,
        failed_attempts: 0,
    };
    *pending_pairing = Some(pending);
    Ok(Json(pair_request_response(&state.config, &body.request_id)))
}

fn record_pairing_request(state: &AppState, now_unix_ms: u128) -> ApiResult<()> {
    let mut requests = state.pairing_requests.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_rate_limit_failed",
            "failed to check pairing request limit",
        )
    })?;
    while requests
        .front()
        .is_some_and(|created| now_unix_ms.saturating_sub(*created) >= PAIRING_RATE_LIMIT_WINDOW_MS)
    {
        requests.pop_front();
    }
    if requests.len() >= MAX_PAIRING_REQUESTS_PER_WINDOW {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "pairing_rate_limited",
            "too many pairing requests; try again later",
        ));
    }
    requests.push_back(now_unix_ms);
    Ok(())
}

async fn pair_claim(
    State(state): State<AppState>,
    Json(body): Json<PairClaimBody>,
) -> ApiResult<Json<Value>> {
    let (device_name, install_id) = {
        let mut pending = state.pending_pairing.lock().map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pairing_lock_failed",
                "failed to lock pending pairing state",
            )
        })?;
        let device_name = pending
            .as_ref()
            .map(|value| value.device_name.clone())
            .unwrap_or_else(|| "Muqun app".into());
        let install_id = pending.as_ref().and_then(|value| value.install_id.clone());
        let code = body.code.trim().to_ascii_uppercase();
        consume_pairing_code(&mut pending, &body.request_id, &code, now_unix_ms()).map_err(
            |error| match error {
                PairingCodeError::Missing => api_error(
                    StatusCode::FORBIDDEN,
                    "pairing_not_requested",
                    "no pending pairing request",
                ),
                PairingCodeError::Expired => api_error(
                    StatusCode::GONE,
                    "pairing_code_expired",
                    "pairing code expired; request a new code",
                ),
                PairingCodeError::Invalid => api_error(
                    StatusCode::FORBIDDEN,
                    "invalid_pairing_code",
                    "invalid pairing code",
                ),
            },
        )?;
        (device_name, install_id)
    };

    // Each device gets its own token so it can be revoked without disturbing
    // the others. The admin token in pairing.json is never handed out.
    let token = generate_token();
    let record = DeviceRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: device_name,
        token_hash: hash_token(&token),
        paired_unix_ms: now_unix_ms(),
        last_seen_unix_ms: now_unix_ms(),
        install_id: install_id.clone(),
    };
    {
        let mut devices = lock_devices(&state)?;
        // One record per install: replace an earlier pairing from the same
        // device rather than accumulating duplicates.
        if let Some(install_id) = install_id.as_deref() {
            devices.retain(|device| device.install_id.as_deref() != Some(install_id));
        }
        devices.push(record);
        devices.sort_by_key(|item| item.paired_unix_ms);
        if devices.len() > MAX_DEVICES {
            let excess = devices.len() - MAX_DEVICES;
            devices.drain(..excess);
        }
        write_devices(&devices).map_err(|err| {
            eprintln!("failed to write device tokens: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device_write_failed",
                "failed to save the new device token",
            )
        })?;
    }

    Ok(Json(json!({
        "kind": "herdr-gateway",
        "server_id": state.config.server_id,
        "label": state.config.label,
        "url": state.config.public_url,
        "token": token
    })))
}

fn pair_request_response(config: &Config, request_id: &str) -> Value {
    json!({
        "request_id": request_id,
        "server_id": config.server_id,
        "server_label": config.label,
        "status": "pending",
        "expires_in_ms": PAIRING_CODE_TTL_MS
    })
}

fn consume_pairing_code(
    pending: &mut Option<PendingPairing>,
    request_id: &str,
    code: &str,
    now_unix_ms: u128,
) -> Result<(), PairingCodeError> {
    let Some(current) = pending.as_mut() else {
        return Err(PairingCodeError::Missing);
    };
    if pairing_code_expired(current, now_unix_ms) {
        *pending = None;
        return Err(PairingCodeError::Expired);
    }
    let request_matches = constant_time_eq(request_id.as_bytes(), current.request_id.as_bytes());
    let valid = valid_pairing_code(code)
        && request_matches
        && constant_time_eq(hash_token(code).as_bytes(), current.code_hash.as_bytes());
    if !valid {
        current.failed_attempts = current.failed_attempts.saturating_add(1);
        if current.failed_attempts >= MAX_PAIRING_CODE_ATTEMPTS {
            *pending = None;
        }
        return Err(PairingCodeError::Invalid);
    }
    *pending = None;
    Ok(())
}

fn pairing_code_expired(pending: &PendingPairing, now_unix_ms: u128) -> bool {
    now_unix_ms.saturating_sub(pending.created_unix_ms) >= PAIRING_CODE_TTL_MS
}

async fn pair_pending(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    // Read by the local manage UI, which holds the admin token: a device has no
    // token of its own until it has read this code and claimed the pairing.
    require_admin(&state.config, &headers)?;
    let mut pending = state.pending_pairing.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_lock_failed",
            "failed to lock pending pairing state",
        )
    })?;
    if pending
        .as_ref()
        .is_some_and(|value| pairing_code_expired(value, now_unix_ms()))
    {
        *pending = None;
    }
    if let Some(pending) = pending.as_ref() {
        return Ok(Json(json!({
            "pending": true,
            "request_id": pending.request_id,
            "device_name": pending.device_name,
            "code": pending.code,
            "created_unix_ms": pending.created_unix_ms,
            "expires_unix_ms": pending.created_unix_ms + PAIRING_CODE_TTL_MS,
            "expires_in_ms": (pending.created_unix_ms + PAIRING_CODE_TTL_MS).saturating_sub(now_unix_ms())
        })));
    }
    Ok(Json(json!({ "pending": false })))
}

async fn register_push_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterPushTokenBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_push_token(&body.token)?;
    let platform = body.platform.trim().to_ascii_lowercase();
    if platform != "ios" && platform != "android" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_platform",
            "platform must be ios or android",
        ));
    }

    let mut tokens = state.push_tokens.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_lock_failed",
            "failed to lock push token state",
        )
    })?;
    let record = PushTokenRecord {
        token: body.token,
        platform,
        device_name: body.device_name.filter(|value| !value.trim().is_empty()),
        updated_unix_ms: now_unix_ms(),
    };
    if let Some(existing) = tokens.iter_mut().find(|item| item.token == record.token) {
        *existing = record;
    } else {
        tokens.push(record);
    }
    tokens.sort_by_key(|item| item.updated_unix_ms);
    if tokens.len() > MAX_PUSH_TOKENS {
        let excess = tokens.len() - MAX_PUSH_TOKENS;
        tokens.drain(..excess);
    }
    write_push_tokens(&tokens).map_err(|err| {
        eprintln!("failed to write push tokens: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_write_failed",
            "failed to save push notification registration",
        )
    })?;
    Ok(Json(json!({ "ok": true, "device_count": tokens.len() })))
}

async fn unregister_push_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UnregisterPushTokenBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_push_token(&body.token)?;
    let mut tokens = state.push_tokens.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_lock_failed",
            "failed to lock push token state",
        )
    })?;
    let previous_len = tokens.len();
    tokens.retain(|record| record.token != body.token);
    if tokens.len() != previous_len {
        write_push_tokens(&tokens).map_err(|err| {
            eprintln!("failed to remove push token: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "push_token_write_failed",
                "failed to remove push notification registration",
            )
        })?;
    }
    Ok(Json(json!({
        "ok": true,
        "removed": tokens.len() != previous_len,
        "device_count": tokens.len()
    })))
}

async fn send_test_notification(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SendPushNotificationBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let tokens = state
        .push_tokens
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "push_token_lock_failed",
                "failed to lock push token state",
            )
        })?
        .clone();
    let result = send_expo_push_notifications(
        &tokens,
        body.title.unwrap_or_else(|| "Herdr Gateway".into()),
        body.body
            .unwrap_or_else(|| "Muqun push notifications are connected.".into()),
        body.data.unwrap_or_else(|| {
            let mut data = serde_json::Map::new();
            data.insert("url".into(), json!("/"));
            data.insert("type".into(), json!("gateway.test"));
            data
        }),
    )
    .await
    .map_err(|err| {
        eprintln!("Expo push request failed: {err:#}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "expo_push_failed",
            "Expo push service request failed",
        )
    })?;
    Ok(Json(
        json!({ "ok": true, "device_count": tokens.len(), "expo": result }),
    ))
}

fn status() -> anyhow::Result<()> {
    let config = load_config(None)?;
    println!("server_id: {}", config.server_id);
    println!("label: {}", config.label);
    println!("listen: {}", config.listen);
    println!("public_url: {}", config.public_url);
    for session in config.sessions {
        println!("session {}: {}", session.id, session.socket_path);
    }
    match read_pid()? {
        Some(pid) if process_running(pid) => println!("gateway: running pid {pid}"),
        Some(pid) => println!("gateway: stale pid {pid}"),
        None => println!("gateway: stopped"),
    }
    Ok(())
}

fn manage() -> anyhow::Result<()> {
    let _terminal = TerminalModeGuard::enter()?;
    let mut message = auto_upgrade_local_public_url()?.unwrap_or_else(|| String::from("ready"));
    let mut pending_pairing = fetch_pending_pairing().ok().flatten();
    let mut devices = read_devices().unwrap_or_default();
    // False by default so a finished pairing lands on the device list; `p` flips
    // it on to add another device.
    let mut show_qr = false;
    print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;

    loop {
        if !poll_event(MANAGE_REFRESH_INTERVAL)? {
            let next_pending_pairing = fetch_pending_pairing().ok().flatten();
            let next_devices = read_devices().unwrap_or_default();
            if next_pending_pairing != pending_pairing || next_devices != devices {
                // A newly paired device flips off the QR so the screen settles on
                // the device list instead of showing a fresh code.
                if next_devices.len() > devices.len() {
                    show_qr = false;
                    message = String::from("device paired");
                }
                pending_pairing = next_pending_pairing;
                devices = next_devices;
                print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
            }
            continue;
        }

        let event = read_event()?;
        let TerminalEvent::Key(event) = event else {
            if matches!(event, TerminalEvent::Resize(_, _)) {
                print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
            }
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        let input = match event.code {
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Enter => String::new(),
            KeyCode::Esc => String::from("q"),
            _ => String::new(),
        };
        match input.as_str() {
            "s" | "start" => {
                start_background_inner(false)?;
                message = String::from("start requested");
            }
            "t" | "stop" => {
                stop_background_inner(false)?;
                message = String::from("stop requested");
            }
            "p" | "pair" => {
                show_qr = true;
                message = String::from("scan to pair another device");
            }
            "x" | "revoke" => {
                if devices.is_empty() {
                    message = String::from("no paired devices to revoke");
                } else if let Some(device) = prompt_revoke_device(&devices)? {
                    if revoke_managed_device(&device.id)? {
                        message =
                            format!("revoked {}; scan to pair again", truncate(&device.name, 32));
                        // Revocation usually means the user is replacing this
                        // app's credential. Return straight to the pairing QR
                        // instead of leaving them on the remaining device list.
                        show_qr = true;
                    } else {
                        message = String::from("device was already revoked");
                    }
                } else {
                    message = String::from("revoke cancelled");
                }
            }
            "r" | "refresh" | "" => {
                show_qr = false;
                message = String::from("refreshed");
            }
            "u" | "url" => match prompt_public_url()? {
                Some(url) => {
                    update_public_url(&url)?;
                    message = format!("url updated: {}", truncate(&url, 36));
                }
                None => {
                    message = String::from("url unchanged");
                }
            },
            "a" | "auto" => {
                let selection = auto_public_url(configured_port());
                update_public_url(&selection.url)?;
                message = format!("auto url: {}", truncate(&selection.url, 36));
            }
            "q" | "quit" => break,
            other => message = format!("unknown command: {other}"),
        }

        pending_pairing = fetch_pending_pairing().ok().flatten();
        devices = read_devices().unwrap_or_default();
        print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
    }
    Ok(())
}

fn prompt_public_url() -> anyhow::Result<Option<String>> {
    let current = load_config(None)
        .map(|config| config.public_url)
        .unwrap_or_else(|_| auto_public_url(configured_port()).url);
    let mut value = current;
    loop {
        render_public_url_prompt(&value)?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter => {
                    if let Ok(url) = validate_public_url(&value) {
                        return Ok(Some(url));
                    }
                    value = String::from("http://");
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(ch) if !ch.is_control() => value.push(ch),
                _ => {}
            }
        }
    }
}

fn render_public_url_prompt(value: &str) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let lines = vec![
        String::from("Gateway URL"),
        String::from(""),
        String::from("Edit the URL encoded into the pairing QR."),
        String::from("Use a Tailscale HTTPS name if Tailscale Serve is configured."),
        format!("Otherwise use http://<tailscale-ip>:{DEFAULT_PORT}."),
        String::from(""),
        format!("url: {value}"),
        String::from(""),
        String::from("Enter saves | Esc cancels | Backspace deletes"),
    ];
    write_centered_panel(&lines)
}

fn prompt_revoke_device(devices: &[DeviceRecord]) -> anyhow::Result<Option<DeviceRecord>> {
    let choices = devices.iter().rev().cloned().collect::<Vec<_>>();
    if choices.is_empty() {
        return Ok(None);
    }
    let mut selected = 0_usize;

    loop {
        render_revoke_device_picker(&choices, selected)?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            KeyCode::Enter => {
                let device = &choices[selected];
                if confirm_revoke_device(device)? {
                    return Ok(Some(device.clone()));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

fn render_revoke_device_picker(devices: &[DeviceRecord], selected: usize) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let mut lines = vec![
        String::from("Revoke a paired device"),
        String::from(""),
        String::from("Up/Down or j/k selects | Enter continues | Esc cancels"),
        String::from(""),
    ];
    for (index, device) in devices.iter().enumerate() {
        lines.push(format!(
            "{} {}   paired {}",
            if index == selected { ">" } else { " " },
            truncate(&device.name, 42),
            relative_since(device.paired_unix_ms)
        ));
    }
    write_centered_panel(&lines)
}

fn confirm_revoke_device(device: &DeviceRecord) -> anyhow::Result<bool> {
    loop {
        execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        let lines = vec![
            String::from("Revoke device?"),
            String::from(""),
            truncate(&device.name, 52),
            String::from("Its access token will stop working immediately."),
            String::from(""),
            String::from("y revoke | n or Esc cancel"),
        ];
        write_centered_panel(&lines)?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

fn update_public_url(public_url: &str) -> anyhow::Result<()> {
    let public_url = validate_public_url(public_url)?;
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.public_url = public_url.clone();
    write_secret_file(&config_path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    let pairing_path = config_dir()?.join(PAIRING_FILE);
    let mut pairing = read_pairing_file()?;
    pairing.payload.url = public_url;
    write_secret_file(&pairing_path, &serde_json::to_vec_pretty(&pairing)?)?;
    Ok(())
}

fn auto_upgrade_local_public_url() -> anyhow::Result<Option<String>> {
    let Ok(config) = load_config(None) else {
        return Ok(None);
    };
    if !is_local_public_url(&config.public_url) {
        return Ok(None);
    }
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let selection = auto_public_url(listen.port());
    if is_local_public_url(&selection.url) || selection.url == config.public_url {
        return Ok(None);
    }
    update_public_url(&selection.url)?;
    Ok(Some(format!("auto url: {}", truncate(&selection.url, 36))))
}

fn is_local_public_url(url: &str) -> bool {
    url.contains("127.0.0.1") || url.contains("localhost")
}

struct TerminalModeGuard;

impl TerminalModeGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn print_manage_screen(
    message: &str,
    pending_pairing: Option<&PendingPairing>,
    devices: &[DeviceRecord],
    show_qr: bool,
) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let config = load_config(None).ok();
    let server = config
        .as_ref()
        .map(|config| truncate(&config.label, 24))
        .unwrap_or_else(|| "not configured".into());
    let url = config
        .as_ref()
        .map(|config| truncate(&config.public_url, 44))
        .unwrap_or_else(|| "run setup first".into());
    let status = match read_pid()? {
        Some(pid) if process_running(pid) => format!("running ({pid})"),
        Some(_) => String::from("stale pid"),
        None => String::from("stopped"),
    };

    let mut lines = vec![
        String::from("Herdr Gateway for Muqun"),
        String::from(""),
        String::from("keys   : [s] start  [t] stop  [p] pair  [x] revoke"),
        String::from("         [u] url    [a] auto  [r] refresh  [q] close"),
        format!("server : {server}"),
        format!("url    : {url}"),
        format!("status : {status}"),
        format!("message: {}", truncate(message, 58)),
        String::from(""),
    ];

    // A device mid-pairing takes priority: show its name + the code to enter.
    if let Some(pending) = pending_pairing {
        lines.extend([
            String::from("Pairing request"),
            format!("device : {}", truncate(&pending.device_name, 48)),
            format!("code   : {}", pending.code),
            String::from(""),
            String::from("Enter this code in Muqun to finish pairing."),
        ]);
        write_centered_panel(&lines)?;
        return Ok(());
    }

    // Once at least one device is paired, the QR is not the default view -- a
    // finished pairing should land on the device list, not another QR. `p` (or a
    // fresh install with nothing paired yet) brings the QR back to add another.
    let show_qr = show_qr || devices.is_empty();

    if !show_qr {
        lines.push(format!("Paired devices ({})", devices.len()));
        lines.push(String::from(""));
        for device in devices.iter().rev() {
            lines.push(format!(
                "  {}   paired {}",
                truncate(&device.name, 40),
                relative_since(device.paired_unix_ms)
            ));
        }
        lines.push(String::from(""));
        lines.push(String::from(
            "Press p to pair another device, or x to revoke one.",
        ));
        write_centered_panel(&lines)?;
        return Ok(());
    }

    if let (Some(config), Ok(pairing)) = (config.as_ref(), read_pairing_file()) {
        if hash_token(&pairing.payload.token) != config.token_hash {
            lines.push(String::from("Pairing identity is stale. Run setup again."));
            write_centered_panel(&lines)?;
            return Ok(());
        }
        let qr_controls = vec![
            String::from("Muqun Gateway"),
            String::from(""),
            String::from("[s] start   [t] stop"),
            String::from("[p] pair    [x] revoke"),
            String::from("[u] URL     [a] auto URL"),
            String::from("[r] devices / refresh"),
            String::from("[q/Esc] close"),
            String::from(""),
            format!("server: {}", truncate(&server, 18)),
            format!("url: {}", truncate(&url, 21)),
            format!("status: {}", truncate(&status, 18)),
            format!("message: {}", truncate(message, 17)),
        ];
        let mut qr_lines = vec![
            String::from("Scan with Muqun"),
            String::from("Code appears after scan"),
            String::from(""),
        ];
        // Config is authoritative for the advertised URL and server id. Older
        // pairing files can retain a stale URL even though their admin token is
        // still valid; rendering from that file made `p` show the wrong server.
        let encoded = pairing_qr_offer(&config.public_url, &config.server_id);
        let code = QrCode::with_error_correction_level(encoded.as_bytes(), EcLevel::L)?;
        let image = render_qr(&code);
        for line in image.lines() {
            qr_lines.push(line.to_string());
        }
        write_two_column_panel(&qr_controls, &qr_lines)?;
        return Ok(());
    } else {
        lines.push(String::from(
            "Gateway pairing is not configured. Run setup first.",
        ));
    }
    write_centered_panel(&lines)?;
    Ok(())
}

/// A compact "3m ago" / "2h ago" / "5d ago" for the manage device list. Falls
/// back to "just now" for anything under a minute and "recently" if the clock
/// looks off (a future timestamp).
fn relative_since(then_unix_ms: u128) -> String {
    let now = now_unix_ms();
    if then_unix_ms > now {
        return String::from("recently");
    }
    let secs = (now - then_unix_ms) / 1000;
    if secs < 60 {
        String::from("just now")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn push_line(output: &mut String, line: impl AsRef<str>) {
    output.push_str(line.as_ref());
    output.push_str("\r\n");
}

fn write_centered_panel(lines: &[String]) -> anyhow::Result<()> {
    let terminal_width = terminal_size()
        .map(|(width, _)| width as usize)
        .unwrap_or(110);
    let content_width = lines
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0)
        .max(56)
        .min(terminal_width.saturating_sub(4));
    let indent = terminal_width.saturating_sub(content_width) / 2;
    // Popup interiors can be a few rows shorter than the child PTY reports.
    // Keep content anchored at the top so a long QR never scrolls the controls
    // out of view on smaller laptop terminals.
    let mut output = String::new();

    for line in lines {
        let line_width = display_width(line);
        let left_padding = if line.contains(':') || line.starts_with("> ") || line.starts_with("  ")
        {
            0
        } else {
            content_width.saturating_sub(line_width) / 2
        };
        push_line(
            &mut output,
            format!("{}{}{}", " ".repeat(indent + left_padding), line, "\x1b[0m"),
        );
    }

    stdout().write_all(output.as_bytes())?;
    stdout().flush()?;
    Ok(())
}

fn write_two_column_panel(left: &[String], right: &[String]) -> anyhow::Result<()> {
    let terminal_width = terminal_size()
        .map(|(width, _)| width as usize)
        .unwrap_or(92);
    let left_width = left
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0);
    let right_width = right
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0);
    let gap = if left_width + right_width + 4 <= terminal_width {
        4
    } else {
        1
    };
    let total_width = left_width + gap + right_width;

    // Extremely narrow terminals cannot preserve QR geometry beside controls.
    // Keep the controls visible first, then render the code below as a fallback.
    if total_width > terminal_width {
        let mut stacked = left.to_vec();
        stacked.push(String::new());
        stacked.extend_from_slice(right);
        return write_centered_panel(&stacked);
    }

    let indent = terminal_width.saturating_sub(total_width) / 2;
    let row_count = left.len().max(right.len());
    let mut output = String::new();
    for row in 0..row_count {
        let left_line = left.get(row).map(String::as_str).unwrap_or("");
        let right_line = right.get(row).map(String::as_str).unwrap_or("");
        let left_padding = left_width.saturating_sub(display_width(left_line));
        push_line(
            &mut output,
            format!(
                "{}{}{}{}{}\x1b[0m",
                " ".repeat(indent),
                left_line,
                " ".repeat(left_padding),
                " ".repeat(gap),
                right_line
            ),
        );
    }
    stdout().write_all(output.as_bytes())?;
    stdout().flush()?;
    Ok(())
}

fn display_width(value: &str) -> usize {
    let mut chars = value.chars().peekable();
    let mut width = 0;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for control in chars.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

fn fetch_pending_pairing() -> anyhow::Result<Option<PendingPairing>> {
    let pairing = read_pairing_file()?;
    let config = load_config(None)?;
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let host_port = local_management_addr(listen).to_string();
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    let request = format!(
        "GET /api/pair/pending HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        pairing.payload.token
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response)?;
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        anyhow::bail!("invalid pending response");
    };
    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(body)?;
    if value.get("pending").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    Ok(Some(PendingPairing {
        request_id: value
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        device_name: value
            .get("device_name")
            .and_then(Value::as_str)
            .unwrap_or("Muqun app")
            .into(),
        install_id: value
            .get("install_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        code: value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        code_hash: String::new(),
        created_unix_ms: value
            .get("created_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .unwrap_or_default(),
        failed_attempts: 0,
    }))
}

fn revoke_managed_device(device_id: &str) -> anyhow::Result<bool> {
    let pairing = read_pairing_file()?;
    let config = load_config(None)?;
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let address = local_management_addr(listen);
    let mut stream = match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) =>
        {
            // With no running gateway there is no in-memory token list to
            // invalidate, so updating the persisted records is sufficient.
            return revoke_device_by_id(device_id);
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = format!(
        "DELETE /api/pairings/{device_id} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        pairing.payload.token
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response)?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.contains(" 200 ") {
        return Ok(true);
    }
    if status_line.contains(" 404 ") {
        return Ok(false);
    }
    anyhow::bail!("gateway refused device revocation: {status_line}")
}

fn local_management_addr(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        if listen.is_ipv6() {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], listen.port()))
        } else {
            SocketAddr::from(([127, 0, 0, 1], listen.port()))
        }
    } else {
        listen
    }
}

fn validate_public_url(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    let parsed = reqwest::Url::parse(value).context("invalid gateway URL")?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "gateway URL must use http:// or https://"
    );
    anyhow::ensure!(parsed.host().is_some(), "gateway URL must include a host");
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "gateway URL cannot contain credentials"
    );
    anyhow::ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "gateway URL cannot contain a query or fragment"
    );
    Ok(value.trim_end_matches('/').to_string())
}

fn pairing_qr_offer(url: &str, server_id: &str) -> String {
    format!(
        "muqun://pair?u={}&s={}",
        url_component(url),
        url_component(server_id)
    )
}

fn url_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn render_qr(code: &QrCode) -> String {
    let image = code.render::<unicode::Dense1x2>().quiet_zone(true).build();
    // Force the standard dark-on-light polarity. Relying on the terminal's
    // foreground/background colors can invert the code under some Herdr themes
    // and native camera scanners do not consistently recover from that.
    image
        .lines()
        .map(|line| format!("\x1b[30;47m{line}\x1b[0m"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(gateway_metadata(&state).await?))
}

async fn api_meta(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(gateway_metadata(&state).await?))
}

#[derive(Deserialize)]
struct SetLabelBody {
    label: String,
}

/// Set the server's display label from a paired device -- the app calls this
/// when the user renames the server, so push notifications carry that name
/// instead of the hostname default.
async fn api_set_label(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetLabelBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let label = update_server_label(&body.label)
        .map_err(|err| api_error(StatusCode::BAD_REQUEST, "invalid_label", &err.to_string()))?;
    Ok(Json(json!({ "label": label })))
}

async fn gateway_metadata(state: &AppState) -> ApiResult<Value> {
    let session = find_session(&state.config, "default")?;
    let herdr = match herdr_request(session, "ping", json!({})).await {
        Ok(value) => {
            let version = value.pointer("/result/version").and_then(Value::as_str);
            let protocol = value.pointer("/result/protocol").and_then(Value::as_u64);
            let compatible = protocol
                .is_some_and(|value| (HERDR_PROTOCOL_MIN..=HERDR_PROTOCOL_MAX).contains(&value));
            json!({
                "connected": true,
                "version": version,
                "protocol": protocol,
                "compatible": compatible,
                "supportedProtocolMin": HERDR_PROTOCOL_MIN,
                "supportedProtocolMax": HERDR_PROTOCOL_MAX,
                "response": value
            })
        }
        Err(err) => {
            eprintln!("Herdr metadata request failed: {err:#}");
            json!({ "connected": false, "error": "Herdr is unavailable" })
        }
    };
    Ok(json!({
        "ok": true,
        "gatewayVersion": env!("CARGO_PKG_VERSION"),
        "apiVersion": GATEWAY_API_VERSION,
        "apiMajor": GATEWAY_API_MAJOR,
        "minimumCompatibleApiVersion": "1.0.0",
        "legacyUnversionedApi": true,
        "capabilities": API_CAPABILITIES,
        "serverId": state.config.server_id,
        "label": state.config.label,
        "herdr": herdr
    }))
}

async fn sessions(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(json!({ "sessions": state.config.sessions })))
}

async fn snapshot(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "session.snapshot", json!({})).await
}

async fn workspaces(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "workspace.list", json!({})).await
}

async fn panes(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "pane.list", json!({})).await
}

async fn agents(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "agent.list", json!({})).await
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Comma-separated allow-list of event names, e.g. `pane_updated,pane_closed`.
    /// A client that only reacts to output changes should not be woken for the
    /// focus and layout churn that dominates the raw stream -- measured at ~20
    /// events per second on a busy session, almost none of it actionable on a
    /// phone. Absent means forward everything, so an old client is unaffected.
    #[serde(default)]
    types: Option<String>,
    /// When set, `pane.updated` events for THIS pane are enriched with the
    /// pane's current output inline, so the client paints immediately instead of
    /// firing a second read round-trip per update. Other panes' events, and all
    /// other event types, pass through unchanged. Absent means no enrichment, so
    /// an old client still works via its own reads.
    #[serde(default)]
    stream_pane: Option<String>,
    #[serde(default)]
    stream_lines: Option<u32>,
    #[serde(default)]
    stream_source: Option<String>,
    #[serde(default)]
    stream_format: Option<String>,
}

/// Resolved output-streaming settings for one events subscription.
struct StreamOutputOpts {
    pane: Option<String>,
    lines: u32,
    source: String,
    format: String,
}

struct StreamPaneFrame {
    revision: u64,
    output: String,
}

/// Pulls the terminal text out of a Herdr `pane.read` response, tolerating both
/// the bare-string and `{ text: ... }` result shapes across Herdr versions.
fn pane_read_text(value: &Value) -> Option<String> {
    // Herdr nests the text under `result.read.text`; tolerate `result.text` and a
    // bare-string `result` too across versions. Missing all three means no inline
    // output and the client falls back to its own read.
    for ptr in ["/result/read/text", "/result/text"] {
        if let Some(text) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    value
        .pointer("/result")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn stream_pane_frame(value: &Value) -> Option<StreamPaneFrame> {
    let revision = ["/result/read/revision", "/result/revision"]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))?;
    let output = pane_read_text(value)?;
    Some(StreamPaneFrame { revision, output })
}

/// Convert one sampled frame into the enriched `pane_updated` payload consumed
/// by Muqun. Output content, rather than revision, is used to decide whether to
/// emit because Herdr can expose new text before its coalesced revision advances.
fn stream_pane_update_payload(frame: &StreamPaneFrame, pane_id: &str) -> Option<String> {
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

async fn poll_stream_pane_update(
    session: &SessionConfig,
    opts: &StreamOutputOpts,
) -> Option<StreamPaneFrame> {
    let pane = opts.pane.as_deref()?;
    let read = tokio::time::timeout(
        STREAM_OUTPUT_READ_TIMEOUT,
        herdr_request(
            session,
            "pane.read",
            json!({
                "pane_id": pane,
                "source": opts.source,
                "lines": opts.lines,
                "format": opts.format,
            }),
        ),
    )
    .await
    .ok()?
    .ok()?;
    stream_pane_frame(&read)
}

/// If `line` is a `pane.updated` for the streamed pane, read that pane's output
/// and fold it into the event as `data.output`. Returns `None` to forward the
/// line untouched (wrong pane, wrong event, or a read failure -- the client
/// still has its revision and can fall back to a read).
async fn enrich_pane_update(
    line: &str,
    session: &SessionConfig,
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
        herdr_request(
            session,
            "pane.read",
            json!({
                "pane_id": pane,
                "source": opts.source,
                "lines": opts.lines,
                "format": opts.format,
            }),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let text = pane_read_text(&read)?;
    value
        .get_mut("data")
        .and_then(Value::as_object_mut)?
        .insert("output".into(), Value::String(text));
    serde_json::to_string(&value).ok()
}

/// Normalises a filter token to the underscore form Herdr tags events with, so
/// a client may ask for either `pane.updated` or `pane_updated`.
fn normalize_event_name(value: &str) -> String {
    value.trim().replace('.', "_")
}

/// The `event` field of a forwarded Herdr line, used to decide whether a
/// subscribed client asked for it.
fn herdr_event_name(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("event")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

async fn events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<Value>),
> {
    require_device(&state, &headers)?;
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
    let stream = async_stream::stream! {
        match herdr_event_stream(&session).await {
            Ok(mut reader) => {
                let mut line = Vec::new();
                let mut output_interval = tokio::time::interval(STREAM_OUTPUT_POLL_INTERVAL);
                output_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_stream_output: Option<String> = None;
                loop {
                    line.clear();
                    tokio::select! {
                        read = reader.read_until(b'\n', &mut line) => match read {
                            Ok(0) => break,
                            Ok(_) => {
                            let data = String::from_utf8_lossy(&line);
                            let data = data.trim();
                            if !data.is_empty() {
                                // Filter at the point of forwarding rather than
                                // by unsubscribing: a subscribed event a client
                                // did not ask for costs nothing here, but waking
                                // the phone for it costs battery.
                                let keep = match &wanted {
                                    Some(set) => herdr_event_name(data)
                                        .map(|name| set.contains(&name))
                                        .unwrap_or(true),
                                    None => true,
                                };
                                if keep {
                                    // Fold the viewed pane's output into its
                                    // update so the client paints on arrival,
                                    // with no follow-up read hop. Everything
                                    // else forwards untouched.
                                    let payload = if stream_opts.pane.is_some() {
                                        enrich_pane_update(data, &session, &stream_opts)
                                            .await
                                            .unwrap_or_else(|| data.to_owned())
                                    } else {
                                        data.to_owned()
                                    };
                                    yield Ok(Event::default().event("herdr").data(payload));
                                }
                            }
                            }
                            Err(err) => {
                            eprintln!("Herdr event stream read failed: {err:#}");
                            yield Ok(Event::default().event("gateway.error").data("Herdr event stream unavailable"));
                            break;
                            }
                        },
                        _ = output_interval.tick(), if stream_opts.pane.is_some() => {
                            if let Some(frame) = poll_stream_pane_update(&session, &stream_opts).await {
                                if last_stream_output.as_deref() != Some(frame.output.as_str()) {
                                    last_stream_output = Some(frame.output.clone());
                                    if let Some(pane_id) = stream_opts.pane.as_deref() {
                                        if let Some(payload) = stream_pane_update_payload(&frame, pane_id) {
                                            yield Ok(Event::default().event("herdr").data(payload));
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
            }
            Err(err) => {
                eprintln!("Herdr event stream connection failed: {err:#}");
                yield Ok(Event::default().event("gateway.error").data("Herdr event stream unavailable"));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn spawn_agent_notification_watchers(state: AppState) {
    for session in state.config.sessions.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            watch_agent_notifications(state, session).await;
        });
    }
}

async fn watch_agent_notifications(state: AppState, session: SessionConfig) {
    let mut statuses = seed_agent_statuses(&session).await;

    loop {
        for notification in poll_agent_notifications(
            &session,
            &mut statuses,
            &state.config.server_id,
            &current_server_label(&state.config.label),
            &session.id,
        )
        .await
        {
            deliver_agent_notification(&state, notification).await;
        }

        match herdr_agent_event_stream(&session).await {
            Ok(mut reader) => {
                let mut line = String::new();
                loop {
                    line.clear();
                    match tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
                        .await
                    {
                        Err(_) | Ok(Ok(0)) => break,
                        Ok(Ok(_)) => {
                            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                                continue;
                            };
                            if let Some(notification) = notification_for_agent_status_event(
                                &event,
                                &mut statuses,
                                &state.config.server_id,
                                &current_server_label(&state.config.label),
                                &session.id,
                            ) {
                                deliver_agent_notification(&state, notification).await;
                            }
                        }
                        Ok(Err(err)) => {
                            eprintln!(
                                "Herdr event stream failed for session {}: {err}",
                                session.id
                            );
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "Herdr event subscription failed for session {}: {err:#}",
                    session.id
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn poll_agent_notifications(
    session: &SessionConfig,
    statuses: &mut HashMap<String, String>,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Vec<AgentPushNotification> {
    let Ok(value) = herdr_request(session, "agent.list", json!({})).await else {
        return Vec::new();
    };
    value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            notification_for_agent_status_event(
                &json!({
                    "event": "pane.agent_status_changed",
                    "data": agent
                }),
                statuses,
                server_id,
                server_label,
                session_id,
            )
        })
        .collect()
}

async fn deliver_agent_notification(state: &AppState, notification: AgentPushNotification) {
    let tokens = match state.push_tokens.lock() {
        Ok(tokens) => tokens.clone(),
        Err(_) => {
            eprintln!("agent notification skipped: push token lock failed");
            return;
        }
    };
    if let Err(err) = send_expo_push_notifications(
        &tokens,
        notification.title,
        notification.body,
        notification.data,
    )
    .await
    {
        eprintln!("agent notification failed: {err:#}");
    }
}

async fn seed_agent_statuses(session: &SessionConfig) -> HashMap<String, String> {
    let Ok(value) = herdr_request(session, "agent.list", json!({})).await else {
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

fn notification_for_agent_status_event(
    event: &Value,
    statuses: &mut HashMap<String, String>,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Option<AgentPushNotification> {
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

    let (event_type, status_label, message) = match (status.as_str(), previous.as_deref()) {
        ("blocked", _) => ("agent.blocked", "Agent blocked", "needs your input."),
        ("idle" | "done" | "completed", Some("working")) => {
            ("agent.completed", "Agent done", "finished running.")
        }
        _ => return None,
    };
    let agent_name = ["display_agent", "agent", "title"]
        .into_iter()
        .find_map(|key| data.get(key).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Agent");
    // Title carries which server so a multi-server user knows where to look;
    // body carries which agent and what happened. Only the server label (which
    // the user set) and the agent name -- never terminal output or prompts.
    let server = server_label.trim();
    let title = if server.is_empty() {
        status_label.to_string()
    } else {
        format!("{status_label} · {}", truncate(server, 32))
    };
    let mut notification_data = serde_json::Map::new();
    notification_data.insert("type".into(), json!(event_type));
    notification_data.insert("url".into(), json!(format!("/servers/{server_id}")));
    notification_data.insert("server_id".into(), json!(server_id));
    notification_data.insert("session_id".into(), json!(session_id));
    notification_data.insert("pane_id".into(), json!(pane_id));

    Some(AgentPushNotification {
        title,
        body: format!("{} {message}", agent_name.trim()),
        data: notification_data,
    })
}

async fn create_workspace(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mut params = serde_json::Map::new();
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "label", body.label);
    insert_opt(&mut params, "focus", body.focus);
    call_session_method(
        &state.config,
        &session_id,
        "workspace.create",
        Value::Object(params),
    )
    .await
}

async fn focus_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.focus",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

async fn rename_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.rename",
        json!({ "workspace_id": workspace_id, "label": body.label }),
    )
    .await
}

async fn close_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.close",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

async fn tabs(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "tab.list", json!({})).await
}

async fn create_tab(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mut params = serde_json::Map::new();
    insert_opt(&mut params, "workspace_id", body.workspace_id);
    insert_opt(&mut params, "label", body.label);
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "focus", body.focus);
    call_session_method(
        &state.config,
        &session_id,
        "tab.create",
        Value::Object(params),
    )
    .await
}

async fn focus_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.focus",
        json!({ "tab_id": tab_id }),
    )
    .await
}

async fn rename_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.rename",
        json!({ "tab_id": tab_id, "label": body.label }),
    )
    .await
}

async fn close_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.close",
        json!({ "tab_id": tab_id }),
    )
    .await
}

async fn pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.get",
        json!({ "pane_id": pane_id }),
    )
    .await
}

async fn focus_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.focus",
        json!({ "pane_id": pane_id }),
    )
    .await
}

async fn rename_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenamePaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.rename",
        json!({ "pane_id": pane_id, "label": body.label }),
    )
    .await
}

async fn close_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.close",
        json!({ "pane_id": pane_id }),
    )
    .await
}

async fn split_pane(
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
    let command = body
        .command
        .map(|parts| parts.join(" "))
        .filter(|text| !text.trim().is_empty());
    if let Some(text) = command.as_deref() {
        validate_text(text)?;
    }
    let mut params = serde_json::Map::new();
    params.insert("target_pane_id".into(), json!(pane_id));
    params.insert("direction".into(), json!(body.direction));
    insert_opt(&mut params, "ratio", body.ratio);
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "env", body.env);
    let result = call_session_method(
        &state.config,
        &session_id,
        "pane.split",
        Value::Object(params),
    )
    .await?;
    if let Some(text) = command {
        let created_pane_id = created_pane_id(&result).ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "invalid_split_response",
                "Herdr did not return the created pane id",
            )
        })?;
        let _ = call_session_method(
            &state.config,
            &session_id,
            "pane.send_text",
            json!({ "pane_id": created_pane_id, "text": text }),
        )
        .await?;
        let _ = call_session_method(
            &state.config,
            &session_id,
            "pane.send_keys",
            json!({ "pane_id": created_pane_id, "keys": ["Enter"] }),
        )
        .await?;
    }
    Ok(result)
}

fn created_pane_id(value: &Value) -> Option<&str> {
    value
        .pointer("/result/pane/pane_id")
        .and_then(Value::as_str)
}

async fn zoom_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
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
    call_session_method(
        &state.config,
        &session_id,
        "pane.zoom",
        json!({ "pane_id": pane_id, "mode": mode }),
    )
    .await
}

async fn agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "agent.get",
        json!({ "target": target }),
    )
    .await
}

async fn focus_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "agent.focus",
        json!({ "target": target }),
    )
    .await
}

async fn send_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AgentSendBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    let result = call_session_method(
        &state.config,
        &session_id,
        "agent.prompt",
        json!({ "target": target, "text": body.text }),
    )
    .await?;
    // Belt and braces for a paste-vs-keypress race in agent TUIs: when the
    // prompt text and its newline arrive in one PTY write, Claude Code's input
    // treats the newline as pasted content and leaves the prompt sitting in its
    // input box unsubmitted (reproduced on camera, muqun card #571). A short
    // beat later, a separate Enter keystroke submits it. When the prompt DID
    // submit, the input box is empty and an Enter there is a no-op, so this is
    // idempotent. Best-effort by design: a pane that vanished between the two
    // calls must not turn a delivered prompt into an error. The target of the
    // agent send is the pane id, which is what the key press needs.
    let config = state.config.clone();
    let pane_id = target.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = call_session_method(
            &config,
            &session_id,
            "pane.send_keys",
            json!({ "pane_id": pane_id, "keys": ["Enter"] }),
        )
        .await;
    });
    Ok(result)
}

async fn pane_output(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<OutputQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
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
    let params = json!({
        "pane_id": pane_id,
        "source": herdr_source,
        "lines": lines,
        "format": format
    });
    call_session_method(&state.config, &session_id, "pane.read", params).await
}

/// Which agents have a key row and command list, and where to add one. Lets a
/// client tell "this agent has no profile yet" from "the gateway is old".
async fn keymaps(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(shortcuts::catalog()))
}

/// The key row and slash commands for whatever this pane is running.
///
/// Resolving this here rather than in the client means a client picks up a new
/// agent when the developer updates the gateway, without shipping a new build.
async fn pane_shortcuts(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    let pane = herdr_request(&session, "pane.get", json!({ "pane_id": pane_id }))
        .await
        .map_err(|err| {
            eprintln!("Herdr request pane.get failed: {err:#}");
            api_error(
                StatusCode::BAD_GATEWAY,
                "herdr_unavailable",
                "Herdr is unavailable",
            )
        })?;
    let pane = pane.pointer("/result/pane").unwrap_or(&pane);

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

    Ok(Json(shortcuts::resolve(agent, title, cwd)))
}

async fn send_text(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendTextBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": body.text }),
    )
    .await
}

async fn send_keys(
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
    call_session_method(
        &state.config,
        &session_id,
        "pane.send_keys",
        json!({ "pane_id": pane_id, "keys": body.keys }),
    )
    .await
}

/// A file type the gateway is willing to store, with the extension and MIME
/// type derived from the bytes themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadKind {
    extension: &'static str,
    mime: &'static str,
}

/// Accept an image from the phone, park it in the gateway's own upload
/// directory and hand back a local path. The app then sends that path to an
/// agent as ordinary text, so the agent reads the file straight off this
/// machine.
///
/// This first version takes images only. The stored name is generated here and
/// the extension comes from the sniffed content, so nothing the client sends
/// reaches the filesystem.
async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    // Taking the rejection by hand keeps authorization ahead of body parsing,
    // and keeps every failure in the same JSON error shape as the other routes.
    require_device(&state, &headers)?;
    let mut multipart = multipart.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "expected a multipart/form-data body with a file field",
        )
    })?;

    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(upload_body_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let client_name = field.file_name().map(str::to_owned);
        let bytes = field.bytes().await.map_err(upload_body_error)?;
        upload = Some((client_name, bytes));
        break;
    }

    let Some((client_name, bytes)) = upload else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_file",
            "expected a multipart/form-data body with a file field",
        ));
    };
    let Some(client_name) = client_name else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_filename",
            "the file field must carry a filename",
        ));
    };
    if bytes.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_file",
            "the file field is empty",
        ));
    }

    // Executables are checked separately from the image allow-list so the
    // refusal says what it means, and so the rule still holds if the allow-list
    // ever grows a format that a script could hide inside.
    if looks_executable(&bytes) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "executable_rejected",
            "executables and scripts are not accepted",
        ));
    }
    let Some(kind) = sniff_upload_kind(&bytes) else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_file_type",
            "only png, jpeg, gif, webp, and heic images are accepted",
        ));
    };

    let dir = ensure_uploads_dir().map_err(|err| {
        eprintln!("failed to prepare the upload directory: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;
    let path = dir.join(stored_upload_name(kind));
    write_upload_file(&path, &bytes).map_err(|err| {
        eprintln!("failed to write upload {}: {err:#}", path.display());
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "name": sanitize_upload_name(&client_name),
        "size": bytes.len(),
        "mime": kind.mime
    })))
}

/// The body limit is enforced by the framework while the body streams, so an
/// oversized upload surfaces here as a length-limit error rather than as a
/// fully buffered file the gateway then has to measure.
fn upload_body_error(err: MultipartError) -> (StatusCode, Json<Value>) {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload_too_large",
            "the upload must be at most 25 MiB",
        );
    }
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_multipart",
        "expected a multipart/form-data body with a file field",
    )
}

/// Reject anything the host could be talked into running, whatever the file is
/// called. Extensions are not consulted: only the leading bytes are.
fn looks_executable(bytes: &[u8]) -> bool {
    /// Mach-O thin and fat binaries, in both byte orders. `cafebabe` also
    /// covers Java class files, which is no loss here.
    const MACH_O_MAGICS: [[u8; 4]; 6] = [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];

    bytes.starts_with(b"#!")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || MACH_O_MAGICS
            .iter()
            .any(|magic| bytes.starts_with(magic.as_slice()))
}

/// Decide what a file is from its content. Only images are accepted, and the
/// client's extension is never consulted, so a `.png` holding anything else is
/// refused however it was named.
fn sniff_upload_kind(bytes: &[u8]) -> Option<UploadKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(UploadKind {
            extension: "png",
            mime: "image/png",
        });
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(UploadKind {
            extension: "jpg",
            mime: "image/jpeg",
        });
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(UploadKind {
            extension: "gif",
            mime: "image/gif",
        });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(UploadKind {
            extension: "webp",
            mime: "image/webp",
        });
    }
    if is_heic(bytes) {
        return Some(UploadKind {
            extension: "heic",
            mime: "image/heic",
        });
    }
    None
}

/// HEIC is an ISO base media file: a `ftyp` box whose brand names the flavour.
fn is_heic(bytes: &[u8]) -> bool {
    const HEIC_BRANDS: [&[u8; 4]; 10] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    ];

    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && HEIC_BRANDS.iter().any(|brand| &bytes[8..12] == *brand)
}

/// The name on disk is generated here in full: a random stem plus the
/// extension the content earned. A client name can therefore never traverse,
/// collide, or smuggle in a second extension.
fn stored_upload_name(kind: UploadKind) -> String {
    format!("{}.{}", uuid::Uuid::new_v4(), kind.extension)
}

/// Reduce the client's file name to something safe to show. It is echoed back
/// so the app can label the attachment; it never touches the filesystem.
fn sanitize_upload_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .replace(|c: char| c.is_control(), "");
    let base = base.trim();
    if base.is_empty() || base.chars().all(|c| c == '.') {
        return String::from("upload");
    }
    base.chars().take(MAX_UPLOAD_NAME_CHARS).collect()
}

fn uploads_dir() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join(UPLOADS_DIR))
}

/// Uploads live beside the gateway's other state, in their own directory so a
/// sweep can never reach a token file.
fn ensure_uploads_dir() -> anyhow::Result<PathBuf> {
    let dir = uploads_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create upload dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to lock down upload dir {}", dir.display()))?;
    }
    Ok(dir)
}

fn write_upload_file(path: &FsPath, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, bytes)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Sweep old uploads at startup and every hour after that. A phone that
/// uploads a screenshot and loses interest should not leave it on the host.
fn spawn_upload_gc() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPLOAD_GC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick completes immediately, which is the startup sweep.
            ticker.tick().await;
            let Ok(dir) = uploads_dir() else {
                continue;
            };
            if let Err(err) = purge_expired_uploads(&dir, SystemTime::now()) {
                eprintln!("failed to sweep old uploads: {err:#}");
            }
        }
    });
}

/// Delete every stored upload older than the retention window. Returns how many
/// files were removed.
fn purge_expired_uploads(dir: &FsPath, now: SystemTime) -> anyhow::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read upload dir {}", dir.display()))?
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if !upload_expired(modified, now) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) => eprintln!("failed to remove {}: {err}", entry.path().display()),
        }
    }
    Ok(removed)
}

/// A file whose timestamp sits in the future is left alone rather than deleted:
/// a clock change should not destroy an upload the user just made.
fn upload_expired(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age >= UPLOAD_RETENTION)
        .unwrap_or(false)
}

async fn call_session_method(
    config: &Config,
    session_id: &str,
    method: &str,
    params: Value,
) -> ApiResult<Json<Value>> {
    let session = find_session(config, session_id)?;
    let value = herdr_request(session, method, params)
        .await
        .map_err(|err| {
            eprintln!("Herdr request {method} failed: {err:#}");
            api_error(
                StatusCode::BAD_GATEWAY,
                "herdr_unavailable",
                "Herdr is unavailable",
            )
        })?;
    Ok(Json(value))
}

async fn herdr_request(
    session: &SessionConfig,
    method: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let request = build_herdr_request(method, params);

    #[cfg(unix)]
    {
        let mut stream = UnixStream::connect(&session.socket_path)
            .await
            .with_context(|| format!("failed to connect Herdr socket {}", session.socket_path))?;
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).await?;
        let value = serde_json::from_str(&response)?;
        Ok(value)
    }

    #[cfg(not(unix))]
    {
        let _ = (session, method, params, request);
        anyhow::bail!("direct Herdr socket access is not implemented on this platform yet");
    }
}

#[cfg(unix)]
async fn herdr_event_stream(session: &SessionConfig) -> anyhow::Result<BufReader<UnixStream>> {
    let pane_ids = session_pane_ids(session).await?;
    open_herdr_event_stream(session, event_subscriptions(&pane_ids)).await
}

#[cfg(unix)]
async fn herdr_agent_event_stream(
    session: &SessionConfig,
) -> anyhow::Result<BufReader<UnixStream>> {
    let pane_ids = session_pane_ids(session).await?;
    open_herdr_event_stream(session, agent_event_subscriptions(&pane_ids)).await
}

#[cfg(unix)]
async fn session_pane_ids(session: &SessionConfig) -> anyhow::Result<Vec<String>> {
    let pane_response = herdr_request(session, "pane.list", json!({})).await?;
    Ok(pane_response
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .context("pane.list response is missing result.panes")?
        .iter()
        .filter_map(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

#[cfg(unix)]
async fn open_herdr_event_stream(
    session: &SessionConfig,
    subscriptions: Vec<Value>,
) -> anyhow::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(&session.socket_path)
        .await
        .with_context(|| format!("failed to connect Herdr socket {}", session.socket_path))?;
    let request = build_herdr_request(
        "events.subscribe",
        json!({
            "subscriptions": subscriptions
        }),
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(BufReader::new(stream))
}

fn event_subscriptions(pane_ids: &[String]) -> Vec<Value> {
    let mut subscriptions = vec![
        json!({ "type": "workspace.created" }),
        json!({ "type": "workspace.updated" }),
        json!({ "type": "workspace.metadata_updated" }),
        json!({ "type": "workspace.renamed" }),
        json!({ "type": "workspace.moved" }),
        json!({ "type": "workspace.closed" }),
        json!({ "type": "workspace.focused" }),
        json!({ "type": "tab.created" }),
        json!({ "type": "tab.closed" }),
        json!({ "type": "tab.focused" }),
        json!({ "type": "tab.renamed" }),
        json!({ "type": "tab.moved" }),
        json!({ "type": "pane.created" }),
        json!({ "type": "pane.updated" }),
        json!({ "type": "pane.closed" }),
        json!({ "type": "pane.focused" }),
        json!({ "type": "pane.moved" }),
        json!({ "type": "pane.exited" }),
        json!({ "type": "pane.agent_detected" }),
        json!({ "type": "layout.updated" }),
        json!({ "type": "worktree.created" }),
        json!({ "type": "worktree.opened" }),
        json!({ "type": "worktree.removed" }),
    ];
    subscriptions.extend(
        pane_ids
            .iter()
            .map(|pane_id| json!({ "type": "pane.agent_status_changed", "pane_id": pane_id })),
    );
    subscriptions
}

fn agent_event_subscriptions(pane_ids: &[String]) -> Vec<Value> {
    pane_ids
        .iter()
        .map(|pane_id| json!({ "type": "pane.agent_status_changed", "pane_id": pane_id }))
        .collect()
}

#[cfg(not(unix))]
async fn herdr_event_stream(
    _session: &SessionConfig,
) -> anyhow::Result<BufReader<tokio::io::Empty>> {
    anyhow::bail!("direct Herdr event streaming is not implemented on this platform yet");
}

#[cfg(not(unix))]
async fn herdr_agent_event_stream(
    _session: &SessionConfig,
) -> anyhow::Result<BufReader<tokio::io::Empty>> {
    anyhow::bail!("direct Herdr event streaming is not implemented on this platform yet");
}

fn build_herdr_request(method: &str, params: Value) -> String {
    json!({
        "id": format!("gateway:{}", uuid::Uuid::new_v4()),
        "method": method,
        "params": params
    })
    .to_string()
}

type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;

fn bearer_token(headers: &HeaderMap) -> ApiResult<&str> {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "missing_authorization",
            "missing Authorization header",
        ));
    };
    let Ok(value) = value.to_str() else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_authorization",
            "invalid Authorization header",
        ));
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_authorization",
            "expected Bearer token",
        ));
    };
    Ok(token)
}

/// Match a presented token against every device token. Every candidate is
/// compared in constant time and the loop is not short-circuited, so a caller
/// cannot learn which device matched from timing.
fn identify_device(devices: &[DeviceRecord], token: &str) -> Option<String> {
    if token.len() > 256 {
        return None;
    }
    let presented = hash_token(token);
    let mut matched = None;
    for device in devices {
        if constant_time_eq(presented.as_bytes(), device.token_hash.as_bytes()) {
            matched = Some(device.id.clone());
        }
    }
    matched
}

/// Control routes are for paired devices only. The admin token deliberately
/// does not authorise these: it sits in plaintext on disk for the manage UI,
/// and these routes can run commands on the host.
fn require_device(state: &AppState, headers: &HeaderMap) -> ApiResult<String> {
    let token = bearer_token(headers)?;
    let mut devices = lock_devices(state)?;
    let Some(device_id) = identify_device(&devices, token) else {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    };
    let now = now_unix_ms();
    if let Some(device) = devices.iter_mut().find(|device| device.id == device_id) {
        let stale = now.saturating_sub(device.last_seen_unix_ms) >= DEVICE_LAST_SEEN_FLUSH_MS;
        device.last_seen_unix_ms = now;
        if stale {
            if let Err(err) = write_devices(&devices) {
                // Losing a last-seen timestamp must not fail the request.
                eprintln!("failed to persist device last-seen: {err:#}");
            }
        }
    }
    Ok(device_id)
}

/// The local manage UI's credential, which authorises nothing but reading the
/// pending pairing code.
fn require_admin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    let token = bearer_token(headers)?;
    if token.len() > 256
        || !constant_time_eq(hash_token(token).as_bytes(), config.token_hash.as_bytes())
    {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }
    Ok(())
}

fn require_pairing_manager(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if require_admin(&state.config, headers).is_ok() {
        return Ok(());
    }
    require_device(state, headers).map(|_| ())
}

fn lock_devices(state: &AppState) -> ApiResult<std::sync::MutexGuard<'_, Vec<DeviceRecord>>> {
    state.devices.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_lock_failed",
            "failed to lock device state",
        )
    })
}

async fn list_paired_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let current_id = require_device(&state, &headers)?;
    let devices = lock_devices(&state)?;
    let items = devices
        .iter()
        .map(|device| {
            json!({
                "id": device.id,
                "name": device.name,
                "paired_unix_ms": device.paired_unix_ms,
                "last_seen_unix_ms": device.last_seen_unix_ms,
                "current": device.id == current_id
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "devices": items })))
}

async fn revoke_paired_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    // Paired devices can manage pairings through the app; the local Manager
    // pane uses its admin token for the same narrow operation. The admin token
    // still cannot access terminal/workspace control routes.
    require_pairing_manager(&state, &headers)?;
    let mut devices = lock_devices(&state)?;
    let previous_len = devices.len();
    devices.retain(|device| device.id != device_id);
    let removed = devices.len() != previous_len;
    if !removed {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            "device not found",
        ));
    }
    write_devices(&devices).map_err(|err| {
        eprintln!("failed to write device tokens: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_write_failed",
            "failed to revoke the device token",
        )
    })?;
    Ok(Json(
        json!({ "ok": true, "revoked": device_id, "device_count": devices.len() }),
    ))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

fn validate_text(text: &str) -> ApiResult<()> {
    if text.len() > MAX_SEND_TEXT_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text_too_large",
            "text must be at most 65536 bytes",
        ));
    }
    Ok(())
}

fn validate_push_token(token: &str) -> ApiResult<()> {
    let valid_prefix =
        token.starts_with("ExponentPushToken[") || token.starts_with("ExpoPushToken[");
    if token.len() > 256 || !valid_prefix || !token.ends_with(']') {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_push_token",
            "token must be an Expo push token",
        ));
    }
    Ok(())
}

async fn send_expo_push_notifications(
    tokens: &[PushTokenRecord],
    title: String,
    body: String,
    data: serde_json::Map<String, Value>,
) -> anyhow::Result<Value> {
    if tokens.is_empty() {
        return Ok(json!({ "data": [] }));
    }
    let messages = tokens
        .iter()
        .map(|record| {
            json!({
                "to": record.token,
                "title": title,
                "body": body,
                "data": data,
                "sound": "default",
                "channelId": "gateway"
            })
        })
        .collect::<Vec<_>>();
    let response = reqwest::Client::new()
        .post("https://exp.host/--/api/v2/push/send")
        .json(&messages)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

fn insert_opt<T: Serialize>(map: &mut serde_json::Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
}

fn find_session<'a>(config: &'a Config, session_id: &str) -> ApiResult<&'a SessionConfig> {
    config
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "session not found",
            )
        })
}

fn api_error(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
}

fn load_config(config_path: Option<String>) -> anyhow::Result<Config> {
    let path = config_path
        .map(Into::into)
        .unwrap_or(config_dir()?.join(CONFIG_FILE));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::config_dir()
        .context("failed to locate config directory")?
        .join("herdr-gateway"))
}

fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::data_dir()
        .or_else(dirs::config_dir)
        .context("failed to locate state directory")?
        .join("herdr-gateway"))
}

fn read_push_tokens() -> anyhow::Result<Vec<PushTokenRecord>> {
    let path = state_dir()?.join(PUSH_TOKENS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read push token file {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_push_tokens(tokens: &[PushTokenRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    let path = dir.join(PUSH_TOKENS_FILE);
    write_secret_file(&path, &serde_json::to_vec_pretty(tokens)?)
}

fn read_devices() -> anyhow::Result<Vec<DeviceRecord>> {
    let path = state_dir()?.join(DEVICES_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read device file {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_devices(devices: &[DeviceRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    write_secret_file(
        &dir.join(DEVICES_FILE),
        &serde_json::to_vec_pretty(devices)?,
    )
}

fn list_devices() -> anyhow::Result<()> {
    let devices = read_devices()?;
    if devices.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for device in devices {
        println!(
            "{}  {}  paired {}  last seen {}",
            device.id,
            truncate(&device.name, 32),
            format_unix_ms(device.paired_unix_ms),
            format_unix_ms(device.last_seen_unix_ms)
        );
    }
    Ok(())
}

fn revoke_device(device_id: Option<String>, all: bool) -> anyhow::Result<()> {
    if all {
        let mut devices = read_devices()?;
        let count = devices.len();
        devices.clear();
        write_devices(&devices)?;
        println!("revoked {count} device token(s)");
        return Ok(());
    }
    let Some(device_id) = device_id else {
        anyhow::bail!("pass a device id from `gateway devices`, or --all");
    };
    if !revoke_device_by_id(&device_id)? {
        anyhow::bail!("device {device_id} not found");
    }
    println!("revoked device {device_id}");
    Ok(())
}

fn revoke_device_by_id(device_id: &str) -> anyhow::Result<bool> {
    let mut devices = read_devices()?;
    let previous_len = devices.len();
    devices.retain(|device| device.id != device_id);
    if devices.len() == previous_len {
        return Ok(false);
    }
    write_devices(&devices)?;
    Ok(true)
}

fn format_unix_ms(value: u128) -> String {
    if value == 0 {
        return String::from("never");
    }
    let seconds_ago = now_unix_ms().saturating_sub(value) / 1000;
    match seconds_ago {
        0..=59 => String::from("just now"),
        60..=3599 => format!("{}m ago", seconds_ago / 60),
        3600..=86399 => format!("{}h ago", seconds_ago / 3600),
        _ => format!("{}d ago", seconds_ago / 86400),
    }
}

fn pid_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(state_dir()?.join(PID_FILE))
}

fn read_pid() -> anyhow::Result<Option<u32>> {
    let path = pid_path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    Ok(text.trim().parse::<u32>().ok())
}

fn read_pairing_file() -> anyhow::Result<PairingFile> {
    let bytes = std::fs::read(config_dir()?.join(PAIRING_FILE))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn remove_pid_file() -> anyhow::Result<()> {
    let path = pid_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn process_running(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn stop_pid(pid: u32) -> anyhow::Result<()> {
    let status = ProcessCommand::new("kill")
        .arg(pid.to_string())
        .status()
        .context("failed to run kill")?;
    if !status.success() {
        anyhow::bail!("failed to stop pid {pid}");
    }
    Ok(())
}

const GATEWAY_PROCESS_NAME: &str = "herdr-gateway";

/// Find gateway processes listening on `port`. Anything that is not the gateway
/// is filtered out by process name: `stop` must never kill an unrelated service
/// that happens to hold the port.
fn gateway_listener_pids(port: u16) -> anyhow::Result<Vec<u32>> {
    #[cfg(target_os = "macos")]
    let mut pids = {
        let output = ProcessCommand::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
            .context("failed to inspect listening sockets")?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!("failed to inspect listening sockets");
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .filter(|pid| process_is_gateway(*pid))
            .collect::<Vec<_>>()
    };

    #[cfg(not(target_os = "macos"))]
    let mut pids = {
        let output = ProcessCommand::new("ss")
            .args(["-ltnp"])
            .output()
            .context("failed to inspect listening sockets")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let port_suffix = format!(":{port}");
        let mut pids = Vec::new();
        for line in text.lines() {
            if !line.contains(&port_suffix) || !line.contains(GATEWAY_PROCESS_NAME) {
                continue;
            }
            let Some(pid_start) = line.find("pid=") else {
                continue;
            };
            let pid_text = line[pid_start + 4..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if let Ok(pid) = pid_text.parse::<u32>() {
                pids.push(pid);
            }
        }
        pids
    };

    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Confirm a pid really belongs to the gateway before signalling it.
fn process_is_gateway(pid: u32) -> bool {
    ProcessCommand::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(GATEWAY_PROCESS_NAME))
        })
}

/// Marks a directory as never-to-be-committed.
///
/// Config directories are routinely symlinked into a dotfiles repository, and a
/// habitual `git add -A` there would publish the admin token and the paired
/// device list. File mode 0600 does not survive a commit; a `.gitignore` does.
/// Written next to the secrets themselves so it travels with them wherever
/// `HERDR_PLUGIN_CONFIG_DIR` points.
fn write_secret_dir_gitignore(dir: &std::path::Path) {
    let path = dir.join(".gitignore");
    if path.exists() {
        return;
    }
    let body = "# The gateway keeps tokens and paired-device records here.\n\
                # Never let them reach a repository.\n\
                *\n";
    if let Err(err) = std::fs::write(&path, body) {
        eprintln!("could not write {}: {err}", path.display());
    }
}

fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        write_secret_dir_gitignore(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies when the file is created, so a file written by an
        // older build keeps its old permissions until they are set explicitly.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, bytes)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

fn default_socket_path() -> String {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("herdr")
        .join("herdr.sock")
        .to_string_lossy()
        .into_owned()
}

fn hostname_label() -> String {
    // HOSTNAME is often unset on macOS, which left every server named the
    // generic "Herdr Server". Fall back to the real machine name so a fresh
    // install is at least recognisable before the user renames it.
    if let Ok(value) = std::env::var("HOSTNAME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if let Ok(name) = String::from_utf8(output.stdout) {
            let name = name.trim().trim_end_matches(".local").trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "Herdr Server".into()
}

/// Persist a new display label into the config (and the pairing payload), so a
/// name set from the app shows up in push notifications. Mirrors
/// `update_public_url`.
fn update_server_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    anyhow::ensure!(
        !label.is_empty()
            && label.chars().count() <= MAX_DEVICE_NAME_CHARS
            && !label.chars().any(char::is_control),
        "label must be non-empty, at most {MAX_DEVICE_NAME_CHARS} characters, and free of control characters"
    );
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.label = label.to_string();
    write_secret_file(&config_path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    if let Ok(mut pairing) = read_pairing_file() {
        pairing.payload.label = label.to_string();
        let _ = write_secret_file(
            &config_dir()?.join(PAIRING_FILE),
            &serde_json::to_vec_pretty(&pairing)?,
        );
    }
    Ok(label.to_string())
}

/// The label as it stands on disk right now, so a rename from the app takes
/// effect for notifications without restarting the gateway.
fn current_server_label(fallback: &str) -> String {
    load_config(None)
        .map(|config| config.label)
        .unwrap_or_else(|_| fallback.to_string())
}

fn generate_token() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn generate_pairing_code() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let characters: String = bytes[..PAIRING_CODE_CHARACTER_COUNT]
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &characters[..4], &characters[4..])
}

fn valid_pairing_code(code: &str) -> bool {
    code.len() == PAIRING_CODE_LENGTH
        && code.as_bytes()[4] == b'-'
        && code
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || b"23456789ABCDEFGHJKMNPQRSTUVWXYZ".contains(&byte))
}

/// Device names are echoed into the manage UI's terminal box, so control
/// characters (ANSI escapes in particular) are rejected outright.
/// A client install identifier: an opaque token the app generates once and
/// keeps. Bounded and control-character-free like every other client string.
fn valid_install_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_device_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_DEVICE_NAME_CHARS
        && !value.chars().any(char::is_control)
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn hash_token(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

fn openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Herdr Gateway API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Token-protected mobile API for controlling a local Herdr server through the Herdr socket API."
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            }
        },
        "security": [{ "bearerAuth": [] }],
        "paths": {
            "/health": { "get": simple_endpoint("Gateway and Herdr health") },
            "/api/meta": { "get": simple_endpoint("Gateway API and Herdr compatibility metadata") },
            "/api/pair/request": {
                "post": {
                    "summary": "Request pairing from Muqun app",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("device_name", "string")], &["request_id"])),
                    "responses": ok_response()
                }
            },
            "/api/pair/claim": {
                "post": {
                    "summary": "Claim pairing token with request id and confirmation code",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("code", "string")], &["request_id", "code"])),
                    "responses": ok_response()
                }
            },
            "/api/pairings": { "get": simple_endpoint("List devices holding a gateway token") },
            "/api/pairings/{deviceId}": {
                "delete": {
                    "summary": "Revoke one device's gateway token",
                    "parameters": [path_param("deviceId")],
                    "responses": ok_response()
                }
            },
            "/api/devices/push-token": {
                "post": {
                    "summary": "Register this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string"), ("platform", "string"), ("device_name", "string")], &["token", "platform"])),
                    "responses": ok_response()
                },
                "delete": {
                    "summary": "Remove this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string")], &["token"])),
                    "responses": ok_response()
                }
            },
            "/api/notifications/test": {
                "post": {
                    "summary": "Send a test push notification to registered Muqun devices",
                    "requestBody": json_body(object_schema(&[("title", "string"), ("body", "string"), ("data", "object")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/uploads": {
                "post": {
                    "summary": "Upload one image and get back a local path for an agent to read",
                    "description": "Images only: png, jpeg, gif, webp, and heic. The type is decided by sniffing the content, not by the filename, and everything else, including executables and scripts, is refused. The stored name is generated by the gateway; the returned name is only the sanitised client name. Uploads are deleted after 48 hours.",
                    "requestBody": multipart_file_body(),
                    "responses": upload_responses()
                }
            },
            "/api/sessions": { "get": simple_endpoint("List configured Herdr sessions") },
            "/api/sessions/{sessionId}/events": {
                "get": {
                    "summary": "Stream Herdr lifecycle events as Server-Sent Events",
                    "parameters": [path_param("sessionId")],
                    "responses": {
                        "200": { "description": "SSE stream of Herdr event JSON lines" },
                        "401": { "description": "Missing or invalid authorization" },
                        "403": { "description": "Invalid token" }
                    }
                }
            },
            "/api/sessions/{sessionId}/snapshot": { "get": session_endpoint("Return Herdr session.snapshot") },
            "/api/sessions/{sessionId}/workspaces": {
                "get": session_endpoint("List Herdr workspaces"),
                "post": {
                    "summary": "Create a workspace",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("cwd", "string"), ("label", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}/focus": { "post": resource_endpoint("Focus a workspace", "workspaceId") },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}": {
                "patch": {
                    "summary": "Rename a workspace",
                    "parameters": [path_param("sessionId"), path_param("workspaceId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a workspace", "workspaceId")
            },
            "/api/sessions/{sessionId}/tabs": {
                "get": session_endpoint("List Herdr tabs"),
                "post": {
                    "summary": "Create a tab",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("workspace_id", "string"), ("label", "string"), ("cwd", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/tabs/{tabId}/focus": { "post": resource_endpoint("Focus a tab", "tabId") },
            "/api/sessions/{sessionId}/tabs/{tabId}": {
                "patch": {
                    "summary": "Rename a tab",
                    "parameters": [path_param("sessionId"), path_param("tabId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a tab", "tabId")
            },
            "/api/sessions/{sessionId}/panes": { "get": session_endpoint("List Herdr panes") },
            "/api/sessions/{sessionId}/panes/{paneId}": {
                "get": resource_endpoint("Get a pane", "paneId"),
                "patch": {
                    "summary": "Rename a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a pane", "paneId")
            },
            "/api/sessions/{sessionId}/panes/{paneId}/focus": { "post": resource_endpoint("Focus a pane", "paneId") },
            "/api/sessions/{sessionId}/panes/{paneId}/split": {
                "post": {
                    "summary": "Split a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["direction"],
                        "properties": {
                            "direction": { "type": "string", "enum": ["right", "down"] },
                            "ratio": { "type": "number" },
                            "command": { "type": "array", "items": { "type": "string" } },
                            "cwd": { "type": "string" },
                            "env": { "type": "object", "additionalProperties": true }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/zoom": {
                "post": {
                    "summary": "Zoom a pane for a single-panel viewport",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "enum": ["on", "off", "toggle"], "default": "on" }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/keymaps": { "get": session_endpoint("Agent keymap coverage") },
            "/api/sessions/{sessionId}/panes/{paneId}/shortcuts": {
                "get": resource_endpoint("Key row and slash commands for a pane", "paneId")
            },
            "/api/sessions/{sessionId}/agents": { "get": session_endpoint("List Herdr agents") },
            "/api/sessions/{sessionId}/agents/{target}": { "get": resource_endpoint("Get an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/focus": { "post": resource_endpoint("Focus an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/send": {
                "post": {
                    "summary": "Send and submit text to an agent",
                    "parameters": [path_param("sessionId"), path_param("target")],
                    "requestBody": json_body(object_schema(&[("text", "string")], &["text"])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/output": {
                "get": {
                    "summary": "Read pane output",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("source", "Pane read source, for example recent-unwrapped, recent, visible, or detection"),
                        query_param("lines", "Maximum line count"),
                        query_param("format", "Output format: text or ansi")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-text": {
                "post": {
                    "summary": "Send text to a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": { "text": { "type": "string" } }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-keys": {
                "post": {
                    "summary": "Send Herdr key names to a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["keys"],
                        "properties": { "keys": { "type": "array", "items": { "type": "string" } } }
                    })),
                    "responses": ok_response()
                }
            }
        }
    })
}

fn simple_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "responses": ok_response()
    })
}

fn session_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId")],
        "responses": ok_response()
    })
}

fn resource_endpoint(summary: &str, resource_param: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId"), path_param(resource_param)],
        "responses": ok_response()
    })
}

fn object_schema(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), json!({ "type": ty })))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "required": required,
        "properties": properties
    })
}

fn path_param(name: &str) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "schema": { "type": "string" }
    })
}

fn query_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": description,
        "schema": { "type": "string" }
    })
}

fn multipart_file_body() -> Value {
    json!({
        "required": true,
        "content": {
            "multipart/form-data": {
                "schema": {
                    "type": "object",
                    "required": ["file"],
                    "properties": { "file": { "type": "string", "format": "binary" } }
                }
            }
        }
    })
}

fn upload_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Stored upload",
        "content": { "application/json": { "schema": object_schema(
            &[("path", "string"), ("name", "string"), ("size", "integer"), ("mime", "string")],
            &["path", "name", "size", "mime"],
        ) } }
    });
    responses["400"] = json!({ "description": "Malformed multipart body, or no usable file field" });
    responses["413"] = json!({ "description": "Upload is larger than 25 MiB" });
    responses["415"] =
        json!({ "description": "Content is an executable or script, or not an accepted image type" });
    responses
}

fn json_body(schema: Value) -> Value {
    json!({
        "required": true,
        "content": { "application/json": { "schema": schema } }
    })
}

fn ok_response() -> Value {
    json!({
        "200": {
            "description": "Successful response",
            "content": { "application/json": { "schema": {} } }
        },
        "401": { "description": "Missing or invalid authorization" },
        "403": { "description": "Invalid token" },
        "502": { "description": "Herdr socket unavailable or returned an error" }
    })
}

const DOCS_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Herdr Gateway API Docs</title>
  </head>
  <body>
    <script id="api-reference" data-url="/openapi.json"></script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
  </body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_config(token: &str) -> Config {
        Config {
            server_id: "server-1".into(),
            label: "test".into(),
            listen: "127.0.0.1:23100".into(),
            public_url: "http://127.0.0.1:23100".into(),
            token_hash: hash_token(token),
            sessions: vec![SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: "/tmp/herdr.sock".into(),
            }],
        }
    }

    fn test_device(id: &str, token: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: format!("device {id}"),
            token_hash: hash_token(token),
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

    #[test]
    fn event_filter_matches_dot_and_underscore_and_reads_the_event_field() {
        assert_eq!(normalize_event_name("pane.updated"), "pane_updated");
        assert_eq!(normalize_event_name(" pane_updated "), "pane_updated");
        assert_eq!(
            herdr_event_name(r#"{"event":"pane_updated","data":{}}"#).as_deref(),
            Some("pane_updated")
        );
        // A line the gateway cannot parse is forwarded rather than dropped, so a
        // filter never silently swallows an event shape we did not anticipate.
        assert_eq!(herdr_event_name("not json"), None);
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

    #[test]
    fn pairing_code_uses_unambiguous_characters() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(valid_pairing_code(&code));
        assert!(!valid_pairing_code("ABCD2345"));
        assert!(!valid_pairing_code("abcD-2345"));
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
        assert!(!code.contains('L'));
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

    #[test]
    fn pairing_code_is_consumed_after_one_successful_claim() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_pairing_code(&mut pending, "request-1", "2345-6789", 1_001),
            Ok(())
        );
        assert!(pending.is_none());
        assert_eq!(
            consume_pairing_code(&mut pending, "request-1", "2345-6789", 1_002),
            Err(PairingCodeError::Missing)
        );
    }

    #[test]
    fn expired_pairing_code_is_rejected_and_cleared() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_pairing_code(
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
                consume_pairing_code(&mut pending, "request-1", "AAAA-AAAA", 1_001),
                Err(PairingCodeError::Invalid)
            );
        }
        assert!(pending.is_none());
    }

    #[test]
    fn herdr_request_shape_matches_socket_api() {
        let encoded = build_herdr_request("pane.read", json!({ "pane_id": "w1:p1" }));
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["method"], "pane.read");
        assert_eq!(value["params"]["pane_id"], "w1:p1");
        assert!(value["id"].as_str().unwrap().starts_with("gateway:"));
    }

    #[test]
    fn stream_pane_read_becomes_inline_update() {
        let read = json!({
            "result": {
                "read": {
                    "pane_id": "w1:p2",
                    "revision": 42,
                    "text": "hello\n"
                }
            }
        });
        let frame = stream_pane_frame(&read).unwrap();
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
    fn stream_pane_update_requires_a_revision() {
        let read = json!({ "result": { "read": { "text": "hello" } } });
        assert!(stream_pane_frame(&read).is_none());
    }

    #[test]
    fn agent_status_subscriptions_are_scoped_to_each_pane() {
        let subscriptions = event_subscriptions(&["w1:p1".into(), "w2:p3".into()]);
        let agent_subscriptions = subscriptions
            .iter()
            .filter(|subscription| subscription["type"] == "pane.agent_status_changed")
            .collect::<Vec<_>>();

        assert_eq!(agent_subscriptions.len(), 2);
        assert_eq!(agent_subscriptions[0]["pane_id"], "w1:p1");
        assert_eq!(agent_subscriptions[1]["pane_id"], "w2:p3");
        assert!(agent_subscriptions
            .iter()
            .all(|subscription| subscription["pane_id"].is_string()));

        let watcher_subscriptions = agent_event_subscriptions(&["w1:p1".into(), "w2:p3".into()]);
        assert_eq!(
            watcher_subscriptions,
            vec![
                json!({ "type": "pane.agent_status_changed", "pane_id": "w1:p1" }),
                json!({ "type": "pane.agent_status_changed", "pane_id": "w2:p3" })
            ]
        );
    }

    #[test]
    fn herdr_compatibility_requires_protocol_17() {
        assert_eq!(HERDR_PROTOCOL_MIN, 17);
        assert_eq!(HERDR_PROTOCOL_MAX, 17);
    }

    #[test]
    fn split_result_uses_protocol_17_pane_shape() {
        let response = json!({
            "result": {
                "type": "pane_created",
                "pane": { "pane_id": "w1:p2" }
            }
        });
        assert_eq!(created_pane_id(&response), Some("w1:p2"));
    }

    #[test]
    fn pairing_payload_contains_mobile_connection_fields() {
        let payload = PairingPayload {
            kind: "herdr-gateway".into(),
            server_id: "server-1".into(),
            label: "machine".into(),
            url: "http://100.1.2.3:23100".into(),
            token: "secret".into(),
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["kind"], "herdr-gateway");
        assert_eq!(value["url"], "http://100.1.2.3:23100");
        assert_eq!(value["token"], "secret");
    }

    #[test]
    fn manager_qr_uses_the_current_config_fields() {
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1"),
            "muqun://pair?u=http%3A%2F%2F100.1.2.3%3A23847&s=server-1"
        );
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
        let notification = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        assert_eq!(notification.title, "Agent blocked · Studio");
        assert_eq!(notification.body, "Codex needs your input.");
        assert_eq!(notification.data["type"], "agent.blocked");
        assert_eq!(notification.data["url"], "/servers/server-1");
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
        let notification = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        assert_eq!(notification.title, "Agent done · Studio");
        assert_eq!(notification.body, "codex finished running.");
        assert_eq!(notification.data["type"], "agent.completed");
        assert_eq!(notification.data["pane_id"], "w1:p2");
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

    #[test]
    fn uploads_are_typed_by_content_not_by_name() {
        assert_eq!(sniff_upload_kind(&png_bytes()).unwrap().mime, "image/png");
        assert_eq!(
            sniff_upload_kind(b"\xff\xd8\xff\xe0\x00\x10JFIF").unwrap().mime,
            "image/jpeg"
        );
        assert_eq!(sniff_upload_kind(b"GIF89a....").unwrap().extension, "gif");
        assert_eq!(sniff_upload_kind(b"GIF87a....").unwrap().extension, "gif");
        assert_eq!(
            sniff_upload_kind(b"RIFF\x24\x00\x00\x00WEBPVP8 ").unwrap().mime,
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

    #[test]
    fn everything_that_is_not_an_image_is_refused() {
        // This first version carries images only. A document or a text file is
        // a 415 like any other unrecognised content, so nothing without an
        // image magic number can be parked on the host.
        assert!(sniff_upload_kind(b"%PDF-1.7\n1 0 obj").is_none());
        assert!(sniff_upload_kind(b"# notes\n\nplain, with no magic number\n").is_none());
        assert!(sniff_upload_kind(b"{\"a\":1}").is_none());
        assert!(sniff_upload_kind(b"a,b,c\n1,2,3\n").is_none());
        assert!(sniff_upload_kind(b"2026-07-25 12:00:00 INFO started\n").is_none());
        assert!(sniff_upload_kind(b"PK\x03\x04").is_none());
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
        assert_eq!(first.len(), "00000000-0000-0000-0000-000000000000.png".len());
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

    #[test]
    fn openapi_spec_contains_docs_routes_and_auth() {
        let spec = openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/zoom"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/events"].is_object());
        assert!(spec["paths"]["/api/pair/request"].is_object());
        assert!(spec["paths"]["/api/pair/claim"].is_object());
        assert!(spec["paths"]["/api/meta"].is_object());
        assert!(spec["paths"]["/api/devices/push-token"]["delete"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/workspaces/{workspaceId}"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/agents/{target}/send"].is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["requestBody"]["content"]
            ["multipart/form-data"]
            .is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["413"].is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["415"].is_object());
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"]["parameters"]
                [4]["name"],
            "format"
        );
    }
}
