//! Connectivity HTTP routes: device pairing, registered device management, and push notifications.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::authority::{self, hash_token, DeviceRecord, PairingCodeError, PendingPairing};
use super::transport;
use crate::{
    api_error, generate_pairing_code, generate_token, lock_devices, now_unix_ms,
    platform::i18n::{self, Locale},
    read_pairing_file, require_admin, require_device, require_pairing_manager,
    rotate_pairing_transport_key, send_expo_push_notifications, valid_device_name,
    valid_install_id, valid_request_id, validate_push_token, write_devices, write_push_tokens,
    ApiResult, AppState, Config, PushTokenRecord, TransportEncryptionMode, MAX_DEVICES,
    MAX_PAIRING_CODE_ATTEMPTS, MAX_PAIRING_REQUESTS_PER_WINDOW, MAX_PUSH_TOKENS,
    PAIRING_CODE_TTL_MS, PAIRING_RATE_LIMIT_WINDOW_MS,
};

#[derive(Deserialize)]
pub(crate) struct PairRequestBody {
    pub(crate) request_id: String,
    pub(crate) device_name: Option<String>,
    /// A stable per-install identifier from the client. When present, a new
    /// pairing replaces any earlier device with the same value, so re-pairing a
    /// device does not leave a duplicate record behind.
    #[serde(default)]
    pub(crate) install_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PairClaimBody {
    pub(crate) request_id: String,
    pub(crate) code: String,
}

#[derive(Deserialize)]
pub(crate) struct RegisterPushTokenBody {
    pub(crate) token: String,
    pub(crate) platform: String,
    pub(crate) device_name: Option<String>,
    /// The language this device wants its notifications in. Additive and
    /// optional: an older client that does not send it registers exactly as it
    /// always did, and the request locale is used instead, which for the app is
    /// the same value it would have sent anyway.
    #[serde(default)]
    pub(crate) locale: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct UnregisterPushTokenBody {
    pub(crate) token: String,
}

#[derive(Deserialize)]
pub(crate) struct SendPushNotificationBody {
    pub(crate) title: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) data: Option<serde_json::Map<String, Value>>,
}

/// Argon2id parameters for turning a claimed pairing code into transport key
/// material. OWASP's second recommended interactive Argon2id setting (m =
/// 19 MiB, t = 2, p = 1): materially more expensive per guess than a bare
/// hash without being slow enough to make the app visibly stall.
const CODE_KDF_MEMORY_KIB: u32 = 19_456;
const CODE_KDF_TIME_COST: u32 = 2;
const CODE_KDF_PARALLELISM: u32 = 1;
const CODE_KDF_OUTPUT_LEN: usize = 32;

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/pair/request", post(pair_request))
        .route("/api/pair/claim", post(pair_claim))
        .route("/api/pair/pending", get(pair_pending))
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
}

pub(crate) async fn pair_request(
    State(state): State<AppState>,
    Json(wire): Json<Value>,
) -> ApiResult<Response> {
    let (body, request_nonce) = decode_pairing_body::<PairRequestBody>(
        wire,
        b"POST /api/pair/request",
        transport::Direction::PairingRequest,
    )?;
    require_pairing_transport(state.config.transport_encryption, request_nonce.is_some())?;
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
        if !authority::pairing_code_expired(pending, now, PAIRING_CODE_TTL_MS) {
            if pending.request_id == body.request_id {
                return pairing_response(
                    pair_request_response(&state.config, &body.request_id),
                    b"POST /api/pair/request",
                    request_nonce.as_deref(),
                );
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
    pairing_response(
        pair_request_response(&state.config, &body.request_id),
        b"POST /api/pair/request",
        request_nonce.as_deref(),
    )
}

pub(crate) fn record_pairing_request(state: &AppState, now_unix_ms: u128) -> ApiResult<()> {
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

pub(crate) async fn pair_claim(
    State(state): State<AppState>,
    Json(wire): Json<Value>,
) -> ApiResult<Response> {
    let (body, request_nonce) = decode_pairing_body::<PairClaimBody>(
        wire,
        b"POST /api/pair/claim",
        transport::Direction::PairingRequest,
    )?;
    require_pairing_transport(state.config.transport_encryption, request_nonce.is_some())?;
    let (device_name, install_id, code) = {
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
        authority::consume_pairing_code(
            &mut pending,
            &body.request_id,
            &code,
            now_unix_ms(),
            PAIRING_CODE_TTL_MS,
            MAX_PAIRING_CODE_ATTEMPTS,
        )
        .map_err(|error| match error {
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
        })?;
        (device_name, install_id, code)
    };

    // Each device gets its own token so it can be revoked without disturbing
    // the others. The admin token in pairing.json is never handed out.
    let token = generate_token();
    let device_transport_key = (state.config.transport_encryption
        == TransportEncryptionMode::Required)
        .then(generate_token);
    let record = DeviceRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: device_name,
        token_hash: hash_token(&token),
        transport_key: device_transport_key.clone(),
        paired_unix_ms: now_unix_ms(),
        last_seen_unix_ms: now_unix_ms(),
        install_id: install_id.clone(),
    };
    let device_id = record.id.clone();
    {
        let mut devices = lock_devices(&state)?;
        authority::enroll_device(&mut devices, record, MAX_DEVICES);
        write_devices(&devices).map_err(|err| {
            eprintln!("failed to write device tokens: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device_write_failed",
                "failed to save the new device token",
            )
        })?;
    }

    let mut response_payload = json!({
        "kind": "muqun-gateway",
        "server_id": state.config.server_id,
        "label": state.config.label,
        "url": state.config.public_url,
        "token": token,
        "device_id": device_id
    });
    if let Some(device_transport_key) = device_transport_key {
        response_payload["transport_key"] = Value::String(device_transport_key);
        response_payload["transport"] = Value::String("muqun-aes-256-gcm-v1".into());
    }
    let response = if request_nonce.is_some() {
        // Scanned the QR: request and response are both sealed with the
        // pre-shared key it carried. Unchanged from before this card.
        pairing_response(
            response_payload,
            b"POST /api/pair/claim",
            request_nonce.as_deref(),
        )?
    } else if state.config.transport_encryption == TransportEncryptionMode::Required {
        // Typed the address and code: there is no pre-shared key, so the code
        // just spent to authenticate this claim is also what protects the
        // response in transit. See `code_pairing_response` for why that is
        // sound with an eight-character code and what it still costs.
        code_pairing_response(
            response_payload,
            b"POST /api/pair/claim",
            &code,
            &body.request_id,
        )
        .await?
    } else {
        // Encryption is disabled -- the response was never going to be sealed
        // either way.
        pairing_response(response_payload, b"POST /api/pair/claim", None)?
    };
    if request_nonce.is_some() {
        if let Err(err) = rotate_pairing_transport_key() {
            // The device is already enrolled and the response is already sealed
            // with the scanned key. Do not strand it by discarding that response.
            // The manager will generate a fresh key on the next successful write.
            eprintln!("warning: failed to rotate pairing transport key: {err:#}");
        }
    }
    Ok(response)
}

fn pairing_transport_material() -> ApiResult<Vec<u8>> {
    let pairing = read_pairing_file().map_err(|err| {
        eprintln!("failed to read pairing transport key: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_transport_unavailable",
            "encrypted pairing is unavailable",
        )
    })?;
    transport::decode_key(&pairing.payload.transport_key).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_transport_unavailable",
            "encrypted pairing is unavailable",
        )
    })
}

/// Rejects only a request sealed with a key that can no longer be right: an
/// encrypted body arriving while this gateway's encryption is `Disabled`
/// means the caller is holding a QR (or a cached pairing key) from before the
/// owner turned it off.
///
/// This used to also reject the opposite pairing (`Required`, unencrypted)
/// with "scan the Gateway QR ...". That case is not an error anymore: card
/// #821 gave `pair_claim` a second way to authenticate an unencrypted
/// request -- the one-time code itself, checked in `pair_claim` and then
/// spent again as the key that seals the response (see
/// `code_pairing_response`). `pair_request` never carried anything secret, so
/// it never needed the QR's key to begin with.
pub(crate) fn require_pairing_transport(
    mode: TransportEncryptionMode,
    encrypted: bool,
) -> ApiResult<()> {
    if mode == TransportEncryptionMode::Disabled && encrypted {
        return Err(api_error(
            StatusCode::CONFLICT,
            "encrypted_pairing_disabled",
            "transport encryption is disabled on this gateway; scan its current QR code",
        ));
    }
    Ok(())
}

fn decode_pairing_body<T: serde::de::DeserializeOwned>(
    wire: Value,
    aad: &[u8],
    direction: transport::Direction,
) -> ApiResult<(T, Option<String>)> {
    if wire.get("version").is_none() {
        return serde_json::from_value(wire)
            .map(|body| (body, None))
            .map_err(|_| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "invalid request",
                )
            });
    }
    let envelope: transport::Envelope = serde_json::from_value(wire).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let plaintext = transport::open(
        &pairing_transport_material()?,
        direction,
        aad,
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
    let nonce = envelope.nonce;
    serde_json::from_slice(&plaintext)
        .map(|body| (body, Some(nonce)))
        .map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid request",
            )
        })
}

fn pairing_response(value: Value, aad: &[u8], request_nonce: Option<&str>) -> ApiResult<Response> {
    let Some(request_nonce) = request_nonce else {
        return Ok(Json(value).into_response());
    };
    let plaintext = serde_json::to_vec(&value).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encoding_failed",
            "failed to encode response",
        )
    })?;
    let response_aad = [aad, b"\n", request_nonce.as_bytes()].concat();
    let envelope = transport::seal(
        &pairing_transport_material()?,
        transport::Direction::PairingResponse,
        &response_aad,
        &plaintext,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encryption_failed",
            "failed to encrypt response",
        )
    })?;
    Ok(Json(envelope).into_response())
}

/// The key material a claimed one-time code stands in for the QR's pre-shared
/// key, for exactly one response.
///
/// A scanned QR carries 256 bits of random key; a code a person can read off
/// a screen and type on a phone carries about 40 (eight glyphs from a
/// 32-symbol alphabet -- see `generate_pairing_code`). That gap is real: it
/// is the reason a PAKE is the "actually correctly shaped" answer to this
/// problem (card #821's own words). What closes enough of the gap to ship
/// tonight is that the code is not used as an AES key directly -- it is run
/// through Argon2id first, so an attacker who captures the sealed claim
/// response off the network cannot test candidate codes at hash speed. At
/// roughly 20-50 ms per guess on ordinary hardware, the full 32^8 space is
/// centuries of compute, not the minutes a bare HKDF would cost. Online
/// guessing is separately bounded the way it always was: eight wrong answers
/// burn the code (`MAX_PAIRING_CODE_ATTEMPTS`) and it is five minutes old at
/// most (`PAIRING_CODE_TTL_MS`).
///
/// The salt is derived from `request_id` rather than being random: both sides
/// need to land on the same key without a round trip to agree on a salt, and
/// `request_id` is already a CSPRNG value unique to this one pairing attempt
/// (see `createRequestId` in the app), so it is exactly as good a salt as one
/// generated fresh, minus the round trip. It is hashed to a fixed 32 bytes
/// first because Argon2's own salt floor is 8 bytes and `request_id` is only
/// guaranteed to be 1-80.
pub(crate) fn code_pairing_material(code: &str, request_id: &str) -> anyhow::Result<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use sha2::{Digest as _, Sha256};

    let mut salt = [0_u8; 32];
    salt.copy_from_slice(&Sha256::digest(
        [
            b"muqun-pairing-code-salt-v1".as_slice(),
            request_id.as_bytes(),
        ]
        .concat(),
    ));
    let params = Params::new(
        CODE_KDF_MEMORY_KIB,
        CODE_KDF_TIME_COST,
        CODE_KDF_PARALLELISM,
        Some(CODE_KDF_OUTPUT_LEN),
    )
    .map_err(|err| anyhow::anyhow!("invalid argon2 parameters: {err}"))?;
    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut material = [0_u8; 32];
    hasher
        .hash_password_into(code.as_bytes(), &salt, &mut material)
        .map_err(|err| anyhow::anyhow!("code-derived key material failed: {err}"))?;
    Ok(material)
}

/// Seals a claim response with material derived from the one-time code that
/// just authenticated it, for a device pairing by address and code rather
/// than by QR. See `code_pairing_material` for why an eight-character code
/// run through Argon2id is enough for this one message.
///
/// Argon2id is deliberately memory-hard and therefore not free to run: it is
/// spawned onto a blocking thread so a burst of pairing attempts cannot stall
/// the async runtime's worker threads.
async fn code_pairing_response(
    value: Value,
    aad: &[u8],
    code: &str,
    request_id: &str,
) -> ApiResult<Response> {
    let plaintext = serde_json::to_vec(&value).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encoding_failed",
            "failed to encode response",
        )
    })?;
    let code = code.to_owned();
    let request_id = request_id.to_owned();
    let material = tokio::task::spawn_blocking(move || code_pairing_material(&code, &request_id))
        .await
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_encryption_failed",
                "failed to encrypt response",
            )
        })?
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_encryption_failed",
                "failed to encrypt response",
            )
        })?;
    let response_aad = [aad, b"\ncode-pairing\n"].concat();
    let envelope = transport::seal(
        &material,
        transport::Direction::PairingResponse,
        &response_aad,
        &plaintext,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encryption_failed",
            "failed to encrypt response",
        )
    })?;
    Ok(Json(envelope).into_response())
}

fn pair_request_response(config: &Config, request_id: &str) -> Value {
    json!({
        "request_id": request_id,
        "server_id": config.server_id,
        "server_label": config.label,
        "status": "pending",
        "expires_in_ms": PAIRING_CODE_TTL_MS,
        "transport_encryption": config.transport_encryption.as_str()
    })
}

pub(crate) async fn pair_pending(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
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
    if pending.as_ref().is_some_and(|value| {
        authority::pairing_code_expired(value, now_unix_ms(), PAIRING_CODE_TTL_MS)
    }) {
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

pub(crate) async fn register_push_token(
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
    // A body that names a language wins; a body that does not falls back to the
    // headers this very request arrived with, which is the same answer for the
    // app and a better one than English for anything else.
    let locale = body
        .locale
        .as_deref()
        .and_then(Locale::from_code)
        .unwrap_or_else(|| Locale::from_headers(&headers));
    let record = PushTokenRecord {
        token: body.token,
        platform,
        device_name: body.device_name.filter(|value| !value.trim().is_empty()),
        locale: Some(locale.as_str().to_owned()),
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

pub(crate) async fn unregister_push_token(
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

pub(crate) async fn send_test_notification(
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
    // The person waiting to see whether push works is the one holding the phone
    // that made this call, so the defaults are written in that request's
    // language. "Muqun Gateway" is the product's name and stays in Latin script
    // in every locale, the same way the app leaves "Gateway" alone.
    let locale = Locale::from_headers(&headers);
    let result = send_expo_push_notifications(
        &tokens,
        body.title.unwrap_or_else(|| "Muqun Gateway".into()),
        body.body.unwrap_or_else(|| {
            i18n::t(locale, "Muqun push notifications are connected.").to_owned()
        }),
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

pub(crate) async fn list_paired_devices(
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

pub(crate) async fn revoke_paired_device(
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
