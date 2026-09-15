//! Recorded execution shared by HTTP and future scoped local task actors.
//! Database transactions end before native I/O; ambiguous operations are never replayed.
use crate::backend::{BackendError, BackendFuture};
use crate::work::model::*;
use crate::work::store::{payload_digest, WorkStore};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartRequest {
    pub request_key: String,
    pub expected_revision: u64,
    pub agent_kind: String,
    pub role: AttemptRole,
    pub branch_name: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeliveryRequest {
    pub request_key: String,
    pub expected_revision: u64,
    pub expected_instance_id: String,
    pub text: String,
    #[serde(default)]
    pub input_refs: Vec<InputRef>,
}
// This port is intentionally small: native adapters own identity/readiness evidence,
// while the existing application helpers own launch command construction.
pub(super) trait ExecutionPort: Send + Sync {
    fn commit_authority(&self) -> WorkResult<crate::work_actor::WorkActor>;
    fn preflight_prompt<'a>(
        &'a self,
        instance: &'a str,
        owner_epoch: Option<&'a str>,
    ) -> BackendFuture<'a, ()>;
    fn input_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn supported(&self) -> BackendFuture<'_, bool>;
    fn preflight_start<'a>(&'a self, _kind: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn place<'a>(
        &'a self,
        task: &'a Task,
        branch: Option<&'a str>,
    ) -> BackendFuture<'a, NativeBinding>;
    fn locate<'a>(&'a self, binding: &'a NativeBinding) -> BackendFuture<'a, NativeBinding>;
    fn start<'a>(
        &'a self,
        operation_id: &'a str,
        pane: &'a str,
        kind: &'a str,
    ) -> BackendFuture<'a, (NativeBinding, Option<String>)>;
    fn lifecycle<'a>(
        &'a self,
        instance_id: &'a str,
        owner_epoch: &'a str,
    ) -> BackendFuture<'a, crate::backend::BoundLifecycle> {
        let _ = (instance_id, owner_epoch);
        Box::pin(async { Ok(crate::backend::BoundLifecycle::Unknown) })
    }
    fn prompt<'a>(&'a self, operation: &'a Operation, text: &'a str) -> BackendFuture<'a, ()>;
}
pub(super) struct Execution<'a> {
    pub store: Arc<Mutex<WorkStore>>,
    pub session: String,
    pub port: &'a dyn ExecutionPort,
}
impl Execution<'_> {
    pub async fn reconcile(
        &self,
        actor: &str,
        task_id: &str,
        attempt_id: &str,
        request: ReconcileInput,
        registry: Arc<Mutex<crate::work_authority::Registry>>,
    ) -> WorkResult<Mutation<ReconciliationReceipt>> {
        if self.port.commit_authority()?.is_local() {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        self.port.commit_authority()?.assert_actor(actor)?;
        let actor = actor.to_owned();
        let tid = task_id.to_owned();
        let aid = attempt_id.to_owned();
        let prepared = self
            .db(move |s, session| {
                s.prepare_reconciliation(&actor, session, &tid, &aid, request, now())
            })
            .await?;
        if prepared.replayed {
            let id = prepared.value.id;
            let receipt = self
                .db(move |s, session| s.get_reconciliation(session, &id))
                .await
                .map_err(|_| WorkError(FailureCode::DeliveryUnconfirmed))?;
            return Ok(Mutation {
                value: receipt,
                replayed: true,
            });
        }
        let id = prepared.value.id.clone();
        let (attempt, revision, refusal) = self
            .db(move |s, session| {
                let op = s.begin_operation(session, &id, now())?;
                let detail = s.detail(session, &op.task_id)?;
                let attempt = detail
                    .attempts
                    .into_iter()
                    .find(|a| Some(&a.id) == op.attempt_id.as_ref())
                    .ok_or(WorkError(FailureCode::NotFound))?;
                let start = detail
                    .operations
                    .iter()
                    .find(|o| {
                        o.kind == OperationKind::StartAttempt && o.attempt_id == op.attempt_id
                    })
                    .ok_or(WorkError(FailureCode::NotFound))?;
                let refusal = s.get_start_refusal(session, &start.id)?;
                Ok((attempt, detail.task.revision, refusal))
            })
            .await?;
        let evidence = if attempt.lifecycle.reservation == Reservation::Released {
            ReconciliationEvidence::Unknown
        } else if attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched {
            ReconciliationEvidence::NotDispatched
        } else if let Some(proof) = refusal {
            ReconciliationEvidence::NativeNotStarted {
                start_operation_id: proof.start_operation_id,
                owner_epoch: proof.owner_epoch,
                receipt_id: proof.receipt_id,
            }
        } else if let (Some(instance), Some(epoch)) = (
            attempt.binding.instance_id.as_deref(),
            attempt.lifecycle.native_owner_epoch.as_deref(),
        ) {
            match self.port.lifecycle(instance, epoch).await {
                Ok(crate::backend::BoundLifecycle::Live) => ReconciliationEvidence::Live {
                    instance_id: instance.into(),
                    owner_epoch: epoch.into(),
                },
                Ok(crate::backend::BoundLifecycle::Exited { receipt_id }) => {
                    ReconciliationEvidence::Exited {
                        instance_id: instance.into(),
                        owner_epoch: epoch.into(),
                        receipt_id,
                    }
                }
                _ => ReconciliationEvidence::Unknown,
            }
        } else {
            ReconciliationEvidence::Unknown
        };
        let store = self.store.clone();
        let session = self.session.clone();
        let id = prepared.value.id;
        let scope = crate::work_authority::Scope {
            session_id: session.clone(),
            task_id: task_id.into(),
            attempt_id: attempt_id.into(),
        };
        let (receipt, cleanup) = tokio::task::spawn_blocking(move || {
            let mut grants = registry
                .lock()
                .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
            let receipt = {
                let mut store = store
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                match store.finish_reconciliation(&session, &id, revision, evidence, now()) {
                    Ok(receipt) => receipt,
                    Err(error) if error.0 == FailureCode::RevisionConflict => {
                        // The read completed against an obsolete task snapshot. No release
                        // committed, so record a terminal refusal and preserve every grant.
                        store.finalize_operation(
                            &session,
                            &id,
                            OperationOutcome {
                                state: OperationState::Refused,
                                resources: NativeBinding::default(),
                                failure_code: Some(FailureCode::RevisionConflict),
                            },
                            now(),
                        )?;
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                }
            };
            let cleanup = if receipt.reservation == Reservation::Released {
                grants.revoke_memory(&scope)
            } else {
                Vec::new()
            };
            Ok::<_, WorkError>((receipt, cleanup))
        })
        .await
        .map_err(|_| WorkError(FailureCode::StorageUnavailable))??;
        tokio::task::spawn_blocking(move || crate::work_authority::cleanup_context_files(cleanup))
            .await
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        Ok(Mutation {
            value: receipt,
            replayed: false,
        })
    }
    async fn db<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut WorkStore, &str) -> WorkResult<T> + Send + 'static,
    ) -> WorkResult<T> {
        let store = self.store.clone();
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
            action(&mut store, &session)
        })
        .await
        .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
    }
    async fn supported(&self) -> WorkResult<()> {
        match self.port.supported().await {
            Ok(true) => Ok(()),
            _ => Err(WorkError(FailureCode::CapabilityUnavailable)),
        }
    }
    async fn check_launch(&self, task_id: &str) -> WorkResult<()> {
        let task_id = task_id.to_owned();
        self.db(move |s, session| s.assert_launch_allowed(session, &task_id))
            .await
    }
    async fn checkpoint(&self, op: &Operation, resources: NativeBinding) -> WorkResult<Operation> {
        let id = op.id.clone();
        self.db(move |s, session| s.checkpoint_operation(session, &id, resources, now()))
            .await
    }
    async fn finish(
        &self,
        op: &Operation,
        result: Result<(), BackendError>,
    ) -> WorkResult<Operation> {
        let (state, failure_code) = match result {
            Err(BackendError::StartNotStarted { code, .. }) => {
                let failure = match code.as_deref() {
                    Some("unsupported_agent_kind") => FailureCode::CapabilityUnavailable,
                    Some("resource_limit") => FailureCode::ResourceLimit,
                    _ => FailureCode::NotReady,
                };
                (OperationState::Refused, Some(failure))
            }
            Ok(()) => (OperationState::Acknowledged, None),
            Err(BackendError::Unsupported(_)) => (
                OperationState::Refused,
                Some(FailureCode::CapabilityUnavailable),
            ),
            Err(BackendError::Refused { code, .. }) => {
                let failure = match code.as_deref() {
                    Some("unsupported_agent_kind") => FailureCode::CapabilityUnavailable,
                    Some("instance_changed" | "launch_changed" | "stale_launch") => {
                        FailureCode::InstanceChanged
                    }
                    Some("approval_required" | "agent_blocked") => FailureCode::ApprovalRequired,
                    _ => FailureCode::NotReady,
                };
                (OperationState::Refused, Some(failure))
            }
            Err(_) => (
                OperationState::Unconfirmed,
                Some(FailureCode::DeliveryUnconfirmed),
            ),
        };
        let id = op.id.clone();
        self.db(move |s, session| {
            s.finalize_operation(
                session,
                &id,
                OperationOutcome {
                    state,
                    resources: NativeBinding::default(),
                    failure_code,
                },
                now(),
            )
        })
        .await
    }
    pub async fn start(
        &self,
        actor: &str,
        task_id: &str,
        request: StartRequest,
    ) -> WorkResult<Mutation<Operation>> {
        self.port.commit_authority()?.assert_actor(actor)?;
        self.supported().await?;
        if let Some(branch) = &request.branch_name {
            crate::tasks::validate_branch_name(branch)
                .map_err(|_| WorkError(FailureCode::InvalidInput))?;
        }
        if request.role == AttemptRole::Worker && request.branch_name.is_none() {
            return Err(WorkError(FailureCode::InvalidInput));
        }
        let digest = payload_digest(&request)?;
        let authority = self.port.commit_authority()?;
        let mutation = authority
            .prepare_start(
                self.store.clone(),
                self.session.clone(),
                task_id.into(),
                request.clone(),
                digest,
            )
            .await?;
        if mutation.replayed {
            return Ok(mutation);
        }
        if self
            .port
            .preflight_start(&request.agent_kind)
            .await
            .is_err()
        {
            let id = mutation.value.id;
            return self
                .db(move |s, session| {
                    s.refuse_prepared_operation(
                        session,
                        &id,
                        FailureCode::CapabilityUnavailable,
                        now(),
                    )
                })
                .await
                .map(|value| Mutation {
                    value,
                    replayed: false,
                });
        }
        let op = match authority
            .advance(
                self.store.clone(),
                self.session.clone(),
                mutation.value.clone(),
                false,
            )
            .await
        {
            Ok(op) => op,
            Err(error) => {
                let id = mutation.value.id;
                return self
                    .db(move |s, session| s.refuse_prepared_operation(session, &id, error.0, now()))
                    .await
                    .map(|value| Mutation {
                        value,
                        replayed: false,
                    });
            }
        };
        let tid = task_id.to_owned();
        let task = self
            .db(move |s, session| Ok(s.detail(session, &tid)?.task))
            .await?;
        let preflight = async {
            let resolved =
                crate::work_prompt::resolve_inputs(self.port.input_root(), task.input_refs.clone())
                    .await?;
            let executable =
                std::env::current_exe().map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
            crate::work_prompt::render_with_inputs(
                &task.brief,
                &executable,
                Some(crate::work_prompt::INSTRUCTIONS_VERSION),
                &resolved,
            )
        }
        .await;
        if let Err(error) = preflight {
            return self.refuse(&op, error.0).await;
        }
        if let Err(error) = self.check_launch(task_id).await {
            return self.refuse(&op, error.0).await;
        }
        if let Err(error) = authority
            .authorize_next_effect(self.store.clone(), self.session.clone(), op.clone())
            .await
        {
            return self.refuse(&op, error.0).await;
        }
        let resources = match self.port.place(&task, request.branch_name.as_deref()).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(Mutation {
                    value: self.finish(&op, Err(e)).await?,
                    replayed: false,
                })
            }
        };
        let op = self.checkpoint(&op, resources).await?;
        if op.state != OperationState::Submitting {
            return Ok(Mutation {
                value: op,
                replayed: false,
            });
        }
        if request.branch_name.is_some() && op.resources.worktree_path.is_none() {
            return self.refuse(&op, FailureCode::NotReady).await;
        }
        if let Err(error) = authority
            .authorize_next_effect(self.store.clone(), self.session.clone(), op.clone())
            .await
        {
            return self.refuse(&op, error.0).await;
        }
        let resources = match self.port.locate(&op.resources).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(Mutation {
                    value: self.finish(&op, Err(e)).await?,
                    replayed: false,
                })
            }
        };
        let op = self.checkpoint(&op, resources).await?;
        if op.state != OperationState::Submitting {
            return Ok(Mutation {
                value: op,
                replayed: false,
            });
        }
        if let Err(error) = self.check_launch(task_id).await {
            return self.refuse(&op, error.0).await;
        }
        let Some(pane) = op.resources.pane_id.as_deref() else {
            return self.refuse(&op, FailureCode::NotReady).await;
        };
        let op = match authority
            .advance(self.store.clone(), self.session.clone(), op.clone(), true)
            .await
        {
            Ok(op) => op,
            Err(error) => return self.refuse(&op, error.0).await,
        };
        let (resources, owner_epoch) =
            match self.port.start(&op.id, pane, &request.agent_kind).await {
                Ok(r) => r,
                Err(e) => {
                    if let BackendError::StartNotStarted {
                        operation_id,
                        owner_epoch,
                        receipt_id,
                        ..
                    } = &e
                    {
                        if operation_id != &op.id {
                            return Err(WorkError(FailureCode::ScopeMismatch));
                        }
                        let id = op.id.clone();
                        let epoch = owner_epoch.clone();
                        let receipt = receipt_id.clone();
                        self.db(move |s, session| {
                            s.record_start_refusal(session, &id, &epoch, &receipt, now())
                        })
                        .await?;
                    }
                    return Ok(Mutation {
                        value: self.finish(&op, Err(e)).await?,
                        replayed: false,
                    });
                }
            };
        let id = op.id.clone();
        let op = if let Some(epoch) = owner_epoch {
            self.db(move |s, session| {
                s.confirm_start_launch(session, &id, resources, &epoch, now())
            })
            .await?
        } else {
            self.checkpoint(&op, resources).await?
        };
        if op.resources.instance_id.is_none() {
            return Ok(Mutation {
                value: self
                    .finish(
                        &op,
                        Err(BackendError::InvalidResponse("native launch identity")),
                    )
                    .await?,
                replayed: false,
            });
        }
        Ok(Mutation {
            value: self.finish(&op, Ok(())).await?,
            replayed: false,
        })
    }
    async fn refuse(&self, op: &Operation, code: FailureCode) -> WorkResult<Mutation<Operation>> {
        let id = op.id.clone();
        let value = self
            .db(move |s, session| {
                s.finalize_operation(
                    session,
                    &id,
                    OperationOutcome {
                        state: OperationState::Refused,
                        resources: NativeBinding::default(),
                        failure_code: Some(code),
                    },
                    now(),
                )
            })
            .await?;
        Ok(Mutation {
            value,
            replayed: false,
        })
    }
    pub async fn deliver(
        &self,
        actor: &str,
        task_id: &str,
        attempt_id: &str,
        request: DeliveryRequest,
    ) -> WorkResult<Mutation<Operation>> {
        self.port.commit_authority()?.assert_actor(actor)?;
        let authority = self.port.commit_authority()?;
        if let Some(replay) = authority
            .replay_delivery(
                self.store.clone(),
                self.session.clone(),
                task_id.into(),
                attempt_id.into(),
                request.clone(),
            )
            .await?
        {
            return Ok(replay);
        }
        self.supported().await?;
        let detail = authority
            .detail(self.store.clone(), self.session.clone(), task_id.into())
            .await?;
        if detail.task.revision != request.expected_revision {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        let attempt = detail
            .attempts
            .iter()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
        if attempt.binding.instance_id.as_deref() != Some(request.expected_instance_id.as_str())
            || attempt.lifecycle.reservation == Reservation::Released
        {
            return Err(WorkError(FailureCode::InstanceChanged));
        }
        let bootstrap = !detail.operations.iter().any(|op| {
            op.attempt_id.as_deref() == Some(attempt_id)
                && op.kind == OperationKind::DeliverPrompt
                && op.state != OperationState::Refused
        });
        let owner_epoch = attempt.lifecycle.native_owner_epoch.clone();
        let frozen = authority
            .inputs(
                self.store.clone(),
                self.session.clone(),
                detail.task,
                request.input_refs.clone(),
            )
            .await?;
        self.port
            .preflight_prompt(&request.expected_instance_id, owner_epoch.as_deref())
            .await
            .map_err(|error| {
                WorkError(match error {
                    BackendError::Refused { code, .. }
                        if code.as_deref() == Some("agent_blocked") =>
                    {
                        FailureCode::ApprovalRequired
                    }
                    BackendError::Unsupported(_) => FailureCode::CapabilityUnavailable,
                    _ => FailureCode::NotReady,
                })
            })?;
        let resolved =
            crate::work_prompt::resolve_inputs(self.port.input_root(), frozen.clone()).await?;
        let executable =
            std::env::current_exe().map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        let prompt = crate::work_prompt::render_with_inputs(
            &request.text,
            &executable,
            bootstrap.then_some(crate::work_prompt::INSTRUCTIONS_VERSION),
            &resolved,
        )?;
        let digest = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(prompt.as_bytes()))
        };
        let mutation = authority
            .prepare_delivery(
                self.store.clone(),
                self.session.clone(),
                task_id.into(),
                attempt_id.into(),
                request,
                frozen,
                digest,
            )
            .await?;
        if mutation.replayed {
            return Ok(mutation);
        }
        let op = match authority
            .advance(
                self.store.clone(),
                self.session.clone(),
                mutation.value.clone(),
                false,
            )
            .await
        {
            Ok(op) => op,
            Err(error) => {
                let id = mutation.value.id;
                return self
                    .db(move |s, session| s.refuse_prepared_operation(session, &id, error.0, now()))
                    .await
                    .map(|value| Mutation {
                        value,
                        replayed: false,
                    });
            }
        };
        let result = self.port.prompt(&op, &prompt).await;
        Ok(Mutation {
            value: self.finish(&op, result).await?,
            replayed: false,
        })
    }
}
fn now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

pub(super) struct SessionPort<'a> {
    pub state: &'a crate::AppState,
    pub session: &'a crate::SessionConfig,
    pub authority: crate::work_actor::WorkActor,
}
impl ExecutionPort for SessionPort<'_> {
    fn commit_authority(&self) -> WorkResult<crate::work_actor::WorkActor> {
        Ok(self.authority.clone())
    }
    fn preflight_prompt<'a>(
        &'a self,
        instance: &'a str,
        owner_epoch: Option<&'a str>,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let backend = crate::terminal_backend(self.session);
            if let Some(epoch) = owner_epoch {
                if backend.lifecycle_bound(instance, epoch).await?
                    != crate::backend::BoundLifecycle::Live
                {
                    return Err(BackendError::Refused {
                        code: Some("instance_changed".into()),
                        message: "Exact owned assistant is not live".into(),
                    });
                }
            }
            backend.preflight_bound_prompt(instance).await
        })
    }
    fn input_root(&self) -> Option<std::path::PathBuf> {
        self.state.work_artifacts.clone()
    }
    fn supported(&self) -> BackendFuture<'_, bool> {
        Box::pin(async move {
            if self.state.work_local.is_none() {
                return Ok(false);
            }
            let metadata = crate::terminal_backend(self.session).metadata().await?;
            Ok(metadata.capabilities.contains(&"instance_bound_start")
                && metadata.capabilities.contains(&"instance_bound_prompt"))
        })
    }
    fn preflight_start<'a>(&'a self, kind: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            if kind == "codex" {
                let metadata = crate::terminal_backend(self.session).metadata().await?;
                if !metadata.capabilities.contains(&"reporting_mcp_codex") {
                    return Err(BackendError::Unsupported("reporting_mcp_codex"));
                }
                let local = self
                    .state
                    .work_local
                    .as_ref()
                    .ok_or(BackendError::Unsupported("local_work_authority"))?;
                crate::work_reporting_binary::pin_reporting_executable(&local.directory)
                    .await
                    .map_err(|_| BackendError::Unsupported("reporting_mcp_codex"))?;
            }
            Ok(())
        })
    }
    fn place<'a>(
        &'a self,
        task: &'a Task,
        branch: Option<&'a str>,
    ) -> BackendFuture<'a, NativeBinding> {
        Box::pin(async move {
            let root = std::path::Path::new(&task.repo_path);
            if let Some(branch) = branch {
                let placement = crate::create_task_worktree(
                    self.session,
                    &crate::BackendWorktreeRequest {
                        cwd: root.into(),
                        branch: branch.into(),
                        label: Some(task.title.clone()),
                        focus: false,
                    },
                )
                .await?;
                Ok(NativeBinding {
                    workspace_id: Some(placement.workspace_id.as_str().into()),
                    pane_id: Some(placement.pane_id.as_str().into()),
                    worktree_path: placement.path.map(|p| p.to_string_lossy().into_owned()),
                    ..NativeBinding::default()
                })
            } else {
                let workspace =
                    crate::create_task_workspace(self.session, root, Some(&task.title)).await?;
                Ok(NativeBinding {
                    workspace_id: Some(workspace.id.as_str().into()),
                    ..NativeBinding::default()
                })
            }
        })
    }
    fn locate<'a>(&'a self, binding: &'a NativeBinding) -> BackendFuture<'a, NativeBinding> {
        Box::pin(async move {
            if binding.pane_id.is_some() {
                return Ok(NativeBinding::default());
            }
            let pane = crate::terminal_backend(self.session)
                .list_panes()
                .await?
                .into_iter()
                .find(|p| Some(p.workspace_id.as_str()) == binding.workspace_id.as_deref())
                .ok_or(BackendError::InvalidResponse("workspace pane"))?;
            Ok(NativeBinding {
                pane_id: Some(pane.id.as_str().into()),
                tab_id: Some(pane.tab_id.as_str().into()),
                ..NativeBinding::default()
            })
        })
    }
    fn start<'a>(
        &'a self,
        operation_id: &'a str,
        pane: &'a str,
        kind: &'a str,
    ) -> BackendFuture<'a, (NativeBinding, Option<String>)> {
        Box::pin(async move {
            let command = crate::tasks::agent_command(kind, &self.state.config.agent_commands);
            let request = crate::backend_start_request(
                pane,
                kind,
                &command,
                &[],
                crate::tasks::DEFAULT_AGENT_START_TIMEOUT_MS,
            );
            let local = self
                .state
                .work_local
                .as_ref()
                .ok_or(BackendError::Unsupported("local_work_authority"))?
                .clone();
            let reporting = if kind == "codex" {
                let metadata = crate::terminal_backend(self.session).metadata().await?;
                if !metadata.capabilities.contains(&"reporting_mcp_codex") {
                    return Err(BackendError::Unsupported("reporting_mcp_codex"));
                }
                let pinned =
                    crate::work_reporting_binary::pin_reporting_executable(&local.directory)
                        .await
                        .map_err(|_| BackendError::Unsupported("reporting_mcp_codex"))?;
                Some(crate::backend::ReportingMcp {
                    executable: pinned.executable,
                    sha256: pinned.sha256,
                })
            } else {
                None
            };
            let store = self.state.work.clone().ok_or(BackendError::Unavailable)?;
            let sid = self.session.id.clone();
            let oid = operation_id.to_owned();
            let grant_local = local.clone();
            let (scope, path) = tokio::task::spawn_blocking(move || {
                let mut registry = grant_local
                    .registry
                    .lock()
                    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                let scope = {
                    let mut store = store
                        .lock()
                        .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
                    let op = store.get_operation(&sid, &oid)?;
                    let attempt_id = op.attempt_id.ok_or(WorkError(FailureCode::ScopeMismatch))?;
                    let attempt = store
                        .detail(&sid, &op.task_id)?
                        .attempts
                        .into_iter()
                        .find(|a| a.id == attempt_id)
                        .ok_or(WorkError(FailureCode::ScopeMismatch))?;
                    if attempt.lifecycle.reservation == Reservation::Released {
                        return Err(WorkError(FailureCode::ScopeMismatch));
                    }
                    crate::work_authority::Scope {
                        session_id: sid,
                        task_id: op.task_id,
                        attempt_id,
                    }
                };
                let path = registry.issue(
                    &grant_local.directory,
                    &grant_local.socket,
                    scope.clone(),
                    now(),
                )?;
                Ok::<_, WorkError>((scope, path))
            })
            .await
            .map_err(|_| BackendError::Unavailable)?
            .map_err(|_| BackendError::Refused {
                code: Some("local_authority_unavailable".into()),
                message: "Scoped task authority is unavailable".into(),
            })?;
            let path = path
                .to_str()
                .ok_or(BackendError::InvalidTarget("context path"))?;
            let result = crate::terminal_backend(self.session)
                .start_bound_agent(&request, operation_id, Some(path), reporting.as_ref())
                .await;
            if matches!(&result, Err(BackendError::StartNotStarted { .. })) {
                if let Ok(mut registry) = local.registry.lock() {
                    registry.revoke(&scope);
                }
            }
            let started = result?;
            Ok((
                NativeBinding {
                    instance_id: started.launch_id,
                    target: started.target,
                    ..NativeBinding::default()
                },
                started.owner_epoch,
            ))
        })
    }
    fn lifecycle<'a>(
        &'a self,
        instance_id: &'a str,
        owner_epoch: &'a str,
    ) -> BackendFuture<'a, crate::backend::BoundLifecycle> {
        Box::pin(async move {
            crate::terminal_backend(self.session)
                .lifecycle_bound(instance_id, owner_epoch)
                .await
        })
    }
    fn prompt<'a>(&'a self, operation: &'a Operation, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let launch = operation
                .resources
                .instance_id
                .as_deref()
                .ok_or(BackendError::InvalidTarget("missing launch identity"))?;
            let receipt: crate::backend::BoundPromptReceipt = crate::terminal_backend(self.session)
                .prompt_bound_agent(&crate::backend::BoundPrompt {
                    expected_launch_id: launch.into(),
                    operation_id: operation.id.clone(),
                    text: text.into(),
                })
                .await?;
            if receipt.operation_id != operation.id || receipt.launch_id != launch {
                return Err(BackendError::InvalidResponse("bound prompt receipt"));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FakePort {
        commit_authority: Option<(crate::AppState, axum::http::HeaderMap, String)>,
        preflight_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        preflight_refused: bool,
        input_root: Option<std::path::PathBuf>,
        lifecycle_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        lifecycle: crate::backend::BoundLifecycle,
        supported: bool,
        calls: AtomicUsize,
        fail_start: bool,
        reporting_available: bool,
        refuse_start: Option<&'static str>,
        fail_prompt: bool,
        prompts: Mutex<Vec<String>>,
        pause_after_place: Option<(Arc<Mutex<WorkStore>>, String)>,
        release_after_place: Option<(Arc<Mutex<WorkStore>>, String)>,
    }
    impl ExecutionPort for FakePort {
        fn commit_authority(&self) -> WorkResult<crate::work_actor::WorkActor> {
            match &self.commit_authority {
                Some((state, headers, actor)) => {
                    crate::work_actor::WorkActor::paired(state.clone(), headers.clone(), actor)
                }
                None => Ok(crate::work_actor::WorkActor::fixture("user")),
            }
        }
        fn preflight_prompt<'a>(
            &'a self,
            _instance: &'a str,
            _owner_epoch: Option<&'a str>,
        ) -> BackendFuture<'a, ()> {
            Box::pin(async move {
                if let Some((entered, resume)) = &self.preflight_gate {
                    entered.notify_one();
                    resume.notified().await;
                }
                if self.preflight_refused {
                    Err(BackendError::Refused {
                        code: Some("agent_blocked".into()),
                        message: "Approval required".into(),
                    })
                } else {
                    Ok(())
                }
            })
        }
        fn input_root(&self) -> Option<std::path::PathBuf> {
            self.input_root.clone()
        }
        fn lifecycle<'a>(
            &'a self,
            _instance: &'a str,
            _epoch: &'a str,
        ) -> BackendFuture<'a, crate::backend::BoundLifecycle> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some((entered, resume)) = &self.lifecycle_gate {
                    entered.notify_one();
                    resume.notified().await;
                }
                Ok(self.lifecycle.clone())
            })
        }
        fn supported(&self) -> BackendFuture<'_, bool> {
            Box::pin(async move { Ok(self.supported) })
        }
        fn place<'a>(
            &'a self,
            _task: &'a Task,
            _branch: Option<&'a str>,
        ) -> BackendFuture<'a, NativeBinding> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some((store, tid)) = &self.release_after_place {
                    let mut store = store.lock().unwrap();
                    let detail = store.detail("session", tid).unwrap();
                    let aid = detail.attempts[0].id.clone();
                    let op = store
                        .prepare_reconciliation(
                            "user",
                            "session",
                            tid,
                            &aid,
                            ReconcileInput {
                                request_key: "release-before-dispatch".into(),
                                expected_revision: detail.task.revision,
                                expected_instance_id: None,
                                expected_native_owner_epoch: None,
                            },
                            10,
                        )
                        .unwrap()
                        .value;
                    store.begin_operation("session", &op.id, 11).unwrap();
                    let revision = store.detail("session", tid).unwrap().task.revision;
                    store
                        .finish_reconciliation(
                            "session",
                            &op.id,
                            revision,
                            ReconciliationEvidence::NotDispatched,
                            12,
                        )
                        .unwrap();
                }
                if let Some((store, tid)) = &self.pause_after_place {
                    let mut store = store.lock().unwrap();
                    let revision = store.detail("session", tid).unwrap().task.revision;
                    store
                        .set_paused(
                            "user",
                            "session",
                            tid,
                            "pause",
                            revision,
                            PauseInput { paused: true },
                            10,
                        )
                        .unwrap();
                }
                Ok(NativeBinding {
                    workspace_id: Some("workspace".into()),
                    ..NativeBinding::default()
                })
            })
        }
        fn locate<'a>(&'a self, _binding: &'a NativeBinding) -> BackendFuture<'a, NativeBinding> {
            Box::pin(async move {
                Ok(NativeBinding {
                    pane_id: Some("pane".into()),
                    ..NativeBinding::default()
                })
            })
        }
        fn preflight_start<'a>(&'a self, _kind: &'a str) -> BackendFuture<'a, ()> {
            Box::pin(async move {
                if self.reporting_available {
                    Ok(())
                } else {
                    Err(BackendError::Unsupported("reporting_mcp_codex"))
                }
            })
        }
        fn start<'a>(
            &'a self,
            operation_id: &'a str,
            _pane: &'a str,
            _kind: &'a str,
        ) -> BackendFuture<'a, (NativeBinding, Option<String>)> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(code) = self.refuse_start {
                    return Err(BackendError::StartNotStarted {
                        code: Some(code.into()),
                        operation_id: operation_id.into(),
                        owner_epoch: "native-owner".into(),
                        receipt_id: "no-process-proof".into(),
                    });
                }
                if self.fail_start {
                    return Err(BackendError::Unavailable);
                }
                Ok((
                    NativeBinding {
                        instance_id: Some("native-generation".into()),
                        ..NativeBinding::default()
                    },
                    Some("native-owner".into()),
                ))
            })
        }
        fn prompt<'a>(&'a self, _operation: &'a Operation, text: &'a str) -> BackendFuture<'a, ()> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.prompts.lock().unwrap().push(text.to_owned());
                if self.fail_prompt {
                    Err(BackendError::Unavailable)
                } else {
                    Ok(())
                }
            })
        }
    }
    fn fixture() -> (Arc<Mutex<WorkStore>>, Task, StartRequest) {
        let mut store = WorkStore::in_memory().unwrap();
        let task = store
            .create_task(
                "user",
                "session",
                "task",
                CreateTask {
                    repo_path: "/tmp/project".into(),
                    title: "Task".into(),
                    brief: "Do work".into(),
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
        (
            Arc::new(Mutex::new(store)),
            task,
            StartRequest {
                request_key: "start".into(),
                expected_revision: 1,
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
                branch_name: None,
            },
        )
    }
    fn port() -> FakePort {
        FakePort {
            commit_authority: None,
            preflight_gate: None,
            preflight_refused: false,
            input_root: None,
            lifecycle_gate: None,
            lifecycle: crate::backend::BoundLifecycle::Unknown,
            supported: true,
            calls: AtomicUsize::new(0),
            fail_start: false,
            reporting_available: true,
            refuse_start: None,
            fail_prompt: false,
            prompts: Mutex::new(Vec::new()),
            pause_after_place: None,
            release_after_place: None,
        }
    }
    #[tokio::test]
    async fn caller_cannot_substitute_another_actor_for_authenticated_execution() {
        let (store, task, request) = fixture();
        let port = port();
        let execution = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        assert_eq!(
            execution
                .start("forged-lead", &task.id, request)
                .await
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        assert!(store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .attempts
            .is_empty());
    }
    #[tokio::test]
    async fn exact_exit_releases_and_revokes_once_without_replaying_native_query() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.lifecycle = crate::backend::BoundLifecycle::Exited {
            receipt_id: "reaped-owned-child".into(),
        };
        let execution = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let started = execution
            .start("user", &task.id, request)
            .await
            .unwrap()
            .value;
        let aid = started.attempt_id.unwrap();
        let directory =
            std::env::temp_dir().join(format!("work-reconcile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let mut grants = crate::work_authority::Registry::default();
        let scope = crate::work_authority::Scope {
            session_id: "session".into(),
            task_id: task.id.clone(),
            attempt_id: aid.clone(),
        };
        let path = grants
            .issue(&directory, &directory.join("socket"), scope.clone(), now())
            .unwrap();
        let context: crate::work_authority::ContextFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let grants = Arc::new(Mutex::new(grants));
        let revision = store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .task
            .revision;
        let input = ReconcileInput {
            request_key: "check".into(),
            expected_revision: revision,
            expected_instance_id: Some("native-generation".into()),
            expected_native_owner_epoch: Some("native-owner".into()),
        };
        let checked = execution
            .reconcile("user", &task.id, &aid, input.clone(), grants.clone())
            .await
            .unwrap();
        assert_eq!(checked.value.reservation, Reservation::Released);
        assert_eq!(checked.value.observation, LifecycleObservation::Exited);
        assert!(grants
            .lock()
            .unwrap()
            .authorize(&context.token, &scope, now())
            .is_err());
        assert!(!path.exists());
        let calls = port.calls.load(Ordering::SeqCst);
        let replay = execution
            .reconcile("user", &task.id, &aid, input, grants)
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.value.operation_id, checked.value.operation_id);
        assert_eq!(port.calls.load(Ordering::SeqCst), calls);
        assert!(store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .attempts[0]
            .binding
            .workspace_id
            .is_some());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn live_and_unknown_checks_keep_reservations() {
        for observation in [
            crate::backend::BoundLifecycle::Live,
            crate::backend::BoundLifecycle::Unknown,
        ] {
            let (store, task, request) = fixture();
            let mut port = port();
            port.lifecycle = observation;
            let execution = Execution {
                store: store.clone(),
                session: "session".into(),
                port: &port,
            };
            let op = execution
                .start("user", &task.id, request)
                .await
                .unwrap()
                .value;
            let revision = store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision;
            let checked = execution
                .reconcile(
                    "user",
                    &task.id,
                    op.attempt_id.as_deref().unwrap(),
                    ReconcileInput {
                        request_key: "check".into(),
                        expected_revision: revision,
                        expected_instance_id: Some("native-generation".into()),
                        expected_native_owner_epoch: Some("native-owner".into()),
                    },
                    Arc::new(Mutex::new(crate::work_authority::Registry::default())),
                )
                .await
                .unwrap();
            assert_eq!(checked.value.reservation, Reservation::Reserved);
            assert!(checked.value.release.is_none());
        }
    }
    #[tokio::test]
    async fn delayed_lifecycle_revision_conflict_has_terminal_read_only_recovery() {
        let (store, task, request) = fixture();
        let mut port = port();
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        port.lifecycle_gate = Some((entered.clone(), resume.clone()));
        port.lifecycle = crate::backend::BoundLifecycle::Exited {
            receipt_id: "exit-proof".into(),
        };
        let execution = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let started = execution
            .start("user", &task.id, request)
            .await
            .unwrap()
            .value;
        let aid = started.attempt_id.unwrap();
        let revision = store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .task
            .revision;
        let directory =
            std::env::temp_dir().join(format!("work-conflict-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let mut registry = crate::work_authority::Registry::default();
        let scope = crate::work_authority::Scope {
            session_id: "session".into(),
            task_id: task.id.clone(),
            attempt_id: aid.clone(),
        };
        let path = registry
            .issue(&directory, &directory.join("socket"), scope.clone(), now())
            .unwrap();
        let context: crate::work_authority::ContextFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let checking = execution.reconcile(
            "user",
            &task.id,
            &aid,
            ReconcileInput {
                request_key: "delayed-check".into(),
                expected_revision: revision,
                expected_instance_id: Some("native-generation".into()),
                expected_native_owner_epoch: Some("native-owner".into()),
            },
            registry.clone(),
        );
        tokio::pin!(checking);
        tokio::select! { _ = &mut checking => panic!("native query must wait"), _ = entered.notified() => {} }
        {
            // Native awaits hold neither grant nor database lock.
            let grants = registry.try_lock().unwrap();
            grants.authorize(&context.token, &scope, now()).unwrap();
            let mut store = store.try_lock().unwrap();
            let revision = store.detail("session", &task.id).unwrap().task.revision;
            store
                .set_paused(
                    "user",
                    "session",
                    &task.id,
                    "pause-concurrently",
                    revision,
                    PauseInput { paused: true },
                    now(),
                )
                .unwrap();
        }
        resume.notify_one();
        assert_eq!(checking.await.unwrap_err().0, FailureCode::RevisionConflict);
        let mut store = store.lock().unwrap();
        let receipt = store
            .get_receipt(
                "user",
                "session",
                OperationKind::ReconcileAttempt,
                "delayed-check",
            )
            .unwrap();
        assert_eq!(receipt.value["receipt_type"], "operation");
        assert_eq!(receipt.value["operation"]["state"], "refused");
        assert_eq!(
            receipt.value["operation"]["failure_code"],
            "revision_conflict"
        );
        assert_eq!(
            store.detail("session", &task.id).unwrap().attempts[0]
                .lifecycle
                .reservation,
            Reservation::Reserved
        );
        registry
            .lock()
            .unwrap()
            .authorize(&context.token, &scope, now())
            .unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn immutable_inputs_are_explicit_and_replay_does_not_reread_a_missing_blob() {
        use sha2::{Digest, Sha256};
        let (store, task, start) = fixture();
        let mut port = port();
        let root =
            std::env::temp_dir().join(format!("work-input-delivery-{}", uuid::Uuid::new_v4()));
        let bytes = b"Reference content for the assistant";
        let hash = format!("{:x}", Sha256::digest(bytes));
        crate::work_artifacts::publish_bytes(
            &root,
            &ArtifactRef {
                path: "reference.txt".into(),
                sha256: hash.clone(),
                size_bytes: bytes.len() as u64,
            },
            bytes,
        )
        .unwrap();
        port.input_root = Some(root.clone());
        let receipt = store
            .lock()
            .unwrap()
            .commit_input_upload(
                "user",
                "session",
                "upload",
                InputUpload {
                    repo_path: task.repo_path.clone(),
                    name: "reference.txt".into(),
                    mime: "text/plain; charset=utf-8".into(),
                    size_bytes: bytes.len() as u64,
                    sha256: hash.clone(),
                },
                now(),
            )
            .unwrap();
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let started = service.start("user", &task.id, start).await.unwrap().value;
        let aid = started.attempt_id.unwrap();
        let revision = || {
            store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision
        };
        let request = DeliveryRequest {
            request_key: "input-delivery".into(),
            expected_revision: revision(),
            expected_instance_id: "native-generation".into(),
            text: "Inspect the reference".into(),
            input_refs: vec![InputRef {
                input_id: receipt.input_id.clone(),
                caption: "Keep as reference".into(),
                use_: InputUse::ReferenceOnly,
            }],
        };
        let delivered = service
            .deliver("user", &task.id, &aid, request.clone())
            .await
            .unwrap();
        assert_eq!(delivered.value.input_refs[0].sha256, hash);
        assert!(port.prompts.lock().unwrap()[0].contains(root.to_str().unwrap()));
        std::fs::remove_file(root.join(&hash)).unwrap();
        assert!(
            service
                .deliver("user", &task.id, &aid, request)
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(port.prompts.lock().unwrap().len(), 1);
        service
            .deliver(
                "user",
                &task.id,
                &aid,
                DeliveryRequest {
                    request_key: "independent-followup".into(),
                    expected_revision: revision(),
                    expected_instance_id: "native-generation".into(),
                    text: "Only this instruction".into(),
                    input_refs: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(port.prompts.lock().unwrap()[1], "Only this instruction");
        let failed = service
            .deliver(
                "user",
                &task.id,
                &aid,
                DeliveryRequest {
                    request_key: "missing-reference".into(),
                    expected_revision: revision(),
                    expected_instance_id: "native-generation".into(),
                    text: "Try to read".into(),
                    input_refs: vec![InputRef {
                        input_id: receipt.input_id,
                        caption: String::new(),
                        use_: InputUse::ReferenceOnly,
                    }],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(failed.0, FailureCode::ArtifactMissing);
        assert_eq!(port.prompts.lock().unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn blocked_preflight_cannot_claim_inputs_or_record_a_delivery() {
        let (store, task, start) = fixture();
        let mut port = port();
        port.preflight_refused = true;
        let receipt = store
            .lock()
            .unwrap()
            .commit_input_upload(
                "user",
                "session",
                "upload",
                InputUpload {
                    repo_path: task.repo_path.clone(),
                    name: "note.txt".into(),
                    mime: "text/plain; charset=utf-8".into(),
                    size_bytes: 1,
                    sha256: "a".repeat(64),
                },
                now(),
            )
            .unwrap();
        let refs = vec![InputRef {
            input_id: receipt.input_id,
            caption: String::new(),
            use_: InputUse::ReferenceOnly,
        }];
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let started = service.start("user", &task.id, start).await.unwrap().value;
        let revision = store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .task
            .revision;
        let result = service
            .deliver(
                "user",
                &task.id,
                started.attempt_id.as_deref().unwrap(),
                DeliveryRequest {
                    request_key: "blocked".into(),
                    expected_revision: revision,
                    expected_instance_id: "native-generation".into(),
                    text: "Use reference".into(),
                    input_refs: refs.clone(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(result.0, FailureCode::ApprovalRequired);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision,
            revision
        );
        assert!(store
            .lock()
            .unwrap()
            .resolve_inputs("user", "session", &task.repo_path, None, &refs, now())
            .is_ok());
        assert!(port.prompts.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn revocation_during_native_preflight_fences_admission_without_affecting_unchanged_actor()
    {
        for change in ["revoked", "transport", "unchanged"] {
            let (store, task, start) = fixture();
            let mut state = crate::tests::test_state(
                "admin",
                vec![crate::tests::test_device("user", "device")],
            );
            state.work = Some(store.clone());
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("authorization", "Bearer device".parse().unwrap());
            let mut port = port();
            port.commit_authority = Some((state.clone(), headers, "user".into()));
            let entered = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            port.preflight_gate = Some((entered.clone(), resume.clone()));
            let service = Execution {
                store: store.clone(),
                session: "session".into(),
                port: &port,
            };
            let started = service.start("user", &task.id, start).await.unwrap().value;
            let revision = store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision;
            let sending = service.deliver(
                "user",
                &task.id,
                started.attempt_id.as_deref().unwrap(),
                DeliveryRequest {
                    request_key: "after-preflight".into(),
                    expected_revision: revision,
                    expected_instance_id: "native-generation".into(),
                    text: "Only if authorized".into(),
                    input_refs: vec![],
                },
            );
            tokio::pin!(sending);
            tokio::select! { _=&mut sending=>panic!("preflight must pause"), _=entered.notified()=>{} }
            if change == "revoked" {
                state.devices.lock().unwrap().clear();
            } else if change == "transport" {
                state.devices.lock().unwrap()[0].transport_key = Some("new-transport-key".into());
            }
            resume.notify_one();
            let result = sending.await;
            if change != "unchanged" {
                assert_eq!(result.unwrap_err().0, FailureCode::ScopeMismatch);
                assert_eq!(
                    store
                        .lock()
                        .unwrap()
                        .detail("session", &task.id)
                        .unwrap()
                        .task
                        .revision,
                    revision
                );
                assert!(port.prompts.lock().unwrap().is_empty());
            } else {
                assert_eq!(result.unwrap().value.state, OperationState::Acknowledged);
                assert_eq!(port.prompts.lock().unwrap().len(), 1);
            }
        }
    }
    #[tokio::test]
    async fn release_during_placement_keeps_resources_and_stops_suspended_start() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.release_after_place = Some((store.clone(), task.id.clone()));
        let execution = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let result = execution.start("user", &task.id, request).await.unwrap();
        assert_eq!(result.value.state, OperationState::Refused);
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
        let detail = store.lock().unwrap().detail("session", &task.id).unwrap();
        assert_eq!(
            detail.attempts[0].lifecycle.reservation,
            Reservation::Released
        );
        assert!(detail.attempts[0].binding.workspace_id.is_some());
        assert!(detail.attempts[0].binding.instance_id.is_none());
    }
    #[tokio::test]
    async fn missing_reporting_refuses_before_native_effects_and_replays_receipt() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.reporting_available = false;
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let result = service
            .start("user", &task.id, request.clone())
            .await
            .unwrap();
        assert_eq!(result.value.state, OperationState::Refused);
        assert_eq!(
            result.value.failure_code,
            Some(FailureCode::CapabilityUnavailable)
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        let detail = store.lock().unwrap().detail("session", &task.id).unwrap();
        assert_eq!(
            detail.attempts[0].lifecycle.reservation,
            Reservation::Released
        );
        assert!(
            service
                .start("user", &task.id, request)
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn no_process_refusal_preserves_profile_reason_and_never_retries() {
        for (code, expected) in [
            ("unsupported_agent_kind", FailureCode::CapabilityUnavailable),
            ("agent_not_ready", FailureCode::NotReady),
            ("resource_limit", FailureCode::ResourceLimit),
        ] {
            let (store, task, request) = fixture();
            let mut port = port();
            port.refuse_start = Some(code);
            let service = Execution {
                store: store.clone(),
                session: "session".into(),
                port: &port,
            };
            let result = service
                .start("user", &task.id, request.clone())
                .await
                .unwrap();
            assert_eq!(result.value.state, OperationState::Refused);
            assert_eq!(result.value.failure_code, Some(expected.clone()));
            assert!(result.value.resources.instance_id.is_none());
            let calls = port.calls.load(Ordering::SeqCst);
            assert_eq!(calls, 2); // One workspace creation, one bound start; no prompt.
            let replay = service.start("user", &task.id, request).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.value.failure_code, Some(expected));
            assert_eq!(port.calls.load(Ordering::SeqCst), calls);
            assert!(port.prompts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn unsupported_backend_creates_no_records_or_native_resources() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.supported = false;
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        assert_eq!(
            service
                .start("user", &task.id, request)
                .await
                .unwrap_err()
                .0,
            FailureCode::CapabilityUnavailable
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        assert!(store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .attempts
            .is_empty());
    }
    #[tokio::test]
    async fn lost_start_ack_preserves_workspace_and_duplicate_never_starts_again() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.fail_start = true;
        let service = Execution {
            store,
            session: "session".into(),
            port: &port,
        };
        let result = service
            .start("user", &task.id, request.clone())
            .await
            .unwrap();
        assert_eq!(result.value.state, OperationState::Unconfirmed);
        assert_eq!(
            result.value.resources.workspace_id.as_deref(),
            Some("workspace")
        );
        let replay = service.start("user", &task.id, request).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.value.id, result.value.id);
        assert_eq!(port.calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn pause_after_workspace_prevents_agent_start_and_keeps_resource() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.pause_after_place = Some((store.clone(), task.id.clone()));
        let service = Execution {
            store,
            session: "session".into(),
            port: &port,
        };
        let result = service.start("user", &task.id, request).await.unwrap();
        assert_eq!(result.value.state, OperationState::Refused);
        assert_eq!(
            result.value.resources.workspace_id.as_deref(),
            Some("workspace")
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn ambiguous_prompt_is_recorded_once_and_second_key_cannot_overlap() {
        let (store, task, request) = fixture();
        let mut port = port();
        port.fail_prompt = true;
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let launch = service
            .start("user", &task.id, request)
            .await
            .unwrap()
            .value;
        let aid = launch.attempt_id.unwrap();
        let revision = store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .task
            .revision;
        let request = DeliveryRequest {
            input_refs: vec![],
            request_key: "send".into(),
            expected_revision: revision,
            expected_instance_id: "native-generation".into(),
            text: "Hello".into(),
        };
        let result = service
            .deliver("user", &task.id, &aid, request.clone())
            .await
            .unwrap();
        assert_eq!(result.value.state, OperationState::Unconfirmed);
        assert!(
            service
                .deliver("user", &task.id, &aid, request.clone())
                .await
                .unwrap()
                .replayed
        );
        let mut next = request;
        next.request_key = "unsafe-retry".into();
        next.expected_revision = store
            .lock()
            .unwrap()
            .detail("session", &task.id)
            .unwrap()
            .task
            .revision;
        assert_eq!(
            service
                .deliver("user", &task.id, &aid, next)
                .await
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 3);
    }
    #[tokio::test]
    async fn onboarding_is_first_only_replayed_once_and_does_not_change_task_brief() {
        let (store, task, start) = fixture();
        let port = port();
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let launch = service.start("user", &task.id, start).await.unwrap().value;
        let attempt = launch.attempt_id.unwrap();
        let revision = || {
            store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision
        };
        let first = DeliveryRequest {
            input_refs: vec![],
            request_key: "initial".into(),
            expected_revision: revision(),
            expected_instance_id: "native-generation".into(),
            text: "  My exact initial request\n".into(),
        };
        let sent = service
            .deliver("user", &task.id, &attempt, first.clone())
            .await
            .unwrap();
        assert_eq!(
            sent.value.bootstrap_version.as_deref(),
            Some(crate::work_prompt::INSTRUCTIONS_VERSION)
        );
        assert!(
            service
                .deliver("user", &task.id, &attempt, first.clone())
                .await
                .unwrap()
                .replayed
        );
        let followup = "  My exact follow-up\n";
        service
            .deliver(
                "user",
                &task.id,
                &attempt,
                DeliveryRequest {
                    input_refs: vec![],
                    request_key: "followup".into(),
                    expected_revision: revision(),
                    expected_instance_id: "native-generation".into(),
                    text: followup.into(),
                },
            )
            .await
            .unwrap();
        let prompts = port.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].starts_with(&first.text));
        assert!(prompts[0].contains("work submit-result"));
        assert_eq!(prompts[1], followup);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .brief,
            task.brief
        );
    }

    #[tokio::test]
    async fn oversized_onboarding_is_refused_before_input_and_releases_preparation() {
        let (store, task, start) = fixture();
        let port = port();
        let service = Execution {
            store: store.clone(),
            session: "session".into(),
            port: &port,
        };
        let launch = service.start("user", &task.id, start).await.unwrap().value;
        let attempt = launch.attempt_id.unwrap();
        let revision = || {
            store
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap()
                .task
                .revision
        };
        let prior_revision = revision();
        let refused = service
            .deliver(
                "user",
                &task.id,
                &attempt,
                DeliveryRequest {
                    input_refs: vec![],
                    request_key: "large".into(),
                    expected_revision: revision(),
                    expected_instance_id: "native-generation".into(),
                    text: "x".repeat(65536),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(refused.0, FailureCode::InvalidInput);
        assert_eq!(revision(), prior_revision);
        assert!(port.prompts.lock().unwrap().is_empty());
        let next = service
            .deliver(
                "user",
                &task.id,
                &attempt,
                DeliveryRequest {
                    input_refs: vec![],
                    request_key: "shortened".into(),
                    expected_revision: revision(),
                    expected_instance_id: "native-generation".into(),
                    text: "Short request".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(next.value.state, OperationState::Acknowledged);
        assert_eq!(
            next.value.bootstrap_version.as_deref(),
            Some(crate::work_prompt::INSTRUCTIONS_VERSION)
        );
        assert_eq!(port.prompts.lock().unwrap().len(), 1);
    }
}
