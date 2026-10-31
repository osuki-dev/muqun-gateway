//! HTTP server startup: middleware stack, transport encryption, and locality checks.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::IntoFuture as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use axum::body::{to_bytes, Body};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tower_http::compression::{
    predicate::{DefaultPredicate, Predicate, SizeAbove},
    CompressionLayer,
};

use super::assets::{AssetIndex, MAX_VIDEO_CONTENT_BYTES};
use super::metadata::SessionLivenessCache;
use super::store::{
    ensure_pairing_transport_key, load_devices_for_service, load_push_tokens_for_service,
};
use super::uploads::{spawn_upload_gc, MAX_VIDEO_UPLOAD_BYTES, UPLOADS_PATH};
use crate::platform;
use crate::platform::i18n::Locale;
use crate::terminal;
use crate::terminal::routes::{
    spawn_agent_notification_watchers, spawn_agent_permission_watchers, spawn_approval_watchers,
};
use crate::{
    agent_events, agents, api_error, backend_startup, clock_skew, connectivity,
    device_repair_required, gateway_listener, hash_token, i18n, load_config, lock_devices,
    now_unix_ms, require_device_or_peer, scrollback, state_dir, state_lock, transport,
    unreachable_listen_warning, warn_about_missing_backend_programs, ApiResult, AppState, Config,
    APPROVAL_EVENT_CAPACITY, MAX_REQUEST_BODY_BYTES,
};

pub(crate) async fn run(config_path: Option<String>) -> anyhow::Result<()> {
    // Register before any owned child can start. File guards below are kept
    // until the stop path has awaited Codex cleanup, not process::exit-ed by
    // a competing MCP signal task.
    let shutdown = register_shutdown_signal()?;
    // Taken before the device list is read and held for the life of the
    // process. This gateway caches the whole list in memory and rewrites the
    // whole file on every change, so a second gateway against the same
    // directory would not interleave with it -- it would overwrite it, and
    // whichever devices the loser had paired would be silently unpaired.
    //
    // The binding has to be named: `let _ = ...` would drop the lock on the
    // spot and leave this gateway believing it owned a directory it had
    // already released.
    let _state_lock = state_lock::StateLock::acquire_owner(&state_dir()?)?;
    ensure_pairing_transport_key()?;
    let config_file = platform::store::config_file_path(config_path.as_deref())?;
    let config = load_config(config_path)?;
    warn_about_missing_backend_programs(&config);
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    // Read before `config` moves into the state, and printed after the routes
    // are built so it is the last thing on screen rather than the first.
    let listen_warning = unreachable_listen_warning(&config.listen, &config.public_url);
    // Same reason: read before `config` moves, said after the routes are up.
    let dev_unauthenticated = config.dev_unauthenticated;

    // Bind successfully before starting anything on the user's behalf.
    let listener = gateway_listener::bind(addr).await?;
    // Per process: minted now, removed when the gateway stops.
    let _identity = if config.mcp.enabled {
        Some(crate::mcp::mint_identity()?)
    } else {
        if let Ok(file) = crate::mcp::identity_file() {
            let _ = std::fs::remove_file(file);
        }
        None
    };
    // A non-loopback listen address: `/mcp` gets its own loopback listener,
    // whose URL every agent registration uses from here on.
    let mcp_listeners = if config.mcp.enabled {
        crate::mcp::bind_loopback(&config.listen)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "no loopback listener for /mcp; agents started here get no muqun tools");
                None
            })
    } else {
        None
    };
    let _endpoint_record = match mcp_listeners
        .as_ref()
        .and_then(|listeners| listeners.first())
    {
        Some(listener) => crate::mcp::announce_loopback(listener.local_addr()?.port(), addr.port()),
        // Needed and not bound: say so where manage and the CLI look.
        None if config.mcp.enabled && crate::mcp::loopback_warning(&config.listen).is_some() => {
            crate::mcp::announce_not_listening()
        }
        None => {
            if let Ok(file) = crate::mcp::endpoint_file() {
                let _ = std::fs::remove_file(file);
            }
            None
        }
    };
    // One background attempt per opted-in backend; no restart/logging loop.
    backend_startup::spawn(&config);

    let agent_runtime = if config.agent_runtime.enabled {
        agents::AgentRuntime::with_local_configs(
            config.opencode.clone(),
            config.deepseek.clone(),
            config.t3.clone(),
            crate::agents::runtime::LocalAgentConfigs {
                codex: config.codex.clone(),
                fx: config.fx.clone(),
                qoder: config.qoder.clone(),
                antigravity: config.antigravity.clone(),
                cursor: config.cursor.clone(),
                pi: config.pi.clone(),
                claude: config.claude.clone(),
            },
            Some(state_dir()?),
        )
    } else {
        agents::AgentRuntime::disabled()
    };

    agent_runtime.observe_terminal_sessions(&config.sessions);

    let scrollback = Arc::new(Mutex::new(scrollback::ScrollbackStore::new(
        config.history.capture,
    )));
    let state_directory = state_dir()?;
    let key = platform::store::db_key::key_for(&config.store, &state_directory)?;
    let db = platform::store::sqlite::SqliteStore::open(
        &platform::store::store_db_path(&state_directory),
        key,
        platform::store::sqlite::ALL_MIGRATIONS,
    )
    .map_err(|err| match config.store.encryption {
        // One sentence that names both files, so the owner knows what to restore.
        crate::platform::config::StoreEncryption::File => anyhow::anyhow!(
            "{err} (key file: {})",
            platform::store::db_key::key_file(&state_directory).display()
        ),
        crate::platform::config::StoreEncryption::Off => err,
    })?;
    if config.store.encryption == crate::platform::config::StoreEncryption::File {
        agent_runtime.persist_t3_credentials(db.clone());
    }
    agent_runtime.restore_native_codex_sessions(db.clone());
    agent_runtime.persist_session_routes(db.clone()).await;
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
        &config.fleet,
        Some(config_file),
        presence,
    ));
    peers.load_names().await;
    let history =
        terminal::history_sqlite::repository(&config, db.clone(), scrollback.clone()).await;
    let state = AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(load_push_tokens_for_service())),
        devices: Arc::new(Mutex::new(load_devices_for_service()?)),
        assets: Arc::new(Mutex::new(AssetIndex::default())),
        scrollback,
        history,
        agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
        approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
        activity: Arc::new(Mutex::new(HashMap::new())),
        session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
        agent_runtime: agent_runtime.clone(),
        ws_connections: Arc::new(agents::ws_routes::WsRegistry::default()),
        generation: crate::new_generation(),
        message_images: Arc::default(),
        mcp_tokens: Arc::new(crate::mcp::tokens_sqlite::SqliteTokenStore::new(db.clone())),
        fleet: Arc::default(),
        peers,
        tasks,
        task_events,
        home_overview: Default::default(),
        db,
    };
    // A registration handed to a session agent's service outlives a switch
    // turned off; its token does not.
    crate::mcp::inject::startup_cleanup(&state.config, state.mcp_tokens.as_ref()).await;
    // After the token store exists, so the first OpenCode start already gets
    // its MCP environment.
    if state.config.mcp.enabled && state.config.mcp.inject_into_agents {
        // The listen address, or the loopback listener bound beside it.
        state.agent_runtime.enable_mcp(
            state.mcp_tokens.clone(),
            crate::mcp::local_url(&state.config.listen),
        );
    }
    for listener in mcp_listeners.into_iter().flatten() {
        let app = crate::mcp::loopback_router(state.clone());
        tokio::spawn(async move {
            if let Err(error) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                tracing::error!(%error, "the loopback MCP listener stopped");
            }
        });
    }
    tracing::info!(
        "agents started from a phone or an MCP caller launch with: {}",
        crate::agents::tasks::launch_defaults(
            &state.config.agent_launch_args,
            state.config.agent_trust_folders
        )
        .join(" | ")
    );
    spawn_agent_notification_watchers(state.clone());
    spawn_agent_permission_watchers(state.clone());
    spawn_approval_watchers(state.clone());
    spawn_upload_gc();
    spawn_memory_watchdog();
    if state.config.mcp.enabled {
        crate::mcp::issue::spawn_sweeper(state.clone());
    }
    // Regardless of `mcp.enabled`: the phone's task list needs it too.
    crate::tasks::driver::spawn(state.clone());
    crate::tasks::driver::spawn_follower(state.clone());
    crate::tasks::driver::spawn_instruction_cleanup(state.clone());
    crate::tasks::notices::spawn(state.clone());
    crate::tasks::pane_agent::warm_claude_probe(&state);

    let app = Router::new();
    let app = platform::routes::mount(app);
    let app = connectivity::routes::mount(app);
    let app = agents::session_routes::mount(app);
    let app = agents::routes::mount(app);
    let app = terminal::routes::mount(app);
    let app = platform::uploads::mount(app);
    let app = platform::assets::mount(app);
    let app = crate::fleet::routes::mount(app);
    let app = crate::fleet::peer_routes::mount(app);
    let app = crate::tasks::routes::mount(app);
    let app = if state.config.mcp.enabled {
        crate::mcp::mount(app)
    } else {
        app
    }
    .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
    .layer(middleware::from_fn_with_state(
        state.clone(),
        large_body_after_auth,
    ))
    // Inside the encrypted transport, so what it compresses is the
    // plaintext body and not the sealed base64 -- ciphertext does not
    // compress, and sealing first is why nothing downstream could.
    //
    // The default predicate already declines `text/event-stream` (a
    // compressor would buffer a stream that is supposed to arrive a frame
    // at a time) and content that is already compressed, such as an
    // uploaded image. `SizeAbove` keeps it off bodies too small to be
    // worth a header: below about half a kilobyte gzip usually costs more
    // than it saves.
    .layer(
        CompressionLayer::new()
            .compress_when(DefaultPredicate::new().and(SizeAbove::new(COMPRESSION_MIN_BYTES))),
    )
    .layer(middleware::from_fn(envelope_compression_gate))
    .layer(middleware::from_fn_with_state(
        state.clone(),
        encrypted_transport,
    ))
    .layer(middleware::from_fn_with_state(
        known_hosts(&state.config),
        known_host,
    ))
    .layer(middleware::from_fn(security_headers))
    .layer(middleware::from_fn(request_locale))
    .with_state(state);

    if let Some(warning) = listen_warning {
        tracing::warn!("{warning}");
    }
    if dev_unauthenticated {
        // Said every time, on stderr, unmissably: this is the one setting that
        // hands the API to anything that can reach the port.
        tracing::warn!(
            "SECURITY WARNING: dev_unauthenticated is on -- every device route \
             answers WITHOUT a token, to anything that can reach {addr}. This is \
             for a local mock only. Remove \"dev_unauthenticated\" from config.json \
             to turn it off."
        );
    }
    println!("terminal gateway listening on http://{addr}");
    // The peer address is what `/mcp` checks for loopback.
    agent_runtime.spawn_supervisor();
    serve_until_shutdown(listener, app, agent_runtime, shutdown).await?;
    Ok(())
}

/// Finish private-child cleanup on both normal signal shutdown and a listener
/// failure. Do not wait indefinitely for SSE/WebSocket clients to disconnect,
/// and never stop adopted agent services or terminal sessions.
async fn serve_until_shutdown(
    listener: tokio::net::TcpListener,
    app: Router,
    runtime: Arc<agents::AgentRuntime>,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let result = tokio::select! {
        biased;
        _ = shutdown => Ok(()),
        result = axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).into_future() => result,
    };
    tokio::join!(
        runtime.shutdown_fx(),
        runtime.shutdown_qoder(),
        runtime.shutdown_claude(),
        runtime.shutdown_antigravity(),
        runtime.shutdown_cursor(),
        runtime.shutdown_pi()
    );
    let cleanup = runtime.shutdown_codex().await;
    match (result, cleanup) {
        (result, Ok(())) => result.context("Gateway HTTP listener stopped"),
        (Ok(()), Err(error)) => Err(error).context("Gateway private Codex cleanup failed"),
        (Err(listener), Err(cleanup)) => Err(cleanup).with_context(|| {
            format!("Gateway HTTP listener stopped ({listener}); private Codex cleanup failed")
        }),
    }
}

#[cfg(unix)]
fn register_shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {_ = term.recv() => {}, _ = int.recv() => {}}
    })
}

#[cfg(not(unix))]
fn register_shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

/// Put the caller's language in scope for the whole request.
///
/// This is the outermost layer because every route under it can refuse, and a
/// refusal is the most common thing a person reads from this gateway. It reads
/// the headers once and hands the answer to [`i18n::scope`]; nothing below has
/// to remember to ask, and nothing below can ask for a locale the resolver
/// would not have given it.
pub(crate) async fn request_locale(request: Request<Body>, next: Next) -> Response {
    let locale = Locale::from_headers(request.headers());
    i18n::scope(locale, next.run(request)).await
}

pub(crate) const TRANSPORT_HEADER: &str = "x-muqun-transport";

/// How a client says it can inflate a compressed sealed body. `gzip` is the
/// only value that does anything.
pub(crate) const ENVELOPE_ACCEPT_HEADER: &str = "x-muqun-envelope-accept";

pub(crate) const TRANSPORT_DEVICE_HEADER: &str = "x-muqun-device";

pub(crate) const TRANSPORT_ENVELOPE_HEADER: &str = "x-muqun-envelope";

pub(crate) const TRANSPORT_PROOF_HEADER: &str = "x-muqun-internal-device-proof";

/// Base64 without padding, in bytes.
pub(crate) const fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Room for the JSON around a sealed body: the envelope's keys, its nonce and
/// timestamp, the payload's own keys, the bearer token, a content type, and
/// the AEAD tag. Deliberately generous -- it is slack on a ceiling, not a
/// budget anything spends.
pub(crate) const SEALED_SCAFFOLD_BYTES: usize = 4096;

/// What an encrypted request carrying `plaintext` bytes of body weighs on the
/// wire.
///
/// The body is base64'd into a JSON payload, that payload is sealed, and the
/// ciphertext is base64'd again into a JSON envelope -- so what arrives runs
/// about 16/9 of what it carries. The middleware buffers the *outer* bytes, so
/// this, and not the plaintext limit, is what its ceiling has to be.
///
/// It was the upload limit plus `MAX_REQUEST_BODY_BYTES`, the plaintext limits
/// added together, which is both too small and too large. Too small for the
/// route it was sized for: 25 MiB of file is about 44 MiB on the wire, so
/// uploads died above roughly 14 MiB -- and died as a bare `invalid_envelope`,
/// because an over-limit read and a corrupt envelope came back the same way.
/// (The app carries a 10 MiB cap and a comment measuring that cliff at "about
/// 14MB"; this is the gateway end of that workaround.) Too large for every
/// other route, which the router holds to 128 KiB in the clear and which the
/// middleware, sitting outside that limit, let buffer 25 MiB.
pub(crate) const fn sealed_body_ceiling(plaintext: usize) -> usize {
    base64_len(base64_len(plaintext) + SEALED_SCAFFOLD_BYTES) + SEALED_SCAFFOLD_BYTES
}

/// The plaintext a route is allowed to carry, matching what the router's own
/// body limits allow it in the clear.
/// How many bodies above the ordinary 128 KiB limit may be in memory at
/// once, process-wide: two of the biggest kind (an upload, up to 64 MiB
/// plus its sealed envelope) and eight of the rest (a prompt, up to 1 MiB).
/// A body is buffered whole, so without this the peak was one body per open
/// connection; past the permits the answer is `503 busy`, before the read.
const HUGE_BODY_PERMITS: usize = 2;
const LARGE_BODY_PERMITS: usize = 8;

static HUGE_BODIES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(HUGE_BODY_PERMITS);
static LARGE_BODIES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(LARGE_BODY_PERMITS);
// Legacy whole-file upload reads remain compatible up to the advertised file
// maximum. Only one large sealed response may occupy memory while draining;
// ranged reads use the ordinary, much smaller response path instead.
static LARGE_FILE_RESPONSES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The devices with a large body in flight, one each on top of the pool,
/// keyed by device id *and* the peer's IP address: the id is a clear header
/// anyone on the path can copy, and keyed on it alone a stranger's
/// connection could hold the real device's slot; keyed on the whole socket
/// address every new connection would be a new slot.
static DEVICES_WITH_LARGE_BODIES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
/// Large bodies in flight per peer IP address, whatever device ids they
/// name: a copied id from one address claims at most this many.
const MAX_LARGE_BODIES_PER_IP: usize = 2;
static LARGE_BODIES_BY_IP: std::sync::LazyLock<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
static UPLOAD_BODIES_BY_IP: IpCounts =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// The sealed path has to read a body before its envelope can be opened,
/// so that read is made on a clear device header alone. It draws from its
/// own small budgets, never from the pool above: one budget for the uploads
/// route and one for the prompt, command and task routes, so a stranger
/// filling one does not starve the other -- four reads at once in each, two
/// per peer address, and one per clear device id. Once the envelope has
/// opened and the device is verified, the request moves to the pool and
/// the device's slot.
const PREAUTH_SEALED_PERMITS: usize = 4;
const MAX_PREAUTH_SEALED_PER_IP: usize = 2;
static PREAUTH_SEALED_UPLOADS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(PREAUTH_SEALED_PERMITS);
static PREAUTH_SEALED_PROMPTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(PREAUTH_SEALED_PERMITS);
static PREAUTH_UPLOADS_BY_IP: IpCounts =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
static PREAUTH_PROMPTS_BY_IP: IpCounts =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
/// The `device id @ address` keys with a pre-authentication read in flight,
/// one each -- never the clear id alone, which anyone on the path can copy
/// and would make an exclusive slot a targeted lockout.
static PREAUTH_SEALED_BY_DEVICE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// The longest any large body may take to arrive in all.
const LARGE_BODY_READ_TIMEOUT: Duration = Duration::from_secs(180);
/// A body must make progress; the absolute admission deadline bounds trickles.
const LARGE_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A place among the large bodies in flight. Owned by the body it admits
/// ([`GatedBody`]): released when that body is fully consumed or dropped,
/// never earlier.
#[derive(Clone)]
pub(crate) struct LargeBodyPermit {
    /// Held, never read: dropping the last clone gives the place back.
    _place: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

/// A verified device's place: a pool permit, its `(device id, peer IP)`
/// slot and its count against the peer IP.
struct LargeBodyPlace {
    _permit: tokio::sync::SemaphorePermit<'static>,
    slot: String,
    ip: Option<std::net::IpAddr>,
    by_ip: &'static IpCounts,
}

impl Drop for LargeBodyPlace {
    fn drop(&mut self) {
        if let Ok(mut devices) = DEVICES_WITH_LARGE_BODIES.lock() {
            devices.remove(&self.slot);
        }
        if let Some(ip) = self.ip {
            release_count(self.by_ip, ip);
        }
    }
}

/// A pre-authentication sealed read's place: a permit of its route's
/// budget, its count against the peer address and its `device id @
/// address` key.
struct PreauthPlace {
    _permit: tokio::sync::SemaphorePermit<'static>,
    by_ip: &'static IpCounts,
    ip: Option<std::net::IpAddr>,
    device_key: String,
}

impl Drop for PreauthPlace {
    fn drop(&mut self) {
        if let Some(ip) = self.ip {
            release_count(self.by_ip, ip);
        }
        if let Ok(mut devices) = PREAUTH_SEALED_BY_DEVICE.lock() {
            devices.remove(&self.device_key);
        }
    }
}

type IpCounts = std::sync::LazyLock<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>>;

/// The address a peer is counted under: an IPv4 address itself; an IPv6
/// address that carries an IPv4 one -- IPv4-mapped `::ffff:a.b.c.d` (what a
/// dual-stack listener reports for every IPv4 client), IPv4-compatible
/// `::a.b.c.d`, NAT64 `64:ff9b::a.b.c.d` -- by that IPv4 address; and a
/// native IPv6 address by its /64, which is what one subscriber holds. The
/// mask is applied only to native IPv6: masking a mapped address would
/// collapse every IPv4 client into one bucket and make the limits global.
fn counted_address(ip: std::net::IpAddr) -> std::net::IpAddr {
    let std::net::IpAddr::V6(v6) = ip else {
        return ip;
    };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return std::net::IpAddr::V4(v4);
    }
    let segments = v6.segments();
    let embedded = |hi: u16, lo: u16| std::net::Ipv4Addr::from(((hi as u32) << 16) | lo as u32);
    // IPv4-compatible: 96 zero bits then the address (but not `::`/`::1`).
    if segments[..6] == [0; 6] && !v6.is_unspecified() && !v6.is_loopback() {
        return std::net::IpAddr::V4(embedded(segments[6], segments[7]));
    }
    // NAT64 well-known prefix 64:ff9b::/96.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return std::net::IpAddr::V4(embedded(segments[6], segments[7]));
    }
    std::net::IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64)))
}

/// Count `ip` in `counts` unless it already stands at `max`.
fn take_count(counts: &IpCounts, ip: Option<std::net::IpAddr>, max: usize) -> bool {
    let Some(ip) = ip else {
        return true;
    };
    let mut counts = counts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let count = counts.entry(ip).or_insert(0);
    if *count >= max {
        return false;
    }
    *count += 1;
    true
}

fn release_count(counts: &IpCounts, ip: std::net::IpAddr) {
    if let Ok(mut counts) = counts.lock() {
        if let Some(count) = counts.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&ip);
            }
        }
    }
}

fn busy() -> (StatusCode, Json<serde_json::Value>) {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "busy",
        "too many large requests are in flight; try again in a moment",
    )
}

/// The per-device slot of a connection: the device id with the address
/// the request came in from (`ConnectInfo`, as [`counted_address`] counts
/// it; the test harness has none).
fn slot_key(device_id: &str, peer: Option<&SocketAddr>) -> String {
    match peer {
        Some(peer) => format!("{device_id}@{}", counted_address(peer.ip())),
        None => device_id.to_owned(),
    }
}

fn body_slot(plaintext_limit: usize, device_id: &str, peer: Option<&SocketAddr>) -> String {
    let class = if plaintext_limit > 25 * 1024 * 1024 {
        "upload"
    } else {
        "message"
    };
    format!("{class}:{}", slot_key(device_id, peer))
}

/// A place for the pre-authentication read of a sealed body of up to
/// `plaintext_limit` bytes from `peer`, naming the clear `device_id`, or
/// `None` when the route takes only the ordinary limit. From the route's
/// small budget, never the pool. `Err`: that budget is spent, in all, for
/// this address, or for this device id.
pub(crate) fn preauth_sealed_permit(
    plaintext_limit: usize,
    peer: Option<&SocketAddr>,
    device_id: &str,
) -> Result<Option<LargeBodyPermit>, (StatusCode, Json<serde_json::Value>)> {
    if plaintext_limit <= MAX_REQUEST_BODY_BYTES {
        return Ok(None);
    }
    let (budget, by_ip): (&'static tokio::sync::Semaphore, &'static IpCounts) =
        if plaintext_limit > 25 * 1024 * 1024 {
            (&PREAUTH_SEALED_UPLOADS, &PREAUTH_UPLOADS_BY_IP)
        } else {
            (&PREAUTH_SEALED_PROMPTS, &PREAUTH_PROMPTS_BY_IP)
        };
    // The permit is dropped with `permit` on every refusal below.
    let permit = budget.try_acquire().map_err(|_| busy())?;
    let ip = peer.map(|peer| counted_address(peer.ip()));
    if !take_count(by_ip, ip, MAX_PREAUTH_SEALED_PER_IP) {
        return Err(busy());
    }
    let device_key = body_slot(plaintext_limit, device_id, peer);
    let taken = PREAUTH_SEALED_BY_DEVICE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(device_key.clone());
    if !taken {
        if let Some(ip) = ip {
            release_count(by_ip, ip);
        }
        return Err(busy());
    }
    Ok(Some(LargeBodyPermit {
        _place: std::sync::Arc::new(PreauthPlace {
            _permit: permit,
            by_ip,
            ip,
            device_key,
        }),
    }))
}

/// A permit for a body of up to `plaintext_limit` bytes on this route from
/// the verified device `device_id` on the connection from `peer`, or
/// `None` when the route takes only the ordinary limit. Taken only once the
/// device is verified -- the opened envelope's token against its record on
/// the sealed path, `require_device_or_peer` on the cleartext one -- so no
/// unidentified connection ever holds a place here. The pool is taken
/// first, then the address's count, then the device's slot. `Err`: any of
/// them is spent.
pub(crate) fn large_body_permit(
    plaintext_limit: usize,
    device_id: &str,
    peer: Option<&SocketAddr>,
) -> Result<Option<LargeBodyPermit>, (StatusCode, Json<serde_json::Value>)> {
    if plaintext_limit <= MAX_REQUEST_BODY_BYTES {
        return Ok(None);
    }
    let (gate, by_ip) = if plaintext_limit > 25 * 1024 * 1024 {
        (&HUGE_BODIES, &UPLOAD_BODIES_BY_IP)
    } else {
        (&LARGE_BODIES, &LARGE_BODIES_BY_IP)
    };
    // The pool permit is dropped with `permit` on every refusal below.
    let permit = gate.try_acquire().map_err(|_| busy())?;
    let ip = peer.map(|peer| counted_address(peer.ip()));
    if !take_count(by_ip, ip, MAX_LARGE_BODIES_PER_IP) {
        return Err(busy());
    }
    let slot = body_slot(plaintext_limit, device_id, peer);
    let taken = DEVICES_WITH_LARGE_BODIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(slot.clone());
    if !taken {
        if let Some(ip) = ip {
            release_count(by_ip, ip);
        }
        return Err(busy());
    }
    Ok(Some(LargeBodyPermit {
        _place: std::sync::Arc::new(LargeBodyPlace {
            _permit: permit,
            slot,
            ip,
            by_ip,
        }),
    }))
}

/// The address a request's connection came in from, when the listener
/// recorded it (`into_make_service_with_connect_info`).
fn peer_of(extensions: &axum::http::Extensions) -> Option<SocketAddr> {
    extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
}

/// `body` with `permit` bound to it: the place is given up when the body
/// is fully consumed or dropped. The body is bounded in time as well: it
/// ends at its admission deadline or after [`LARGE_BODY_IDLE_TIMEOUT`] with
/// no nonempty data frame. Slow continuous progress has no minimum byte rate.
pub(crate) fn gated_body(body: Body, permit: Option<LargeBodyPermit>) -> Body {
    match permit {
        Some(permit) => {
            let now = tokio::time::Instant::now();
            Body::from_stream(GatedBody {
                inner: body.into_data_stream(),
                deadline: now + LARGE_BODY_READ_TIMEOUT,
                last_progress: now,
                timer: Box::pin(tokio::time::sleep_until(now + LARGE_BODY_IDLE_TIMEOUT)),
                _permit: permit,
            })
        }
        None => body,
    }
}

struct GatedBody {
    inner: axum::body::BodyDataStream,
    deadline: tokio::time::Instant,
    last_progress: tokio::time::Instant,
    /// Fires at the idle deadline or absolute deadline, whichever is first.
    timer: std::pin::Pin<Box<tokio::time::Sleep>>,
    _permit: LargeBodyPermit,
}

impl GatedBody {
    /// A stalled body expires independently of the absolute admission budget.
    fn judge(&mut self, now: tokio::time::Instant) -> Result<(), axum::BoxError> {
        if now >= self.deadline {
            return Err(Box::new(BodyReadTimeout));
        }
        if now >= self.last_progress + LARGE_BODY_IDLE_TIMEOUT {
            return Err(Box::new(BodyReadTimeout));
        }
        let next = (self.last_progress + LARGE_BODY_IDLE_TIMEOUT).min(self.deadline);
        self.timer.as_mut().reset(next);
        Ok(())
    }
}

impl futures::Stream for GatedBody {
    type Item = Result<axum::body::Bytes, axum::BoxError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future as _;
        use std::task::Poll;
        let now = tokio::time::Instant::now();
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if !bytes.is_empty() {
                    self.last_progress = now;
                }
                if let Err(error) = self.judge(now) {
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if self.timer.as_mut().poll(cx).is_ready() {
                    if let Err(error) = self.judge(now) {
                        return Poll::Ready(Some(Err(error)));
                    }
                    // Re-armed by `judge`: register the new timer.
                    if self.timer.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Some(Err(Box::new(BodyReadTimeout))));
                    }
                }
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
struct BodyReadTimeout;

impl std::fmt::Display for BodyReadTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the request body was not received in time")
    }
}

impl std::error::Error for BodyReadTimeout {}

fn error_has<E: std::error::Error + 'static>(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if error.is::<E>() {
            return true;
        }
        source = error.source();
    }
    false
}

pub(crate) fn body_read_timed_out(error: &(dyn std::error::Error + 'static)) -> bool {
    error_has::<BodyReadTimeout>(error)
}

fn body_read_error(error: axum::Error) -> (StatusCode, Json<serde_json::Value>) {
    if body_read_timed_out(&error) {
        api_error(
            StatusCode::REQUEST_TIMEOUT,
            "body_timeout",
            "the request body was not received in time",
        )
    } else if error_has::<http_body_util::LengthLimitError>(&error) {
        api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "the request body is too large",
        )
    } else {
        api_error(
            StatusCode::BAD_REQUEST,
            "body_read_failed",
            "the request body could not be received",
        )
    }
}

pub(crate) fn plaintext_body_limit(path: &str) -> usize {
    if path == UPLOADS_PATH {
        super::uploads::MAX_UPLOAD_BODY_BYTES
    } else if crate::platform::config::takes_large_body(path) {
        crate::MAX_PROMPT_BODY_BYTES
    } else {
        MAX_REQUEST_BODY_BYTES
    }
}

/// On the routes that take a larger body (`config::takes_large_body`),
/// every request is authenticated here, before any extractor reads its
/// body: a caller without a device token is refused on its headers alone
/// and never has a megabyte buffered for it. The handlers authenticate
/// again themselves, as they always did.
///
/// This runs inside `encrypted_transport`, which has by now opened a
/// sealed request -- its body read under the route's sealed ceiling, as an
/// upload's is -- and put the device's bearer token and proof on it, so
/// the one check here reads the same headers for either transport and
/// never looks at the transport header itself.
pub(crate) async fn large_body_after_auth(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let plaintext_limit = plaintext_body_limit(request.uri().path());
    if plaintext_limit <= MAX_REQUEST_BODY_BYTES {
        return next.run(request).await;
    }
    let caller = match require_device_or_peer(&state, request.headers()) {
        Ok(caller) => caller,
        Err(refusal) => return refusal.into_response(),
    };
    // A sealed request holds its place from the decryption path, bound to
    // its opened body; a cleartext one takes it here, past authentication,
    // bound to the body the handler is about to read, so it is given up
    // only when that body is consumed or dropped.
    if request
        .extensions()
        .get::<EncryptedStreamContext>()
        .is_some()
    {
        return next.run(request).await;
    }
    let peer = peer_of(request.extensions());
    let permit = match large_body_permit(plaintext_limit, &caller.device_id, peer.as_ref()) {
        Ok(permit) => permit,
        Err(refusal) => return refusal.into_response(),
    };
    let (parts, body) = request.into_parts();
    next.run(Request::from_parts(parts, gated_body(body, permit)))
        .await
}

#[derive(Serialize, Deserialize)]
pub(crate) struct EncryptedRequestPayload {
    pub(crate) token: String,
    #[serde(default)]
    pub(crate) content_type: Option<String>,
    pub(crate) body: String,
}

#[derive(Serialize)]
pub(crate) struct EncryptedResponsePayload {
    pub(crate) status: u16,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: String,
    /// How `body` is encoded, when it is not plain bytes.
    ///
    /// A top-level field rather than a line in `headers`, because it describes
    /// the sealed payload itself and not the response the client is
    /// reconstructing -- and because a client has to be able to find it
    /// without case-folding its way through a header map. Absent means the
    /// body is the response body. The only value ever sent is `gzip`, and it
    /// is sent only to a client that asked for it by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_encoding: Option<String>,
}

/// What an encrypted-transport request leaves behind for a handler that
/// answers with a stream instead of a finite body. A whole-response envelope
/// cannot authenticate a response that never ends, so the events handler
/// seals each event on its own -- and this is the key material and request
/// binding it seals under. Injected by `decrypt_transport_request`, so its
/// presence also proves the request itself authenticated.
#[derive(Clone)]
pub(crate) struct EncryptedStreamContext {
    pub(crate) material: Vec<u8>,
    pub(crate) request_aad: String,
    pub(crate) request_nonce: String,
    pub(crate) transport_version: u8,
}

/// Decrypt an authenticated request before Axum extractors see it, then seal
/// the complete response. Route names and byte counts remain HTTP metadata;
/// credentials and application payloads do not.
pub(crate) async fn encrypted_transport(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // The proof header is this middleware's own signal to the handlers below:
    // "this request really arrived sealed, and here is the key it was sealed
    // with". Only the decryption path may set it. A client could otherwise
    // send it itself on the cleartext path and be taken for the encrypted
    // device it is claiming to be -- and, worse, would have put the transport
    // key on the wire in the clear to do so, which is the one thing the
    // encrypted transport exists to prevent. Strip it on the way in, always,
    // before anything else looks at the request.
    let mut request = request;
    request.headers_mut().remove(TRANSPORT_PROOF_HEADER);

    // Absent: cleartext. `1`: sealed. Anything else is refused outright,
    // so no other layer can read the header differently from this one.
    match request.headers().get(TRANSPORT_HEADER) {
        None => return next.run(request).await,
        Some(value) if value.as_bytes() == b"1" => {}
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_transport",
                "unsupported transport header",
            )
            .into_response();
        }
    }

    let websocket = is_websocket_upgrade(request.headers());
    match decrypt_transport_request(&state, request).await {
        Ok((request, material, aad, request_nonce)) => {
            let upload_read = request.method() == axum::http::Method::GET
                && request.uri().path().starts_with("/api/uploads/");
            let version = request
                .extensions()
                .get::<EncryptedStreamContext>()
                .expect("decrypted context")
                .transport_version;
            let response = next.run(request).await;
            // A WebSocket upgrade has no body to seal: the 101 hands the
            // connection to the socket handler, which seals every frame under
            // the stream context injected above (see `agents::ws_routes`).
            // Sealing it here would buffer an empty body and replace the 101
            // with a 200 the client cannot upgrade on. A refusal (401, 403,
            // 429) is an ordinary response and is sealed like any other.
            if websocket && response.status() == StatusCode::SWITCHING_PROTOCOLS {
                let mut response = response;
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(TRANSPORT_HEADER),
                    HeaderValue::from_static("1"),
                );
                return response;
            }
            // An event stream never ends, so it cannot ride the one-envelope
            // response path -- buffering it here would simply hang the
            // connection. The events handler has already sealed every event
            // individually under the stream context injected above, so the
            // response passes through as standard SSE.
            if response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                let mut response = response;
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(TRANSPORT_HEADER),
                    HeaderValue::from_static("1"),
                );
                return response;
            }
            encrypt_transport_response(
                response,
                &material,
                &aad,
                &request_nonce,
                version,
                upload_read,
            )
            .await
        }
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn decrypt_transport_request(
    state: &AppState,
    request: Request<Body>,
) -> ApiResult<(Request<Body>, Vec<u8>, String, String)> {
    let (mut parts, body) = request.into_parts();
    let device_id = parts
        .headers
        .get(TRANSPORT_DEVICE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 80)
        .ok_or_else(|| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "missing_transport_device",
                "missing encrypted transport device",
            )
        })?
        .to_owned();
    let device_id = device_id.as_str();
    let (token_hash, transport_key) = {
        let devices = lock_devices(state)?;
        let device = devices
            .iter()
            .find(|device| device.id == device_id)
            .ok_or_else(|| api_error(StatusCode::FORBIDDEN, "invalid_token", "invalid token"))?;
        let transport_key = device
            .transport_key
            .clone()
            .ok_or_else(device_repair_required)?;
        (device.token_hash.clone(), transport_key)
    };
    let material = transport::decode_key(&transport_key).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "transport_key_unavailable",
            "encrypted transport is unavailable",
        )
    })?;
    let aad = format!(
        "{} {}",
        parts.method,
        parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), |value| value.as_str())
    );
    // Sized to the route, because this read happens outside the router's own
    // body limits and is therefore the only thing bounding them. A large one
    // takes a place in the pre-authentication budget first -- the header
    // above showed a paired record holding a key, nothing more yet -- or is
    // refused before a byte of it is read, and the read is bounded in time
    // and throughput as well as size so a stalled connection gives the
    // place back.
    let plaintext_limit = plaintext_body_limit(parts.uri.path());
    let peer = peer_of(&parts.extensions);
    // Not the pool: the small pre-authentication budget, until the envelope
    // has opened and the device is verified below.
    let preauth = preauth_sealed_permit(plaintext_limit, peer.as_ref(), device_id)?;
    let large = preauth.is_some();
    let body = gated_body(body, preauth);
    let read = to_bytes(body, sealed_body_ceiling(plaintext_limit));
    let body_bytes = if large {
        tokio::time::timeout(LARGE_BODY_READ_TIMEOUT, read)
            .await
            .map_err(|_| {
                api_error(
                    StatusCode::REQUEST_TIMEOUT,
                    "body_timeout",
                    "the request body was not received in time",
                )
            })?
    } else {
        read.await
    }
    .map_err(body_read_error)?;
    let envelope_bytes = match parts.headers.get(TRANSPORT_ENVELOPE_HEADER) {
        Some(value) => transport::decode_key(value.to_str().unwrap_or_default()).map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_envelope",
                "invalid encrypted request",
            )
        })?,
        None => body_bytes.to_vec(),
    };
    let envelope: transport::Envelope = serde_json::from_slice(&envelope_bytes).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let plaintext = transport::open_checked(
        &material,
        transport::Direction::Request,
        aad.as_bytes(),
        &envelope,
        now_unix_ms(),
    )
    .map_err(|error| match error {
        transport::OpenError::Expired => clock_skew(now_unix_ms()),
        transport::OpenError::Invalid => api_error(
            StatusCode::FORBIDDEN,
            "invalid_envelope",
            "invalid encrypted request",
        ),
    })?;
    let payload: EncryptedRequestPayload = serde_json::from_slice(&plaintext).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid request",
        )
    })?;
    if hash_token(&payload.token) != token_hash {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }
    // Only authenticated envelopes enter the replay cache. Otherwise anyone
    // who knows a device id could fill it with arbitrary nonces.
    remember_transport_nonce(
        state,
        device_id,
        &token_hash,
        &transport_key,
        &envelope,
        now_unix_ms,
    )
    .await?;
    let body = transport::decode_key(&payload.body).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid request",
        )
    })?;
    parts.headers.remove(TRANSPORT_HEADER);
    parts.headers.remove(TRANSPORT_DEVICE_HEADER);
    parts.headers.remove(TRANSPORT_ENVELOPE_HEADER);
    parts.headers.insert(
        axum::http::HeaderName::from_static(TRANSPORT_PROOF_HEADER),
        HeaderValue::from_str(&transport_key).map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
        })?,
    );
    parts.headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", payload.token))
            .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid_token", "invalid token"))?,
    );
    if let Some(content_type) = payload.content_type {
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type).map_err(|_| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_content_type",
                    "invalid content type",
                )
            })?,
        );
    } else {
        parts.headers.remove(axum::http::header::CONTENT_TYPE);
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.extensions.insert(EncryptedStreamContext {
        material: material.clone(),
        request_aad: aad.clone(),
        request_nonce: envelope.nonce.clone(),
        transport_version: envelope.version,
    });
    // Verified now: the request moves from the pre-authentication budget
    // (given up with the read above) to the pool and this device's slot,
    // which the opened body keeps until the handler has consumed it.
    let permit = large_body_permit(plaintext_limit, device_id, peer.as_ref())?;
    Ok((
        Request::from_parts(parts, gated_body(Body::from(body), permit)),
        material,
        aad,
        envelope.nonce,
    ))
}

async fn remember_transport_nonce<F>(
    state: &AppState,
    device_id: &str,
    token_hash: &str,
    transport_key: &str,
    envelope: &transport::Envelope,
    clock: F,
) -> ApiResult<()>
where
    F: FnOnce() -> u128 + Send + 'static,
{
    use crate::connectivity::replay_store::{self, Acceptance};
    let nonce = transport::nonce_bytes(&envelope.nonce).map_err(|_| {
        api_error(
            StatusCode::FORBIDDEN,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let credential_hash = hash_token(&format!("{token_hash}\n{transport_key}"));
    let accepted = replay_store::accept(
        &state.db,
        device_id.to_owned(),
        credential_hash,
        nonce,
        envelope.version,
        envelope.timestamp_ms,
        clock,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "transport replay store unavailable");
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "replay_store_unavailable",
            "encrypted request could not be safely recorded",
        )
    })?;
    match accepted {
        Acceptance::Accepted => Ok(()),
        // Freshness is judged again on the store thread (the request may
        // have waited in its queue past the window); the envelope
        // authenticated above, so a stale one is told the same as at open.
        Acceptance::InvalidMetadata => {
            let now = now_unix_ms();
            Err(
                if transport::timestamp_is_fresh(envelope.timestamp_ms, now) {
                    api_error(
                        StatusCode::FORBIDDEN,
                        "invalid_envelope",
                        "invalid encrypted request",
                    )
                } else {
                    clock_skew(now)
                },
            )
        }
        Acceptance::Replayed => Err(api_error(
            StatusCode::CONFLICT,
            "replayed_request",
            "request was already used",
        )),
        Acceptance::VersionRequired => Err(api_error(
            StatusCode::UPGRADE_REQUIRED,
            "transport_version_required",
            "This device requires encrypted transport version 2",
        )),
        Acceptance::Capacity if envelope.version == transport::VERSION => Err(api_error(
            StatusCode::UPGRADE_REQUIRED,
            "transport_version_required",
            "Legacy request history is full; upgrade to encrypted transport version 2",
        )),
        Acceptance::Capacity => Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "transport_replay_capacity",
            "Encrypted request history is full; wait for current requests to expire",
        )),
    }
}

pub(crate) async fn encrypt_transport_response(
    response: Response,
    material: &[u8],
    aad: &str,
    request_nonce: &str,
    version: u8,
    upload_read: bool,
) -> Response {
    let (parts, body) = response.into_parts();
    let ordinary_limit = MAX_VIDEO_CONTENT_BYTES as usize + MAX_REQUEST_BODY_BYTES;
    let large_file = upload_read
        && parts
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > ordinary_limit as u64);
    let permit = if large_file {
        match LARGE_FILE_RESPONSES.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => return busy().into_response(),
        }
    } else {
        None
    };
    let response_limit = if large_file {
        MAX_VIDEO_UPLOAD_BYTES
    } else {
        ordinary_limit
    };
    // A response is buffered as plaintext and sealed afterwards, so the bound
    // is on what a handler can produce: an asset preview is the largest, and
    // every other route answers JSON well under the request limit.
    let body = match to_bytes(body, response_limit).await {
        Ok(body) => body,
        Err(error) => {
            let code = if error_has::<http_body_util::LengthLimitError>(&error) {
                "response_too_large"
            } else {
                "response_read_failed"
            };
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                code,
                "failed to read encrypted response",
            )
            .into_response();
        }
    };
    // `content-encoding` describes the bytes being sealed, not the response
    // the client rebuilds -- leaving it in the header map would have the
    // client try to inflate a body it had already inflated. It moves to the
    // payload's own field.
    let content_encoding = parts
        .headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| *name != axum::http::header::CONTENT_ENCODING)
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let payload = EncryptedResponsePayload {
        status: parts.status.as_u16(),
        headers,
        body: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body),
        content_encoding,
    };
    let plaintext = match serde_json::to_vec(&payload) {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let response_aad = format!("{aad}\n{request_nonce}");
    let envelope = match transport::seal_version(
        version,
        material,
        transport::Direction::Response,
        response_aad.as_bytes(),
        &plaintext,
        now_unix_ms(),
    ) {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut response = Json(envelope).into_response();
    response.headers_mut().insert(
        axum::http::HeaderName::from_static(TRANSPORT_HEADER),
        HeaderValue::from_static("1"),
    );
    if let Some(permit) = permit {
        let (parts, body) = response.into_parts();
        let bytes = match to_bytes(body, sealed_body_ceiling(response_limit)).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let stream = async_stream::stream! {
            let _permit = permit;
            for offset in (0..bytes.len()).step_by(super::assets::ASSET_CONTENT_CHUNK_BYTES) {
                let end = (offset + super::assets::ASSET_CONTENT_CHUNK_BYTES).min(bytes.len());
                yield Ok::<_, std::convert::Infallible>(bytes.slice(offset..end));
            }
        };
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        response
    }
}

/// Refuse a request that arrived under a name this gateway does not answer to.
///
/// The attack this closes is DNS rebinding, and it is the one thing a bearer
/// token does not stop. A page the user opens serves itself from a name the
/// attacker controls, that name is re-resolved to the gateway's address, and
/// from then on the browser considers the page *same-origin with the gateway*.
/// Same-origin means the same-origin policy is no longer in the way: no
/// preflight, and the script reads every response. It has no device token, so
/// the control routes still refuse it -- but the pairing routes are open by
/// necessity, and from there it can read the machine's label, occupy the single
/// pending-pairing slot for five minutes, burn the request budget for ten, and
/// put a name it chose in front of whoever is watching the manager panel.
///
/// The tell is the `Host` header: it carries the attacker's own name, because
/// that is the name the page was fetched from. The gateway knows which names
/// are its own, so it can simply not answer to any other.
///
/// Deliberately generous, because refusing a request the owner meant is worse
/// than the attack. An address literal always passes -- rebinding needs a name
/// whose resolution can be flipped, and a page served from a bare IP is already
/// same-origin with nothing but this gateway. `localhost` and any `.ts.net`
/// name pass, since the tailnet's names are Tailscale's to hand out and not an
/// attacker's. Everything else has to be the host of the configured public URL
/// or of the listen address. A refusal says so in the log, because a name the
/// owner reaches their own gateway by and this gateway has never been told
/// about should be a line to read, not a mystery.
pub(crate) async fn known_host(
    State(known): State<Vec<String>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().host().map(str::to_owned));
    if let Some(host) = host.as_deref() {
        if !host_is_known(host, &known) {
            tracing::warn!(
                "refused a request addressed to {host}: not a name this gateway answers to \
                 (known: {})",
                known.join(", ")
            );
            return api_error(
                StatusCode::FORBIDDEN,
                "unknown_host",
                "this gateway does not answer to that host name",
            )
            .into_response();
        }
    }
    next.run(request).await
}

/// The names configuration says this gateway is reached by.
pub(crate) fn known_hosts(config: &Config) -> Vec<String> {
    let mut hosts = vec![String::from("localhost")];
    if let Some(host) = reqwest::Url::parse(&config.public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    {
        hosts.push(host_name(&host));
    }
    hosts.push(host_name(&config.listen));
    hosts.retain(|host| !host.is_empty());
    hosts.sort();
    hosts.dedup();
    hosts
}

pub(crate) fn host_is_known(host: &str, known: &[String]) -> bool {
    let name = host_name(host);
    if name.is_empty() {
        return true;
    }
    // Rebinding needs a name. An address is not one, and a page served from an
    // address literal is same-origin with this gateway and nothing else.
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    // MagicDNS names live under a zone Tailscale hands out, not an attacker.
    if name.ends_with(".ts.net") {
        return true;
    }
    known.iter().any(|candidate| candidate == &name)
}

/// A `Host` header, or a `host:port` from configuration, reduced to the name.
pub(crate) fn host_name(host: &str) -> String {
    let host = host.trim();
    // A bracketed IPv6 literal fences the colons of the address itself.
    if let Some(rest) = host.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    // An unbracketed address is not a legal `Host`, but reading one as a name
    // with a port would cut `::1` down to `::`, so it is taken whole.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_ascii_lowercase();
    }
    host.rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(host, |(name, _)| name)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Bodies below this are sent as they are: a gzip header, trailer and the
/// `content-encoding` line cost more than they save, and every one of them is
/// a byte on a phone's radio too.
pub(crate) const COMPRESSION_MIN_BYTES: u64 = 512;

/// Keep compression away from the sealed envelope, and say that the answer
/// varies by what the client will accept.
///
/// A response that is about to be sealed must not also be compressed yet. The
/// envelope carries its own headers inside the ciphertext, so a
/// `content-encoding: gzip` on a body the client sees as base64 would have it
/// try to inflate the ciphertext. Compressing inside the envelope is worth
/// doing -- it is the one place the gateway's own inflation can be paid back
/// -- but it needs the client to say it understands the flag, and until it
/// does, an encrypted request is answered exactly as it is today.
///
/// Whether the request arrived encrypted is read from the stream context the
/// decryption path injects, not from a header, because a header can be sent by
/// anyone.
pub(crate) async fn envelope_compression_gate(mut request: Request<Body>, next: Next) -> Response {
    let sealed = request
        .extensions()
        .get::<EncryptedStreamContext>()
        .is_some();
    if is_websocket_upgrade(request.headers()) {
        // A 101 carries no body, and the connection it hands over is not HTTP
        // any more: a `content-encoding` on it would be a lie about bytes the
        // compressor never sees. Taking the client's list away makes the
        // layer answer identity whatever it would otherwise have decided.
        request
            .headers_mut()
            .remove(axum::http::header::ACCEPT_ENCODING);
    } else if sealed {
        // Inside the envelope the client cannot use `content-encoding` -- it
        // is reading a base64 body, and the real headers are sealed with it --
        // so it says separately what it can inflate. Only gzip, and only when
        // asked for by name: a client that says nothing gets exactly what it
        // got before this existed.
        let opted_in = request
            .headers()
            .get(ENVELOPE_ACCEPT_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("gzip"))
            });
        if opted_in {
            // Pinned to gzip rather than passed through: br is the other thing
            // the layer can produce, and a client that asked for gzip must not
            // be handed something it will refuse.
            request.headers_mut().insert(
                axum::http::header::ACCEPT_ENCODING,
                HeaderValue::from_static("gzip"),
            );
        } else {
            request
                .headers_mut()
                .remove(axum::http::header::ACCEPT_ENCODING);
        }
    }
    let mut response = next.run(request).await;
    // Said whether or not this particular answer was compressed: a cache that
    // holds one must not serve it to a client that asked differently. A route
    // that has already named what it varies by keeps it -- the validated
    // routes name the locale too -- and this only makes sure encoding is in
    // the list.
    let headers = response.headers_mut();
    let existing = headers
        .get(axum::http::header::VARY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match existing {
        Some(existing) if existing.to_ascii_lowercase().contains("accept-encoding") => {}
        Some(existing) => {
            if let Ok(value) = HeaderValue::from_str(&format!("{existing}, accept-encoding")) {
                headers.insert(axum::http::header::VARY, value);
            }
        }
        None => {
            headers.insert(
                axum::http::header::VARY,
                HeaderValue::from_static("accept-encoding"),
            );
        }
    }
    response
}

/// Whether a request asks to become a WebSocket (`Upgrade: websocket`).
pub(crate) fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"))
}

pub(crate) async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A floor, not an override: a handler that has something more specific to
    // say about its own body keeps it. `upload_content` is the one that does,
    // and it only ever adds to this -- `private` on top of `no-store`. Nothing
    // here can weaken the blanket, because a handler that says nothing gets it.
    if !headers.contains_key("cache-control") {
        headers.insert(
            "cache-control",
            HeaderValue::from_static("no-store, max-age=0"),
        );
    }
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

/// Log the process's own peak and current resident set, the cgroup's peak, and
/// hand free heap pages back to the OS, every ten minutes.
///
/// The systemd unit reports one `MemoryPeak` for everything the cgroup ever
/// ran, and that number cannot say whether a spike was this process, a
/// transient allocation inside it, or something else the unit started. A
/// timestamped series of `max_rss_mb` (this process's high-water mark) beside
/// `cgroup_peak_mb` (the unit's) is what tells the two apart the next time the
/// journal shows a spike. `rss_before_mb`/`rss_after_mb` straddle the trim, so
/// the same line also says how much was allocator retention rather than live
/// data.
fn spawn_memory_watchdog() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let max_rss_mb = process_peak_rss_mb();
            let rss_before_mb = process_rss_mb();
            trim_allocator();
            let rss_after_mb = process_rss_mb();
            if max_rss_mb.is_some() || rss_before_mb.is_some() {
                tracing::info!(
                    max_rss_mb = max_rss_mb.unwrap_or(0),
                    rss_before_mb = rss_before_mb.unwrap_or(0),
                    rss_after_mb = rss_after_mb.unwrap_or(0),
                    cgroup_peak_mb = cgroup_memory_mb("memory.peak").unwrap_or(0),
                    cgroup_current_mb = cgroup_memory_mb("memory.current").unwrap_or(0),
                    cgroup_anon_mb = cgroup_stat_mb("anon").unwrap_or(0),
                    cgroup_slab_mb = cgroup_stat_mb("slab").unwrap_or(0),
                    "gateway memory"
                );
            }
        }
    });
}

/// Hand free heap pages back to the OS.
///
/// glibc keeps freed memory in per-thread arenas: one burst of large
/// allocations (a 25 MiB upload, a sealed envelope) leaves each arena's
/// high-water mark in RSS long after the buffers are gone. `malloc_trim`
/// returns what is free at the top of each arena. Linux/glibc only -- musl
/// has no arenas, and macOS has its own allocator, so neither needs this.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_allocator() {
    // Safety: `malloc_trim` only walks the allocator's own free lists.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_allocator() {}

/// This process's current resident set, in MiB.
#[cfg(target_os = "linux")]
fn process_rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?;
        value
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb / 1024)
    })
}

#[cfg(not(target_os = "linux"))]
fn process_rss_mb() -> Option<u64> {
    None
}

/// This process's high-water resident set, in MiB.
///
/// `/proc/self/status` is Linux-only; on other platforms the line is simply
/// absent and the cgroup figure, where the init system has one, still speaks.
#[cfg(target_os = "linux")]
fn process_peak_rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmHWM:")?;
        value
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb / 1024)
    })
}

#[cfg(not(target_os = "linux"))]
fn process_peak_rss_mb() -> Option<u64> {
    None
}

/// This process's own cgroup v2 directory, if it is in one.
///
/// `None` outside a cgroup v2 host -- macOS, or a Linux session started
/// without an init system.
fn cgroup_dir() -> Option<String> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    Some(format!(
        "/sys/fs/cgroup{}",
        membership.trim().strip_prefix("0::")?
    ))
}

/// One cgroup v2 memory file for this process's own cgroup, in MiB.
fn cgroup_memory_mb(file: &str) -> Option<u64> {
    let bytes = std::fs::read_to_string(format!("{}/{file}", cgroup_dir()?))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(bytes / 1024 / 1024)
}

/// One key from this cgroup's `memory.stat`, in MiB.
///
/// `anon` is what the gateway's heap actually holds; `slab` is kernel memory
/// charged to the cgroup (dentry/inode caches from scanning, mostly
/// reclaimable). Logging both is what separates a process-heap spike from the
/// kernel's own accounting the next time `memory.peak` jumps.
fn cgroup_stat_mb(key: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("{}/memory.stat", cgroup_dir()?)).ok()?;
    stat.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        if name != key {
            return None;
        }
        value.parse::<u64>().ok().map(|bytes| bytes / 1024 / 1024)
    })
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn upload_admission_does_not_take_the_devices_interactive_slot() {
        let id = uuid::Uuid::new_v4().to_string();
        let upload =
            super::preauth_sealed_permit(platform::uploads::MAX_UPLOAD_BODY_BYTES, None, &id)
                .unwrap();
        assert!(
            super::preauth_sealed_permit(platform::uploads::MAX_UPLOAD_BODY_BYTES, None, &id)
                .is_err()
        );
        let message = super::preauth_sealed_permit(MAX_PROMPT_BODY_BYTES, None, &id).unwrap();
        drop((upload, message));
        let upload =
            super::large_body_permit(platform::uploads::MAX_UPLOAD_BODY_BYTES, &id, None).unwrap();
        assert!(
            super::large_body_permit(platform::uploads::MAX_UPLOAD_BODY_BYTES, &id, None).is_err()
        );
        let message = super::large_body_permit(MAX_PROMPT_BODY_BYTES, &id, None).unwrap();
        drop((upload, message));
        assert!(
            super::large_body_permit(platform::uploads::MAX_UPLOAD_BODY_BYTES, &id, None).is_ok()
        );
    }

    #[tokio::test]
    async fn body_deadlines_size_and_broken_reads_have_distinct_refusals() {
        use axum::body::{to_bytes, Body, Bytes};
        let stream = futures::stream::once(async { Err::<Bytes, _>(super::BodyReadTimeout) });
        let error = to_bytes(Body::from_stream(stream), 100).await.unwrap_err();
        assert_eq!(super::body_read_error(error).0, StatusCode::REQUEST_TIMEOUT);
        let error = to_bytes(Body::from("too long"), 1).await.unwrap_err();
        assert_eq!(
            super::body_read_error(error).0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let stream = futures::stream::once(async {
            Err::<Bytes, _>(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
        });
        let error = to_bytes(Body::from_stream(stream), 100).await.unwrap_err();
        assert_eq!(super::body_read_error(error).0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_progress_is_not_a_rate_failure_but_idle_and_total_remain_bounded() {
        use axum::body::{to_bytes, Body, Bytes};
        let permit = || {
            Some(super::LargeBodyPermit {
                _place: std::sync::Arc::new(()),
            })
        };
        let progress = || {
            Body::from_stream(async_stream::stream! {
                for _ in 0..400 {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    yield Ok::<_, std::io::Error>(Bytes::from_static(b"x"));
                }
            })
        };
        let body = super::gated_body(
            Body::from_stream(async_stream::stream! {
                for _ in 0..12 {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    yield Ok::<_, std::io::Error>(Bytes::from_static(b"x"));
                }
            }),
            permit(),
        );
        assert_eq!(to_bytes(body, 1000).await.unwrap().len(), 12);
        let body = super::gated_body(
            Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>()),
            permit(),
        );
        assert_eq!(
            super::body_read_error(to_bytes(body, 1000).await.unwrap_err()).0,
            StatusCode::REQUEST_TIMEOUT
        );
        let body = super::gated_body(progress(), permit());
        assert_eq!(
            super::body_read_error(to_bytes(body, 1000).await.unwrap_err()).0,
            StatusCode::REQUEST_TIMEOUT
        );
    }

    #[tokio::test]
    async fn accepted_video_readback_is_sealed_and_holds_its_budget_until_drained() {
        use axum::body::{to_bytes, Body};
        use base64::Engine as _;
        let size = 33 * 1024 * 1024;
        let response = axum::response::Response::builder()
            .header("content-length", size)
            .body(Body::from(vec![7; size]))
            .unwrap();
        let key = [19; 32];
        let response = super::encrypt_transport_response(
            response,
            &key,
            "GET /api/uploads/fixture.mp4",
            "fixture-nonce",
            2,
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            super::LARGE_FILE_RESPONSES.try_acquire().is_err(),
            "buffer budget belongs to the response body"
        );
        let bytes = to_bytes(
            response.into_body(),
            super::sealed_body_ceiling(platform::uploads::MAX_VIDEO_UPLOAD_BYTES),
        )
        .await
        .unwrap();
        assert!(super::LARGE_FILE_RESPONSES.try_acquire().is_ok());
        let envelope: transport::Envelope = serde_json::from_slice(&bytes).unwrap();
        let plain = transport::open(
            &key,
            transport::Direction::Response,
            b"GET /api/uploads/fixture.mp4\nfixture-nonce",
            &envelope,
            now_unix_ms(),
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(payload["status"], 200);
        let file = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload["body"].as_str().unwrap())
            .unwrap();
        assert_eq!(file.len(), size);
        assert!(file.iter().all(|byte| *byte == 7));
    }

    /// What a dual-stack listener reports for an IPv4 client is the mapped
    /// `::ffff:a.b.c.d`; masking it to a /64 like native IPv6 would put every
    /// IPv4 client in one bucket and make the per-address limits global.
    /// Mapped, compatible and NAT64 addresses count as their IPv4; native
    /// IPv6 counts by its /64.
    #[test]
    fn ipv4_clients_behind_a_dual_stack_listener_are_counted_apart() {
        use std::net::IpAddr;
        let counted = |text: &str| super::counted_address(text.parse::<IpAddr>().unwrap());
        assert_eq!(
            counted("::ffff:10.0.0.7"),
            "10.0.0.7".parse::<IpAddr>().unwrap()
        );
        assert_ne!(counted("::ffff:10.0.0.7"), counted("::ffff:10.0.0.8"));
        assert_eq!(counted("::10.0.0.7"), "10.0.0.7".parse::<IpAddr>().unwrap());
        assert_eq!(
            counted("64:ff9b::10.0.0.7"),
            "10.0.0.7".parse::<IpAddr>().unwrap()
        );
        assert_eq!(counted("10.0.0.7"), "10.0.0.7".parse::<IpAddr>().unwrap());
        assert_eq!(
            counted("2001:db8:1:2:aaaa::1"),
            counted("2001:db8:1:2:bbbb::2"),
            "one /64 is one subscriber"
        );
        assert_ne!(counted("2001:db8:1:2::1"), counted("2001:db8:1:3::1"));
        assert_eq!(
            counted("::1"),
            "::".parse::<IpAddr>().unwrap(),
            "loopback is native, masked"
        );
    }

    /// The larger body of a prompt route is reachable only past
    /// authentication, whatever the transport header says: an unknown
    /// `x-muqun-transport` value is refused by the transport layer, a
    /// cleartext request without a token by the body-limit layer, and
    /// neither lets the handler (and so the body) be reached.
    #[tokio::test]
    async fn a_large_body_route_authenticates_before_any_body_is_read() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::ServiceExt as _;
        let reached = std::sync::Arc::new(AtomicUsize::new(0));
        let state = test_state("admin", vec![test_device("phone", "device-token")]);
        let app = {
            let reached = reached.clone();
            Router::new()
                .route(
                    "/api/tasks",
                    post(move |body: axum::body::Bytes| {
                        let reached = reached.clone();
                        async move {
                            reached.fetch_add(1, Ordering::SeqCst);
                            format!("{}", body.len())
                        }
                    })
                    .layer(crate::agents::routes::large_body()),
                )
                .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    large_body_after_auth,
                ))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    encrypted_transport,
                ))
                .with_state(state)
        };
        let send = |headers: Vec<(&'static str, &'static str)>| {
            let app = app.clone();
            async move {
                let mut request = Request::builder().method("POST").uri("/api/tasks");
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let response = app
                    .oneshot(request.body(Body::from(vec![b'x'; 200 * 1024])).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), 1 << 16)
                    .await
                    .unwrap();
                (status, String::from_utf8_lossy(&body).into_owned())
            }
        };
        // A transport header that is not `1` is not "sealed" to anyone.
        let (status, body) = send(vec![(TRANSPORT_HEADER, "0")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("invalid_transport"), "{body}");
        assert_eq!(reached.load(Ordering::SeqCst), 0, "the body was never read");
        // Cleartext without a token: refused on the headers.
        let (status, _) = send(vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(reached.load(Ordering::SeqCst), 0);
        // A device gets the larger limit: 200 KiB is over the ordinary one.
        let (status, body) = send(vec![("authorization", "Bearer device-token")]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, (200 * 1024).to_string());
        assert_eq!(reached.load(Ordering::SeqCst), 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_stop_propagates_unverified_cleanup_after_reaping_the_private_fake_tree() {
        let (runtime, root, tool) =
            agents::runtime::codex::tests::runtime_with_unavailable_identity().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            super::serve_until_shutdown(listener, axum::Router::new(), runtime, async {}),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Gateway private Codex cleanup failed"));
        assert!(format!("{error:#}").contains("cleanup could not be verified"));
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        // An Err does not permit an early return before the awaited EOF/tool
        // cleanup. The normal run() file guards outlive this entire boundary.
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        assert_eq!(unsafe { libc::kill(tool, 0) }, -1);
    }

    #[tokio::test]
    async fn codex_stop_cleans_private_client_without_waiting_for_an_open_http_stream() {
        use crate::agents::adapters::codex::tests::Fixture;
        let mut fixture = Fixture::new(|_| serde_json::json!({})).await;
        fixture.ready().await;
        let runtime = agents::AgentRuntime::disabled();
        let (events, _) = tokio::sync::broadcast::channel(32);
        let manager = agents::manager::AgentManager::connect_codex(
            fixture.client.clone(),
            fixture.events.take().unwrap(),
            events,
        );
        runtime.attach_for_test(std::sync::Arc::new(manager)).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/synthetic-stream",
            axum::routing::get(|| async {
                axum::response::Sse::new(futures::stream::pending::<
                    Result<axum::response::sse::Event, std::convert::Infallible>,
                >())
            }),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(super::serve_until_shutdown(listener, app, runtime, async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            http.get(format!("http://{address}/synthetic-stream"))
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.status().is_success());
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!fixture.client.is_ready());
        // No ordinary prompt or callback was submitted during shutdown.
        assert!(fixture.requests.try_recv().is_err());
        drop(response);
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

    /// The gateway advertises a 25 MiB upload limit and answers 413 with "the
    /// upload must be at most 25 MiB". Over its own encrypted transport --
    /// which is on by default -- it could not actually accept one: the
    /// middleware buffered the sealed bytes against a ceiling sized for the
    /// plaintext, and a 64 MiB file is about 112 MiB sealed.
    #[test]
    fn a_maximum_upload_fits_through_the_encrypted_transport() {
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();

        // Measured at a size that is quick to seal rather than at the ceiling
        // itself. The expansion is affine -- base64, JSON, seal, base64 -- so
        // an empty body fixes the intercept, one sample the slope, and
        // `MAX_VIDEO_UPLOAD_BYTES` is exactly 64 of these. The exact version is
        // `a_literal_maximum_upload_seals_within_the_ceiling`, kept ignored
        // because sealing 64 MiB in a debug build takes half a minute.
        const SAMPLE: usize = 1024 * 1024;
        let sampled = sealed_wire_len(&material, "device-token", UPLOADS_PATH, SAMPLE);
        assert!(sampled <= sealed_body_ceiling(SAMPLE));

        let empty = sealed_wire_len(&material, "device-token", UPLOADS_PATH, 0);
        let at_maximum = empty + (sampled - empty) * (MAX_VIDEO_UPLOAD_BYTES / SAMPLE);
        let previous = MAX_VIDEO_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES;
        assert!(
            at_maximum > previous,
            "a {MAX_VIDEO_UPLOAD_BYTES}-byte upload seals to about {at_maximum} bytes, \
             which the old {previous}-byte ceiling should not have fit"
        );
        assert!(
            at_maximum <= sealed_body_ceiling(MAX_VIDEO_UPLOAD_BYTES),
            "sealed about {at_maximum} bytes against a ceiling of {}",
            sealed_body_ceiling(MAX_VIDEO_UPLOAD_BYTES)
        );
    }

    #[test]
    #[ignore = "seals 64 MiB; slow in a debug build"]
    fn a_literal_maximum_upload_seals_within_the_ceiling() {
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let wire = sealed_wire_len(
            &material,
            "device-token",
            UPLOADS_PATH,
            MAX_VIDEO_UPLOAD_BYTES,
        );
        assert!(
            wire > MAX_VIDEO_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES,
            "sealed to {wire} bytes"
        );
        assert!(
            wire <= sealed_body_ceiling(MAX_VIDEO_UPLOAD_BYTES),
            "sealed {wire} bytes against a ceiling of {}",
            sealed_body_ceiling(MAX_VIDEO_UPLOAD_BYTES)
        );
    }

    /// And the other half: every route that is not the upload route is held to
    /// the same 128 KiB the router holds it to in the clear. The middleware
    /// runs outside those limits, so before this it let any encrypted request
    /// on any route buffer 25 MiB.
    #[test]
    fn every_other_route_is_held_to_the_small_body_limit() {
        assert_eq!(
            plaintext_body_limit(UPLOADS_PATH),
            platform::uploads::MAX_UPLOAD_BODY_BYTES
        );
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
            sealed_body_ceiling(MAX_REQUEST_BODY_BYTES) < MAX_VIDEO_UPLOAD_BYTES,
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
        let state = required_test_state("admin-token", vec![device]);
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
            transport_version: transport::VERSION,
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
                transport_version: transport::VERSION,
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

        // A device without a transport key, on a `disabled` deployment (the
        // only mode that serves one), keeps the plaintext stream byte for
        // byte: no sealer, no envelope, the event under its own name.
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
}

#[cfg(test)]
mod transport_v2_tests {
    use super::*;
    use axum::routing::get;
    use serde_json::json;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn admission_metadata_errors_are_generic_forbidden_not_store_failures() {
        let key = [33; 32];
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);
        let token_hash = hash_token("fixture-token");
        let mut device = crate::test_device("fixture-device", "fixture-token");
        device.transport_key = Some(encoded.clone());
        let state = crate::test_state("fixture-admin", vec![device]);
        let timestamp = 1_000_000;
        let boundary = timestamp + transport::MAX_CLOCK_SKEW_MS;
        let envelope = transport::seal_version(
            2,
            &key,
            transport::Direction::Request,
            b"GET /fixture",
            br#"{"token":"fixture-token","body":""}"#,
            timestamp,
        )
        .unwrap();
        assert!(transport::open(
            &key,
            transport::Direction::Request,
            b"GET /fixture",
            &envelope,
            boundary,
        )
        .is_ok());
        // Authentication at the inclusive boundary does not authorize later
        // admission. Its refusal must not be reported as an infrastructure
        // error: the envelope authenticated and is stale by now, so it is
        // the clock-skew refusal, a 403 carrying this computer's time.
        for (envelope, now) in [
            (envelope, boundary + 1),
            (
                transport::seal_version(
                    2,
                    &key,
                    transport::Direction::Request,
                    b"GET /fixture",
                    b"body",
                    u128::MAX,
                )
                .unwrap(),
                u128::MAX,
            ),
        ] {
            let response = remember_transport_nonce(
                &state,
                "fixture-device",
                &token_hash,
                &encoded,
                &envelope,
                move || now,
            )
            .await
            .unwrap_err()
            .into_response();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "clock_skew");
            assert!(body["error"]["server_time_ms"].is_u64(), "{body}");
        }
        let rows: (i64, i64, i64) = state
            .db
            .call(|db| {
                Ok((
                    db.query_row("SELECT COUNT(*) FROM transport_nonces", [], |r| r.get(0))?,
                    db.query_row("SELECT COUNT(*) FROM transport_credentials", [], |r| {
                        r.get(0)
                    })?,
                    db.query_row("SELECT cutoff_ms FROM transport_pruning", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(rows, (0, 0, 0));

        // Actual database failures remain a fail-closed 503.
        state
            .db
            .call(|db| {
                db.execute("DROP TABLE transport_pruning", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let response = remember_transport_nonce(
            &state,
            "fixture-device",
            &token_hash,
            &encoded,
            &fixture(2, &key),
            now_unix_ms,
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "replay_store_unavailable");
    }

    #[tokio::test]
    async fn full_legacy_history_can_upgrade_without_repairing() {
        let key = [32; 32];
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);
        let credential = hash_token(&format!("{}\n{encoded}", hash_token("fixture-token")));
        let mut device = crate::test_device("fixture-device", "fixture-token");
        device.transport_key = Some(encoded);
        let state = crate::test_state("fixture-admin", vec![device]);
        // Populate only the isolated fixture database, directly and in one
        // transaction. No Gateway requests or command handlers are exercised.
        state.db.call(move |db| {
            db.execute("WITH RECURSIVE entries(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM entries WHERE n < ?1) INSERT INTO transport_nonces(device_id,credential_hash,nonce,version,timestamp_ms) SELECT 'fixture-device',?2,CAST(printf('%012d',n) AS BLOB),1,1 FROM entries", rusqlite::params![crate::connectivity::replay_store::MAX_NONCES_PER_CREDENTIAL, credential])?;
            Ok(())
        }).await.unwrap();
        let full = request(&state, &fixture(1, &key)).await;
        assert_eq!(full.status(), StatusCode::UPGRADE_REQUIRED);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(full.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "transport_version_required");
        assert_eq!(
            request(&state, &fixture(2, &key)).await.status(),
            StatusCode::OK
        );
        let legacy_count: i64 = state
            .db
            .call(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM transport_nonces WHERE version=1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(legacy_count, 0);
        assert_eq!(
            request(&state, &fixture(1, &key)).await.status(),
            StatusCode::UPGRADE_REQUIRED
        );
    }

    fn fixture(version: u8, key: &[u8]) -> transport::Envelope {
        transport::seal_version(
            version,
            key,
            transport::Direction::Request,
            b"GET /fixture",
            br#"{"token":"fixture-token","body":""}"#,
            now_unix_ms(),
        )
        .unwrap()
    }

    async fn request(state: &AppState, envelope: &transport::Envelope) -> Response {
        let app = Router::new()
            .route("/fixture", get(|| async { Json(json!({"ok":true})) }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                encrypted_transport,
            ))
            .with_state(state.clone());
        app.oneshot(
            Request::get("/fixture")
                .header(TRANSPORT_HEADER, "1")
                .header(TRANSPORT_DEVICE_HEADER, "fixture-device")
                .header(
                    TRANSPORT_ENVELOPE_HEADER,
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(serde_json::to_vec(envelope).unwrap()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn authenticated_v2_pins_before_dispatch_and_answers_in_the_same_version() {
        let key = [31; 32];
        let mut device = crate::test_device("fixture-device", "fixture-token");
        device.transport_key = Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key));
        let state = crate::test_state("fixture-admin", vec![device]);
        let mut invalid = fixture(2, &key);
        invalid.timestamp_ms += 1;
        assert_eq!(
            request(&state, &invalid).await.status(),
            StatusCode::FORBIDDEN
        );
        // A failed v2 authentication neither advances the pin nor poisons a
        // valid legacy request from the same paired credential.
        let legacy = request(&state, &fixture(1, &key)).await;
        let legacy: transport::Envelope =
            serde_json::from_slice(&to_bytes(legacy.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(legacy.version, 1);
        let envelope = fixture(2, &key);
        let reply = request(&state, &envelope).await;
        let reply: transport::Envelope =
            serde_json::from_slice(&to_bytes(reply.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(reply.version, 2);
        let plaintext = transport::open(
            &key,
            transport::Direction::Response,
            format!("GET /fixture\n{}", envelope.nonce).as_bytes(),
            &reply,
            now_unix_ms(),
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(body["status"], 200);
        assert_eq!(
            request(&state, &envelope).await.status(),
            StatusCode::CONFLICT
        );
        let downgrade = request(&state, &fixture(1, &key)).await;
        assert_eq!(downgrade.status(), StatusCode::UPGRADE_REQUIRED);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(downgrade.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "transport_version_required");
    }
}
