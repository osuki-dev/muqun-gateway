//! Private Unix adapter for scoped reporting and configured lead delegation.
//! Native effects use the shared guarded execution service; human review is unavailable.
use crate::work::model::*;
use crate::work_authority::{ContextFile, Registry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub(super) struct LocalState {
    pub registry: Arc<Mutex<Registry>>,
    pub directory: PathBuf,
    pub socket: PathBuf,
}
#[derive(clap::Subcommand)]
pub(super) enum LocalCommand {
    ReportingMcp,
    Context,
    Receipt { operation_id: String },
    SubmitResult,
    CreateChild,
    StartChild,
    DeliverChild,
    SetChildDependencies,
    Child,
    ChildReceipt,
    ChildOperation,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Delegate {
        action: DelegationAction,
    },
    Context,
    ResultReceipt {
        request_key: String,
    },
    Receipt {
        operation_id: String,
    },
    SubmitResult {
        request_key: String,
        expected_revision: u64,
        input: ResultInput,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum DelegationAction {
    SetChildDependencies {
        task_id: String,
        request_key: String,
        expected_revision: u64,
        dependencies: Vec<TaskDependency>,
    },
    CreateChild {
        input: crate::work_delegation::CreateChildRequest,
    },
    StartChild {
        task_id: String,
        input: crate::work_execution::StartRequest,
    },
    DeliverChild {
        task_id: String,
        attempt_id: String,
        input: crate::work_execution::DeliveryRequest,
    },
    Child {
        #[serde(flatten)]
        input: crate::work_delegation::ChildReadRequest,
    },
    ChildReceipt {
        kind: OperationKind,
        request_key: String,
    },
    ChildOperation {
        operation_id: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    action: Action,
}
const MAX_MESSAGE: u64 = 256 * 1024;
const MAX_RESPONSE: u64 = 3 * 1024 * 1024;
fn unavailable() -> WorkError {
    WorkError(FailureCode::StorageUnavailable)
}
fn now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

#[cfg(unix)]
pub(super) fn prepare(root: &Path) -> WorkResult<(LocalState, tokio::net::UnixListener)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    std::fs::create_dir_all(root).map_err(|_| unavailable())?;
    let meta = std::fs::symlink_metadata(root).map_err(|_| unavailable())?;
    if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(unavailable());
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| unavailable())?;
    // Only remove our fixed stale socket and UUID-named, owned regular context files.
    let socket = root.join("local.sock");
    if let Ok(meta) = std::fs::symlink_metadata(&socket) {
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(unavailable());
        }
        std::fs::remove_file(&socket).map_err(|_| unavailable())?;
    }
    for entry in std::fs::read_dir(root)
        .map_err(|_| unavailable())?
        .take(1024)
    {
        let entry = entry.map_err(|_| unavailable())?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json")
            || path
                .file_stem()
                .and_then(|v| v.to_str())
                .is_none_or(|v| uuid::Uuid::parse_str(v).is_err())
        {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        if meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.permissions().mode() & 0o777 == 0o600
        {
            std::fs::remove_file(path).map_err(|_| unavailable())?;
        }
    }
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|_| unavailable())?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| unavailable())?;
    Ok((
        LocalState {
            registry: Arc::new(Mutex::new(Registry::default())),
            directory: root.into(),
            socket,
        },
        listener,
    ))
}
#[cfg(unix)]
pub(super) fn serve(state: crate::AppState, listener: tokio::net::UnixListener) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    tokio::spawn(async move {
        let permits = Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            let Ok(permit) = permits.clone().acquire_owned().await else {
                break;
            };
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if !stream
                    .peer_cred()
                    .is_ok_and(|p| p.uid() == unsafe { libc::geteuid() })
                {
                    return;
                }
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read).take(MAX_MESSAGE + 1);
                let mut bytes = Vec::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    read.read_until(b'\n', &mut bytes),
                )
                .await;
                if !matches!(read_result, Ok(Ok(_)))
                    || bytes.len() as u64 > MAX_MESSAGE
                    || bytes.last() != Some(&b'\n')
                {
                    return;
                }
                let result = match serde_json::from_slice::<Request>(&bytes) {
                    Ok(request) => handle(&state, request).await,
                    Err(_) => Err(WorkError(FailureCode::InvalidInput)),
                };
                let response = match result {
                    Ok(value) => json!({"result":value}),
                    Err(error) => json!({"error":{"code":error.0}}),
                };
                if let Ok(mut bytes) = serde_json::to_vec(&response) {
                    if bytes.len() as u64 > MAX_RESPONSE {
                        bytes = serde_json::to_vec(&json!({"error":{"code":"resource_limit"}}))
                            .unwrap_or_default();
                    }
                    bytes.push(b'\n');
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        write.write_all(&bytes),
                    )
                    .await;
                }
            });
        }
    });
}
async fn handle(state: &crate::AppState, request: Request) -> WorkResult<Value> {
    let local = state.work_local.as_ref().ok_or_else(unavailable)?;
    let scope = local
        .registry
        .lock()
        .map_err(|_| unavailable())?
        .identify(&request.token, now())?;
    match request.action {
        Action::Delegate { action } => handle_delegation(state, request.token, action).await,
        Action::SubmitResult {
            request_key,
            expected_revision,
            input,
        } => {
            if input.attempt_id != scope.attempt_id {
                return Err(WorkError(FailureCode::ScopeMismatch));
            }
            let actor = crate::work_results::Actor {
                id: format!("attempt:{}", scope.attempt_id),
                local: Some((local.registry.clone(), request.token, scope.clone())),
            };
            Ok(serde_json::to_value(
                crate::work_results::submit(
                    state,
                    actor,
                    scope.session_id,
                    scope.task_id,
                    request_key,
                    expected_revision,
                    input,
                )
                .await?,
            )?)
        }
        action => {
            let store = state.work.clone().ok_or_else(unavailable)?;
            let registry = local.registry.clone();
            tokio::task::spawn_blocking(move || {
                let registry = registry.lock().map_err(|_| unavailable())?;
                registry.authorize(&request.token, &scope, now())?;
                let mut store = store.lock().map_err(|_| unavailable())?;
                match action {
                    Action::Context => {
                        let detail = store.detail(&scope.session_id, &scope.task_id)?;
                        let attempt = detail
                            .attempts
                            .into_iter()
                            .find(|a| a.id == scope.attempt_id)
                            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
                        Ok(json!({"task":detail.task,"attempt":attempt,"cursor":detail.cursor}))
                    }
                    Action::ResultReceipt { request_key } => {
                        let receipt = store.get_receipt(
                            &format!("attempt:{}", scope.attempt_id),
                            &scope.session_id,
                            OperationKind::SubmitResult,
                            &request_key,
                        )?;
                        let result: ResultSubmission =
                            serde_json::from_value(receipt.value.clone())?;
                        if result.task_id != scope.task_id
                            || result.result.attempt_id != scope.attempt_id
                        {
                            return Err(WorkError(FailureCode::ScopeMismatch));
                        }
                        Ok(receipt.value)
                    }
                    Action::Receipt { operation_id } => {
                        let op = store.get_operation(&scope.session_id, &operation_id)?;
                        if op.task_id != scope.task_id
                            || op.attempt_id.as_deref() != Some(&scope.attempt_id)
                        {
                            return Err(WorkError(FailureCode::ScopeMismatch));
                        }
                        Ok(serde_json::to_value(op)?)
                    }
                    Action::SubmitResult { .. } | Action::Delegate { .. } => {
                        Err(WorkError(FailureCode::InvalidInput))
                    }
                }
            })
            .await
            .map_err(|_| unavailable())?
        }
    }
}
#[cfg(unix)]
async fn handle_delegation(
    state: &crate::AppState,
    token: String,
    action: DelegationAction,
) -> WorkResult<Value> {
    let authority = crate::work_actor::WorkActor::local_lead(state.clone(), token)?;
    let scope = authority.local_scope()?;
    let store = state.work.clone().ok_or_else(unavailable)?;
    let session = crate::find_session(&state.config, &scope.session_id)
        .map_err(|_| WorkError(FailureCode::ScopeMismatch))?;
    let port = crate::work_execution::SessionPort {
        state,
        session,
        authority: authority.clone(),
    };
    let execution = crate::work_execution::Execution {
        store: store.clone(),
        session: scope.session_id.clone(),
        port: &port,
    };
    match action {
        DelegationAction::SetChildDependencies {
            task_id,
            request_key,
            expected_revision,
            dependencies,
        } => Ok(serde_json::to_value(
            authority
                .set_child_dependencies(
                    store,
                    task_id,
                    crate::work_delegation::DependencyUpdateRequest {
                        request_key,
                        expected_revision,
                        dependencies,
                    },
                )
                .await?,
        )?),
        DelegationAction::CreateChild { input } => Ok(serde_json::to_value(
            authority.create_child(store, input).await?,
        )?),
        DelegationAction::StartChild { task_id, input } => Ok(serde_json::to_value(
            execution.start(authority.id(), &task_id, input).await?,
        )?),
        DelegationAction::DeliverChild {
            task_id,
            attempt_id,
            input,
        } => Ok(serde_json::to_value(
            execution
                .deliver(authority.id(), &task_id, &attempt_id, input)
                .await?,
        )?),
        DelegationAction::Child { input } => {
            authority.read_child(store, scope.session_id, input).await
        }
        DelegationAction::ChildReceipt { kind, request_key } => Ok(serde_json::to_value(
            authority
                .child_receipt(store, scope.session_id, kind, request_key)
                .await?,
        )?),
        DelegationAction::ChildOperation { operation_id } => Ok(serde_json::to_value(
            authority
                .child_operation(store, scope.session_id, operation_id)
                .await?,
        )?),
    }
}
#[cfg(unix)]
pub(super) async fn client(command: LocalCommand) -> anyhow::Result<()> {
    use std::io::Read;
    if matches!(command, LocalCommand::ReportingMcp) {
        return crate::work_reporting_mcp::run().await;
    }
    let action = match command {
        LocalCommand::ReportingMcp => unreachable!(),
        LocalCommand::CreateChild
        | LocalCommand::StartChild
        | LocalCommand::DeliverChild
        | LocalCommand::SetChildDependencies
        | LocalCommand::Child
        | LocalCommand::ChildReceipt
        | LocalCommand::ChildOperation => {
            let name = match command {
                LocalCommand::CreateChild => "create_child",
                LocalCommand::StartChild => "start_child",
                LocalCommand::DeliverChild => "deliver_child",
                LocalCommand::SetChildDependencies => "set_child_dependencies",
                LocalCommand::Child => "child",
                LocalCommand::ChildReceipt => "child_receipt",
                LocalCommand::ChildOperation => "child_operation",
                _ => unreachable!(),
            };
            let mut bytes = Vec::new();
            std::io::stdin()
                .take(MAX_MESSAGE + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_MESSAGE,
                "Delegation input exceeds the size limit."
            );
            let mut value: Value = serde_json::from_slice(&bytes)?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("Expected a JSON object."))?;
            anyhow::ensure!(
                !object.contains_key("command"),
                "Command is selected by the CLI."
            );
            object.insert("command".into(), Value::String(name.into()));
            Action::Delegate {
                action: serde_json::from_value(value)?,
            }
        }
        LocalCommand::Context => Action::Context,
        LocalCommand::Receipt { operation_id } => Action::Receipt { operation_id },
        LocalCommand::SubmitResult => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                request_key: String,
                expected_revision: u64,
                input: ResultInput,
            }
            let mut bytes = Vec::new();
            std::io::stdin()
                .take(MAX_MESSAGE + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_MESSAGE,
                "Result input exceeds the size limit."
            );
            let input: Input = serde_json::from_slice(&bytes)?;
            Action::SubmitResult {
                request_key: input.request_key,
                expected_revision: input.expected_revision,
                input: input.input,
            }
        }
    };
    let value = exchange(action).await?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    if value.get("error").is_some() {
        anyhow::bail!("The scoped task request was refused.");
    }
    Ok(())
}
#[cfg(unix)]
async fn exchange(action: Action) -> anyhow::Result<Value> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let path = std::env::var_os("MUQUN_WORK_CONTEXT_FILE")
        .ok_or_else(|| anyhow::anyhow!("No scoped task context is available."))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.permissions().mode() & 0o777 == 0o600
            && meta.len() <= 16384,
        "Invalid task context file."
    );
    let context: ContextFile = serde_json::from_reader(file)?;
    let stream = tokio::net::UnixStream::connect(context.socket).await?;
    anyhow::ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "Unexpected local service owner."
    );
    let (read, mut write) = stream.into_split();
    let mut request = serde_json::to_vec(&Request {
        token: context.token,
        action,
    })?;
    request.push(b'\n');
    anyhow::ensure!(
        request.len() as u64 <= MAX_MESSAGE,
        "Local request exceeds the size limit."
    );
    write.write_all(&request).await?;
    let mut read = BufReader::new(read).take(MAX_RESPONSE + 1);
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        read.read_until(b'\n', &mut response),
    )
    .await??;
    anyhow::ensure!(
        response.len() as u64 <= MAX_RESPONSE,
        "Local response exceeds the size limit."
    );
    let value: Value = serde_json::from_slice(&response)?;
    Ok(value)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportingSubmission {
    request_key: String,
    expected_revision: u64,
    input: ResultInput,
}
/// Closed adapter: tool arguments can never select delegation or another local action.
pub(super) async fn reporting_request(name: &str, arguments: Value) -> anyhow::Result<Value> {
    let action = match name {
        "context" => {
            anyhow::ensure!(
                arguments
                    .as_object()
                    .is_some_and(|object| object.is_empty()),
                "Invalid context arguments"
            );
            Action::Context
        }
        "submit_result" => {
            let input: ReportingSubmission = serde_json::from_value(arguments)?;
            Action::SubmitResult {
                request_key: input.request_key,
                expected_revision: input.expected_revision,
                input: input.input,
            }
        }
        "result_receipt" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ReceiptInput {
                request_key: String,
            }
            let input: ReceiptInput = serde_json::from_value(arguments)?;
            Action::ResultReceipt {
                request_key: input.request_key,
            }
        }
        _ => anyhow::bail!("Unknown reporting tool"),
    };
    #[cfg(unix)]
    {
        tokio::time::timeout(std::time::Duration::from_secs(30), exchange(action)).await?
    }
    #[cfg(not(unix))]
    {
        let _ = action;
        anyhow::bail!("Local task authority requires Unix.")
    }
}
#[cfg(not(unix))]
pub(super) async fn client(_command: LocalCommand) -> anyhow::Result<()> {
    anyhow::bail!("Local task authority requires Unix.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_authority::Scope;
    fn fixture() -> (crate::AppState, PathBuf, ContextFile, Operation) {
        let mut state = crate::tests::test_state("admin", Vec::new());
        let directory = std::env::temp_dir().join(format!("muqun-local-{}", uuid::Uuid::new_v4()));
        let (local, _listener) = prepare(&directory).unwrap();
        let mut store = crate::work::store::WorkStore::in_memory().unwrap();
        let task = store
            .create_task(
                "human",
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
        let op = store
            .prepare_attempt(
                "human",
                "session",
                &task.id,
                "start",
                1,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "digest",
                2,
            )
            .unwrap()
            .value;
        let scope = Scope {
            session_id: "session".into(),
            task_id: task.id,
            attempt_id: op.attempt_id.clone().unwrap(),
        };
        let path = local
            .registry
            .lock()
            .unwrap()
            .issue(&directory, &local.socket, scope, now())
            .unwrap();
        let context = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        state.work = Some(Arc::new(Mutex::new(store)));
        state.work_local = Some(local);
        (state, directory, context, op)
    }
    #[tokio::test]
    async fn configured_lead_reads_only_children_and_stale_binding_cannot_commit() {
        let (state, dir, context, op) = fixture();
        let (fence, child) = {
            let mut registry = state.work_local.as_ref().unwrap().registry.lock().unwrap();
            let mut store = state.work.as_ref().unwrap().lock().unwrap();
            store.begin_operation("session", &op.id, now()).unwrap();
            store
                .claim_start_dispatch("session", &op.id, now())
                .unwrap();
            store
                .confirm_start_launch(
                    "session",
                    &op.id,
                    NativeBinding {
                        instance_id: Some("controller".into()),
                        ..Default::default()
                    },
                    "owner",
                    now(),
                )
                .unwrap();
            store
                .finalize_operation(
                    "session",
                    &op.id,
                    OperationOutcome {
                        state: OperationState::Acknowledged,
                        resources: Default::default(),
                        failure_code: None,
                    },
                    now(),
                )
                .unwrap();
            let task = store
                .detail("session", &context.scope.task_id)
                .unwrap()
                .task;
            let configured = store
                .configure_delegation(
                    "human",
                    "session",
                    &task.id,
                    "enable",
                    task.revision,
                    DelegationConfig {
                        policy: DelegationPolicy {
                            enabled: true,
                            ..Default::default()
                        },
                        coordinator_attempt_id: Some(context.scope.attempt_id.clone()),
                    },
                    now(),
                )
                .unwrap()
                .value;
            let fence = DelegationFence {
                coordinator_task_id: task.id.clone(),
                coordinator_attempt_id: context.scope.attempt_id.clone(),
                coordinator_epoch: configured.delegation.coordinator_epoch,
                instance_id: "controller".into(),
                native_owner_epoch: "owner".into(),
            };
            let permit = registry.prepare_activation(&context.scope, now()).unwrap();
            registry.apply_activation(permit, fence.clone());
            let child = store
                .create_delegated_task(
                    &format!(
                        "delegation:{}:{}",
                        context.scope.attempt_id, fence.coordinator_epoch
                    ),
                    "session",
                    "child",
                    &fence,
                    configured.revision,
                    CreateTask {
                        repo_path: task.repo_path,
                        title: "Child".into(),
                        brief: "Child brief".into(),
                        parent_task_id: Some(task.id),
                        policy: task.policy,
                    },
                    vec![],
                    now(),
                )
                .unwrap()
                .value;
            (fence, child)
        };
        let actor =
            crate::work_actor::WorkActor::local_lead(state.clone(), context.token.clone()).unwrap();
        let store = state.work.clone().unwrap();
        assert_eq!(
            actor
                .detail(store.clone(), "session".into(), child.id.clone())
                .await
                .unwrap()
                .task
                .id,
            child.id
        );
        assert_eq!(
            actor
                .detail(
                    store.clone(),
                    "session".into(),
                    context.scope.task_id.clone()
                )
                .await
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        // Even a valid configured local lead cannot run a generic store closure.
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        assert_eq!(
            actor
                .commit(store.clone(), move |_| {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        let receipt = actor
            .child_receipt(
                store.clone(),
                "session".into(),
                OperationKind::CreateTask,
                "child".into(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.value["id"], child.id);
        // A persisted control generation change fences a captured actor even if its token lives.
        {
            let mut store = store.lock().unwrap();
            let task = store
                .detail("session", &fence.coordinator_task_id)
                .unwrap()
                .task;
            store
                .configure_delegation(
                    "human",
                    "session",
                    &task.id,
                    "disable",
                    task.revision,
                    DelegationConfig {
                        policy: DelegationPolicy::default(),
                        coordinator_attempt_id: None,
                    },
                    now(),
                )
                .unwrap();
        }
        assert_eq!(
            actor
                .detail(store, "session".into(), child.id)
                .await
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert!(state
            .work_local
            .as_ref()
            .unwrap()
            .registry
            .lock()
            .unwrap()
            .authorize(&context.token, &context.scope, now())
            .is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn reporting_token_cannot_delegate_and_cli_commands_are_explicit() {
        use clap::Parser;
        for command in [
            "reporting-mcp",
            "create-child",
            "start-child",
            "deliver-child",
            "set-child-dependencies",
            "child",
            "child-receipt",
            "child-operation",
        ] {
            assert!(crate::Cli::try_parse_from(["gateway", "work", command]).is_ok());
        }
        let pin = json!({"command":"set_child_dependencies","task_id":"child","request_key":"pin","expected_revision":3,"dependencies":[{"prerequisite_task_id":"producer","submission_id":"result"}]});
        assert!(serde_json::from_value::<DelegationAction>(pin.clone()).is_ok());
        let mut forged = pin.clone();
        forged["fence"] = json!({});
        assert!(serde_json::from_value::<DelegationAction>(forged).is_err());
        let mut missing = pin;
        missing.as_object_mut().unwrap().remove("dependencies");
        assert!(serde_json::from_value::<DelegationAction>(missing).is_err());
        let (state, dir, context, _) = fixture();
        assert!(
            crate::work_actor::WorkActor::local_lead(state.clone(), context.token.clone()).is_err()
        );
        let result = handle(
            &state,
            Request {
                token: context.token,
                action: Action::Delegate {
                    action: DelegationAction::Child {
                        input: crate::work_delegation::ChildReadRequest {
                            task_id: context.scope.task_id,
                            kind: None,
                            snapshot_revision: None,
                            after_id: None,
                            record_id: None,
                        },
                    },
                },
            },
        )
        .await;
        assert_eq!(result.unwrap_err().0, FailureCode::ScopeMismatch);
        assert!(serde_json::from_value::<DelegationAction>(
            json!({"command":"accept_result","task_id":"x"})
        )
        .is_err());
        assert!(serde_json::from_value::<DelegationAction>(
            json!({"command":"child","task_id":"x","actor":"forged"})
        )
        .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn local_actor_reads_own_receipt_submits_result_and_cannot_accept() {
        let (state, dir, context, op) = fixture();
        let value = handle(
            &state,
            Request {
                token: context.token.clone(),
                action: Action::Receipt {
                    operation_id: op.id.clone(),
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(value["id"], op.id);
        let other_op = {
            let mut store = state.work.as_ref().unwrap().lock().unwrap();
            let mut input = store
                .detail("session", &context.scope.task_id)
                .unwrap()
                .task;
            input.title = "Other task".into();
            let task = store
                .create_task(
                    "human",
                    "session",
                    "other-task",
                    CreateTask {
                        repo_path: input.repo_path,
                        title: input.title,
                        brief: input.brief,
                        parent_task_id: None,
                        policy: input.policy,
                    },
                    1,
                )
                .unwrap()
                .value;
            store
                .prepare_attempt(
                    "human",
                    "session",
                    &task.id,
                    "other-start",
                    1,
                    NewAttempt {
                        agent_kind: "codex".into(),
                        role: AttemptRole::Lead,
                    },
                    "digest",
                    2,
                )
                .unwrap()
                .value
        };
        assert_eq!(
            handle(
                &state,
                Request {
                    token: context.token.clone(),
                    action: Action::Receipt {
                        operation_id: other_op.id.clone()
                    }
                }
            )
            .await
            .unwrap_err()
            .0,
            FailureCode::ScopeMismatch
        );

        let details = handle(
            &state,
            Request {
                token: context.token.clone(),
                action: Action::Context,
            },
        )
        .await
        .unwrap();
        let revision = details["task"]["revision"].as_u64().unwrap();
        let input = ResultInput {
            attempt_id: context.scope.attempt_id.clone(),
            summary: "Ready for human review".into(),
            artifacts: vec![],
            evidence: vec!["Agent reported".into()],
        };
        let result = handle(
            &state,
            Request {
                token: context.token.clone(),
                action: Action::SubmitResult {
                    request_key: "result".into(),
                    expected_revision: revision,
                    input,
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(result["value"]["attempt_id"], context.scope.attempt_id);
        let recovered = handle(
            &state,
            Request {
                token: context.token.clone(),
                action: Action::ResultReceipt {
                    request_key: "result".into(),
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(recovered, result["value"]);
        assert_eq!(
            handle(
                &state,
                Request {
                    token: context.token.clone(),
                    action: Action::ResultReceipt {
                        request_key: "missing".into()
                    }
                }
            )
            .await
            .unwrap_err()
            .0,
            FailureCode::NotFound
        );
        let foreign_scope = Scope {
            session_id: "session".into(),
            task_id: other_op.task_id.clone(),
            attempt_id: other_op.attempt_id.clone().unwrap(),
        };
        let foreign_file = {
            let local = state.work_local.as_ref().unwrap();
            local
                .registry
                .lock()
                .unwrap()
                .issue(&local.directory, &local.socket, foreign_scope, now())
                .unwrap()
        };
        let foreign_context: ContextFile =
            serde_json::from_slice(&std::fs::read(foreign_file).unwrap()).unwrap();
        assert_eq!(
            handle(
                &state,
                Request {
                    token: foreign_context.token,
                    action: Action::ResultReceipt {
                        request_key: "result".into()
                    }
                }
            )
            .await
            .unwrap_err()
            .0,
            FailureCode::NotFound
        );

        assert!(serde_json::from_value::<Request>(
            json!({"token":context.token,"action":{"command":"review_result","decision":"accepted"}})
        )
        .is_err());
        assert!(state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail("session", &context.scope.task_id)
            .unwrap()
            .reviews
            .is_empty());
        state
            .work_local
            .as_ref()
            .unwrap()
            .registry
            .lock()
            .unwrap()
            .revoke(&context.scope);
        assert!(handle(
            &state,
            Request {
                token: context.token,
                action: Action::Context
            }
        )
        .await
        .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn foreign_attempt_submission_is_refused_without_record_mutation() {
        let (state, dir, context, _) = fixture();
        let revision = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail("session", &context.scope.task_id)
            .unwrap()
            .task
            .revision;
        let result = handle(
            &state,
            Request {
                token: context.token,
                action: Action::SubmitResult {
                    request_key: "result".into(),
                    expected_revision: revision,
                    input: ResultInput {
                        attempt_id: "foreign".into(),
                        summary: "Wrong task".into(),
                        artifacts: vec![],
                        evidence: vec![],
                    },
                },
            },
        )
        .await;
        assert_eq!(result.unwrap_err().0, FailureCode::ScopeMismatch);
        let detail = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .detail("session", &context.scope.task_id)
            .unwrap();
        assert!(detail.results.is_empty());
        assert_eq!(detail.task.revision, revision);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn private_socket_round_trip_uses_scoped_token() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (mut state, dir, _, _) = fixture();
        let (local, listener) = prepare(&dir).unwrap();
        let scope = {
            let store = state.work.as_ref().unwrap().lock().unwrap();
            let task = store.list_tasks("session", None, 1).unwrap().remove(0);
            drop(store);
            let detail = state
                .work
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .detail("session", &task.id)
                .unwrap();
            Scope {
                session_id: "session".into(),
                task_id: task.id,
                attempt_id: detail.attempts[0].id.clone(),
            }
        };
        let path = local
            .registry
            .lock()
            .unwrap()
            .issue(&dir, &local.socket, scope, now())
            .unwrap();
        let context: ContextFile = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        state.work_local = Some(local);
        serve(state, listener);
        let mut stream = tokio::net::UnixStream::connect(&context.socket)
            .await
            .unwrap();
        let mut request = serde_json::to_vec(&Request {
            token: context.token,
            action: Action::Context,
        })
        .unwrap();
        request.push(b'\n');
        stream.write_all(&request).await.unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert!(value.get("result").is_some(), "{value}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
