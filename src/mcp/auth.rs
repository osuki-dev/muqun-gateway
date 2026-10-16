//! Who may call `/mcp`: a loopback peer, not relayed by a proxy, holding a live
//! assistant token.
//!
//! An assistant token authorises nothing but this endpoint, and a device token
//! is not an assistant token, so neither credential stands in for the other.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::Value;

use super::tokens::{AssistantSession, AssistantTokenStore, Resolved};
use crate::{api_error, bearer_token};

/// Headers a reverse proxy adds. Tailscale Serve forwards to `127.0.0.1`, so a
/// request it relays from the tailnet arrives from a loopback peer; these are
/// how it is told apart from a local agent.
const PROXY_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "tailscale-user-login",
];

const LOOPBACK_ONLY: &str = "the MCP endpoint answers only local callers on this machine";

pub(crate) fn is_loopback(peer: Option<SocketAddr>) -> bool {
    match peer.map(|peer| peer.ip()) {
        Some(IpAddr::V4(ip)) => ip.is_loopback(),
        // `::ffff:127.0.0.1` is loopback too, when a dual-stack socket maps it.
        Some(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        None => false,
    }
}

pub(crate) fn refuse_for_proxy(headers: &HeaderMap) -> bool {
    PROXY_HEADERS.iter().any(|name| headers.contains_key(*name))
}

/// The caller's assistant session, or the error response to send instead.
/// Checked in order: loopback peer, no proxy headers, a bearer token, a live
/// assistant token. An expired token is 401, so a host that re-runs its
/// header helper on 401 recovers; an unknown or revoked one is 403.
pub(crate) async fn authenticate(
    tokens: &dyn AssistantTokenStore,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
) -> Result<AssistantSession, (StatusCode, Json<Value>)> {
    if !is_loopback(peer) || refuse_for_proxy(headers) {
        tracing::debug!(?peer, "refused a non-local /mcp request");
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "mcp_loopback_only",
            LOOPBACK_ONLY,
        ));
    }
    let token = bearer_token(headers)?;
    match tokens.resolve(token).await {
        Ok(Resolved::Live(session)) => Ok(session),
        Ok(Resolved::Expired) => Err(api_error(
            StatusCode::UNAUTHORIZED,
            "token_expired",
            "Token expired",
        )),
        Ok(Resolved::Invalid) => Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        )),
        Err(error) => {
            tracing::warn!(%error, "could not resolve an assistant token");
            Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token_store_unavailable",
                "the assistant token store is unavailable",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn only_loopback_peers_are_loopback() {
        for peer in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
            assert!(is_loopback(Some(peer.parse().unwrap())), "{peer}");
        }
        for peer in [
            "100.64.0.2:1",
            "192.168.1.5:1",
            "[fd7a:115c::1]:1",
            "0.0.0.0:1",
        ] {
            assert!(!is_loopback(Some(peer.parse().unwrap())), "{peer}");
        }
        assert!(!is_loopback(None), "an unknown peer fails closed");
    }

    #[test]
    fn any_reverse_proxy_header_is_refused() {
        assert!(!refuse_for_proxy(&crate::bearer_headers("t")));
        for name in PROXY_HEADERS {
            let mut headers = crate::bearer_headers("t");
            headers.insert(*name, HeaderValue::from_static("x"));
            assert!(refuse_for_proxy(&headers), "{name}");
        }
    }
}
