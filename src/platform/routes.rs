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
    let device_id = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-device-token").and_then(|h| h.to_str().ok()));
    let sealed = device_id
        .map(|id| device_seals_its_transport(&state, id))
        .unwrap_or(false);

    let discovery = super::discovery::build_discovery(&state, sealed).await;
    Ok(Json(discovery))
}

pub async fn api_discovery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device_id = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-device-token").and_then(|h| h.to_str().ok()));
    let sealed = device_id
        .map(|id| device_seals_its_transport(&state, id))
        .unwrap_or(false);

    let discovery = super::discovery::build_discovery(&state, sealed).await;
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
