//! Platform Plane HTTP Routes: documentation, capabilities, health, discovery, and server metadata.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::openapi::{openapi_spec, DOCS_HTML};
use crate::{
    api_error, device_seals_its_transport, gateway_metadata, require_device, update_server_label,
    ApiResult, AppState,
};

#[derive(Deserialize)]
pub struct SetLabelBody {
    pub label: String,
}

pub async fn docs() -> Html<&'static str> {
    Html(DOCS_HTML)
}

pub async fn openapi_json() -> Json<Value> {
    Json(openapi_spec())
}

pub async fn health(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let sealed = device_seals_its_transport(&state, &device_id);
    Ok(Json(gateway_metadata(&state, sealed).await?))
}

pub async fn api_meta(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let sealed = device_seals_its_transport(&state, &device_id);
    Ok(Json(gateway_metadata(&state, sealed).await?))
}

pub async fn api_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers).ok();
    let sealed = device_id
        .as_deref()
        .map(|id| device_seals_its_transport(&state, id))
        .unwrap_or(false);

    let discovery = super::discovery::build_discovery(&state, sealed, device_id.is_some()).await;
    Ok(Json(discovery))
}

pub async fn api_discovery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers).ok();
    let sealed = device_id
        .as_deref()
        .map(|id| device_seals_its_transport(&state, id))
        .unwrap_or(false);

    let discovery = super::discovery::build_discovery(&state, sealed, device_id.is_some()).await;
    Ok(Json(discovery))
}

pub async fn api_set_label(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetLabelBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let label = update_server_label(&body.label)
        .map_err(|err| api_error(StatusCode::BAD_REQUEST, "invalid_label", &err.to_string()))?;
    Ok(Json(json!({ "label": label })))
}

/// Mount all platform-plane routes onto the Axum router.
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi_json))
        .route("/health", get(health))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/discovery", get(api_discovery))
        .route("/api/meta", get(api_meta).patch(api_set_label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn openapi_spec_contains_docs_routes_and_auth() {
        let spec = openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        let output = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"];
        assert!(output.is_object());
        assert_eq!(output["parameters"][4]["name"], "start");
        assert_eq!(output["parameters"][5]["name"], "end");
        assert_eq!(
            output["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["result"]["properties"]["read"]["properties"]["generation"]["type"],
            "string"
        );
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/zoom"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/events"].is_object());
        assert!(spec["paths"]["/api/pair/request"].is_object());
        assert!(spec["paths"]["/api/pair/claim"].is_object());
        assert!(spec["paths"]["/api/meta"].is_object());
        assert!(spec["paths"]["/api/devices/push-token"]["delete"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/workspaces/{workspaceId}"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/agents/{target}/send"].is_object());
        // The cursor and the geometry have to be in the spec, and so does
        // which backend leaves which of them null: a client that assumes
        // `width` is always there measures a herdr pane wrong.
        for route in [
            "/api/sessions/{sessionId}/panes",
            "/api/sessions/{sessionId}/panes/{paneId}",
        ] {
            let pane = &spec["paths"][route]["get"];
            let described = pane["description"].as_str().unwrap_or_default();
            assert!(described.contains("cursor_x"), "{route} omits the cursor");
            assert!(
                described.contains("herdr"),
                "{route} does not say which backend leaves these null"
            );
            let item = &pane["responses"]["200"]["content"]["application/json"]["schema"]
                ["properties"]["result"]["properties"]["panes"]["items"]["properties"];
            for field in ["width", "height", "cursor_x", "cursor_y"] {
                assert_eq!(
                    item[field]["type"],
                    json!(["integer", "null"]),
                    "{route}.{field} must be documented as nullable"
                );
            }
            assert_eq!(
                item["scroll"]["properties"]["alternate_on"]["type"],
                json!(["boolean", "null"])
            );
        }

        // The mode has to be in the spec or a client has no way to learn the
        // field exists, and no way to know that leaving it out is a paste.
        let send_text =
            &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/send-text"]["post"];
        let mode = &send_text["requestBody"]["content"]["application/json"]["schema"]["properties"]
            ["mode"];
        assert_eq!(mode["enum"], json!(["paste", "keys"]));
        assert_eq!(mode["default"], "paste");
        // Required stays exactly `text`: adding the field must not make every
        // existing client's body invalid.
        assert_eq!(
            send_text["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["text"])
        );
        assert!(
            spec["paths"]["/api/uploads"]["post"]["requestBody"]["content"]["multipart/form-data"]
                .is_object()
        );
        assert!(spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"].is_object());
        let parts = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/parts"]["get"];
        assert!(parts.is_object());
        assert_eq!(parts["parameters"][2]["name"], "lines");
        let part = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["parts"]["items"];
        // A client dispatches on `type` and falls back on `fallback_text`, so
        // the spec has to require exactly those two of every part.
        assert_eq!(part["required"], json!(["type", "fallback_text"]));
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("tool-block")));
        // v2's one addition to the closed set. It has to be in the spec's enum
        // or a client has no way to learn the type exists without meeting one.
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("approval")));
        assert!(part["properties"]["approval_id"].is_object());
        assert_eq!(
            part["properties"]["options"]["items"]["required"],
            json!(["index", "label", "decision"])
        );
        // Which source answered is a value of an enum that already had two.
        let pane = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"];
        assert_eq!(
            pane["parts"]["enum"],
            json!(["native", "dictionary", "text"])
        );
        assert!(pane["native"].is_object());
        assert_eq!(
            parts["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["data"]["properties"]["source"]["enum"],
            json!(["recent-unwrapped", "native"])
        );
        // The composer descriptor rides on the pane, and its source vocabulary
        // is closed the same way the part types are.
        let composer = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"]["composer"];
        assert_eq!(
            composer["properties"]["slash_commands"]["items"]["properties"]["source"]["enum"],
            json!(["builtin", "workspace"])
        );
        let files = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/files"]["get"];
        assert!(files.is_object());
        assert_eq!(files["parameters"][2]["name"], "query");
        assert_eq!(files["parameters"][3]["name"], "limit");
        assert_eq!(
            files["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["schema_version"]["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"]["responses"]
                ["200"]["content"]["application/json"]["schema"]["properties"]["schema_version"]
                ["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["415"].is_object()
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["413"].is_object()
        );
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["413"].is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["415"].is_object());
        // The upload answers with both ways of reaching the file, and both are
        // required: a client that only got `path` could not draw the
        // attachment it just sent.
        let stored = &spec["paths"]["/api/uploads"]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            stored["required"],
            json!(["path", "url", "name", "size", "mime"])
        );
        assert!(stored["properties"]["url"].is_object());
        let upload_read = &spec["paths"]["/api/uploads/{fileName}"]["get"];
        assert!(upload_read.is_object());
        assert_eq!(upload_read["parameters"][0]["name"], "fileName");
        assert!(upload_read["responses"]["404"].is_object());
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"]["parameters"]
                [6]["name"],
            "format"
        );

        // Task dispatch, including the partial answer, which a client that only
        // handles 200 and "error" would silently mishandle.
        let tasks = &spec["paths"]["/api/sessions/{sessionId}/tasks"]["post"];
        assert!(tasks.is_object());
        assert!(tasks["responses"]["207"].is_object());
        assert!(tasks["responses"]["403"].is_object());
        assert_eq!(
            tasks["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["repo_path", "agent"])
        );
        assert!(spec["paths"]["/api/agents/catalog"]["get"].is_object());
        let cap = &spec["paths"]["/api/capabilities"]["get"];
        assert!(cap.is_object());
        assert_eq!(
            cap["responses"]["200"]["content"]["application/json"]["schema"]["required"],
            json!(["serverVersion", "protocolVersion", "planes", "capabilities"])
        );
        assert!(spec["paths"]["/api/agent-status"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-catalog"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-sessions"]["get"].is_object());
        assert!(spec["paths"]["/api/agent-sessions"]["post"].is_object());
        assert!(spec["paths"]["/api/agent-sessions/{asid}/prompt"]["post"].is_object());
        assert!(spec["paths"]["/api/agent-sessions/{asid}/events"]["get"].is_object());
    }

    #[test]
    fn the_api_version_and_capabilities_announce_task_dispatch() {
        // A minor bump: the routes are additive, so an older client keeps
        // working, and a newer one can gate on the capability rather than on
        // probing for a 404.
        assert!(GATEWAY_API_VERSION.starts_with("1.9."));
        assert!(API_CAPABILITIES.contains(&"ws_events"));
        assert_eq!(GATEWAY_API_MAJOR, 1);
        assert!(API_CAPABILITIES.contains(&"tasks"));
        assert!(API_CAPABILITIES.contains(&"agent_catalog"));
        assert!(API_CAPABILITIES.contains(&"terminal_backends"));
        assert!(API_CAPABILITIES.contains(&"multiple_terminal_backends"));
        assert!(API_CAPABILITIES.contains(&"capabilities_discovery"));
        assert!(API_CAPABILITIES.contains(&"agent_discovery"));
        assert!(API_CAPABILITIES.contains(&"multi_agent"));
    }

    #[tokio::test]
    async fn capabilities_discovery_endpoint_and_health_expose_dual_planes() {
        use tower::ServiceExt;
        let mut state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        let app = Router::new()
            .route("/health", axum::routing::get(health))
            .route("/api/capabilities", axum::routing::get(api_capabilities))
            .with_state(state);

        // 1. GET /api/capabilities
        let req = Request::builder()
            .uri("/api/capabilities")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["planes"]["terminal"].is_object());
        assert!(json["planes"]["agents"].is_object());
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("capabilities_discovery")));
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("agent_discovery")));
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("multi_agent")));

        // 2. GET /health contains planes
        let req = Request::builder()
            .uri("/health")
            .method("GET")
            .header("authorization", "Bearer device-token")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["planes"]["terminal"].is_object());
        assert!(json["planes"]["agents"].is_object());
        assert!(json["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("multi_agent")));
    }

    #[tokio::test]
    async fn headless_gateway_exposes_capabilities_without_crashing() {
        use tower::ServiceExt;
        let mut state = test_state("admin", vec![test_device("phone-1", "device-token")]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        state.config.sessions.clear();

        let app = Router::new()
            .route("/health", axum::routing::get(health))
            .route("/api/capabilities", axum::routing::get(api_capabilities))
            .with_state(state);

        // 1. GET /api/capabilities
        let req = Request::builder()
            .uri("/api/capabilities")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["planes"]["terminal"]["supported"], false);
        assert_eq!(
            json["planes"]["terminal"]["degradedReason"],
            "no_terminal_backend_configured"
        );

        // 2. GET /health
        let req = Request::builder()
            .uri("/health")
            .method("GET")
            .header("authorization", "Bearer device-token")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["planes"]["terminal"]["supported"], false);
        assert_eq!(json["backend"], Value::Null);
    }

    /// The snapshot is one call where the app used to make four, and its
    /// `agents` array is now the real agent list -- so a client can prewarm a
    /// session from it and drop the separate `/agents` call. It is announced
    /// because the alternative is the app probing for a 404 and then guessing
    /// whether the `agents` it got back carry `instance_id` and `target`.
    #[test]
    fn the_session_snapshot_is_announced_as_a_capability() {
        assert!(API_CAPABILITIES.contains(&"session_snapshot"));
        assert!(
            gateway_capabilities(false).contains(&"session_snapshot"),
            "it is a property of this build, not of a session's backend"
        );
        assert!(gateway_capabilities(true).contains(&"session_snapshot"));
    }

    #[test]
    fn the_pane_context_and_git_routes_are_documented_and_announced() {
        let spec = openapi_spec();
        for path in [
            "/api/sessions/{sessionId}/panes/{paneId}/context",
            "/api/sessions/{sessionId}/panes/{paneId}/git/status",
            "/api/sessions/{sessionId}/panes/{paneId}/git/diff",
            "/api/sessions/{sessionId}/panes/{paneId}/vcs/files",
            "/api/sessions/{sessionId}/panes/{paneId}/vcs/file",
        ] {
            assert!(
                spec["paths"][path]["get"].is_object(),
                "{path} is not documented"
            );
        }
        let names: Vec<&str> = spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/git/diff"]
            ["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "sessionId",
                "paneId",
                "path",
                "old_path",
                "staged",
                "context",
                "from",
                "lines"
            ]
        );
        assert!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/vcs/discard"]["post"]
                .is_object()
        );
        for capability in ["pane_context", "git_diff", "pane_vcs_files"] {
            assert!(
                API_CAPABILITIES.contains(&capability),
                "{capability} is not announced"
            );
        }
        assert!(CONTENT_SCHEMA_VERSION.starts_with("1.5."));
    }

    #[test]
    fn the_digest_endpoint_is_documented_and_announced() {
        let spec = openapi_spec();
        let events = &spec["paths"]["/api/sessions/{sessionId}/agent-events"]["get"];
        assert!(events.is_object());
        let names: Vec<&str> = events["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sessionId", "since"]);
        // A client must be able to tell a gateway that keeps this from one old
        // enough to answer 404, without probing for the 404.
        assert!(API_CAPABILITIES.contains(&"agent_events"));
    }

    #[test]
    fn dispatch_and_stop_are_documented_and_announced() {
        let spec = openapi_spec();
        let spawn = &spec["paths"]["/api/sessions/{sessionId}/spawn"]["post"];
        assert!(spawn.is_object());
        assert_eq!(
            spawn["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["agent"])
        );
        assert!(spawn["responses"]["207"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/recent-cwds"]["get"].is_object());
        assert!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/interrupt"]["post"].is_object()
        );

        for capability in ["agent_spawn", "recent_cwds", "pane_interrupt"] {
            assert!(
                API_CAPABILITIES.contains(&capability),
                "{capability} is not announced"
            );
        }
    }

    #[test]
    fn approvals_are_announced_as_a_capability_and_documented() {
        // Additive: the routes and events are new, so a client gates on the
        // capability rather than probing for a 404.
        assert!(API_CAPABILITIES.contains(&"pane_approvals"));
        let spec = openapi_spec();
        let approval = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/approval"];
        assert!(approval["get"].is_object());
        assert!(approval["post"]["requestBody"].is_object());
        assert!(approval["post"]["responses"]["409"].is_object());
    }
}
