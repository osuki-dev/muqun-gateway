//! Shared inbound HTTP concerns: the content envelope, error shapes, device
//! authentication, and request validation.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde_json::{json, Value};

use super::i18n::{self, Locale};
use crate::{
    authority, hash_token, identify_device, now_unix_ms, write_devices, ApiResult, AppState,
    Config, DeviceRecord, PushTokenRecord, SessionConfig, TransportEncryptionMode,
    CONTENT_SCHEMA_VERSION, DEVICE_LAST_SEEN_FLUSH_MS, MAX_AGENT_ARGS, MAX_AGENT_ARG_CHARS,
    MAX_SEND_TEXT_BYTES, TRANSPORT_PROOF_HEADER,
};

/// The versioned envelope every content-model response carries.
pub(crate) fn content_envelope(data: Value) -> Value {
    json!({
        "schema_version": CONTENT_SCHEMA_VERSION,
        "capabilities": {
            "parts": true,
            "assets": true,
            "image_upload": true,
            // Slash-command catalogue and `@` file search. A pane's own
            // descriptor still says whether this particular agent has a table.
            "composer": true,
        },
        "data": data,
    })
}

/// The caller identity recorded for a request that was let through without a
/// token, which only `dev_unauthenticated` can produce. It is deliberately
/// unlike any real device id.
pub(crate) const DEV_UNAUTHENTICATED_DEVICE: &str = "dev-unauthenticated-device";

pub(crate) fn bearer_token(headers: &HeaderMap) -> ApiResult<&str> {
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

/// Control routes are for paired devices only. The admin token deliberately
/// does not authorise these: it sits in plaintext on disk for the manage UI,
/// and these routes can run commands on the host.
pub(crate) fn require_device(state: &AppState, headers: &HeaderMap) -> ApiResult<String> {
    if state.config.transport_encryption == TransportEncryptionMode::Disabled {
        // Cleartext mode drops the envelope and the per-device transport
        // proof; it does not drop authentication. A device paired while
        // encryption was on still holds a `transport_key` and will never send
        // a proof over cleartext, so the proof check is the part that is
        // skipped here -- the token is still the token.
        let token = bearer_token(headers);
        if let Ok(token) = token {
            let mut devices = lock_devices(state)?;
            if let Some(device_id) = identify_device(&devices, token) {
                let _ = authority::touch_device(
                    &mut devices,
                    &device_id,
                    now_unix_ms(),
                    DEVICE_LAST_SEEN_FLUSH_MS,
                );
                return Ok(device_id);
            }
            if authority::authenticates_admin(&state.config.token_hash, token) {
                return Ok("admin".to_string());
            }
        }
        if state.config.dev_unauthenticated {
            return Ok(DEV_UNAUTHENTICATED_DEVICE.to_string());
        }
        // Absent or malformed is 401 and a wrong token is 403, exactly as in
        // the encrypted mode: the two answers must not diverge by mode.
        token?;
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }

    let token = bearer_token(headers)?;
    let mut devices = lock_devices(state)?;
    let Some(device_id) = identify_device(&devices, token) else {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    };
    if let Some(transport_key) = devices
        .iter()
        .find(|device| device.id == device_id)
        .and_then(|device| device.transport_key.as_deref())
    {
        let proof = headers
            .get(TRANSPORT_PROOF_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !authority::authenticates_admin(&hash_token(transport_key), proof) {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                "device_proof_required",
                "encrypted device proof is required",
            ));
        }
    }
    if authority::touch_device(
        &mut devices,
        &device_id,
        now_unix_ms(),
        DEVICE_LAST_SEEN_FLUSH_MS,
    ) {
        if let Err(err) = write_devices(&devices) {
            // Losing a last-seen timestamp must not fail the request.
            eprintln!("failed to persist device last-seen: {err:#}");
        }
    }
    Ok(device_id)
}

/// Whether a device authorised earlier is still authorised now.
///
/// For a long-lived connection, which is authorised once and then outlives the
/// decision by hours. A poisoned lock answers "still paired": this decides
/// whether to *cut* a stream the gateway already accepted, and dropping every
/// open connection because an unrelated mutex was poisoned would be a worse
/// failure than the one it is guarding against. A revoked device stays cut off
/// from everything else regardless -- `require_device` fails closed on the same
/// lock, so it cannot open a new stream or make any other request.
pub(crate) fn still_paired(state: &AppState, device_id: &str) -> bool {
    match state.devices.lock() {
        Ok(devices) => devices.iter().any(|device| device.id == device_id),
        Err(_) => true,
    }
}

/// The local manage UI's credential, which authorises nothing but reading the
/// pending pairing code.
pub(crate) fn require_admin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    if config.transport_encryption == TransportEncryptionMode::Disabled {
        return Ok(());
    }
    let token = bearer_token(headers)?;
    if !authority::authenticates_admin(&config.token_hash, token) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }
    Ok(())
}

pub(crate) fn require_pairing_manager(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if require_admin(&state.config, headers).is_ok() {
        return Ok(());
    }
    require_device(state, headers).map(|_| ())
}

pub(crate) fn lock_devices(
    state: &AppState,
) -> ApiResult<std::sync::MutexGuard<'_, Vec<DeviceRecord>>> {
    state.devices.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_lock_failed",
            "failed to lock device state",
        )
    })
}

pub(crate) fn validate_text(text: &str) -> ApiResult<()> {
    if text.len() > MAX_SEND_TEXT_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text_too_large",
            "text must be at most 65536 bytes",
        ));
    }
    Ok(())
}

/// The one client-supplied field that becomes the argv of a process on the
/// host.
///
/// It is not an escalation and the bound is not pretending otherwise: a caller
/// who can reach this endpoint holds a device token, and a device token can
/// already type into the pane the agent is about to run in. Nor is it a shell
/// -- Herdr starts an agent by argv and never through one, so nothing in here
/// is parsed by anything but the agent's own option parser.
///
/// It is bounded because every other string a client sends this gateway is,
/// and "the argument list of a program on your machine" is the wrong field to
/// be the exception. Control characters go with it, for the same reason a
/// device name cannot carry them: an argument list is echoed back in the task's
/// step log, and a newline in one forges a line.
pub(crate) fn validate_agent_args(args: &[String]) -> ApiResult<()> {
    if args.len() > MAX_AGENT_ARGS {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "too_many_agent_args",
            "agent_args must contain at most 32 entries",
        ));
    }
    if args
        .iter()
        .any(|arg| arg.chars().count() > MAX_AGENT_ARG_CHARS)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "agent_arg_too_long",
            "each agent_args entry must be at most 512 characters",
        ));
    }
    if args.iter().any(|arg| arg.chars().any(char::is_control)) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_agent_args",
            "agent_args entries must not contain control characters",
        ));
    }
    Ok(())
}

pub(crate) fn validate_push_token(token: &str) -> ApiResult<()> {
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

pub(crate) async fn send_expo_push_notifications(
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

pub(crate) fn find_session<'a>(
    config: &'a Config,
    session_id: &str,
) -> ApiResult<&'a SessionConfig> {
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

/// Every refusal this gateway makes, in the language the request asked for.
///
/// `code` is the wire's vocabulary: a client dispatches on it, so it is the
/// same bytes in every locale and is never looked up in the catalog. `message`
/// is prose for a person, and is passed in as its own English text -- which is
/// also the key it is translated by, so a message nobody has translated yet
/// still reads correctly, just in English.
///
/// The locale is ambient rather than an argument because this constructor is
/// reached from seventy-nine places, many of them helpers several calls below a
/// handler that have no business knowing a request exists. See
/// [`request_locale`] for the scope it is set in and [`i18n::current`] for what
/// happens outside one.
pub(crate) fn api_error(
    status: StatusCode,
    code: &str,
    message: &str,
) -> (StatusCode, Json<Value>) {
    api_error_in(i18n::current(), status, code, message)
}

/// The same constructor with the language named outright, for the tests and for
/// any caller that knows its reader better than the ambient scope does.
pub(crate) fn api_error_in(
    locale: Locale,
    status: StatusCode,
    code: &str,
    message: &str,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "code": code, "message": i18n::t(locale, message) } })),
    )
}
