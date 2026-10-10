//! The gateway as an MCP server for agents on this machine (demo slice).
//!
//! - `/mcp` on the existing listener: rmcp Streamable HTTP, stateless
//!   (MCP 2026-07-28; older handshakes are served per request too).
//! - Loopback peers only, no proxy headers, a live assistant token
//!   (`auth.rs`, `tokens.rs`). The token authorises this endpoint and nothing
//!   else; each request is served for the session it resolves to.
//! - Tools in `tools.rs`; `exec.run` runs in a visible pane through
//!   `tasks::run_command`; every task goes through `tasks::service`.
//! - `hosts.rs`: the config `muqun-gateway mcp enable <host>` writes or prints.

pub(crate) mod auth;
pub(crate) mod hosts;
pub(crate) mod inject;
pub(crate) mod issue;
pub(crate) mod lineage;
pub(crate) mod token_routes;
pub(crate) mod tokens;
pub(crate) mod tokens_sqlite;
pub(crate) mod tools;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::platform::config::MAX_REQUEST_BODY_BYTES;
use crate::AppState;

pub(crate) const MCP_PATH: &str = "/mcp";

/// The `/mcp` URL a process on this machine uses for a gateway listening on
/// `listen`: the listen host, or `127.0.0.1` for a wildcard (or unparsable)
/// address; the configured port.
/// A warning for a gateway whose listen address is neither a wildcard nor
/// loopback: `/mcp` refuses non-loopback peers, and a local process
/// connecting to that address arrives from it.
pub(crate) fn loopback_warning(listen: &str) -> Option<String> {
    let ip = listen.parse::<SocketAddr>().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| {
        format!(
            "warning: /mcp only accepts loopback callers; this gateway listens on {ip}, \
             so MCP tools will be refused until it also listens on 127.0.0.1 or 0.0.0.0"
        )
    })
}

/// The `/mcp` URL this gateway's own loopback listener serves, set once at
/// startup when the configured address is not loopback ([`bind_loopback`]).
static LOOPBACK_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Where the running gateway's loopback MCP address is recorded, for the
/// CLI (another process) and `manage`.
pub(crate) fn endpoint_file() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::platform::store::state_dir()?.join("mcp-endpoint"))
}

/// A gateway that listens on one non-loopback address (a tailnet IP, say)
/// cannot be called on `/mcp` by its own agents, which `/mcp` requires to
/// be loopback callers: it also listens on `127.0.0.1` (and `::1`) for
/// `/mcp` alone -- the configured port when free, else any. The URL is what
/// agents are handed from then on ([`local_url`]) and is recorded in
/// [`endpoint_file`]. `None`: the configured address already serves loopback.
pub(crate) async fn bind_loopback(
    listen: &str,
) -> anyhow::Result<Option<Vec<tokio::net::TcpListener>>> {
    if loopback_warning(listen).is_none() {
        return Ok(None);
    }
    let port = listen.parse::<SocketAddr>().map_or(0, |addr| addr.port());
    let v4 = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    // Best effort: a machine without IPv6 loopback still has 127.0.0.1.
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    Ok(Some(listeners))
}

/// Record `url` as this process's loopback `/mcp` address.
pub(crate) fn use_loopback_url(url: String) {
    let _ = LOOPBACK_URL.set(url);
}

/// The address `/mcp` is served at on this computer's loopback, from the
/// running gateway's record when it has one (see [`bind_loopback`]), else
/// from the listen address ([`local_url`]).
pub(crate) fn recorded_url(listen: &str) -> String {
    endpoint_file()
        .ok()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .map(|url| url.trim().to_owned())
        .filter(|url| url.starts_with("http://127.0.0.1:"))
        .unwrap_or_else(|| local_url(listen))
}

/// `/mcp` alone, for the loopback listener: none of the device routes.
pub(crate) fn loopback_router(state: AppState) -> Router {
    mount(Router::new())
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

pub(crate) fn local_url(listen: &str) -> String {
    if let Some(url) = LOOPBACK_URL.get() {
        return url.clone();
    }
    let addr = match listen.parse::<SocketAddr>() {
        Ok(addr) if !addr.ip().is_unspecified() => addr,
        Ok(addr) => SocketAddr::from(([127, 0, 0, 1], addr.port())),
        Err(_) => SocketAddr::from(([127, 0, 0, 1], crate::DEFAULT_PORT)),
    };
    format!("http://{addr}{MCP_PATH}")
}

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    token_routes::mount(router.route(MCP_PATH, any(serve)))
}

/// The request reaches rmcp unread, so axum's `DefaultBodyLimit` does not
/// apply; rmcp enforces this limit itself while streaming the body (413).
fn config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
}

/// Authenticate, then serve the request with a stateless rmcp service whose
/// handler is bound to the caller's assistant session.
async fn serve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let session =
        match auth::authenticate(state.mcp_tokens.as_ref(), Some(peer), request.headers()).await {
            Ok(session) => session,
            Err(refusal) => return refusal.into_response(),
        };
    let in_task = tools::started_for_task(&state, &session).await;
    let service = StreamableHttpService::new(
        move || Ok(tools::MuqunMcp::new(state.clone(), session.clone()).in_task(in_task)),
        Arc::new(NeverSessionManager::default()),
        config(),
    );
    let Ok(response) = tower::ServiceExt::oneshot(service, request).await;
    response.map(Body::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tokens::TokenBinding;
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    use tower::ServiceExt as _;

    fn app(state: &AppState) -> Router {
        Router::new()
            .merge(mount(Router::new()))
            .with_state(state.clone())
    }

    fn pane() -> TokenBinding {
        TokenBinding::Pane {
            session_id: "s1".into(),
            pane_id: "%1".into(),
        }
    }

    async fn issue(state: &AppState) -> String {
        state
            .mcp_tokens
            .issue(pane(), "test".into(), 60_000)
            .await
            .unwrap()
            .token
    }

    fn post(peer: &str, token: Option<&str>, extra: &[(&str, &str)]) -> Request {
        rpc(peer, token, extra, "tools/list", json!({}))
    }

    /// A stateless `method` request; `params` gains the per-request `_meta`.
    fn rpc(
        peer: &str,
        token: Option<&str>,
        extra: &[(&str, &str)],
        method: &str,
        mut params: Value,
    ) -> Request {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut request = Request::post(MCP_PATH)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("host", "127.0.0.1:23999")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let mut request = request.body(Body::from(body.to_string())).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        request
    }

    async fn send(state: &AppState, request: Request) -> (StatusCode, Value) {
        let response = app(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }

    fn refused(answer: &(StatusCode, Value), status: StatusCode, code: &str) {
        assert_eq!(answer.0, status, "{}", answer.1);
        assert_eq!(answer.1["error"]["code"], code, "{}", answer.1);
    }

    #[tokio::test]
    async fn a_local_caller_with_a_live_token_lists_the_tools() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let (status, body) = send(&state, post("127.0.0.1:5", Some(&token), &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"exec.run"), "{body}");
    }

    /// Each tool answers over `/mcp` with one JSON text block. Arguments
    /// that cannot succeed keep this off the terminal, agents and probes.
    #[tokio::test]
    async fn every_tool_answers_a_tools_call_over_mcp() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let calls = [
            (
                "fleet.describe",
                json!({ "machine": "mac" }),
                "machine_unknown",
            ),
            (
                "exec.run",
                json!({ "cwd": "relative", "command": "true" }),
                "invalid_arguments",
            ),
            (
                "tasks.delegate",
                json!({ "goal": "g", "machine": "mac" }),
                "machine_unknown",
            ),
            (
                "tasks.wait",
                json!({ "task_id": "t_x", "timeout_s": 1 }),
                "task_not_found",
            ),
            (
                "tasks.read",
                json!({ "task_id": "t_x", "what": "summary" }),
                "task_not_found",
            ),
            (
                "tasks.cancel",
                json!({ "task_id": "t_x" }),
                "task_not_found",
            ),
        ];
        for (name, arguments, code) in calls {
            let params = json!({ "name": name, "arguments": arguments });
            let request = rpc(
                "127.0.0.1:5",
                Some(&token),
                &[("mcp-name", name)],
                "tools/call",
                params,
            );
            let (status, body) = send(&state, request).await;
            assert_eq!(status, StatusCode::OK, "{name}: {body}");
            let result = &body["result"];
            assert_eq!(result["isError"], true, "{name}: {body}");
            let text = result["content"][0]["text"].as_str().unwrap();
            let answer: Value = serde_json::from_str(text).unwrap();
            assert_eq!(answer["error"], code, "{name}: {body}");
        }
    }

    /// `fleet.list` succeeds over `/mcp` (it runs this machine's probes).
    #[tokio::test]
    async fn fleet_list_answers_over_mcp() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let params = json!({ "name": "fleet.list", "arguments": {} });
        let request = rpc(
            "127.0.0.1:5",
            Some(&token),
            &[("mcp-name", "fleet.list")],
            "tools/call",
            params,
        );
        let (status, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result = &body["result"];
        assert_ne!(result["isError"], true, "{body}");
        let text = result["content"][0]["text"].as_str().unwrap();
        let answer: Value = serde_json::from_str(text).unwrap();
        let machines = answer["machines"].as_array().unwrap();
        assert_eq!(machines.len(), 1, "{answer}");
        assert_eq!(machines[0]["machineId"], state.config.server_id, "{answer}");
    }

    /// The listener's 128 KiB body limit holds on `/mcp` too: a 200 KiB call
    /// is refused with 413 before any handler runs, and the next one works.
    #[tokio::test]
    async fn an_oversized_body_is_413_and_a_normal_call_still_works() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let padding = "x".repeat(200 * 1024);
        let request = rpc(
            "127.0.0.1:5",
            Some(&token),
            &[],
            "tools/list",
            json!({ "padding": padding }),
        );
        let response = app(&state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let (status, body) = send(&state, post("127.0.0.1:5", Some(&token), &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn a_proxied_or_remote_request_is_refused_even_with_a_live_token() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let proxied = post(
            "127.0.0.1:5",
            Some(&token),
            &[("x-forwarded-for", "10.0.0.9")],
        );
        refused(
            &send(&state, proxied).await,
            StatusCode::FORBIDDEN,
            "mcp_loopback_only",
        );
        let remote = post("100.64.0.5:1", Some(&token), &[]);
        refused(
            &send(&state, remote).await,
            StatusCode::FORBIDDEN,
            "mcp_loopback_only",
        );
    }

    #[tokio::test]
    async fn a_missing_wrong_or_device_token_is_refused() {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "device-tok")]);
        refused(
            &send(&state, post("127.0.0.1:5", None, &[])).await,
            StatusCode::UNAUTHORIZED,
            "missing_authorization",
        );
        refused(
            &send(&state, post("127.0.0.1:5", Some("nope"), &[])).await,
            StatusCode::FORBIDDEN,
            "invalid_token",
        );
        refused(
            &send(&state, post("127.0.0.1:5", Some("device-tok"), &[])).await,
            StatusCode::FORBIDDEN,
            "invalid_token",
        );
    }

    #[tokio::test]
    async fn a_revoked_token_fails_on_the_next_call() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let (status, body) = send(&state, post("127.0.0.1:5", Some(&token), &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state.mcp_tokens.revoke_binding(&pane()).await.unwrap(), 1);
        refused(
            &send(&state, post("127.0.0.1:5", Some(&token), &[])).await,
            StatusCode::FORBIDDEN,
            "invalid_token",
        );
    }

    /// Expired is 401 (hosts re-run their helper on 401); revoked stays 403.
    #[tokio::test]
    async fn an_expired_token_is_401_token_expired() {
        let state = crate::test_state("admin", Vec::new());
        let token = state
            .mcp_tokens
            .issue(pane(), "test".into(), 0)
            .await
            .unwrap()
            .token;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        refused(
            &send(&state, post("127.0.0.1:5", Some(&token), &[])).await,
            StatusCode::UNAUTHORIZED,
            "token_expired",
        );
    }

    /// A gateway that listens on a tailnet address only still serves its own
    /// agents' `/mcp` -- on a loopback listener of its own; a loopback or
    /// wildcard gateway needs none. (The public address keeps refusing it:
    /// `a_proxied_or_remote_request_is_refused_even_with_a_live_token`.)
    #[tokio::test]
    async fn a_tailnet_only_gateway_serves_mcp_on_its_own_loopback() {
        let state = crate::test_state("admin", Vec::new());
        let token = issue(&state).await;
        let listeners = bind_loopback("100.64.0.7:0").await.unwrap().unwrap();
        let address = listeners[0].local_addr().unwrap();
        assert!(address.ip().is_loopback());
        for listener in listeners {
            let app = loopback_router(state.clone());
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
            });
        }
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        } });
        let answer = reqwest::Client::new()
            .post(format!("http://{address}{MCP_PATH}"))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/list")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 200, "{}", answer.text().await.unwrap());
        for served in ["127.0.0.1:0", "0.0.0.0:0", "[::]:0"] {
            assert!(bind_loopback(served).await.unwrap().is_none(), "{served}");
        }
    }

    #[test]
    fn a_non_loopback_listen_address_is_warned_about() {
        let warning = loopback_warning("10.0.0.9:23847").unwrap();
        assert_eq!(
            warning,
            "warning: /mcp only accepts loopback callers; this gateway listens on 10.0.0.9, \
             so MCP tools will be refused until it also listens on 127.0.0.1 or 0.0.0.0"
        );
        for quiet in [
            "127.0.0.1:23847",
            "0.0.0.0:23847",
            "[::]:1",
            "[::1]:1",
            "nonsense",
        ] {
            assert_eq!(loopback_warning(quiet), None, "{quiet}");
        }
    }

    #[test]
    fn the_local_url_follows_the_listen_address() {
        assert_eq!(local_url("0.0.0.0:9000"), "http://127.0.0.1:9000/mcp");
        assert_eq!(local_url("[::]:9000"), "http://127.0.0.1:9000/mcp");
        assert_eq!(local_url("127.0.0.1:23847"), "http://127.0.0.1:23847/mcp");
        assert_eq!(local_url("[::1]:23847"), "http://[::1]:23847/mcp");
        assert_eq!(local_url("100.64.0.7:23847"), "http://100.64.0.7:23847/mcp");
        let default = format!("http://127.0.0.1:{}/mcp", crate::DEFAULT_PORT);
        assert_eq!(local_url("nonsense"), default);
    }

    #[test]
    fn an_assistant_token_is_not_a_device_or_admin_credential() {
        let state = crate::test_state("admin", Vec::new());
        let headers = crate::bearer_headers("tok");
        assert!(crate::require_device(&state, &headers).is_err());
        assert!(crate::require_admin(&state.config, &headers).is_err());
    }
}
