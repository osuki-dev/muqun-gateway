//! Paired coordinator activation and bounded local delegation request shapes.
use crate::work::model::*;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateChildRequest {
    pub request_key: String,
    pub expected_parent_revision: u64,
    pub title: String,
    pub brief: String,
    pub policy: TaskPolicy,
    #[serde(default)]
    pub dependencies: Vec<TaskDependency>,
    #[serde(default)]
    pub input_refs: Vec<InputRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigureRequest {
    pub request_key: String,
    pub expected_revision: u64,
    pub input: DelegationConfig,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChildReadRequest {
    pub task_id: String,
    pub kind: Option<String>,
    pub snapshot_revision: Option<u64>,
    pub after_id: Option<String>,
    pub record_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DependencyUpdateRequest {
    pub request_key: String,
    pub expected_revision: u64,
    pub dependencies: Vec<TaskDependency>,
}

pub(super) async fn set_dependencies(
    State(state): State<crate::AppState>,
    Path((session, task)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<DependencyUpdateRequest>,
) -> crate::ApiResult<Json<Mutation<Task>>> {
    use crate::work_http::work_error;
    let actor = crate::require_device(&state, &headers)?;
    crate::find_session(&state.config, &session)?;
    let authority =
        crate::work_actor::WorkActor::paired(state.clone(), headers, &actor).map_err(work_error)?;
    let store = state
        .work
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    authority
        .commit(store, move |s| {
            s.set_dependencies(
                &actor,
                &session,
                &task,
                &request.request_key,
                request.expected_revision,
                request.dependencies,
                None,
                crate::now_unix_ms().min(i64::MAX as u128) as i64,
            )
        })
        .await
        .map(Json)
        .map_err(work_error)
}

#[cfg(test)]
mod dependency_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn paired_dependency_update_is_strict_replayable_and_does_not_dispatch() {
        let mut state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        state.config.sessions[0].socket_path =
            format!("/tmp/missing-dependency-{}.sock", uuid::Uuid::new_v4());
        let session = state.config.sessions[0].id.clone();
        let task = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create_task(
                "phone",
                &session,
                "task",
                CreateTask {
                    repo_path: "/repo".into(),
                    title: "Task".into(),
                    brief: "Brief".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 0,
                    },
                },
                1,
            )
            .unwrap()
            .value;
        let body = json!({"request_key":"pin","expected_revision":task.revision,"dependencies":[]});
        let mut unexpected = body.clone();
        unexpected["actor"] = json!("forged");
        assert!(serde_json::from_value::<DependencyUpdateRequest>(unexpected).is_err());
        for token in ["admin", "unknown", "device", "device"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            let result = set_dependencies(
                State(state.clone()),
                Path((session.clone(), task.id.clone())),
                headers,
                Json(serde_json::from_value(body.clone()).unwrap()),
            )
            .await;
            if token == "device" {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err());
            }
        }
        let detail = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail(&session, &task.id)
            .unwrap();
        assert!(detail.attempts.is_empty());
        assert!(detail.operations.is_empty());
        assert_eq!(detail.task.revision, task.revision + 1);
        assert!(!std::path::Path::new(&state.config.sessions[0].socket_path).exists());
    }
}

pub(super) async fn configure(
    State(state): State<crate::AppState>,
    Path((session_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<ConfigureRequest>,
) -> crate::ApiResult<Json<Mutation<Task>>> {
    use crate::work_http::{with_store, work_error};
    let actor = crate::require_device(&state, &headers)?;
    let config = crate::find_session(&state.config, &session_id)?;
    let sid = session_id.clone();
    let tid = task_id.clone();
    let detail = with_store(&state, move |store| store.detail(&sid, &tid)).await?;
    let observed = if request.input.policy.enabled {
        let attempt = detail
            .attempts
            .iter()
            .find(|attempt| Some(&attempt.id) == request.input.coordinator_attempt_id.as_ref())
            .ok_or_else(|| work_error(WorkError(FailureCode::ScopeMismatch)))?;
        if attempt.role != AttemptRole::Lead
            || attempt.lifecycle.launch_phase != LaunchPhase::LaunchConfirmed
            || attempt.lifecycle.reservation != Reservation::Reserved
        {
            return Err(work_error(WorkError(FailureCode::NotReady)));
        }
        let instance = attempt
            .binding
            .instance_id
            .clone()
            .ok_or_else(|| work_error(WorkError(FailureCode::NotReady)))?;
        let epoch = attempt
            .lifecycle
            .native_owner_epoch
            .clone()
            .ok_or_else(|| work_error(WorkError(FailureCode::NotReady)))?;
        if crate::terminal_backend(config)
            .lifecycle_bound(&instance, &epoch)
            .await
            .map_err(|_| work_error(WorkError(FailureCode::NotReady)))?
            != crate::backend::BoundLifecycle::Live
        {
            return Err(work_error(WorkError(FailureCode::NotReady)));
        }
        Some((attempt.id.clone(), instance, epoch))
    } else {
        None
    };
    tokio::task::spawn_blocking(move || {
        let token = crate::bearer_token(&headers)?;
        let devices = crate::lock_devices(&state)?;
        if crate::identify_device(&devices, token).as_deref() != Some(actor.as_str()) {
            return Err(work_error(WorkError(FailureCode::ScopeMismatch)));
        }
        if let Some(key) = devices
            .iter()
            .find(|device| device.id == actor)
            .and_then(|device| device.transport_key.as_deref())
        {
            let proof = headers
                .get(crate::TRANSPORT_PROOF_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !crate::authority::authenticates_admin(&crate::hash_token(key), proof) {
                return Err(work_error(WorkError(FailureCode::ScopeMismatch)));
            }
        }
        let local = state
            .work_local
            .as_ref()
            .ok_or_else(|| work_error(WorkError(FailureCode::CapabilityUnavailable)))?;
        let mut registry = local
            .registry
            .lock()
            .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?;
        let mut store = state
            .work
            .as_ref()
            .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?
            .lock()
            .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?;
        let permit = match &observed {
            Some((attempt_id, instance, epoch)) => {
                let current = store.detail(&session_id, &task_id).map_err(work_error)?;
                let attempt = current
                    .attempts
                    .iter()
                    .find(|attempt| &attempt.id == attempt_id)
                    .ok_or_else(|| work_error(WorkError(FailureCode::ScopeMismatch)))?;
                if attempt.binding.instance_id.as_ref() != Some(instance)
                    || attempt.lifecycle.native_owner_epoch.as_ref() != Some(epoch)
                {
                    return Err(work_error(WorkError(FailureCode::InstanceChanged)));
                }
                Some(
                    registry
                        .prepare_activation(
                            &crate::work_authority::Scope {
                                session_id: session_id.clone(),
                                task_id: task_id.clone(),
                                attempt_id: attempt_id.clone(),
                            },
                            delegation_now(),
                        )
                        .map_err(work_error)?,
                )
            }
            None => None,
        };
        let result = store
            .configure_delegation(
                &actor,
                &session_id,
                &task_id,
                &request.request_key,
                request.expected_revision,
                request.input,
                delegation_now(),
            )
            .map_err(work_error)?;
        // A replay is a receipt read. It never restores or upgrades runtime authority.
        if !result.replayed {
            registry.clear_delegation(&session_id, &task_id);
            if let (Some(permit), Some((attempt, instance, epoch))) = (permit, observed) {
                registry.apply_activation(
                    permit,
                    DelegationFence {
                        coordinator_task_id: task_id,
                        coordinator_attempt_id: attempt,
                        coordinator_epoch: result.value.delegation.coordinator_epoch,
                        instance_id: instance,
                        native_owner_epoch: epoch,
                    },
                );
            }
        }
        Ok(Json(result))
    })
    .await
    .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
}
fn delegation_now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn paired_disable_replays_without_reactivation_and_rejects_admin() {
        use std::sync::{Arc, Mutex};
        let mut state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        state.config.sessions[0].id = "session".into();
        let dir = std::env::temp_dir().join(format!("delegation-config-{}", uuid::Uuid::new_v4()));
        let (local, _listener) = crate::work_local::prepare(&dir).unwrap();
        state.work_local = Some(local);
        let mut store = crate::work::store::WorkStore::in_memory().unwrap();
        let task = store
            .create_task(
                "phone",
                "session",
                "task",
                CreateTask {
                    repo_path: "/tmp/project".into(),
                    title: "Task".into(),
                    brief: "Brief".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 1,
                    },
                },
                delegation_now(),
            )
            .unwrap()
            .value;
        state.work = Some(Arc::new(Mutex::new(store)));
        let request = || ConfigureRequest {
            request_key: "disable".into(),
            expected_revision: task.revision,
            input: DelegationConfig {
                policy: DelegationPolicy::default(),
                coordinator_attempt_id: None,
            },
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer admin".parse().unwrap());
        assert!(configure(
            State(state.clone()),
            Path(("session".into(), task.id.clone())),
            headers.clone(),
            Json(request())
        )
        .await
        .is_err());
        headers.insert("authorization", "Bearer device".parse().unwrap());
        let first = configure(
            State(state.clone()),
            Path(("session".into(), task.id.clone())),
            headers.clone(),
            Json(request()),
        )
        .await
        .unwrap()
        .0;
        assert!(!first.replayed);
        let replay = configure(
            State(state.clone()),
            Path(("session".into(), task.id.clone())),
            headers.clone(),
            Json(request()),
        )
        .await
        .unwrap()
        .0;
        assert!(replay.replayed);
        assert_eq!(
            replay.value.delegation.coordinator_epoch,
            first.value.delegation.coordinator_epoch
        );
        // Current transport proof is required even for a receipt replay.
        state.devices.lock().unwrap()[0].transport_key = Some("rotated-key".into());
        assert!(configure(
            State(state),
            Path(("session".into(), task.id)),
            headers,
            Json(request())
        )
        .await
        .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
