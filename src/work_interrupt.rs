//! Paired human interruption: durable intent, one native effect, retained evidence.
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::backend::{BackendError, BoundInterrupt, BoundInterruptReceipt};
use crate::work::model::*;
use crate::work::store::WorkStore;
use crate::work_actor::WorkActor;
use crate::work_http::work_error;
use crate::{ApiResult, AppState};

pub(super) async fn interrupt_attempt(
    State(state): State<AppState>,
    Path((session, task, attempt)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(input): Json<InterruptInput>,
) -> ApiResult<Json<Mutation<Operation>>> {
    let actor = crate::require_device(&state, &headers)?;
    crate::find_session(&state.config, &session)?;
    let authority = WorkActor::paired(state.clone(), headers, &actor).map_err(work_error)?;
    // Dropping the HTTP response future must not abandon a received native fact.
    tokio::spawn(run(state, authority, actor, session, task, attempt, input))
        .await
        .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
        .map(Json)
        .map_err(work_error)
}

async fn run(
    state: AppState,
    authority: WorkActor,
    actor: String,
    session: String,
    task: String,
    attempt: String,
    input: InterruptInput,
) -> WorkResult<Mutation<Operation>> {
    let store = state
        .work
        .clone()
        .ok_or(WorkError(FailureCode::StorageUnavailable))?;
    let (who, sid, tid, aid, body) = (
        actor.clone(),
        session.clone(),
        task.clone(),
        attempt.clone(),
        input.clone(),
    );
    if let Some(replay) = authority
        .commit(store.clone(), move |s| {
            s.replay_interrupt(&who, &sid, &tid, &aid, &body)
        })
        .await?
    {
        return Ok(replay);
    }
    let config = crate::find_session(&state.config, &session)
        .map_err(|_| WorkError(FailureCode::ScopeMismatch))?;
    let backend = crate::terminal_backend(config);
    let metadata = backend
        .metadata()
        .await
        .map_err(|_| WorkError(FailureCode::CapabilityUnavailable))?;
    if !metadata.capabilities.contains(&"instance_bound_interrupt") {
        return Err(WorkError(FailureCode::CapabilityUnavailable));
    }
    let sid = session.clone();
    let prepared = authority
        .commit(store.clone(), move |s| {
            let mut mutation = s.prepare_interrupt(&actor, &sid, &task, &attempt, input, now())?;
            if !mutation.replayed {
                // Both transactions share the paired-device and store guards: no
                // native I/O or revocation gap between prepare and dispatch claim.
                mutation.value = match s.begin_operation(&sid, &mutation.value.id, now()) {
                    Ok(op) => op,
                    Err(error) => {
                        s.refuse_prepared_operation(
                            &sid,
                            &mutation.value.id,
                            error.0.clone(),
                            now(),
                        )?;
                        return Err(error);
                    }
                };
            }
            Ok(mutation)
        })
        .await?;
    if prepared.replayed {
        return Ok(prepared);
    }
    let op = prepared.value;
    let request = BoundInterrupt {
        operation_id: op.id.clone(),
        expected_launch_id: op
            .resources
            .instance_id
            .clone()
            .ok_or(WorkError(FailureCode::InstanceChanged))?,
        expected_owner_epoch: op
            .interruption_owner_epoch
            .clone()
            .ok_or(WorkError(FailureCode::InstanceChanged))?,
    };
    let result = backend.interrupt_bound_agent(&request).await;
    let outcome = outcome(result);
    // Authority/lifecycle may change during I/O. Recording the observed fact is
    // not a new user action and must not depend on still-valid paired authority.
    let final_op = db(store, move |s| {
        s.finalize_interrupt(&session, &op.id, outcome, now())
    })
    .await?;
    Ok(Mutation {
        value: final_op,
        replayed: false,
    })
}

fn outcome(result: Result<BoundInterruptReceipt, BackendError>) -> InterruptionOutcome {
    match result {
        Ok(receipt) => InterruptionOutcome::Acknowledged(InterruptionReceipt {
            operation_id: receipt.operation_id,
            launch_id: receipt.launch_id,
            owner_epoch: receipt.owner_epoch,
            receipt_id: receipt.receipt_id,
            key: receipt.key,
            bytes_written: receipt.bytes_written,
            input_disposition: receipt.input_disposition,
        }),
        Err(BackendError::Unsupported(_)) => {
            InterruptionOutcome::Refused(FailureCode::CapabilityUnavailable)
        }
        Err(BackendError::Refused {
            code: Some(code), ..
        }) => match code.as_str() {
            "resource_limit" => InterruptionOutcome::Refused(FailureCode::ResourceLimit),
            "instance_changed" => InterruptionOutcome::Refused(FailureCode::InstanceChanged),
            "unsupported" => InterruptionOutcome::Refused(FailureCode::CapabilityUnavailable),
            _ => InterruptionOutcome::Unconfirmed,
        },
        Err(_) => InterruptionOutcome::Unconfirmed,
    }
}

async fn db<T: Send + 'static>(
    store: Arc<Mutex<WorkStore>>,
    f: impl FnOnce(&mut WorkStore) -> WorkResult<T> + Send + 'static,
) -> WorkResult<T> {
    tokio::task::spawn_blocking(move || {
        let mut store = store
            .lock()
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        f(&mut store)
    })
    .await
    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
}

fn now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    fn fixture() -> (AppState, String, String, InterruptInput) {
        let mut state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        state.config.sessions[0].socket_path =
            format!("/tmp/interrupt-service-{}.sock", uuid::Uuid::new_v4());
        let session = state.config.sessions[0].id.clone();
        let mut store = state.work.as_ref().unwrap().lock().unwrap();
        let task = store
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
        let start = store
            .prepare_attempt(
                "phone",
                &session,
                &task.id,
                "start",
                task.revision,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "digest",
                2,
            )
            .unwrap()
            .value;
        store.begin_operation(&session, &start.id, 3).unwrap();
        store.claim_start_dispatch(&session, &start.id, 4).unwrap();
        let binding = NativeBinding {
            instance_id: Some("launch".into()),
            ..Default::default()
        };
        store
            .confirm_start_launch(&session, &start.id, binding.clone(), "owner", 5)
            .unwrap();
        store
            .finalize_operation(
                &session,
                &start.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: binding,
                    failure_code: None,
                },
                6,
            )
            .unwrap();
        let revision = store.detail(&session, &task.id).unwrap().task.revision;
        drop(store);
        (
            state,
            task.id,
            start.attempt_id.unwrap(),
            InterruptInput {
                request_key: "interrupt".into(),
                expected_revision: revision,
                expected_instance_id: "launch".into(),
                expected_native_owner_epoch: "owner".into(),
            },
        )
    }

    struct Native {
        calls: Arc<Mutex<Vec<String>>>,
        path: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Native {
        fn start(
            state: &AppState,
            revoke_at: Option<&'static str>,
            failure: Option<&'static str>,
        ) -> Self {
            let path = state.config.sessions[0].socket_path.clone();
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let record = calls.clone();
            let devices = state.devices.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap();
                    record.lock().unwrap().push(method.into());
                    if revoke_at == Some(method) {
                        devices.lock().unwrap().clear();
                    }
                    let response = if method == "ping" {
                        json!({"result":{"capabilities":{"agent_interrupt_bound":true,"agent_lifecycle_bound":true,"owner_epoch":"owner"}}})
                    } else {
                        assert_eq!(method, "agent.interrupt_bound", "no pane/key fallback");
                        if let Some(code) = failure {
                            json!({"error":{"code":code,"message":"test outcome"}})
                        } else {
                            json!({"result":{"type":"agent_interrupted_bound","operation_id":req["params"]["operation_id"],"launch_id":"launch","owner_epoch":"owner","receipt_id":"receipt","key":"Escape","bytes_written":1,"input_disposition":"written"}})
                        }
                    };
                    let mut stream = reader.into_inner();
                    stream
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
            Self { calls, path, task }
        }
    }
    impl Drop for Native {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }
    async fn call(
        state: &AppState,
        task: &str,
        attempt: &str,
        input: InterruptInput,
        token: &str,
    ) -> ApiResult<Json<Mutation<Operation>>> {
        interrupt_attempt(
            State(state.clone()),
            Path((
                state.config.sessions[0].id.clone(),
                task.into(),
                attempt.into(),
            )),
            headers(token),
            Json(input),
        )
        .await
    }

    #[tokio::test]
    async fn interruption_replays_without_discovery_and_preserves_write_receipt_after_revocation() {
        let (state, task, attempt, input) = fixture();
        let native = Native::start(&state, None, None);
        let original = call(&state, &task, &attempt, input.clone(), "device")
            .await
            .unwrap()
            .0;
        assert_eq!(original.value.state, OperationState::Acknowledged);
        let count = native.calls.lock().unwrap().len();
        let replay = call(&state, &task, &attempt, input, "device")
            .await
            .unwrap()
            .0;
        assert!(replay.replayed);
        assert_eq!(
            replay.value.interruption_receipt,
            original.value.interruption_receipt
        );
        assert_eq!(native.calls.lock().unwrap().len(), count);
        drop(native);
        let (state, task, attempt, input) = fixture();
        let _native = Native::start(&state, Some("agent.interrupt_bound"), None);
        let result = call(&state, &task, &attempt, input, "device")
            .await
            .unwrap()
            .0;
        assert!(state.devices.lock().unwrap().is_empty());
        assert_eq!(result.value.state, OperationState::Acknowledged);
        assert!(result.value.interruption_receipt.is_some());
        let detail = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail("default", &task)
            .unwrap();
        assert_eq!(
            detail.attempts[0].lifecycle.reservation,
            Reservation::Reserved
        );
    }

    #[tokio::test]
    async fn interruption_revocation_during_discovery_prevents_admission_and_input() {
        let (state, task, attempt, input) = fixture();
        let native = Native::start(&state, Some("ping"), None);
        assert!(call(&state, &task, &attempt, input, "device")
            .await
            .is_err());
        assert_eq!(*native.calls.lock().unwrap(), vec!["ping"]);
        let detail = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail("default", &task)
            .unwrap();
        assert!(!detail
            .operations
            .iter()
            .any(|op| op.kind == OperationKind::InterruptAttempt));
    }

    #[tokio::test]
    async fn interruption_authority_and_binding_refusals_never_dispatch() {
        let (state, task, attempt, input) = fixture();
        let native = Native::start(&state, None, None);
        for token in ["admin", "unknown"] {
            assert!(call(&state, &task, &attempt, input.clone(), token)
                .await
                .is_err());
        }
        assert!(native.calls.lock().unwrap().is_empty());
        let mut wrong = input;
        wrong.expected_instance_id = "replacement".into();
        assert!(call(&state, &task, &attempt, wrong, "device")
            .await
            .is_err());
        assert_eq!(*native.calls.lock().unwrap(), vec!["ping"]);
    }

    #[tokio::test]
    async fn interruption_uncertainty_is_retained_and_capacity_is_definitely_refused() {
        for (code, expected) in [
            ("delivery_unconfirmed", OperationState::Unconfirmed),
            ("resource_limit", OperationState::Refused),
        ] {
            let (state, task, attempt, input) = fixture();
            let native = Native::start(&state, None, Some(code));
            let result = call(&state, &task, &attempt, input.clone(), "device")
                .await
                .unwrap()
                .0;
            assert_eq!(result.value.state, expected);
            let before = native.calls.lock().unwrap().len();
            let replay = call(&state, &task, &attempt, input, "device")
                .await
                .unwrap()
                .0;
            assert!(replay.replayed);
            assert_eq!(replay.value.state, expected);
            assert_eq!(native.calls.lock().unwrap().len(), before);
        }
    }
}
