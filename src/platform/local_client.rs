//! The local client credential: a token a desktop app on this computer reads
//! from the state directory (`local-client.json`) and presents on loopback.
//!
//! The token is minted per gateway process and removed with it. It is only
//! ever accepted from a loopback peer (without proxy headers) and, because
//! the file's permissions are the credential, without the transport proof or
//! the sealed envelope. From any other address it is refused outright.

use std::net::SocketAddr;
use std::sync::RwLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::http::{api_error, bearer_token, Caller};
use crate::{authority, hash_token, DeviceKind};

/// The device id every local client acts as.
pub(crate) const DEVICE_ID: &str = "local-desktop";
/// Set by [`gate`] alone (a client's own copy is dropped first): this request
/// presented the local token from a loopback peer.
const VERIFIED_HEADER: &str = "x-muqun-local-client";

static TOKEN: RwLock<Option<String>> = RwLock::new(None);

pub(crate) fn file_in(state: &std::path::Path) -> std::path::PathBuf {
    state.join("local-client.json")
}

/// Mint this process's token and publish `<state>/local-client.json` for the
/// gateway listening on loopback at `port`. The record removes the file again
/// when dropped.
pub(crate) fn mint(port: u16) -> anyhow::Result<crate::mcp::FileRecord> {
    let token =
        uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
    let file = file_in(&crate::platform::store::state_dir()?);
    let body = serde_json::json!({
        "url": format!("http://127.0.0.1:{port}"),
        "token": token,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    crate::platform::store::write_secret_file(&file, body.to_string().as_bytes())?;
    set_token(Some(token));
    Ok(crate::mcp::FileRecord::keep(file))
}

fn set_token(token: Option<String>) {
    if let Ok(mut slot) = TOKEN.write() {
        *slot = token;
    }
}

/// Whether `presented` is the minted token (constant time).
fn matches(presented: &str) -> bool {
    TOKEN
        .read()
        .ok()
        .and_then(|slot| slot.as_deref().map(hash_token))
        .is_some_and(|hash| authority::authenticates_admin(&hash, presented))
}

/// Outermost request check. Drops any client-supplied marker, then: the local
/// token from a loopback peer is marked verified; the same token from any
/// other peer is refused (403) before anything else sees it.
pub(crate) async fn gate(mut request: Request<Body>, next: Next) -> Response {
    let marker = HeaderName::from_static(VERIFIED_HEADER);
    request.headers_mut().remove(&marker);
    // Sealed requests carry no bearer on the outside; this only matches
    // cleartext ones.
    if let Ok(token) = bearer_token(request.headers()) {
        if matches(token) {
            let peer = request
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|info| info.0);
            if !crate::mcp::auth::is_loopback(peer)
                || crate::mcp::auth::refuse_for_proxy(request.headers())
            {
                return api_error(StatusCode::FORBIDDEN, "invalid_token", "invalid token")
                    .into_response();
            }
            request
                .headers_mut()
                .insert(marker, HeaderValue::from_static("1"));
        }
    }
    next.run(request).await
}

/// The local desktop, when [`gate`] verified this request.
pub(crate) fn caller(headers: &HeaderMap) -> Option<Caller> {
    if !headers.contains_key(VERIFIED_HEADER) {
        return None;
    }
    // Defence in depth: the marker alone is not the credential.
    if !bearer_token(headers).is_ok_and(matches) {
        return None;
    }
    Some(Caller {
        device_id: DEVICE_ID.to_owned(),
        kind: DeviceKind::App,
        name: format!("{} desktop", hostname()),
        server_id: None,
    })
}

/// Whether `device_id` is the local client while its token is live (it is
/// not in the device list, so `still_paired` would otherwise cut its streams).
pub(crate) fn is_live(device_id: &str) -> bool {
    device_id == DEVICE_ID && TOKEN.read().is_ok_and(|slot| slot.is_some())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|name| name.trim().trim_end_matches(".local").to_owned())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "This computer".to_owned())
}

#[cfg(test)]
pub(crate) fn set_token_for_test(token: &str) {
    set_token(Some(token.to_owned()));
}
