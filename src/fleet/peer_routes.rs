//! `/api/peers*` and `PUT /api/fleet/settings`: the owner (their phone, or
//! the CLI on this computer with the admin token) links this gateway with
//! their other ones, lists, renames and unlinks them, and turns the accept
//! switch. Another computer's token reaches none of these.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::peers::{LinkError, LinkRequest, RenameError};
use crate::tasks::domain::now_ms;
use crate::{
    api_error, require_admin, require_device, valid_device_name, valid_request_id, ApiResult,
    AppState,
};

const MAX_URL_BYTES: usize = 512;
const MAX_CODE_CHARS: usize = 16;

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/peers", get(list).post(link))
        .route("/api/peers/{peer_id}", patch(rename).delete(unlink))
        .route("/api/fleet/settings", put(settings))
}

/// The owner (their phone, or the CLI on this computer with the admin token).
fn owner(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if require_admin(&state.config, headers).is_ok() {
        return Ok(());
    }
    require_device(state, headers).map(|_| ())
}

fn invalid_request() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "invalid request",
    )
}

fn store_failed(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::warn!("peer store failed: {error:#}");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "peer_store_failed",
        "the peer request failed",
    )
}

fn peer_not_found() -> (StatusCode, Json<Value>) {
    api_error(StatusCode::NOT_FOUND, "peer_not_found", "peer not found")
}

/// Why the other computer refused, as one of the few reasons a client acts
/// on; anything else it said is `refused`.
fn refusal_reason(code: &str) -> &'static str {
    match code {
        "pairing_code_expired" => "pairing_code_expired",
        "invalid_pairing_code" => "invalid_pairing_code",
        "pairing_not_requested" => "pairing_not_requested",
        "invalid_token" => "invalid_token",
        _ => "refused",
    }
}

fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && valid_device_name(name)
}

/// `GET /api/peers`: the switch and every peer, those not heard from for a
/// minute pinged first (concurrently, 3 s each).
async fn list(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let peers = state.peers.list().await.map_err(store_failed)?;
    let peers =
        futures::future::join_all(peers.into_iter().map(|peer| state.peers.checked(peer))).await;
    let now = now_ms();
    // A peer that no longer knows this gateway was just forgotten.
    let views: Vec<_> = peers.iter().flatten().map(|peer| peer.view(now)).collect();
    Ok(Json(json!({
        "accept_delegations": state.peers.accepts_delegations(),
        "peers": views,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkBody {
    url: String,
    name: String,
    request_id: String,
    code: String,
}

/// `POST /api/peers`: pair with the gateway at `url` through the code it
/// minted (`POST /api/pair/pending` over there) and keep it as `name`.
async fn link(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<LinkBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let Json(body) = body.map_err(|_| invalid_request())?;
    let url = body.url.trim();
    let code = body.code.trim();
    if url.len() > MAX_URL_BYTES
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || !valid_name(&body.name)
        || !valid_request_id(&body.request_id)
        || code.is_empty()
        || code.chars().count() > MAX_CODE_CHARS
    {
        return Err(invalid_request());
    }
    let request = LinkRequest {
        url: url.to_owned(),
        name: body.name,
        request_id: body.request_id,
        code: code.to_owned(),
    };
    let peer = state
        .peers
        .link(&state.config.server_id, request)
        .await
        .map_err(|error| match error {
            LinkError::Unreachable => api_error(
                StatusCode::BAD_GATEWAY,
                "peer_unreachable",
                "the other computer did not answer",
            ),
            LinkError::Refused(code) => {
                let (status, Json(mut body)) = api_error(
                    StatusCode::CONFLICT,
                    "peer_refused",
                    "the other computer refused the link",
                );
                body["error"]["reason"] = json!(refusal_reason(&code));
                (status, Json(body))
            }
            LinkError::Conflict => api_error(
                StatusCode::CONFLICT,
                "peer_conflict",
                "a computer with that id is already linked; remove it first",
            ),
            LinkError::NameTaken => api_error(
                StatusCode::CONFLICT,
                "peer_name_taken",
                "another computer already has that name",
            ),
            LinkError::IsSelf => api_error(
                StatusCode::BAD_REQUEST,
                "peer_is_self",
                "that address is this computer",
            ),
            LinkError::Store(error) => store_failed(error),
        })?;
    Ok(Json(json!(peer.view(now_ms()))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

/// `PATCH /api/peers/{peer_id}`: what this computer calls that one (§10.16).
async fn rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer_id): Path<String>,
    body: Result<Json<RenameBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let Json(body) = body.map_err(|_| invalid_request())?;
    let peer = state
        .peers
        .rename(&peer_id, &body.name)
        .await
        .map_err(|error| match error {
            RenameError::Invalid => invalid_request(),
            RenameError::Taken => api_error(
                StatusCode::CONFLICT,
                "peer_name_taken",
                "another computer already has that name",
            ),
            RenameError::Store(error) => store_failed(error),
        })?
        .ok_or_else(peer_not_found)?;
    Ok(Json(json!(peer.view(now_ms()))))
}

/// `DELETE /api/peers/{peer_id}`: forget it, and ask it to forget this one.
async fn unlink(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer_id): Path<String>,
) -> ApiResult<Response> {
    owner(&state, &headers)?;
    if !state
        .peers
        .unlink(&state, &peer_id)
        .await
        .map_err(store_failed)?
    {
        return Err(peer_not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    accept_delegations: bool,
}

/// `PUT /api/fleet/settings`: the phone's "Accept work from other
/// computers" switch, written to `config.json`.
async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SettingsBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let Json(body) = body.map_err(|_| invalid_request())?;
    state
        .peers
        .set_accepts_delegations(body.accept_delegations)
        .map_err(store_failed)?;
    Ok(Json(
        json!({ "accept_delegations": state.peers.accepts_delegations() }),
    ))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt as _;

    use crate::connectivity::authority::DeviceKind;
    use crate::test_support::{
        named, output_state, peer_router, serve, test_peer_device, FakeHerdr,
    };
    use crate::AppState;

    async fn call(
        state: &AppState,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"));
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = peer_router(state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The whole §5.1 flow on two in-process gateways: the introducer mints
    /// a code on mini, hands it to coding, coding pairs itself with mini as a
    /// gateway device (mini runs `transport_encryption: required`, so every
    /// call after the claim is sealed), lists it, turns its own switch, and
    /// unlinks it, which revokes coding's token on mini.
    #[tokio::test]
    async fn a_gateway_links_to_another_through_a_pairing_code_and_unlinks() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mini = named(output_state(&herdr), "srv-mini", "mini");
        let url = serve(peer_router(mini.clone())).await;
        let coding = named(output_state(&herdr), "srv-coding", "coding");
        let other = named(output_state(&herdr), "srv-other", "other");
        let other_url = serve(peer_router(other.clone())).await;
        let gateways = |state: &AppState| -> Vec<(String, String)> {
            let devices = state.devices.lock().unwrap();
            devices
                .iter()
                .filter(|device| device.kind == DeviceKind::Gateway)
                .map(|device| (device.id.clone(), device.name.clone()))
                .collect()
        };

        // Linked, then linked again (the reconciler ran twice): the id mini
        // gave is display data, so the second link is a conflict, never a
        // merge, and it takes back the device it enrolled -- mini keeps the
        // first device, which a claim could not replace. A second computer
        // under a name a peer holds is refused and keeps nothing either.
        let mut first = Vec::new();
        let mut mini_id = String::new();
        for (target, at, name, expected, code) in [
            (&mini, &url, "mini", StatusCode::OK, Value::Null),
            (
                &mini,
                &url,
                "mini 2",
                StatusCode::CONFLICT,
                json!("peer_conflict"),
            ),
            (
                &other,
                &other_url,
                "MINI",
                StatusCode::CONFLICT,
                json!("peer_name_taken"),
            ),
        ] {
            let (status, minted) = call(
                target,
                "POST",
                "/api/pair/pending",
                "token",
                Some(json!({ "device_name": "coding", "kind": "gateway" })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{minted}");
            let (status, peer) = call(
                &coding,
                "POST",
                "/api/peers",
                "token",
                Some(json!({
                    "url": at, "name": name,
                    "request_id": minted["request_id"], "code": minted["code"],
                })),
            )
            .await;
            assert_eq!(
                (status, &peer["error"]["code"]),
                (expected, &code),
                "{name}: {peer}"
            );
            assert!(
                peer.get("token").is_none(),
                "the token never leaves the gateway"
            );
            if status == StatusCode::OK {
                assert_eq!(peer["server_id"], "srv-mini");
                assert_ne!(peer["peer_id"], "srv-mini", "keyed by coding's own id");
                mini_id = peer["peer_id"].as_str().unwrap().to_owned();
                first = gateways(&mini);
            }
        }
        assert_eq!(
            first
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>(),
            ["coding"]
        );
        assert_eq!(
            gateways(&mini),
            first,
            "the first device stays, the second went"
        );
        assert!(
            gateways(&other).is_empty(),
            "a refused link revokes what it enrolled"
        );

        for (body, accept) in [
            (json!({ "accept_delegations": false }), false),
            (json!({ "accept_delegations": true }), true),
        ] {
            let (status, _) =
                call(&coding, "PUT", "/api/fleet/settings", "token", Some(body)).await;
            assert_eq!(status, StatusCode::OK);
            let (_, listed) = call(&coding, "GET", "/api/peers", "token", None).await;
            assert_eq!(listed["accept_delegations"], accept);
            assert_eq!(listed["peers"][0]["name"], "mini");
            assert_eq!(
                listed["peers"][0]["reachable"], true,
                "the link itself was a successful call"
            );
        }

        // The phone renamed mini: the reconciler keeps coding's name in step (§10.16).
        let (status, renamed) = call(
            &coding,
            "PATCH",
            &format!("/api/peers/{mini_id}"),
            "token",
            Some(json!({ "name": "mac mini" })),
        )
        .await;
        assert_eq!(
            (status, renamed["name"].clone()),
            (StatusCode::OK, json!("mac mini"))
        );
        assert_eq!(
            coding
                .peers
                .resolve("MAC MINI")
                .await
                .map(|peer| peer.server_id),
            Some("srv-mini".to_owned())
        );
        let (status, _) = call(
            &coding,
            "PATCH",
            "/api/peers/srv-nope",
            "token",
            Some(json!({ "name": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(
            &coding,
            "DELETE",
            &format!("/api/peers/{mini_id}"),
            "token",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(mini
            .devices
            .lock()
            .unwrap()
            .iter()
            .all(|device| device.kind != DeviceKind::Gateway));
        let (_, listed) = call(&coding, "GET", "/api/peers", "token", None).await;
        assert_eq!(listed["peers"], json!([]));
    }

    /// What a peer's token may do on the computer that enrolled it: its
    /// tasks and its own revocation, nothing an owner's phone does.
    #[tokio::test]
    async fn another_computer_s_token_reaches_only_the_peer_routes() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let state = output_state(&herdr);
        state
            .devices
            .lock()
            .unwrap()
            .push(test_peer_device("peer-1", "peer-token"));
        for (method, path, status) in [
            ("GET", "/api/tasks", StatusCode::OK),
            ("GET", "/api/tasks/t_missing", StatusCode::NOT_FOUND),
            ("GET", "/api/tasks?root=t_missing", StatusCode::FORBIDDEN),
            ("GET", "/api/tasks/defaults", StatusCode::FORBIDDEN),
            // Opens a task as a phone does (this body has no goal).
            ("POST", "/api/tasks", StatusCode::BAD_REQUEST),
            ("GET", "/api/pairings", StatusCode::FORBIDDEN),
            ("GET", "/health", StatusCode::FORBIDDEN),
            ("PATCH", "/api/peers/srv-x", StatusCode::FORBIDDEN),
            ("POST", "/api/pair/pending", StatusCode::FORBIDDEN),
            ("GET", "/api/peers", StatusCode::FORBIDDEN),
            ("PUT", "/api/fleet/settings", StatusCode::FORBIDDEN),
            (
                "POST",
                "/api/tasks/t_missing/complete",
                StatusCode::FORBIDDEN,
            ),
            ("DELETE", "/api/pairings/d1", StatusCode::FORBIDDEN),
            ("DELETE", "/api/pairings/peer-1", StatusCode::OK),
        ] {
            let body = (method != "GET" && method != "DELETE")
                .then(|| json!({ "device_name": "x", "name": "x", "accept_delegations": true }));
            let (got, answer) = call(&state, method, path, "peer-token", body).await;
            assert_eq!(got, status, "{method} {path}: {answer}");
            if got == StatusCode::FORBIDDEN {
                assert_eq!(answer["error"]["code"], "peer_forbidden", "{method} {path}");
            }
        }
    }
}
