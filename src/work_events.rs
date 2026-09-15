//! Durable task notifications. Native backend availability does not own this stream.
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{sse::KeepAlive, IntoResponse, Response, Sse};
use axum::Extension;
use serde::Deserialize;
use std::time::Duration;

use crate::work::model::{FailureCode, WorkError};
use crate::work_http::{with_store, work_error};
use crate::{
    find_session, require_device, stream_event, ApiResult, AppState, EncryptedStreamContext,
    EventStreamSealer, GatewayEventStream,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventsQuery {
    pub after_cursor: u64,
}

pub(super) async fn events(
    State(state): State<AppState>,
    Path(session): Path<String>,
    Query(query): Query<EventsQuery>,
    crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    if state.work.is_none() {
        return Err(work_error(WorkError(FailureCode::StorageUnavailable)));
    }
    let mut sealer = crypto
        .map(|Extension(context)| EventStreamSealer::new(&context))
        .transpose()
        .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let stream: GatewayEventStream = Box::pin(async_stream::stream! {
        let mut cursor = query.after_cursor;
        loop {
            // Revocation applies to an already-open connection before its next page.
            if require_device(&state, &headers).is_err() {
                break;
            }
            let scope = session.clone();
            let page = with_store(&state, move |store| store.changes(&scope, cursor, 100)).await;
            let page = match page {
                Ok(page) => page,
                Err(_) => {
                    if let Some(event) = stream_event(&mut sealer, "work.unavailable", "{}") {
                        yield Ok(event);
                    }
                    break;
                }
            };
            let reset = page.reset_required;
            let full = page.changes.len() == 100;
            cursor = page.cursor;
            let Ok(payload) = serde_json::to_string(&page) else { break };
            let name = if reset { "work.reset" } else { "work.changes" };
            let Some(event) = stream_event(&mut sealer, name, &payload) else { break };
            yield Ok(event);
            // A reset asks the reader to obtain a fresh snapshot. Continuing here
            // would silently skip the expired interval before that snapshot exists.
            if reset { break; }
            if !full {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::model::{CreateTask, TaskPolicy};
    use futures::StreamExt;

    fn fixture() -> (AppState, String, HeaderMap) {
        let state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        let session = state.config.sessions[0].id.clone();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer device".parse().unwrap());
        (state, session, headers)
    }

    #[tokio::test]
    async fn stream_replays_only_scoped_changes_and_closes_on_reset() {
        let (state, session, headers) = fixture();
        let input = || CreateTask {
            repo_path: "/repo".into(),
            title: "Task".into(),
            brief: "Work".into(),
            parent_task_id: None,
            policy: TaskPolicy {
                allowed_agents: vec!["codex".into()],
                max_workers: 0,
            },
        };
        {
            let mut store = state.work.as_ref().unwrap().lock().unwrap();
            store
                .create_task("phone", "another-session", "foreign", input(), 1)
                .unwrap();
            store
                .create_task("phone", &session, "own", input(), 2)
                .unwrap();
        }
        let response = events(
            State(state.clone()),
            Path(session.clone()),
            Query(EventsQuery { after_cursor: 0 }),
            None,
            headers.clone(),
        )
        .await
        .unwrap();
        let mut body = response.into_body().into_data_stream();
        let frame = body.next().await.unwrap().unwrap();
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        assert!(frame.contains("event: work.changes"));
        let payload: serde_json::Value = serde_json::from_str(
            frame
                .lines()
                .find_map(|s| s.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["changes"].as_array().unwrap().len(), 1);
        assert_eq!(payload["cursor"], 2);
        drop(body);
        let response = events(
            State(state),
            Path(session),
            Query(EventsQuery { after_cursor: 999 }),
            None,
            headers,
        )
        .await
        .unwrap();
        let mut body = response.into_body().into_data_stream();
        let frame = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8(frame.to_vec())
            .unwrap()
            .contains("event: work.reset"));
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn administrative_token_does_not_grant_task_stream_access() {
        let (state, session, mut headers) = fixture();
        headers.insert("authorization", "Bearer admin".parse().unwrap());
        assert!(events(
            State(state),
            Path(session),
            Query(EventsQuery { after_cursor: 0 }),
            None,
            headers
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn revocation_before_first_page_closes_without_task_data() {
        let (state, session, headers) = fixture();
        let response = events(
            State(state.clone()),
            Path(session),
            Query(EventsQuery { after_cursor: 0 }),
            None,
            headers,
        )
        .await
        .unwrap();
        state.devices.lock().unwrap().clear();
        assert!(response
            .into_body()
            .into_data_stream()
            .next()
            .await
            .is_none());
    }

    #[tokio::test]
    async fn encrypted_stream_seals_task_event_names_and_payloads() {
        let (state, session, headers) = fixture();
        let context = EncryptedStreamContext {
            material: vec![42; 32],
            request_nonce: "nonce".into(),
            request_aad: "request".into(),
        };
        let response = events(
            State(state),
            Path(session),
            Query(EventsQuery { after_cursor: 999 }),
            Some(Extension(context.clone())),
            headers,
        )
        .await
        .unwrap();
        let frame = response
            .into_body()
            .into_data_stream()
            .next()
            .await
            .unwrap()
            .unwrap();
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        assert!(frame.contains("event: muqun.encrypted"));
        assert!(!frame.contains("work.reset"));
        assert!(!frame.contains("reset_required"));
        let record: serde_json::Value = serde_json::from_str(
            frame
                .lines()
                .find_map(|s| s.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        let sid = record["sid"].as_str().unwrap();
        let seq = record["seq"].as_u64().unwrap();
        let key =
            crate::transport::derive_stream_key(&context.material, sid, &context.request_nonce)
                .unwrap();
        let aad = format!("{}\n{}\n{}", context.request_aad, sid, seq);
        let plain = crate::transport::open_stream_event(
            &key,
            seq,
            aad.as_bytes(),
            record["ciphertext"].as_str().unwrap(),
        )
        .unwrap();
        let event: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(event["event"], "work.reset");
        let payload: serde_json::Value =
            serde_json::from_str(event["data"].as_str().unwrap()).unwrap();
        assert_eq!(payload["reset_required"], true);
    }
}
