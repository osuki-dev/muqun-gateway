//! Adopt the one shared Codex daemon, starting it when the socket is absent.
//! Never start a private app-server, restart or stop the daemon, log in, or
//! write global configuration.
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::adapters::codex::{CodexClient, CodexEvents};
use crate::agents::manager::AgentManager;
use crate::agents::ports::agent::AgentError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    #[serde(default = "super::default_true")]
    pub enabled: bool,
    /// Exact executable, otherwise normal PATH lookup. No flags are accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
        }
    }
}

impl CodexConfig {
    pub(crate) fn resolve_binary(&self) -> Result<PathBuf, AgentError> {
        if !self.enabled {
            return Err(AgentError::NotAvailable("Codex is disabled".into()));
        }
        let path = if let Some(binary) = &self.binary {
            let path = PathBuf::from(binary);
            if !path.is_absolute() {
                return Err(AgentError::InvalidRequest(
                    "codex.binary must name an absolute executable path".into(),
                ));
            }
            path
        } else {
            crate::agents::tasks::find_on_path("codex").ok_or_else(|| {
                AgentError::NotAvailable("Codex executable was not found on PATH".into())
            })?
        };
        if !path.is_file() {
            return Err(AgentError::NotAvailable(
                "configured Codex executable was not found".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path)
                .map(|m| m.permissions().mode() & 0o111 == 0)
                .unwrap_or(true)
            {
                return Err(AgentError::NotAvailable(
                    "configured Codex executable is not executable".into(),
                ));
            }
        }
        Ok(path)
    }
}

pub(super) struct CodexState {
    pub(super) config: CodexConfig,
    start: Mutex<()>,
    retry_at: Mutex<Option<tokio::time::Instant>>,
    failure: Mutex<Option<AgentError>>,
    cleanup_failure: Mutex<Option<AgentError>>,
    child: Mutex<Option<Arc<ChildExit>>>,
    shutdown: tokio_util::sync::CancellationToken,
}

/// Start the official shared daemon when its socket is absent.
/// `codex app-server daemon start` leaves an already-running daemon unchanged.
#[cfg(not(test))]
async fn ensure_shared_daemon(config: &CodexConfig) -> Result<(), AgentError> {
    let mut command = tokio::process::Command::new(config.resolve_binary()?);
    command
        .args(["app-server", "daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    let status = tokio::time::timeout(Duration::from_secs(60), command.status())
        .await
        .map_err(|_| AgentError::NotAvailable("Codex shared daemon start timed out".into()))?
        .map_err(|err| {
            AgentError::NotAvailable(format!("Codex shared daemon could not start: {err}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AgentError::NotAvailable(
            "Codex shared daemon did not start".into(),
        ))
    }
}

#[cfg(test)]
async fn ensure_shared_daemon(_config: &CodexConfig) -> Result<(), AgentError> {
    Ok(())
}

impl CodexState {
    pub(super) fn new(config: CodexConfig) -> Self {
        Self {
            config,
            start: Mutex::new(()),
            retry_at: Mutex::new(None),
            failure: Mutex::new(None),
            cleanup_failure: Mutex::new(None),
            child: Mutex::new(None),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub(super) async fn supervise(&self, runtime: &AgentRuntime) -> bool {
        if !self.config.enabled || self.shutdown.is_cancelled() {
            return false;
        }
        let _guard = self.start.lock().await;
        if self.shutdown.is_cancelled() {
            return false;
        }
        if runtime.keep_if_healthy("codex").await {
            return true;
        }
        if self
            .retry_at
            .lock()
            .await
            .is_some_and(|t| tokio::time::Instant::now() < t)
        {
            return false;
        }
        if let Some(child) = self.child.lock().await.take() {
            if let Err(err) = child.wait().await {
                self.cleanup_failed(err).await;
                return false;
            }
        }
        // Adopting the daemon owns only a connection. No ChildExit/process ledger
        // exists for it, so shutdown cannot signal or reap the user's daemon.
        // One start attempt if the socket is absent, then connect to that same socket.
        for attempt in 0..2 {
            if self.shutdown.is_cancelled() {
                return false;
            }
            let shared = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return false,
                result = crate::agents::adapters::codex::daemon::connect_existing() => result,
            };
            match shared {
                Ok(Some((client, events))) => {
                    if self.shutdown.is_cancelled() {
                        client.stop();
                        return false;
                    }
                    *self.failure.lock().await = None;
                    *self.retry_at.lock().await = None;
                    tracing::info!("adopted the running Codex daemon");
                    runtime
                        .install(
                            "codex",
                            AgentManager::connect_codex_recoverable(
                                client,
                                events,
                                runtime.events_tx.clone(),
                                runtime.terminal_runtime.get().cloned(),
                                runtime.native_codex_sessions.get().cloned(),
                            ),
                            AgentOrigin::Adopted,
                        )
                        .await;
                    return true;
                }
                Ok(None) if attempt == 0 => {
                    // Boot has no terminal yet. Start the official shared daemon
                    // once; a later Codex joins this socket instead of a second one.
                    tracing::info!("starting the Codex shared daemon");
                    if let Err(error) = ensure_shared_daemon(&self.config).await {
                        tracing::warn!(%error, "Codex shared daemon was not started");
                        self.failed(error).await;
                        return false;
                    }
                }
                Ok(None) => {
                    self.waiting_for_daemon().await;
                    return false;
                }
                Err(error) => {
                    tracing::warn!(%error, "Codex shared daemon was not adopted");
                    *self.failure.lock().await = Some(error);
                    *self.retry_at.lock().await = None;
                    return false;
                }
            }
        }
        self.waiting_for_daemon().await;
        false
    }

    /// Remember that Codex is absent without backing off. A later socket must
    /// be noticed on the next supervision pass, not after a spawn retry.
    async fn waiting_for_daemon(&self) {
        *self.failure.lock().await = Some(AgentError::NotAvailable(
            "Codex shared daemon is not running".into(),
        ));
        *self.retry_at.lock().await = None;
    }

    async fn failed(&self, err: AgentError) {
        // Generic errors only: never emit raw upstream stderr/auth diagnostics.
        tracing::debug!(%err,"Codex adapter unavailable; no login or prompt replay attempted");
        *self.failure.lock().await = Some(err);
        *self.retry_at.lock().await = Some(tokio::time::Instant::now() + Duration::from_secs(120));
    }

    async fn cleanup_failed(&self, err: AgentError) {
        // A later shutdown must not report success just because a failed
        // startup already consumed/reaped its child handle.
        *self.cleanup_failure.lock().await = Some(err.clone());
        self.failed(err).await;
    }

    async fn stop_failed_start(&self, client: &CodexClient, child: &ChildExit, err: AgentError) {
        client.stop();
        let failure = match child.wait().await {
            Ok(()) => err,
            Err(cleanup) => {
                self.cleanup_failed(cleanup.clone()).await;
                AgentError::NotAvailable(format!(
                    "Codex startup failed ({err}); shutdown also failed ({cleanup})"
                ))
            }
        };
        self.failed(failure).await;
    }

    pub(super) async fn discovery(
        &self,
        runtime: &AgentRuntime,
    ) -> crate::discovery::AgentDiscoveryInfo {
        use crate::discovery::{AgentAvailability, AgentDiscoveryInfo};
        let mut info = AgentDiscoveryInfo {
            id: "codex".into(),
            name: "Codex".into(),
            kind: "codex".into(),
            enabled: self.config.enabled,
            status: AgentAvailability::Disabled,
            endpoint: None,
            version: None,
            reason: None,
            models: vec![],
            modes: vec![],
            features: features(),
        };
        if !self.config.enabled {
            return info;
        }
        if let Some(manager) = runtime
            .manager_for_agent("codex")
            .await
            .filter(|m| m.stream_connected())
        {
            info.status = AgentAvailability::Connected;
            info.endpoint = Some(manager.endpoint_url().into());
            info.version = manager.version();
            (info.models, info.modes) = super::catalog_entry(manager.agent().as_ref()).await;
            info.features = connection_features(Some(manager.endpoint_url()), &info.models);
            // True only when skills/list actually answered. An unsupported
            // method or an error leaves the catalog skills empty and this flag
            // false. The static claim stays false until that read happens.
            info.features.skills = manager.agent().skills_listed();
            // True only when the downloaded Codex command catalog contributed
            // rows on that same catalog read. A missing file leaves `/` on the
            // app's own commands.
            info.features.slash_commands = manager.agent().slash_commands_listed();
        } else if let Some(failure) = self.failure.lock().await.as_ref() {
            info.status = match failure {
                AgentError::Unsupported(_) => AgentAvailability::Unsupported,
                AgentError::NotAvailable(s) if s.contains("authentication") => {
                    AgentAvailability::Unconfigured
                }
                AgentError::NotAvailable(s)
                    if s.contains("executable") && s.contains("not found") =>
                {
                    AgentAvailability::NotInstalled
                }
                _ => AgentAvailability::Offline,
            };
            info.reason = Some(failure.to_string());
        } else if self.config.resolve_binary().is_err() {
            info.status = AgentAvailability::NotInstalled;
            info.reason = Some(
                "Install/configure Codex 0.163.0 or newer on this host, or disable codex.enabled"
                    .into(),
            );
        } else {
            // CLI discovery is read-only: it must not spawn an agent merely to
            // inspect its auth. Auth readiness is unknown until supervision.
            info.status = AgentAvailability::Unconfigured;
            info.reason = Some("Codex is installed; authentication/readiness has not been checked by the running Gateway".into());
        }
        info
    }
}

pub(super) fn features() -> crate::discovery::AgentFeatures {
    crate::discovery::AgentFeatures {
        streaming: true,
        reasoning_effort: false,
        model_selection: false,
        tool_approvals: true,
        worktrees: false,
        revert: false,
        staged_revert: false,
        // The Gateway owns Codex's queue (`adapters/codex/inbox.rs`).
        inbox: true,
        modes: false,
        skills: false,
        slash_commands: false,
        compaction: true,
        background_shells: false,
        attachments: true,
        attachments_by_path: false,
        extra: std::collections::BTreeMap::from([
            ("sessionPagination".into(), serde_json::json!(true)),
            ("timelineEarlier".into(), serde_json::json!(true)),
            ("sharedDaemon".into(), serde_json::json!(cfg!(unix))),
            ("sharedTaskStartup".into(), serde_json::json!(false)),
            ("sharedDaemonConnected".into(), serde_json::json!(false)),
            ("runtimeTransport".into(), serde_json::json!("unavailable")),
            ("runtimeScopedControls".into(), serde_json::json!(true)),
            ("shellToolItems".into(), serde_json::json!(true)),
            ("fileChangeItems".into(), serde_json::json!(true)),
            ("transcriptImages".into(), serde_json::json!(true)),
            ("nativeImageInput".into(), serde_json::json!(true)),
            (
                "attachmentInputScope".into(),
                serde_json::json!("gateway_uploads"),
            ),
            ("audioPlayback".into(), serde_json::json!(false)),
            ("generatedImagePreview".into(), serde_json::json!(false)),
            ("legacyVcsDiff".into(), serde_json::json!(false)),
            (
                "sessionGitInfoScope".into(),
                serde_json::json!("producer_snapshot"),
            ),
            ("sessionRename".into(), serde_json::json!(true)),
            ("sessionFork".into(), serde_json::json!(true)),
            ("sessionDelete".into(), serde_json::json!(true)),
            ("forms".into(), serde_json::json!(true)),
            ("asyncQuestions".into(), serde_json::json!(true)),
            ("promptSteering".into(), serde_json::json!(true)),
            ("fileMentions".into(), serde_json::json!(true)),
            ("typedToolItems".into(), serde_json::json!(true)),
            ("subagentSessions".into(), serde_json::json!(true)),
            ("sessionTokenUsage".into(), serde_json::json!(true)),
            ("sessionContextUsage".into(), serde_json::json!(true)),
            ("accountLimits".into(), serde_json::json!(true)),
            ("sessionGitInfo".into(), serde_json::json!(true)),
            ("modelSelectionScope".into(), serde_json::json!("next_turn")),
            (
                "modelSelectionPersistence".into(),
                serde_json::json!("gateway_until_turn_start"),
            ),
            ("mcpElicitation".into(), serde_json::json!(false)),
            (
                "protocolBaseline".into(),
                serde_json::json!("rust-v0.163.0"),
            ),
        ]),
    }
}

/// Connection availability is separate from supported protocol projections.
/// Catalog failure leaves model controls unavailable, never a guessed default.
pub(super) fn connection_features(
    endpoint: Option<&str>,
    models: &[crate::discovery::AgentModelInfo],
) -> crate::discovery::AgentFeatures {
    let mut result = features();
    result.model_selection = endpoint.is_some() && !models.is_empty();
    result.reasoning_effort = endpoint.is_some() && models.iter().any(|m| m.supports_reasoning);
    let shared = cfg!(unix) && endpoint == Some("unix://shared-codex-app-server");
    result
        .extra
        .insert("sharedDaemonConnected".into(), serde_json::json!(shared));
    result
        .extra
        .insert("sharedTaskStartup".into(), serde_json::json!(shared));
    result.extra.insert(
        "runtimeTransport".into(),
        serde_json::json!(match endpoint {
            Some("unix://shared-codex-app-server") => "shared_daemon",
            Some("stdio://private-codex-app-server") => "private_stdio",
            _ => "unavailable",
        }),
    );
    result
}

impl AgentRuntime {
    /// Shut down only the private Codex child and wait for it to be reaped.
    /// Terminal agents and adopted services are deliberately untouched.
    pub(crate) async fn shutdown_codex(&self) -> Result<(), AgentError> {
        self.codex.shutdown.cancel();
        let _guard = self.codex.start.lock().await;
        if let Some(manager) = self.manager_for_agent("codex").await {
            manager.shutdown();
        }
        if let Some(child) = self.codex.child.lock().await.take() {
            if let Err(err) = child.wait().await {
                self.codex.cleanup_failed(err).await;
            }
        }
        self.codex
            .cleanup_failure
            .lock()
            .await
            .clone()
            .map_or(Ok(()), Err)
    }
}

struct ChildExit {
    done: std::sync::atomic::AtomicBool,
    notify: Notify,
    failure: Mutex<Option<AgentError>>,
}

type ChildConnection = (Arc<CodexClient>, CodexEvents, Arc<ChildExit>);
impl ChildExit {
    async fn wait(&self) -> Result<(), AgentError> {
        loop {
            let notified = self.notify.notified();
            if self.done.load(std::sync::atomic::Ordering::Acquire) {
                return self.failure.lock().await.clone().map_or(Ok(()), Err);
            }
            notified.await;
        }
    }
}

/// Owns and reaps exactly the child supplied here. Production no longer starts
/// one; tests supply an explicit fake executable for shutdown coverage.
async fn start_child(command: tokio::process::Command) -> Result<ChildConnection, AgentError> {
    start_child_with_tracking(
        command,
        crate::agents::adapters::codex::process_tree::ProcessTree::new,
    )
    .await
}

async fn start_child_with_tracking(
    mut command: tokio::process::Command,
    make_tree: impl FnOnce(u32) -> crate::agents::adapters::codex::process_tree::ProcessTree,
) -> Result<ChildConnection, AgentError> {
    // Match the private child's startup config context. config/read without cwd
    // intentionally excludes project layers, unlike model/list's startup config.
    let cwd = command
        .as_std()
        .get_current_dir()
        .map(std::path::Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(|_| {
            AgentError::NotAvailable("cannot resolve private Codex config context".into())
        })?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| {
            AgentError::InvalidRequest("Codex config context requires a UTF-8 path".into())
        })?
        .to_owned();
    command.current_dir(&cwd);
    #[cfg(unix)]
    command.process_group(0);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| AgentError::NotAvailable("could not start private Codex app-server".into()))?;
    let mut tree = make_tree(child.id().expect("newly spawned child has a PID"));
    if !tree.tracking_reliable() {
        tracing::warn!(
            "Codex process ownership tracking unavailable; preserving full upstream shutdown grace"
        );
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Protocol("missing Codex stdout".into()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Protocol("missing Codex stdin".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Protocol("missing Codex stderr".into()))?;
    let (client, events) = CodexClient::from_io_at(stdout, stdin, cwd);
    let cancel = client.cancel_token();
    // Continue draining diagnostics during upstream's EOF cleanup. Cancelling
    // this pipe at the same time as stdin can itself block graceful shutdown.
    let stderr_cancel = tokio_util::sync::CancellationToken::new();
    let drain_cancel = stderr_cancel.clone();
    let drain = tokio::spawn(async move {
        // Stderr is drained and discarded, not stored or logged. This is both
        // a fixed-memory bound and complete redaction of upstream diagnostics.
        let mut bytes = [0; 8192];
        loop {
            tokio::select! {
                _ = drain_cancel.cancelled() => break,
                result = stderr.read(&mut bytes) => if !matches!(result,Ok(n) if n > 0) {break;},
            }
        }
    });
    let exit = Arc::new(ChildExit {
        done: std::sync::atomic::AtomicBool::new(false),
        notify: Notify::new(),
        failure: Mutex::new(None),
    });
    let finished = exit.clone();
    tokio::spawn(async move {
        let mut observations = tokio::time::interval(Duration::from_millis(25));
        let mut stop_deadline = None;
        let exited = loop {
            tokio::select! {
                biased;
                result = child.wait() => break Some(result),
                _ = cancel.cancelled(), if stop_deadline.is_none() => {
                    // Pinned upstream drains RPCs (30s) BEFORE tool/thread
                    // cleanup and has a 45s EOF watchdog. Even a healthy ledger
                    // cannot prove it saw a helper's children before reparenting.
                    // Never truncate that cleanup with a short parent-only kill.
                    stop_deadline = Some(tokio::time::Instant::now()+Duration::from_secs(46));
                },
                _ = observations.tick() => {
                    tree.observe();
                    if stop_deadline.is_some_and(|deadline|tokio::time::Instant::now()>=deadline) {break None;}
                }
            }
        };
        let mut cleanup_verified = tree.tracking_reliable();
        if exited.is_none() {
            // Failed acquisition/enumeration/signal must not be treated as an
            // empty, successfully cleaned tree. Proven pidfds remain safe to
            // signal, but cannot justify claiming complete descendant cleanup.
            if !tree.freeze_and_kill_descendants() {
                cleanup_verified = false;
                let _ = tree.kill_observed_descendants();
            }
            // Give the app-server a short opportunity to collect its killed
            // tools before forcing and reaping the direct child itself.
            match tokio::time::timeout(Duration::from_millis(250), child.wait()).await {
                Ok(result) => cleanup_verified &= result.is_ok(),
                Err(_) => {
                    let _ = child.start_kill();
                    cleanup_verified &= child.wait().await.is_ok();
                }
            }
        } else {
            cleanup_verified &= exited.is_some_and(|result| result.is_ok());
            // A root that exited (including its watchdog) cannot be frozen.
            // Kill only already-proven descendant incarnations in that case.
            cleanup_verified &= tree.kill_observed_descendants();
        }
        if tokio::time::timeout(Duration::from_secs(1), async {
            while tree.live_descendants() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err()
        {
            cleanup_verified = false;
        }
        if !cleanup_verified {
            let err = AgentError::NotAvailable(
                "Codex stopped, but descendant cleanup could not be verified".into(),
            );
            tracing::warn!(%err);
            *finished.failure.lock().await = Some(err);
        }
        cancel.cancel();
        stderr_cancel.cancel();
        let _ = drain.await;
        finished
            .done
            .store(true, std::sync::atomic::Ordering::Release);
        finished.notify.notify_waiters();
    });
    Ok((client, events, exit))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::runtime::{DeepseekConfig, OpencodeConfig, T3Config};
    use crate::discovery::AgentAvailability;

    #[cfg(target_os = "linux")]
    async fn five_second_cleanup(
        make_tree: impl FnOnce(u32) -> crate::agents::adapters::codex::process_tree::ProcessTree,
        orphan_before_observation: bool,
    ) -> (Arc<CodexClient>, CodexEvents, Arc<ChildExit>, i32, i32) {
        let mut command = tokio::process::Command::new("/bin/sh");
        let ready_file =
            std::env::temp_dir().join(format!("gateway-codex-shutdown-{}", uuid::Uuid::new_v4()));
        command.env("READY_FILE", &ready_file);
        command.env(
            "ORPHAN_TOOL",
            if orphan_before_observation { "1" } else { "0" },
        );
        // A delayed exit makes sending SIGTERM observably different from
        // awaiting cleanup, even when the test runs alone on an idle machine.
        command.env(
            "ORPHAN_TOOL_SCRIPT",
            "trap 'sleep 0.1; exit' TERM; i=0; while [ \"$i\" -lt 6000 ]; do sleep 0.01; i=$((i+1)); done",
        );
        command.args(["-c", r#"
            if [ "$ORPHAN_TOOL" = 1 ]; then
                # Command substitution waits for the helper to exit, not its
                # fully redirected child. The server retains the tool handle.
                tool=$(sh -c 'setsid /bin/sh -c "$ORPHAN_TOOL_SCRIPT" </dev/null >/dev/null 2>&1 & echo $!')
            else
                setsid /bin/sleep 60 </dev/null >/dev/null 2>&1 & tool=$!
            fi
            printf '%s\n' "$tool" > "$READY_FILE"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{"userAgent":"codex_cli_rs/0.163.0 (synthetic fixture)"}}' ;;
                    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":null,"requiresOpenaiAuth":false}}' ;;
                    *'"method":"config/read"'*) printf '%s\n' '{"id":3,"result":{"config":{"model_provider":null}}}' ;;
                    *'"method":"synthetic/pids"'*) printf '{"id":4,"result":{"root":%s,"tool":%s}}\n' "$$" "$tool" ;;
                esac
            done
            # EOF cleanup diagnostics must still be drained, not block the
            # very tool cleanup that Gateway is giving upstream time to run.
            i=0; while [ "$i" -lt 20000 ]; do printf '%s\n' 'synthetic shutdown diagnostic' >&2; i=$((i+1)); done
            sleep 5
            if [ "$ORPHAN_TOOL" = 1 ]; then
                # /proc fields belong to this known fixture shell; its comm
                # contains no spaces. Keep its birth time across PID reuse.
                IFS=' ' read -r tool_pid tool_comm tool_state tool_fields < "/proc/$tool/stat"
                set -f
                set -- $tool_fields
                tool_birth=${19}
                kill "$tool"
                # wait returns 127 for a reparented non-child, before it has
                # necessarily processed SIGTERM. Await that incarnation's exit.
                # Builtins avoid introducing short-lived tracked descendants.
                polls=0
                while IFS=' ' read -r tool_pid tool_comm tool_state tool_fields 2>/dev/null < "/proc/$tool/stat"; do
                    set -- $tool_fields
                    [ "${19}" = "$tool_birth" ] || break
                    case "$tool_state" in Z|X) break ;; esac
                    polls=$((polls+1))
                    [ "$polls" -lt 100000 ] || exit 1
                done
            else
                kill "$tool"
                wait "$tool" 2>/dev/null || :
            fi
        "#]);
        // The test factory can delay tracker construction until the helper is
        // gone. initialize writes are impossible before this gate completes.
        let (client, events, exit) = start_child_with_tracking(command, |root| {
            if orphan_before_observation {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !ready_file.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture helper did not exit"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let mut tree = make_tree(root);
            if orphan_before_observation {
                tree.observe();
                assert!(
                    !tree.live_descendants(),
                    "runtime must actually miss the already-reparented tool"
                );
            }
            tree
        })
        .await
        .unwrap();
        client.initialize().await.unwrap();
        let pids = client
            .call("synthetic/pids", serde_json::json!({}))
            .await
            .unwrap();
        std::fs::remove_file(ready_file).unwrap();
        (
            client,
            events,
            exit,
            pids["root"].as_i64().unwrap() as i32,
            pids["tool"].as_i64().unwrap() as i32,
        )
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn runtime_with_unavailable_identity() -> (Arc<AgentRuntime>, i32, i32) {
        let (client, events, exit, root, tool) = five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::without_identity,
            false,
        )
        .await;
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig::default(),
            None,
        );
        *runtime.codex.child.lock().await = Some(exit);
        runtime
            .install(
                "codex",
                AgentManager::connect_codex_recoverable(
                    client,
                    events,
                    runtime.events_tx.clone(),
                    runtime.terminal_runtime.get().cloned(),
                    runtime.native_codex_sessions.get().cloned(),
                ),
                AgentOrigin::Spawned,
            )
            .await;
        (runtime, root, tool)
    }

    #[cfg(target_os = "linux")]
    async fn assert_five_second_cleanup(
        make_tree: impl FnOnce(u32) -> crate::agents::adapters::codex::process_tree::ProcessTree,
        orphan: bool,
    ) {
        use crate::agents::adapters::codex::process_tree::ProcessTree;
        let (client, _events, exit, root, tool) = five_second_cleanup(make_tree, orphan).await;
        // A separate exact-incarnation handle is a test oracle, NOT injected
        // into the runtime's deliberately unavailable/missed ownership ledger.
        let tool_identity = ProcessTree::new(tool as u32);
        assert!(tool_identity.can_force());
        if orphan {
            let stat = std::fs::read_to_string(format!("/proc/{tool}/stat")).unwrap();
            let parent = stat[stat.rfind(')').unwrap() + 2..]
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<i32>()
                .unwrap();
            assert_ne!(parent, root, "helper must have exited and reparented tool");
        }
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig::default(),
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig::default(),
            None,
        );
        *runtime.codex.child.lock().await = Some(exit.clone());
        let started = tokio::time::Instant::now();
        client.stop();
        let result = tokio::time::timeout(Duration::from_secs(8), runtime.shutdown_codex())
            .await
            .unwrap();
        let tool_exited_before_completion = !tool_identity.can_force();
        // Keep a failed regression from leaving a running fixture tool behind.
        if !tool_exited_before_completion {
            tool_identity.kill_root_for_test();
        }
        assert!(
            tool_exited_before_completion,
            "shutdown must not complete with a live tool"
        );
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        if orphan {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("cleanup could not be verified"),
                "empty ledger cannot claim successful verified cleanup"
            );
            let info = runtime.codex.discovery(&runtime).await;
            assert_eq!(info.status, AgentAvailability::Offline);
            assert!(info
                .reason
                .unwrap()
                .contains("cleanup could not be verified"));
            assert!(
                runtime.shutdown_codex().await.is_err(),
                "consuming the child handle must not erase a cleanup failure"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_failed_start_preserves_initial_diagnostic_and_shutdown_failure() {
        let (client, _events, exit, root, tool) = five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::without_identity,
            false,
        )
        .await;
        let runtime = AgentRuntime::disabled();
        runtime
            .codex
            .stop_failed_start(
                &client,
                &exit,
                AgentError::Protocol("synthetic initialization failure".into()),
            )
            .await;
        let failure = runtime
            .codex
            .failure
            .lock()
            .await
            .clone()
            .unwrap()
            .to_string();
        assert!(failure.contains("synthetic initialization failure"));
        assert!(failure.contains("cleanup could not be verified"));
        // No child was installed: explicit shutdown must propagate the cleanup
        // error retained by the failed/cancelled startup path nonetheless.
        assert!(runtime
            .shutdown_codex()
            .await
            .unwrap_err()
            .to_string()
            .contains("cleanup could not be verified"));
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        assert_eq!(unsafe { libc::kill(tool, 0) }, -1);
        assert!(!client.is_ready());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_identity_failure_preserves_five_second_eof_cleanup() {
        assert_five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::without_identity,
            false,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_pidfd_failure_preserves_five_second_eof_cleanup() {
        assert_five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::without_pidfd,
            false,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_descendant_pidfd_failure_preserves_five_second_eof_cleanup() {
        assert_five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::without_descendant_pidfds,
            false,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_unobserved_reparented_tool_preserves_five_second_eof_cleanup() {
        assert_five_second_cleanup(
            crate::agents::adapters::codex::process_tree::ProcessTree::new,
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn codex_explicit_terminal_tasks_keep_their_pane_route_with_or_without_service_manager() {
        let mut state = crate::test_support::test_state("synthetic-admin", vec![]);
        state
            .config
            .agent_commands
            .insert("codex".into(), "sh".into());
        assert!(crate::tasks::service::runs_in_pane(&state, "codex").await);
        let adapter = crate::test_support::FakeAgent::new("codex");
        state.agent_runtime.attach_for_test(adapter.manager()).await;
        assert!(state
            .agent_runtime
            .manager_for_agent("codex")
            .await
            .is_some());
        assert!(crate::tasks::service::runs_in_pane(&state, "codex").await);
        for service in ["opencode", "deepseek", "t3", "fx", "qoder", "pi"] {
            assert!(!crate::tasks::service::runs_in_pane(&state, service).await);
        }
    }

    #[cfg(target_os = "linux")]
    async fn fake_tree(
        graceful: bool,
    ) -> (Arc<CodexClient>, CodexEvents, Arc<ChildExit>, i32, i32) {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c",if graceful {r#"
            setsid /bin/sleep 60 </dev/null >/dev/null 2>&1 & tool=$!
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{"userAgent":"codex_cli_rs/0.163.0 (synthetic fixture)"}}' ;;
                    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":null,"requiresOpenaiAuth":false}}' ;;
                    *'"method":"config/read"'*) printf '%s\n' '{"id":3,"result":{"config":{"model_provider":null}}}' ;;
                    *'"method":"synthetic/pids"'*) printf '{"id":4,"result":{"root":%s,"tool":%s}}\n' "$$" "$tool" ;;
                esac
            done
            sleep 3
            kill "$tool"; wait "$tool"
        "#} else {r#"
            setsid /bin/sleep 60 </dev/null >/dev/null 2>&1 & tool=$!
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{"userAgent":"codex_cli_rs/0.163.0 (synthetic fixture)"}}' ;;
                    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":null,"requiresOpenaiAuth":false}}' ;;
                    *'"method":"config/read"'*) printf '%s\n' '{"id":3,"result":{"config":{"model_provider":null}}}' ;;
                    *'"method":"synthetic/pids"'*) printf '{"id":4,"result":{"root":%s,"tool":%s}}\n' "$$" "$tool" ;;
                esac
            done
            # Hung upstream cleanup: only descendant cleanup releases this wait.
            wait "$tool"
            # Even after collecting the tool, root cleanup itself remains hung.
            while :; do :; done
        "#}]);
        let (client, events, exit) = start_child(command).await.unwrap();
        client.initialize().await.unwrap();
        let pids = client
            .call("synthetic/pids", serde_json::json!({}))
            .await
            .unwrap();
        let root = pids["root"].as_i64().unwrap() as i32;
        let tool = pids["tool"].as_i64().unwrap() as i32;
        // Wait for the tool's private session and ownership polling to settle.
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::getpgid(tool) } != tool {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_ne!(unsafe { libc::getpgid(root) }, unsafe {
            libc::getpgid(tool)
        });
        (client, events, exit, root, tool)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_shutdown_allows_three_second_cleanup_and_reaps_separate_session_tool() {
        let (client, _events, exit, root, tool) = fake_tree(true).await;
        let started = tokio::time::Instant::now();
        client.stop();
        tokio::time::timeout(Duration::from_secs(6), exit.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "must not kill parent at the obsolete two-second deadline"
        );
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        assert_eq!(unsafe { libc::kill(tool, 0) }, -1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_forced_cleanup_kills_owned_new_session_tool_not_unrelated_process() {
        let mut unrelated = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (client, _events, exit, root, tool) = fake_tree(false).await;
        let started = tokio::time::Instant::now();
        client.stop();
        tokio::time::timeout(Duration::from_secs(50), exit.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(45),
            "even reliable polling cannot justify truncating upstream drain"
        );
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        assert_eq!(unsafe { libc::kill(tool, 0) }, -1);
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().await.unwrap();
        unrelated.wait().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_external_manager_drop_reaps_initialized_silent_private_child() {
        let (client, events, exit, root, tool) = fake_tree(false).await;
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let manager = Arc::new(AgentManager::connect_codex(client.clone(), events, tx));
        let retained = manager.clone();
        drop(client);
        drop(manager);
        assert!(!exit.done.load(std::sync::atomic::Ordering::Acquire));
        drop(retained);
        tokio::time::timeout(Duration::from_secs(50), exit.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unsafe { libc::kill(root, 0) }, -1);
        assert_eq!(unsafe { libc::kill(tool, 0) }, -1);
    }

    #[test]
    fn codex_defaults_enabled_and_explicit_disable_wins() {
        assert!(CodexConfig::default().enabled);
        assert!(serde_json::from_str::<CodexConfig>("{}").unwrap().enabled);
        let config: CodexConfig =
            serde_json::from_str(r#"{"enabled":false,"binary":"/synthetic/missing-codex"}"#)
                .unwrap();
        assert!(!config.enabled);
        assert!(config
            .resolve_binary()
            .unwrap_err()
            .to_string()
            .contains("disabled"));
    }

    #[tokio::test]
    async fn codex_disabled_and_master_disabled_have_no_spawn_lookup_or_calls() {
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig {
                enabled: false,
                binary: Some("not-even-an-absolute-path".into()),
            },
            None,
        );
        assert!(!runtime.codex.supervise(&runtime).await);
        let info = runtime.codex.discovery(&runtime).await;
        assert_eq!(info.status, AgentAvailability::Disabled);
        assert!(runtime.codex.failure.lock().await.is_none());
        assert!(runtime.codex.child.lock().await.is_none());
        let runtime = AgentRuntime::disabled();
        assert!(!runtime.codex.config.enabled);
        assert!(!runtime.codex.supervise(&runtime).await);
    }

    #[tokio::test]
    async fn codex_missing_binary_and_auth_are_nonfatal_truthful_discovery() {
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig {
                enabled: true,
                binary: Some("/synthetic/missing-codex".into()),
            },
            None,
        );
        assert_eq!(
            runtime.codex.discovery(&runtime).await.status,
            AgentAvailability::NotInstalled
        );
        runtime
            .codex
            .failed(AgentError::NotAvailable(
                "Configure Codex authentication on this host".into(),
            ))
            .await;
        assert_eq!(
            runtime.codex.discovery(&runtime).await.status,
            AgentAvailability::Unconfigured
        );
        assert!(runtime.manager_for_agent("codex").await.is_none());
        assert!(
            !runtime.codex.supervise(&runtime).await,
            "backoff must not launch anything"
        );
    }

    #[test]
    fn codex_feature_claims_do_not_include_file_rollback_or_optional_apis() {
        let features = features();
        assert!(features.streaming && features.tool_approvals && features.compaction);
        assert!(
            !features.skills,
            "skills stay off until skills/list actually answers"
        );
        assert!(features.attachments && !features.attachments_by_path);
        assert_eq!(features.extra["nativeImageInput"], true);
        assert_eq!(features.extra["attachmentInputScope"], "gateway_uploads");
        assert!(
            !features.revert
                && !features.staged_revert
                && !features.worktrees
                && !features.modes
                && !features.background_shells
        );
        // Only a Gateway that holds Codex's queue may claim an inbox; older
        // Apps ignore the flag and keep steering.
        assert!(features.inbox);
        assert_eq!(features.extra["sessionPagination"], true);
        assert_eq!(
            super::super::AGENT_PREFERENCE,
            ["codex", "opencode", "deepseek", "t3", "fx", "qoder", "pi", "claude"]
        );
    }

    #[test]
    fn codex_connection_features_use_actual_transport_and_catalog() {
        let model = crate::discovery::AgentModelInfo {
            id: "synthetic-model".into(),
            name: "Synthetic".into(),
            provider_id: "synthetic".into(),
            supports_reasoning: true,
            reasoning_effort_tiers: vec!["high".into()],
            extra: Default::default(),
        };
        for (endpoint, models, selectable, shared, transport) in [
            (None, vec![model.clone()], false, false, "unavailable"),
            (
                Some("stdio://private-codex-app-server"),
                vec![],
                false,
                false,
                "private_stdio",
            ),
            (
                Some("stdio://private-codex-app-server"),
                vec![model.clone()],
                true,
                false,
                "private_stdio",
            ),
            (
                Some("unix://shared-codex-app-server"),
                vec![model.clone()],
                true,
                cfg!(unix),
                "shared_daemon",
            ),
        ] {
            let features = connection_features(endpoint, &models);
            assert_eq!(features.model_selection, selectable);
            assert_eq!(features.reasoning_effort, selectable);
            assert_eq!(features.extra["runtimeTransport"], transport);
            assert_eq!(features.extra["sharedDaemonConnected"], shared);
            assert_eq!(features.extra["sharedTaskStartup"], shared);
            assert!(features.extra["shellToolItems"].as_bool().unwrap());
            assert!(!features.background_shells);
            assert_eq!(features.extra["sessionGitInfoScope"], "producer_snapshot");
            assert_eq!(features.extra["legacyVcsDiff"], false);
        }
        let mut model = model;
        model.supports_reasoning = false;
        model.reasoning_effort_tiers.clear();
        let features = connection_features(Some("stdio://private-codex-app-server"), &[model]);
        assert!(features.model_selection);
        assert!(!features.reasoning_effort);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_fake_child_stderr_flood_is_drained_and_stop_kills_waits_reaps() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c",r#"
            i=0; while [ "$i" -lt 20000 ]; do printf '%s\n' 'synthetic discarded diagnostic' >&2; i=$((i+1)); done
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{"userAgent":"codex_cli_rs/0.163.0 (synthetic fixture)"}}' ;;
                    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":null,"requiresOpenaiAuth":false}}' ;;
                    *'"method":"config/read"'*) printf '%s\n' '{"id":3,"result":{"config":{"model_provider":null}}}' ;;
                    *'"method":"synthetic/pid"'*) printf '{"id":4,"result":{"pid":%s}}\n' "$$" ;;
                esac
            done
        "#]);
        let (client, events, exit) = start_child(command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), client.initialize())
            .await
            .unwrap()
            .unwrap();
        let pid = client
            .call("synthetic/pid", serde_json::json!({}))
            .await
            .unwrap()["pid"]
            .as_u64()
            .unwrap() as i32;
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig::default(),
            None,
        );
        *runtime.codex.child.lock().await = Some(exit.clone());
        runtime
            .install(
                "codex",
                AgentManager::connect_codex(client.clone(), events, runtime.events_tx.clone()),
                AgentOrigin::Spawned,
            )
            .await;
        let status = runtime.status_for("codex").await.unwrap();
        assert!(status.autostart);
        assert_eq!(status.origin, AgentOrigin::Spawned);
        runtime.shutdown_codex().await.unwrap();
        assert!(
            !runtime.codex.supervise(&runtime).await,
            "shutdown must never restart a private child"
        );
        tokio::time::timeout(Duration::from_secs(2), exit.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "the private child must be reaped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_fake_child_startup_failure_is_cancelled_and_reaped() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "printf '%s\\n' 'synthetic malformed startup'; exit 1"]);
        let (client, _events, exit) = start_child(command).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), client.initialize())
                .await
                .unwrap()
                .is_err()
        );
        let cleanup = tokio::time::timeout(Duration::from_secs(2), exit.wait())
            .await
            .unwrap();
        // This fixture can exit before initial identity acquisition. Reaping
        // still completes, but that race must honestly report unverified
        // ownership rather than turn an empty ledger into verified cleanup.
        if let Err(err) = cleanup {
            assert!(err.to_string().contains("cleanup could not be verified"));
        }
        assert!(!client.is_ready());
    }
}
