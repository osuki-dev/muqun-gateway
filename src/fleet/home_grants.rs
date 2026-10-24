//! Explicit source-side authorization for read-only Home task summaries.
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
    Json, Router,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::connectivity::authority::DeviceKind;
use crate::platform::store::sqlite::Migration;
use crate::{api_error, require_device, still_paired, ApiResult, AppState};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "home_summary",
    steps: &["CREATE TABLE home_summary_grants (id TEXT PRIMARY KEY, owner TEXT NOT NULL, reader TEXT NOT NULL, audience TEXT NOT NULL, UNIQUE(owner,reader,audience));"],
};

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/home/grants", post(grant))
        .route("/api/home/grants/{id}", delete(revoke))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantBody {
    collector_server_id: String,
    audience: String,
}

fn failure() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::BAD_REQUEST,
        "home_grant_unavailable",
        "Link the collector before allowing summary access",
    )
}

async fn grant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<GrantBody>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    if body.collector_server_id.is_empty()
        || body.collector_server_id.len() > 128
        || body.collector_server_id.chars().any(char::is_control)
        || body.audience.is_empty()
        || body.audience.len() > 128
        || body.audience.chars().any(char::is_control)
    {
        return Err(failure());
    }
    // Only the paired App can approve the existing peer credential. Ambiguous
    // display identities never select an arbitrary login.
    let reader = {
        let devices = state.devices.lock().map_err(|_| failure())?;
        let matches: Vec<_> = devices
            .iter()
            .filter(|d| {
                d.kind == DeviceKind::Gateway
                    && d.server_id.as_deref() == Some(&body.collector_server_id)
            })
            .collect();
        if matches.len() != 1 {
            return Err(failure());
        }
        matches[0].id.clone()
    };
    let audience = body.audience;
    let id = state.db.call(move |db| {
        db.execute("INSERT OR IGNORE INTO home_summary_grants(id,owner,reader,audience) VALUES (?1,?2,?3,?4)", rusqlite::params![uuid::Uuid::new_v4().to_string(), owner, reader, audience])?;
        Ok(db.query_row("SELECT id FROM home_summary_grants WHERE owner=?1 AND reader=?2 AND audience=?3", rusqlite::params![owner, reader, audience], |row| row.get::<_, String>(0))?)
    }).await.map_err(|_| failure())?;
    Ok(Json(json!({"grant_id":id})))
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    state
        .db
        .call(move |db| {
            db.execute(
                "DELETE FROM home_summary_grants WHERE id=?1 AND owner=?2",
                rusqlite::params![id, owner],
            )?;
            Ok(())
        })
        .await
        .map_err(|_| failure())?;
    Ok(Json(json!({"revoked":true})))
}

pub(crate) async fn grant_owner(
    state: &AppState,
    reader: &str,
    id: &str,
    audience: &str,
) -> Option<String> {
    if !still_paired(state, reader) {
        return None;
    }
    let (reader, id, audience) = (reader.to_owned(), id.to_owned(), audience.to_owned());
    let owner = state.db.call(move |db| {
        Ok(db.query_row("SELECT owner FROM home_summary_grants WHERE id=?1 AND reader=?2 AND audience=?3", rusqlite::params![id,reader,audience], |row| row.get::<_, String>(0)).optional()?)
    }).await;
    owner
        .ok()
        .flatten()
        .filter(|owner| still_paired(state, owner))
}

pub(crate) async fn allowed(state: &AppState, reader: &str, id: &str, audience: &str) -> bool {
    grant_owner(state, reader, id, audience).await.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn an_unapproved_peer_cannot_read_summaries() {
        let state = test_state("admin", vec![]);
        assert!(!allowed(&state, "peer", "grant", "phone").await);
    }
}
