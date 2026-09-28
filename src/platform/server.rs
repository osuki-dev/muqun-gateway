//! HTTP server startup: middleware stack, transport encryption, and locality checks.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::body::{to_bytes, Body};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tower_http::compression::{
    predicate::{DefaultPredicate, Predicate, SizeAbove},
    CompressionLayer,
};

use super::assets::{asset_content, session_assets, AssetIndex, MAX_ASSET_CONTENT_BYTES};
use super::metadata::SessionLivenessCache;
use super::store::{
    ensure_pairing_transport_key, load_devices_for_service, load_push_tokens_for_service,
};
use super::uploads::{
    spawn_upload_gc, upload_content, upload_file, MAX_UPLOAD_BYTES, UPLOADS_PATH,
};
use crate::platform;
use crate::platform::i18n::Locale;
use crate::terminal::routes::{
    close_pane, close_tab, close_workspace, create_tab, create_workspace, events, focus_pane,
    focus_tab, focus_workspace, interrupt_pane, keymaps, pane, pane_context, pane_files,
    pane_git_diff, pane_git_status, pane_output, pane_parts, pane_shortcuts, panes, recent_cwds,
    rename_pane, rename_tab, rename_workspace, send_keys, send_text, sessions, snapshot,
    spawn_agent_engine_watchers, spawn_agent_notification_watchers, spawn_approval_watchers,
    split_pane, tabs, workspaces, zoom_pane,
};
use crate::{
    agent_events, agents, api_error, backend_startup, connectivity, gateway_listener, hash_token,
    i18n, load_config, lock_devices, now_unix_ms, scrollback, state_dir, state_lock, transport,
    unreachable_listen_warning, warn_about_missing_backend_programs, ApiResult, AppState, Config,
    APPROVAL_EVENT_CAPACITY, MAX_REQUEST_BODY_BYTES,
};

pub(crate) async fn run(config_path: Option<String>) -> anyhow::Result<()> {
    // Taken before the device list is read and held for the life of the
    // process. This gateway caches the whole list in memory and rewrites the
    // whole file on every change, so a second gateway against the same
    // directory would not interleave with it -- it would overwrite it, and
    // whichever devices the loser had paired would be silently unpaired.
    //
    // The binding has to be named: `let _ = ...` would drop the lock on the
    // spot and leave this gateway believing it owned a directory it had
    // already released.
    let _state_lock = state_lock::StateLock::acquire(&state_dir()?)?;
    ensure_pairing_transport_key()?;
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
    // One background attempt per opted-in backend; no restart/logging loop.
    backend_startup::spawn(&config);

    let agent_runtime =
        agents::AgentRuntime::with_configs(config.opencode.clone(), config.deepseek.clone());
    agent_runtime.spawn_supervisor();

    let state = AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(load_push_tokens_for_service())),
        devices: Arc::new(Mutex::new(load_devices_for_service()?)),
        assets: Arc::new(Mutex::new(AssetIndex::default())),
        scrollback: Arc::new(Mutex::new(scrollback::ScrollbackStore::default())),
        agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
        approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
        activity: Arc::new(Mutex::new(HashMap::new())),
        session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
        agent_runtime,
    };
    spawn_agent_notification_watchers(state.clone());
    spawn_agent_engine_watchers(state.clone());
    spawn_approval_watchers(state.clone());
    spawn_upload_gc();

    let app = Router::new();
    let app = platform::routes::mount(app);
    let app = connectivity::routes::mount(app);
    let app = agents::session_routes::mount(app);
    let app = agents::routes::mount(app)
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
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}/assets",
            get(session_assets),
        )
        .route("/api/assets/{asset_id}/content", get(asset_content))
        // A route-level limit is applied inside the router-wide one, so uploads
        // get their own ceiling while every JSON route keeps the small one.
        .route(
            UPLOADS_PATH,
            post(upload_file).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        // Reading one back. The app needs this to draw the user's own
        // attachment in the transcript: the timeline item carries the host
        // path, which a phone cannot open.
        .route("/api/uploads/{file_name}", get(upload_content))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
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
        eprintln!("{warning}");
    }
    if dev_unauthenticated {
        // Said every time, on stderr, unmissably: this is the one setting that
        // hands the API to anything that can reach the port.
        eprintln!(
            "SECURITY WARNING: dev_unauthenticated is on -- every device route \
             answers WITHOUT a token, to anything that can reach {addr}. This is \
             for a local mock only. Remove \"dev_unauthenticated\" from config.json \
             to turn it off."
        );
    }
    println!("terminal gateway listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
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
/// It was `MAX_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES`, the plaintext limits
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
pub(crate) fn plaintext_body_limit(path: &str) -> usize {
    if path == UPLOADS_PATH {
        MAX_UPLOAD_BYTES
    } else {
        MAX_REQUEST_BODY_BYTES
    }
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

    if request
        .headers()
        .get(TRANSPORT_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return next.run(request).await;
    }

    match decrypt_transport_request(&state, request).await {
        Ok((request, material, aad, request_nonce)) => {
            let response = next.run(request).await;
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
            encrypt_transport_response(response, &material, &aad, &request_nonce).await
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
        })?;
    let (token_hash, transport_key) = {
        let devices = lock_devices(state)?;
        let device = devices
            .iter()
            .find(|device| device.id == device_id)
            .ok_or_else(|| api_error(StatusCode::FORBIDDEN, "invalid_token", "invalid token"))?;
        let transport_key = device.transport_key.clone().ok_or_else(|| {
            api_error(
                StatusCode::UPGRADE_REQUIRED,
                "device_repair_required",
                "pair this device again to enable encrypted transport",
            )
        })?;
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
    // body limits and is therefore the only thing bounding them.
    let body_bytes = to_bytes(
        body,
        sealed_body_ceiling(plaintext_body_limit(parts.uri.path())),
    )
    .await
    .map_err(|_| {
        api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "the request body is too large",
        )
    })?;
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
    let plaintext = transport::open(
        &material,
        transport::Direction::Request,
        aad.as_bytes(),
        &envelope,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::FORBIDDEN,
            "invalid_envelope",
            "invalid encrypted request",
        )
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
    remember_transport_nonce(device_id, &envelope)?;
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
    });
    Ok((
        Request::from_parts(parts, Body::from(body)),
        material,
        aad,
        envelope.nonce,
    ))
}

pub(crate) fn remember_transport_nonce(
    device_id: &str,
    envelope: &transport::Envelope,
) -> ApiResult<()> {
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashMap<String, u128>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "replay_cache_failed",
                "request failed",
            )
        })?;
    let now = now_unix_ms();
    seen.retain(|_, timestamp| now.abs_diff(*timestamp) <= transport::MAX_CLOCK_SKEW_MS);
    let key = format!("{device_id}:{}", envelope.nonce);
    if seen.insert(key, envelope.timestamp_ms).is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "replayed_request",
            "request was already used",
        ));
    }
    Ok(())
}

pub(crate) async fn encrypt_transport_response(
    response: Response,
    material: &[u8],
    aad: &str,
    request_nonce: &str,
) -> Response {
    let (parts, body) = response.into_parts();
    // A response is buffered as plaintext and sealed afterwards, so the bound
    // is on what a handler can produce: an asset preview is the largest, and
    // every other route answers JSON well under the request limit.
    let body = match to_bytes(
        body,
        MAX_ASSET_CONTENT_BYTES as usize + MAX_REQUEST_BODY_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_too_large",
                "failed to encrypt response",
            )
            .into_response()
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
    let envelope = match transport::seal(
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
    response
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
            eprintln!(
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
pub(crate) const COMPRESSION_MIN_BYTES: u16 = 512;

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
    if sealed {
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
