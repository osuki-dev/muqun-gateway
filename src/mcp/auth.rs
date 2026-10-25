//! Who may call `/mcp`: a loopback peer, not relayed by a proxy, holding a live
//! assistant token.
//!
//! An assistant token authorises nothing but this endpoint, and a device token
//! is not an assistant token, so neither credential stands in for the other.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::Value;

use super::tokens::{AssistantSession, AssistantTokenStore, Resolved, TokenBinding};
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
        Ok(Resolved::Live(session))
            if session.label == "codex (runtime)"
                && matches!(&session.binding, TokenBinding::Agent { agent_id } if agent_id == "codex") =>
        {
            // Old shared native credentials remain inert until their existing
            // expiry, and must refresh before entering any tool.
            // HTTP 401 lets the official header helper obtain an explicit
            // runtime grant; a tool-level error would leave it cached forever.
            Err(api_error(
                StatusCode::UNAUTHORIZED,
                "token_refresh_required",
                "Refresh the managed MCP credential before calling tools",
            ))
        }
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

    #[tokio::test]
    async fn managed_codex_refresh_preserves_other_cached_clients_and_explicit_revocation() {
        let state = crate::test_support::test_state("synthetic-admin", Vec::new());
        let tokens = state.mcp_tokens.as_ref();
        let legacy = tokens
            .issue(
                TokenBinding::Agent {
                    agent_id: "codex".into(),
                },
                "codex (runtime)".into(),
                60_000,
            )
            .await
            .unwrap();
        let second_cached = tokens
            .issue(
                legacy.session.binding.clone(),
                legacy.session.label.clone(),
                60_000,
            )
            .await
            .unwrap();
        let explicitly_revoked = tokens
            .issue(
                legacy.session.binding.clone(),
                legacy.session.label.clone(),
                60_000,
            )
            .await
            .unwrap();
        assert!(tokens
            .revoke(&explicitly_revoked.session.token_id)
            .await
            .unwrap());
        let mut untouched = Vec::new();
        for (binding, label) in [
            (
                TokenBinding::Agent {
                    agent_id: "opencode".into(),
                },
                "codex (runtime)",
            ),
            (
                TokenBinding::Agent {
                    agent_id: "codex".into(),
                },
                "user-managed",
            ),
            (
                TokenBinding::Pane {
                    session_id: "terminal".into(),
                    pane_id: "pane".into(),
                },
                "codex (runtime)",
            ),
            (
                TokenBinding::Task {
                    task_id: "retained-task".into(),
                },
                "codex (runtime)",
            ),
        ] {
            untouched.push(tokens.issue(binding, label.into(), 60_000).await.unwrap());
        }
        let peer = Some("127.0.0.1:12345".parse().unwrap());
        let headers = crate::bearer_headers(&legacy.token);
        let (status, body) = authenticate(tokens, peer, &headers).await.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0["error"]["code"], "token_refresh_required");
        // A retained restricted session blocks the new grant. Authentication
        // never deletes old records just to force renewal past this guard.
        state
            .db
            .call(|db| {
                db.execute(
                    "INSERT INTO task_runtime_restrictions(agent_id) VALUES('codex')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(tokens
            .issue(
                TokenBinding::Runtime {
                    agent_id: "codex".into()
                },
                "codex (runtime)".into(),
                60_000
            )
            .await
            .is_err());
        assert!(matches!(
            tokens.resolve(&legacy.token).await.unwrap(),
            Resolved::Live(_)
        ));
        state
            .db
            .call(|db| {
                db.execute(
                    "DELETE FROM task_runtime_restrictions WHERE agent_id = 'codex'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let runtime = tokens
            .issue(
                TokenBinding::Runtime {
                    agent_id: "codex".into(),
                },
                "codex (runtime)".into(),
                60_000,
            )
            .await
            .unwrap();
        assert!(
            authenticate(tokens, peer, &crate::bearer_headers(&runtime.token))
                .await
                .is_ok()
        );
        // Both cached clients need the helper handshake after the first one
        // issues its replacement. Neither legacy credential authenticates.
        for old in [&legacy, &second_cached] {
            let (status, body) = authenticate(tokens, peer, &crate::bearer_headers(&old.token))
                .await
                .unwrap_err();
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(body.0["error"]["code"], "token_refresh_required");
        }
        let (status, body) = authenticate(
            tokens,
            peer,
            &crate::bearer_headers(&explicitly_revoked.token),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.0["error"]["code"], "invalid_token");
        for token in untouched {
            assert!(matches!(
                tokens.resolve(&token.token).await.unwrap(),
                Resolved::Live(_)
            ));
        }
        // Explicit administrator revocation remains final for that credential.
        tokens.revoke(&legacy.session.token_id).await.unwrap();
        assert_eq!(
            tokens.resolve(&legacy.token).await.unwrap(),
            Resolved::Invalid
        );
        assert_eq!(
            tokens.resolve(&second_cached.token).await.unwrap(),
            Resolved::Live(second_cached.session.clone())
        );
        tokens.revoke_all().await.unwrap();
        assert_eq!(
            tokens.resolve(&second_cached.token).await.unwrap(),
            Resolved::Invalid
        );
        assert_eq!(
            tokens.resolve(&runtime.token).await.unwrap(),
            Resolved::Invalid
        );
        assert!(tokens.purge_dead(u64::MAX >> 1).await.unwrap() > 0);
        let remaining = state
            .db
            .call(|db| {
                Ok(
                    db.query_row("SELECT COUNT(*) FROM assistant_tokens", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

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
