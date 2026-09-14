//! Authenticated commit ownership, separate from native execution capabilities.
//! Local lead authority only reaches closed operations guarded by its grant and store fence.
use crate::work::model::*;
use crate::work::store::WorkStore;
use axum::http::HeaderMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct WorkActor {
    id: String,
    authority: Authority,
}
#[derive(Clone)]
enum Authority {
    Paired(Box<PairedAuthority>),
    LocalLead(Box<LocalAuthority>),
    #[cfg(test)]
    Fixture,
}
#[derive(Clone)]
struct PairedAuthority {
    state: crate::AppState,
    headers: HeaderMap,
}
#[derive(Clone)]
struct LocalAuthority {
    state: crate::AppState,
    token: String,
    principal: crate::work_authority::DelegationAuthority,
}
impl WorkActor {
    pub async fn read_child(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        request: crate::work_delegation::ChildReadRequest,
    ) -> WorkResult<serde_json::Value> {
        self.scoped(store, session, move |s, session, _, fence| {
            s.assert_delegation_authority(
                session,
                fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                Some(&request.task_id),
            )?;
            match (request.kind.as_deref(), request.record_id.as_deref()) {
                (None, None)
                    if request.snapshot_revision.is_none() && request.after_id.is_none() =>
                {
                    let detail = s.detail(session, &request.task_id)?;
                    Ok(serde_json::json!({"task":detail.task,"cursor":detail.cursor}))
                }
                (Some(kind), Some(id))
                    if request.snapshot_revision.is_none() && request.after_id.is_none() =>
                {
                    s.immutable_record(session, &request.task_id, kind, id)
                }
                (Some(kind), None) => Ok(serde_json::to_value(
                    s.task_records(
                        session,
                        &request.task_id,
                        kind,
                        request
                            .snapshot_revision
                            .ok_or(WorkError(FailureCode::InvalidInput))?,
                        request.after_id.as_deref(),
                        20,
                    )?,
                )?),
                _ => Err(WorkError(FailureCode::InvalidInput)),
            }
        })
        .await
    }
    pub fn local_lead(state: crate::AppState, token: String) -> WorkResult<Self> {
        let principal = state
            .work_local
            .as_ref()
            .ok_or(WorkError(FailureCode::CapabilityUnavailable))?
            .registry
            .lock()
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
            .authorize_delegation(&token, actor_now())?;
        Ok(Self {
            id: principal.actor_id.clone(),
            authority: Authority::LocalLead(Box::new(LocalAuthority {
                state,
                token,
                principal,
            })),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_local(&self) -> bool {
        matches!(self.authority, Authority::LocalLead(_))
    }
    pub fn local_scope(&self) -> WorkResult<crate::work_authority::Scope> {
        match &self.authority {
            Authority::LocalLead(local) => Ok(local.principal.scope.clone()),
            _ => Err(WorkError(FailureCode::ScopeMismatch)),
        }
    }
    pub async fn observe_controller(&self) -> WorkResult<()> {
        if let Authority::LocalLead(local) = &self.authority {
            let session =
                crate::find_session(&local.state.config, &local.principal.scope.session_id)
                    .map_err(|_| WorkError(FailureCode::ScopeMismatch))?;
            let fence = &local.principal.fence;
            let observation = crate::terminal_backend(session)
                .lifecycle_bound(&fence.instance_id, &fence.native_owner_epoch)
                .await
                .map_err(|_| WorkError(FailureCode::NotReady))?;
            if matches!(observation, crate::backend::BoundLifecycle::Exited { .. }) {
                // The native query has ended. Compare the captured binding while
                // holding only the grant guard; a successor must remain intact.
                local
                    .state
                    .work_local
                    .as_ref()
                    .ok_or(WorkError(FailureCode::CapabilityUnavailable))?
                    .registry
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
                    .invalidate_delegation(&local.principal.scope, fence);
            }
            if observation != crate::backend::BoundLifecycle::Live {
                return Err(WorkError(FailureCode::NotReady));
            }
        }
        Ok(())
    }
    pub async fn authorize_next_effect(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        operation: Operation,
    ) -> WorkResult<()> {
        if !self.is_local() {
            return Ok(());
        }
        self.observe_controller().await?;
        self.scoped(store, session, move |s, session, _, fence| {
            let fence = fence.ok_or(WorkError(FailureCode::ScopeMismatch))?;
            let current = s.get_delegated_operation(session, fence, &operation.id)?;
            if current.delegation_fence.as_ref() != Some(fence)
                || current.task_id != operation.task_id
                || current.attempt_id != operation.attempt_id
                || current.state != OperationState::Submitting
            {
                return Err(WorkError(FailureCode::ScopeMismatch));
            }
            s.assert_launch_allowed(session, &operation.task_id)
        })
        .await
    }
    // Private implementation detail: callers only receive the closed methods below.
    // No caller-supplied closure may pass through a local authority boundary.
    async fn scoped<T: Send + 'static>(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        action: impl FnOnce(&mut WorkStore, &str, &str, Option<&DelegationFence>) -> WorkResult<T>
            + Send
            + 'static,
    ) -> WorkResult<T> {
        if let Authority::LocalLead(local) = &self.authority {
            let local = local.clone();
            return tokio::task::spawn_blocking(move || {
                let registry = local
                    .state
                    .work_local
                    .as_ref()
                    .ok_or(WorkError(FailureCode::CapabilityUnavailable))?
                    .registry
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                let current = registry.authorize_delegation(&local.token, actor_now())?;
                if current.scope != local.principal.scope
                    || current.fence != local.principal.fence
                    || current.scope.session_id != session
                {
                    return Err(WorkError(FailureCode::ScopeMismatch));
                }
                let mut store = local
                    .state
                    .work
                    .as_ref()
                    .ok_or(WorkError(FailureCode::StorageUnavailable))?
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                store.assert_delegation_authority(&session, &current.fence, None)?;
                action(
                    &mut store,
                    &session,
                    &current.actor_id,
                    Some(&current.fence),
                )
            })
            .await
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        }
        let id = self.id.clone();
        self.commit(store, move |store| action(store, &session, &id, None))
            .await
    }
    pub fn paired(
        state: crate::AppState,
        headers: HeaderMap,
        expected_actor: &str,
    ) -> WorkResult<Self> {
        let id = crate::require_device(&state, &headers)
            .map_err(|_| WorkError(FailureCode::ScopeMismatch))?;
        if id != expected_actor {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        Ok(Self {
            id,
            authority: Authority::Paired(Box::new(PairedAuthority { state, headers })),
        })
    }
    #[cfg(test)]
    pub fn fixture(id: &str) -> Self {
        Self {
            id: id.into(),
            authority: Authority::Fixture,
        }
    }
    pub fn assert_actor(&self, expected: &str) -> WorkResult<()> {
        if self.id != expected {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        Ok(())
    }
    pub async fn commit<T: Send + 'static>(
        &self,
        fixture_store: Arc<Mutex<WorkStore>>,
        action: impl FnOnce(&mut WorkStore) -> WorkResult<T> + Send + 'static,
    ) -> WorkResult<T> {
        match &self.authority {
            Authority::LocalLead(_) => Err(WorkError(FailureCode::ScopeMismatch)),
            Authority::Paired(authority) => {
                // Revalidate the token and transport proof under the devices guard,
                // which remains held through the store transaction. Domain errors
                // stay typed rather than being mistaken for authorization failures.
                let _ = fixture_store;
                crate::work_inputs::authorized_store(
                    authority.state.clone(),
                    authority.headers.clone(),
                    self.id.clone(),
                    move |store| Ok(action(store)),
                )
                .await
                .map_err(|(status, _)| {
                    WorkError(if status.is_server_error() {
                        FailureCode::StorageUnavailable
                    } else {
                        FailureCode::ScopeMismatch
                    })
                })?
            }
            #[cfg(test)]
            Authority::Fixture => tokio::task::spawn_blocking(move || {
                let mut store = fixture_store
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                action(&mut store)
            })
            .await
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?,
        }
    }
}

fn actor_now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

impl WorkActor {
    pub async fn child_receipt(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        kind: OperationKind,
        key: String,
    ) -> WorkResult<RequestReceipt> {
        self.scoped(store, session, move |s, session, actor, fence| {
            s.get_delegated_receipt(
                actor,
                session,
                fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                kind,
                &key,
            )
        })
        .await
    }
    pub async fn child_operation(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        id: String,
    ) -> WorkResult<Operation> {
        self.scoped(store, session, move |s, session, _, fence| {
            s.get_delegated_operation(
                session,
                fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                &id,
            )
        })
        .await
    }
    pub async fn set_child_dependencies(
        &self,
        store: Arc<Mutex<WorkStore>>,
        task_id: String,
        request: crate::work_delegation::DependencyUpdateRequest,
    ) -> WorkResult<Mutation<Task>> {
        let scope = self.local_scope()?;
        let task = task_id.clone();
        let req = request.clone();
        let replay = self
            .scoped(
                store.clone(),
                scope.session_id.clone(),
                move |s, session, actor, fence| {
                    s.replay_delegated_dependencies(
                        actor,
                        session,
                        &task,
                        &req.request_key,
                        req.expected_revision,
                        &req.dependencies,
                        fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                    )
                },
            )
            .await?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
        self.observe_controller().await?;
        self.scoped(store, scope.session_id, move |s, session, actor, fence| {
            s.set_delegated_dependencies(
                actor,
                session,
                &task_id,
                &request.request_key,
                request.expected_revision,
                request.dependencies,
                fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                actor_now(),
            )
        })
        .await
    }

    pub async fn create_child(
        &self,
        store: Arc<Mutex<WorkStore>>,
        request: crate::work_delegation::CreateChildRequest,
    ) -> WorkResult<Mutation<Task>> {
        let scope = self.local_scope()?;
        let req = request.clone();
        let (input, replay, frozen) = self
            .scoped(
                store.clone(),
                scope.session_id.clone(),
                move |s, session, actor, fence| {
                    let fence = fence.ok_or(WorkError(FailureCode::ScopeMismatch))?;
                    let parent = s.detail(session, &fence.coordinator_task_id)?.task;
                    let input = CreateTask {
                        repo_path: parent.repo_path,
                        parent_task_id: Some(parent.id),
                        title: req.title,
                        brief: req.brief,
                        policy: req.policy,
                    };
                    let replay = s.replay_delegated_create_with_inputs(
                        actor,
                        session,
                        &req.request_key,
                        fence,
                        req.expected_parent_revision,
                        &input,
                        &req.dependencies,
                        &req.input_refs,
                    )?;
                    let frozen = if replay.is_none() {
                        s.resolve_delegated_inputs(
                            actor,
                            session,
                            fence,
                            None,
                            &req.input_refs,
                            actor_now(),
                        )?
                    } else {
                        vec![]
                    };
                    Ok((input, replay, frozen))
                },
            )
            .await?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
        let root = match &self.authority {
            Authority::LocalLead(local) => local.state.work_artifacts.clone(),
            _ => None,
        };
        let resolved = crate::work_prompt::resolve_inputs(root, frozen).await?;
        let executable =
            std::env::current_exe().map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        crate::work_prompt::render_with_inputs(
            &input.brief,
            &executable,
            Some(crate::work_prompt::INSTRUCTIONS_VERSION),
            &resolved,
        )?;
        self.observe_controller().await?;
        self.scoped(store, scope.session_id, move |s, session, actor, fence| {
            s.create_delegated_task_with_inputs(
                actor,
                session,
                &request.request_key,
                fence.ok_or(WorkError(FailureCode::ScopeMismatch))?,
                request.expected_parent_revision,
                input,
                request.dependencies,
                request.input_refs,
                actor_now(),
            )
        })
        .await
    }
    pub async fn detail(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        task: String,
    ) -> WorkResult<TaskDetail> {
        self.scoped(store, session, move |s, session, _, fence| {
            if let Some(fence) = fence {
                s.assert_delegation_authority(session, fence, Some(&task))?;
            }
            s.detail(session, &task)
        })
        .await
    }
    pub async fn inputs(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        task: Task,
        refs: Vec<InputRef>,
    ) -> WorkResult<Vec<FrozenInputRef>> {
        self.scoped(
            store,
            session,
            move |s, session, actor, fence| match fence {
                Some(fence) => s.resolve_delegated_inputs(
                    actor,
                    session,
                    fence,
                    Some(&task.id),
                    &refs,
                    actor_now(),
                ),
                None => s.resolve_inputs(
                    actor,
                    session,
                    &task.repo_path,
                    Some(&task.id),
                    &refs,
                    actor_now(),
                ),
            },
        )
        .await
    }
    pub async fn prepare_start(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        task: String,
        request: crate::work_execution::StartRequest,
        digest: String,
    ) -> WorkResult<Mutation<Operation>> {
        self.observe_controller().await?;
        self.scoped(store, session, move |s, session, actor, fence| {
            let input = NewAttempt {
                agent_kind: request.agent_kind,
                role: request.role,
            };
            match fence {
                Some(fence) => s.prepare_delegated_attempt(
                    actor,
                    session,
                    &task,
                    &request.request_key,
                    request.expected_revision,
                    input,
                    &digest,
                    fence,
                    actor_now(),
                ),
                None => s.prepare_attempt(
                    actor,
                    session,
                    &task,
                    &request.request_key,
                    request.expected_revision,
                    input,
                    &digest,
                    actor_now(),
                ),
            }
        })
        .await
    }
    pub async fn replay_delivery(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        task: String,
        attempt: String,
        request: crate::work_execution::DeliveryRequest,
    ) -> WorkResult<Option<Mutation<Operation>>> {
        self.scoped(
            store,
            session,
            move |s, session, actor, fence| match fence {
                Some(fence) => s.replay_delegated_delivery(
                    actor,
                    session,
                    &task,
                    &attempt,
                    &request.request_key,
                    request.expected_revision,
                    &request.expected_instance_id,
                    &request.text,
                    Some(crate::work_prompt::INSTRUCTIONS_VERSION),
                    &request.input_refs,
                    fence,
                ),
                None => s.replay_delivery_with_inputs(
                    actor,
                    session,
                    &task,
                    &attempt,
                    &request.request_key,
                    request.expected_revision,
                    &request.expected_instance_id,
                    &request.text,
                    Some(crate::work_prompt::INSTRUCTIONS_VERSION),
                    &request.input_refs,
                ),
            },
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_delivery(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        task: String,
        attempt: String,
        request: crate::work_execution::DeliveryRequest,
        inputs: Vec<FrozenInputRef>,
        digest: String,
    ) -> WorkResult<Mutation<Operation>> {
        self.observe_controller().await?;
        self.scoped(
            store,
            session,
            move |s, session, actor, fence| match fence {
                Some(fence) => s.prepare_delegated_delivery_with_inputs(
                    actor,
                    session,
                    &task,
                    &attempt,
                    &request.request_key,
                    request.expected_revision,
                    &request.expected_instance_id,
                    &request.text,
                    Some(crate::work_prompt::INSTRUCTIONS_VERSION),
                    request.input_refs,
                    &inputs,
                    &digest,
                    fence,
                    actor_now(),
                ),
                None => s.prepare_delivery_with_inputs(
                    actor,
                    session,
                    &task,
                    &attempt,
                    &request.request_key,
                    request.expected_revision,
                    &request.expected_instance_id,
                    &request.text,
                    Some(crate::work_prompt::INSTRUCTIONS_VERSION),
                    request.input_refs,
                    &inputs,
                    &digest,
                    actor_now(),
                ),
            },
        )
        .await
    }
    pub async fn advance(
        &self,
        store: Arc<Mutex<WorkStore>>,
        session: String,
        operation: Operation,
        dispatch: bool,
    ) -> WorkResult<Operation> {
        self.observe_controller().await?;
        self.scoped(store, session, move |s, session, _, fence| {
            if let Some(fence) = fence {
                let attempt = operation
                    .attempt_id
                    .as_deref()
                    .ok_or(WorkError(FailureCode::ScopeMismatch))?;
                if dispatch {
                    s.claim_delegated_start_dispatch(
                        session,
                        &operation.id,
                        &operation.task_id,
                        attempt,
                        fence,
                        actor_now(),
                    )
                } else {
                    s.begin_delegated_operation(
                        session,
                        &operation.id,
                        &operation.task_id,
                        attempt,
                        operation.kind,
                        fence,
                        actor_now(),
                    )
                }
            } else if dispatch {
                s.claim_start_dispatch(session, &operation.id, actor_now())
            } else {
                s.begin_operation(session, &operation.id, actor_now())
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }
    #[test]
    fn production_actor_rejects_admin_unknown_and_forged_identity() {
        let state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        for (token, id) in [
            ("admin", "phone"),
            ("local-agent-token", "phone"),
            ("device", "another-device"),
        ] {
            assert!(WorkActor::paired(state.clone(), headers(token), id).is_err());
        }
        assert!(WorkActor::paired(state, headers("device"), "phone").is_ok());
    }
    #[tokio::test]
    async fn captured_actor_revalidates_revocation_before_any_store_action() {
        let state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        let actor = WorkActor::paired(state.clone(), headers("device"), "phone").unwrap();
        state.devices.lock().unwrap().clear();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let result = actor
            .commit(
                Arc::new(Mutex::new(WorkStore::in_memory().unwrap())),
                move |_| {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
        assert_eq!(result.unwrap_err().0, FailureCode::ScopeMismatch);
        assert!(!ran.load(Ordering::SeqCst));
    }
}
