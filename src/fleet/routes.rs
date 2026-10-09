//! `GET /api/fleet/self`: this machine's manifest, for a paired device.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};

use super::manifest::Manifest;
use crate::{require_device, ApiResult, AppState};

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/fleet/self", get(fleet_self))
}

async fn fleet_self(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Manifest>> {
    require_device(&state, &headers)?;
    Ok(Json(state.fleet.manifest(&state).await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bearer_headers;
    use crate::test_support::{output_state, FakeHerdr};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt as _;

    async fn get_self(state: &AppState, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().uri("/api/fleet/self");
        if let Some(token) = bearer {
            for (name, value) in bearer_headers(token) {
                request = request.header(name.unwrap(), value);
            }
        }
        let response = mount(Router::new())
            .with_state(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
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

    #[tokio::test]
    async fn a_device_reads_the_manifest_and_a_stranger_gets_401() {
        let herdr = FakeHerdr::start(vec!["x"], None);
        let state = output_state(&herdr);
        let (status, body) = get_self(&state, Some("token")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["machineId"], state.config.server_id.as_str());
        assert_eq!(body["terminal"][0]["connected"], true);
        assert_eq!(body["os"], std::env::consts::OS);
        assert_eq!(body["tasks"]["active"], 0);

        let (status, _) = get_self(&state, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
