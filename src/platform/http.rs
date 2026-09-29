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
            tracing::warn!("failed to persist device last-seen: {err:#}");
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
/// pending pairing code and the device list. It is required in every
/// transport mode: `disabled` drops the envelope, never authentication, and
/// the pending code plus its request id is exactly what an attacker on the
/// same network needs to claim a pairing before the user does.
pub(crate) fn require_admin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
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

/// Lowercase hex for a digest. `sha2` 0.11's output array no longer formats
/// with `{:x}`, and both the approval fingerprint and the JSON ETag want the
/// same spelling.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::*;

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

    /// A device that was never paired is not paired now either -- the recheck
    /// is an identity test, not a "did anything change" test.
    #[test]
    fn a_device_that_was_never_paired_is_not_still_paired() {
        let state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        assert!(!still_paired(&state, "phone-2"));
        assert!(!still_paired(&state, ""));
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
}
