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
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tower_http::compression::{
    predicate::{DefaultPredicate, Predicate, SizeAbove},
    CompressionLayer,
};

use super::assets::{AssetIndex, MAX_ASSET_CONTENT_BYTES};
use super::metadata::SessionLivenessCache;
use super::store::{
    ensure_pairing_transport_key, load_devices_for_service, load_push_tokens_for_service,
};
use super::uploads::{spawn_upload_gc, MAX_UPLOAD_BYTES, UPLOADS_PATH};
use crate::platform;
use crate::platform::i18n::Locale;
use crate::terminal;
use crate::terminal::routes::{
    spawn_agent_engine_watchers, spawn_agent_notification_watchers, spawn_approval_watchers,
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
    spawn_memory_watchdog();

    let app = Router::new();
    let app = platform::routes::mount(app);
    let app = connectivity::routes::mount(app);
    let app = agents::session_routes::mount(app);
    let app = agents::routes::mount(app);
    let app = terminal::routes::mount(app);
    let app = platform::uploads::mount(app);
    let app = platform::assets::mount(app)
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
