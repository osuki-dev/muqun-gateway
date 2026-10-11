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

use super::peers::{LinkError, LinkRequest, PeerView, RenameError};
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

/// `name` is this computer's own: a peer called so would make `machine`
/// ambiguous to the agent, and the phone shows both under one name.
fn own_name(state: &AppState, name: &str) -> ApiResult<()> {
    if super::peers::same_name(name.trim(), &state.config.label) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "name_conflict",
            "that is this computer's own name; give the other computer another one",
        ));
    }
    Ok(())
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

/// An address a peer is reached at: http(s), at most [`MAX_URL_BYTES`].
fn valid_url(url: &str) -> bool {
    let url = url.trim();
    url.len() <= MAX_URL_BYTES && (url.starts_with("http://") || url.starts_with("https://"))
}

fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && valid_device_name(name) && !super::peers::reserved_name(name)
}

/// `GET /api/peers`: the switch and every peer, those not heard from for a
/// minute pinged first (concurrently, 3 s each).
async fn list(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let peers = state.peers.list().await.map_err(store_failed)?;
    futures::future::join_all(
        peers
            .into_iter()
            .map(|peer| state.peers.checked(&state, peer)),
    )
    .await;
    // Read again: a peer that no longer takes this gateway's token was just
    // marked so, and is listed as `unauthorized` for the phone to link again.
    let peers = state.peers.list().await.map_err(store_failed)?;
    let now = now_ms();
    let views: Vec<_> = peers
        .iter()
        .map(|peer| PeerView {
            reachable: state.peers.reachable(peer, now),
            ..peer.view(now)
        })
        .collect();
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
    if !valid_url(url)
        || !valid_name(&body.name)
        || !valid_request_id(&body.request_id)
        || code.is_empty()
        || code.chars().count() > MAX_CODE_CHARS
    {
        return Err(invalid_request());
    }
    // Before the claim: the code stays good for a link under another name.
    own_name(&state, &body.name)?;
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
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// `PATCH /api/peers/{peer_id}`: what this computer calls that one (§10.16),
/// and/or where it is reached now (its address changed), proven with one
/// call before it is kept.
async fn rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer_id): Path<String>,
    body: Result<Json<RenameBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    owner(&state, &headers)?;
    let Json(body) = body.map_err(|_| invalid_request())?;
    if (body.name.is_none() && body.url.is_none())
        || body.url.as_deref().is_some_and(|url| !valid_url(url))
    {
        return Err(invalid_request());
    }
    if let Some(name) = &body.name {
        own_name(&state, name)?;
    }
    let peer = state
        .peers
        .edit(&peer_id, body.name.as_deref(), body.url.as_deref())
        .await
        .map_err(|error| match error {
            RenameError::Invalid => invalid_request(),
            RenameError::Taken => api_error(
                StatusCode::CONFLICT,
                "peer_name_taken",
                "another computer already has that name",
            ),
            RenameError::Unreachable => api_error(
                StatusCode::BAD_GATEWAY,
                "peer_unreachable",
                "nothing answered at the new address",
            ),
            RenameError::Refused(code) => {
                let (status, Json(mut body)) = api_error(
                    StatusCode::CONFLICT,
                    "peer_refused",
                    "what answered at the new address does not know this link",
                );
                body["error"]["reason"] = json!(refusal_reason(&code));
                (status, Json(body))
            }
            RenameError::Store(error) => store_failed(error),
        })?
        .ok_or_else(peer_not_found)?;
    Ok(Json(json!(peer.view(now_ms()))))
}

/// `DELETE /api/peers/{peer_id}`: forget it, ask it to forget this one, and
/// revoke its own login here; says which of the two revokes took.
async fn unlink(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer_id): Path<String>,
) -> ApiResult<Response> {
    // The owner only: the admin token or an app device, never a peer's token.
    owner(&state, &headers)?;
    let unlinked = state
        .peers
        .unlink(&state, &peer_id)
        .await
        .ok_or_else(peer_not_found)?;
    if !unlinked.forgotten {
        // The revokes ran; say how, though the row could not go.
        let (status, Json(mut body)) = api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "peer_store_failed",
            "the peer could not be forgotten",
        );
        body.as_object_mut()
            .expect("an error body is an object")
            .extend(json!(unlinked).as_object().cloned().unwrap_or_default());
        return Ok((status, Json(body)).into_response());
    }
    Ok(Json(json!(unlinked)).into_response())
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

    use crate::connectivity::authority::{DeviceKind, DeviceRecord};
    use crate::fleet::peers::Peer;
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
            // This computer's own name: refused before the claim, so the
            // code is still pending over there until its minter takes it back.
            (
                &other,
                &other_url,
                "Coding",
                StatusCode::CONFLICT,
                json!("name_conflict"),
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
                Some(json!({ "device_name": "coding", "kind": "gateway", "server_id": "srv-coding" })),
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
            if code == json!("name_conflict") {
                let cancel = format!(
                    "/api/pair/pending/{}",
                    minted["request_id"].as_str().unwrap()
                );
                for expected in [StatusCode::OK, StatusCode::NOT_FOUND] {
                    let (status, _) = call(target, "DELETE", &cancel, "token", None).await;
                    assert_eq!(status, expected, "cancel the unclaimed code");
                }
            }
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
            mini.devices
                .lock()
                .unwrap()
                .iter()
                .any(|device| device.server_id.as_deref() == Some("srv-coding")),
            "the phone said whose login it is"
        );
        assert!(
            gateways(&other).is_empty(),
            "a refused link revokes what it enrolled"
        );

        // An app is never paired this way. A minted code nobody claimed
        // within a minute (a link that failed) gives its place to the next,
        // and is neither shown nor claimable.
        let mint = |kind: &str| json!({ "device_name": "coding", "kind": kind, "server_id": "srv-coding" });
        for (kind, stale, expected) in [
            ("app", false, StatusCode::BAD_REQUEST),
            ("gateway", false, StatusCode::OK),
            ("gateway", false, StatusCode::CONFLICT),
            ("gateway", true, StatusCode::OK),
        ] {
            if stale {
                let mut pending = other.pending_pairing.lock().unwrap();
                pending.as_mut().unwrap().created_unix_ms -= 61_000;
            }
            let (status, _) = call(
                &other,
                "POST",
                "/api/pair/pending",
                "token",
                Some(mint(kind)),
            )
            .await;
            assert_eq!(status, expected, "{kind}, stale: {stale}");
        }
        let (_, minted) = call(&other, "GET", "/api/pair/pending", "admin", None).await;
        other
            .pending_pairing
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .created_unix_ms -= 61_000;
        let (_, shown) = call(&other, "GET", "/api/pair/pending", "admin", None).await;
        assert_eq!(
            (minted["pending"].clone(), shown["pending"].clone()),
            (json!(true), json!(false))
        );
        *other.pending_pairing.lock().unwrap() = None;
        let (_, minted) = call(
            &other,
            "POST",
            "/api/pair/pending",
            "token",
            Some(mint("gateway")),
        )
        .await;
        other
            .pending_pairing
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .created_unix_ms -= 61_000;
        let (status, refused) = call(
            &coding,
            "POST",
            "/api/peers",
            "token",
            Some(json!({
                "url": other_url, "name": "other",
                "request_id": minted["request_id"], "code": minted["code"],
            })),
        )
        .await;
        assert_eq!(
            (status, &refused["error"]["reason"]),
            (StatusCode::CONFLICT, &json!("pairing_code_expired")),
            "{refused}"
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
        // Its address changed: kept once the link's token works there; an
        // address where another computer answers, or nothing, is refused.
        let renamed_host = url.replace("127.0.0.1", "localhost");
        for (moved, expected, code) in [
            (format!("{renamed_host}/"), StatusCode::OK, Value::Null),
            (
                other_url.clone(),
                StatusCode::CONFLICT,
                json!("peer_refused"),
            ),
            (
                "http://127.0.0.1:9".to_owned(),
                StatusCode::BAD_GATEWAY,
                json!("peer_unreachable"),
            ),
            (
                "ftp://mini".to_owned(),
                StatusCode::BAD_REQUEST,
                json!("invalid_request"),
            ),
        ] {
            let (status, answer) = call(
                &coding,
                "PATCH",
                &format!("/api/peers/{mini_id}"),
                "token",
                Some(json!({ "url": moved })),
            )
            .await;
            assert_eq!(
                (status, &answer["error"]["code"]),
                (expected, &code),
                "{moved}: {answer}"
            );
            let kept = coding.peers.get(&mini_id).await.unwrap();
            assert_eq!(
                (kept.url.as_str(), kept.name.as_str()),
                (renamed_host.as_str(), "mac mini"),
                "{moved}"
            );
        }
        // Nor this computer's own name: the agent could not tell them apart.
        let (status, refused) = call(
            &coding,
            "PATCH",
            &format!("/api/peers/{mini_id}"),
            "token",
            Some(json!({ "name": "CODING" })),
        )
        .await;
        assert_eq!(
            (status, &refused["error"]["code"]),
            (StatusCode::CONFLICT, &json!("name_conflict"))
        );
        // `self` always means the computer asked, so no peer may take it.
        let (status, _) = call(
            &coding,
            "PATCH",
            &format!("/api/peers/{mini_id}"),
            "token",
            Some(json!({ "name": "Self" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &coding,
            "PATCH",
            "/api/peers/srv-nope",
            "token",
            Some(json!({ "name": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Mini stops taking coding's token: the link is kept and shown as
        // unauthorized, and linking again under its name replaces it.
        mini.devices
            .lock()
            .unwrap()
            .retain(|device| device.kind != DeviceKind::Gateway);
        crate::fleet::mirror::unauthorized(&coding, &mini_id).await;
        let (_, listed) = call(&coding, "GET", "/api/peers", "token", None).await;
        assert_eq!(
            (
                &listed["peers"][0]["unauthorized"],
                &listed["peers"][0]["reachable"]
            ),
            (&json!(true), &json!(false))
        );
        let (status, minted) = call(
            &mini,
            "POST",
            "/api/pair/pending",
            "token",
            Some(json!({ "device_name": "coding", "kind": "gateway", "server_id": "srv-coding" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{minted}");
        let (status, relinked) = call(
            &coding,
            "POST",
            "/api/peers",
            "token",
            Some(json!({
                "url": url, "name": "Mac Mini",
                "request_id": minted["request_id"], "code": minted["code"],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{relinked}");
        assert!(relinked.get("unauthorized").is_none(), "{relinked}");
        let (_, listed) = call(&coding, "GET", "/api/peers", "token", None).await;
        assert_eq!(
            listed["peers"].as_array().unwrap().len(),
            1,
            "replaced, not added"
        );
        mini_id = relinked["peer_id"].as_str().unwrap().to_owned();

        // Mini's own login here, and records that only look like it: unlinking
        // mini takes back both directions, and only a gateway device that
        // paired as exactly mini's id.
        for (id, server_id, kind) in [
            ("mini-login", "srv-mini", DeviceKind::Gateway),
            ("other-login", "srv-other", DeviceKind::Gateway),
            ("near-login", "srv-mini-2", DeviceKind::Gateway),
            ("phone", "srv-mini", DeviceKind::App),
        ] {
            coding.devices.lock().unwrap().push(DeviceRecord {
                server_id: Some(server_id.into()),
                kind,
                ..test_peer_device(id, id)
            });
        }
        // Neither a stranger nor mini's own login can unlink it.
        for token in ["nobody", "mini-login"] {
            let (status, _) = call(
                &coding,
                "DELETE",
                &format!("/api/peers/{mini_id}"),
                token,
                None,
            )
            .await;
            assert!(status.is_client_error(), "{token}: {status}");
        }
        let (status, unlinked) = call(
            &coding,
            "DELETE",
            &format!("/api/peers/{mini_id}"),
            "token",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{unlinked}");
        assert_eq!(
            unlinked,
            json!({ "remote_revoked": true, "counterpart_found": true, "counterpart_revoked": true })
        );
        let left: Vec<String> = coding
            .devices
            .lock()
            .unwrap()
            .iter()
            .map(|device| device.id.clone())
            .collect();
        for (id, kept) in [
            ("mini-login", false),
            ("other-login", true),
            ("near-login", true),
            ("phone", true),
        ] {
            assert_eq!(left.iter().any(|left| left == id), kept, "{id}");
        }
        assert!(mini
            .devices
            .lock()
            .unwrap()
            .iter()
            .all(|device| device.kind != DeviceKind::Gateway));
        let (_, listed) = call(&coding, "GET", "/api/peers", "token", None).await;
        assert_eq!(listed["peers"], json!([]));

        // Each step is told apart, and none waits on another: a peer that does
        // not answer is still forgotten, and its login here still revoked.
        for (server_id, login, expected) in [
            (
                "srv-nas",
                true,
                json!({ "remote_revoked": false, "counterpart_found": true, "counterpart_revoked": true }),
            ),
            (
                "srv-gone",
                false,
                json!({ "remote_revoked": false, "counterpart_found": false, "counterpart_revoked": false }),
            ),
        ] {
            coding
                .peers
                .store_for_test(Peer {
                    peer_id: format!("p-{server_id}"),
                    server_id: server_id.into(),
                    name: server_id.into(),
                    url: "http://127.0.0.1:9".into(),
                    token: "t".into(),
                    device_id: "d".into(),
                    transport_key: None,
                    transport_min_version: 1,
                    added_ms: 0,
                    last_seen_ms: None,
                    capabilities: vec![],
                    manifest: None,
                    unauthorized: false,
                })
                .await;
            if login {
                coding.devices.lock().unwrap().push(DeviceRecord {
                    server_id: Some(server_id.into()),
                    ..test_peer_device("nas-login", "nas-login")
                });
                // The phone's list carries it, to match the login by.
                let (_, listed) = call(&coding, "GET", "/api/pairings", "token", None).await;
                let login = listed["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|d| d["id"] == "nas-login");
                assert_eq!(login.unwrap()["server_id"], server_id, "{listed}");
            }
            let (status, unlinked) = call(
                &coding,
                "DELETE",
                &format!("/api/peers/p-{server_id}"),
                "token",
                None,
            )
            .await;
            assert_eq!(
                (status, unlinked),
                (StatusCode::OK, expected),
                "{server_id}"
            );
            assert!(coding.peers.get(&format!("p-{server_id}")).await.is_none());
        }
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
        // Files are a device's uploads here: another computer has none.
        let body = json!({ "goal": "g", "attachments": ["/tmp/x.png"] });
        let (got, answer) = call(&state, "POST", "/api/tasks", "peer-token", Some(body)).await;
        assert_eq!(got, StatusCode::BAD_REQUEST, "{answer}");
        assert_eq!(answer["error"]["code"], "attachments_unsupported");
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
            ("DELETE", "/api/peers/srv-x", StatusCode::FORBIDDEN),
            ("POST", "/api/pair/pending", StatusCode::FORBIDDEN),
            ("DELETE", "/api/pair/pending/mint-1", StatusCode::FORBIDDEN),
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
