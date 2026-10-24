//! Start or join one DeepSeek Harness (`dsh web`).
//!
//! Adoption wins: a healthy endpoint is used as it is. Otherwise, when
//! DeepSeek is enabled and `autostart` is on, one detached `web --no-open`
//! is started. The process is its own session, so a gateway restart does
//! not deliver SIGHUP to it, and the user systemd unit's `KillMode=process`
//! does not signal it either. The next start adopts the live port. Nothing
//! here stops the harness when the gateway stops.
//!
//! Discovery and status never spawn a process. A missing runner is
//! reported and left alone; the rest of the gateway keeps running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::process::Command;

use super::{AgentOrigin, AgentRuntime, DeepseekConfig, MAX_START_BACKOFF, STARTUP_WAIT};

/// Official `@deepseek-ai/dsh` web profile port (`port ?? 3080`).
pub(crate) const DEFAULT_DEEPSEEK_PORT: u16 = 3080;
pub(crate) const DEFAULT_DEEPSEEK_URL: &str = "http://127.0.0.1:3080";
const PACKAGE: &str = "@deepseek-ai/dsh";

pub(crate) const MISSING_RUNNER: &str =
    "DeepSeek Harness was not started: no dsh, bunx, or npx on PATH";

pub(crate) const MISSING_RUNNER_SETUP: &str = "\
No dsh, bunx, or npx is on PATH, so DeepSeek Harness was not started. \
Install one of them, or start the harness yourself. The gateway keeps running \
and joins a healthy endpoint.";

pub(crate) const NOTHING_ANSWERING: &str = "\
Nothing is answering yet. deepseek.autostart defaults to on once DeepSeek is \
enabled; a running gateway starts the harness and does not stop it on shutdown.";

/// How a harness this gateway started is remembered. Dropping the slot does
/// not signal the process.
#[derive(Default)]
pub(crate) struct SpawnSlot {
    pub(crate) url: Option<String>,
    running: Option<Arc<AtomicBool>>,
    pub(crate) retry_at: Option<tokio::time::Instant>,
    pending_mcp: Option<(super::McpEnv, Vec<String>)>,
}

impl SpawnSlot {
    pub(crate) fn running_on(&self, url: &str) -> bool {
        self.url.as_deref() == Some(url)
            && self
                .running
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// The last child has exited. The slot does not reap or kill it.
    pub(crate) fn exited(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|flag| !flag.load(Ordering::SeqCst))
    }

    pub(crate) fn backed_off(&self, now: tokio::time::Instant) -> bool {
        self.retry_at.is_some_and(|at| now < at)
    }

    pub(crate) fn note(&mut self, url: String, running: Arc<AtomicBool>) {
        self.url = Some(url);
        self.running = Some(running);
        self.retry_at = None;
    }
}

/// Host and port a spawned harness listens on, and the URL adoption probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListenTarget {
    pub host: String,
    pub port: u16,
    pub url: String,
}

/// One runner invocation. Arguments never carry tokens or secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub url: String,
}

impl Launch {
    pub(crate) fn display(&self) -> String {
        let mut parts = Vec::with_capacity(1 + self.args.len());
        parts.push(self.program.clone());
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runner {
    Dsh,
    Bunx,
    Npx,
}

/// `dsh` on `PATH`, else `bunx`, else `npx`. Lookup must not execute them:
/// `npx --yes` and `bunx` download when they run.
pub(crate) fn choose_runner(lookup: impl Fn(&str) -> bool) -> Option<Runner> {
    if lookup("dsh") {
        Some(Runner::Dsh)
    } else if lookup("bunx") {
        Some(Runner::Bunx)
    } else if lookup("npx") {
        Some(Runner::Npx)
    } else {
        None
    }
}

/// The configured HTTP(S) endpoint, otherwise the official loopback port.
/// Only the absent endpoint defaults to 3080; configured URLs use their
/// scheme's default port and retain reverse-proxy paths.
pub(crate) fn listen_target(endpoint: Option<&str>) -> Result<ListenTarget, &'static str> {
    let raw = endpoint.map(str::trim).unwrap_or(DEFAULT_DEEPSEEK_URL);
    if raw.is_empty() {
        return Err("deepseek.endpoint is not a usable listen address");
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let mut parsed = reqwest::Url::parse(&with_scheme)
        .map_err(|_| "deepseek.endpoint is not a usable listen address")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("deepseek.endpoint is not a usable listen address");
    }
    // Userinfo is not part of the supported bearer/cookie credential contract.
    // Reject it before it can enter Host headers, cookie signing or logs.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("deepseek.endpoint must not contain userinfo");
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or("deepseek.endpoint is not a usable listen address")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    // Includes explicit :80/:443 even though Url::port() normalizes them away.
    let port = parsed
        .port_or_known_default()
        .unwrap_or(DEFAULT_DEEPSEEK_PORT);
    // A base URL is not a complete request URL. Keep the prefix, but strip
    // query/fragment as before so API paths cannot be appended inside them.
    parsed.set_query(None);
    parsed.set_fragment(None);
    let url = parsed.as_str().trim_end_matches('/').to_string();
    Ok(ListenTarget { host, port, url })
}

pub(crate) fn launch(runner: Runner, target: &ListenTarget) -> Launch {
    let mut args = Vec::new();
    let program = match runner {
        Runner::Dsh => "dsh".to_string(),
        Runner::Bunx => {
            args.push(PACKAGE.to_string());
            "bunx".to_string()
        }
        Runner::Npx => {
            args.push("--yes".to_string());
            args.push(PACKAGE.to_string());
            "npx".to_string()
        }
    };
    // `bunx --bun` crashes this package: its native addon needs Node.
    args.extend([
        "web".to_string(),
        "--no-open".to_string(),
        "--host".to_string(),
        target.host.clone(),
        "--port".to_string(),
        target.port.to_string(),
    ]);
    debug_assert!(args.iter().all(|arg| arg != "--bun"));
    Launch {
        program,
        args,
        url: target.url.clone(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Adopt,
    Spawn(Launch),
    /// Leave DeepSeek unattached. `warning` is safe to log: no tokens.
    Leave {
        warning: Option<&'static str>,
    },
}

/// Adopt a healthy endpoint. Otherwise spawn at most one harness, and only
/// when enabled, autostart is on, and no child we started is still running
/// on that listen URL. A missing runner is [`Decision::Leave`], not a failure.
pub(crate) fn decide(
    config: &DeepseekConfig,
    endpoint_healthy: bool,
    child_running_on_target: bool,
    lookup: impl Fn(&str) -> bool,
) -> Decision {
    if endpoint_healthy {
        return Decision::Adopt;
    }
    if !config.enabled || !config.autostart {
        return Decision::Leave { warning: None };
    }
    if child_running_on_target {
        return Decision::Leave { warning: None };
    }
    let target = match listen_target(config.endpoint.as_deref()) {
        Ok(target) => target,
        Err(_) => {
            return Decision::Leave {
                warning: Some(
                    "deepseek.endpoint is not a usable listen address; DeepSeek Harness was not started",
                ),
            };
        }
    };
    match choose_runner(lookup) {
        Some(runner) => Decision::Spawn(launch(runner, &target)),
        None => Decision::Leave {
            warning: Some(MISSING_RUNNER),
        },
    }
}

pub(crate) fn setup_summary(url: &str) -> String {
    format!("Gateway starts or joins DeepSeek Harness at {url}.")
}

pub(crate) fn setup_manual_line(target: &ListenTarget, lookup: impl Fn(&str) -> bool) -> String {
    match choose_runner(lookup) {
        Some(runner) => format!("Manual command: {}", launch(runner, target).display()),
        None => MISSING_RUNNER_SETUP.to_string(),
    }
}

impl AgentRuntime {
    /// One DeepSeek pass. `false` leaves the agent unattached and does not
    /// stop OpenCode, T3, or Codex.
    pub(super) async fn supervise_deepseek(&self) -> bool {
        let env_endpoint = environment_endpoint(|key| {
            std::env::var_os(key).map(|value| value.into_string().unwrap_or_default())
        });
        self.supervise_deepseek_with_override(env_endpoint.as_deref())
            .await
    }

    async fn supervise_deepseek_with_override(&self, env_endpoint: Option<&str>) -> bool {
        self.settle_deepseek_mcp().await;
        if self.keep_if_healthy("deepseek").await {
            return true;
        }
        if !(self.deepseek_config.enabled || self.deepseek_config.endpoint.is_some()) {
            return false;
        }
        let healthy = self.healthy_deepseek_with_override(env_endpoint).await;
        // An explicit environment endpoint is attach-only and exclusive. Never
        // spawn/probe a default target after that override fails authentication.
        if self.deepseek_config.endpoint.is_none() && env_endpoint.is_some() && healthy.is_none() {
            return false;
        }
        let now = tokio::time::Instant::now();
        if healthy.is_none() {
            let mut slot = self.deepseek_spawn.lock().await;
            if slot.backed_off(now) {
                return false;
            }
            if slot.exited() {
                if let (Some(wiring), Some((minted, previous))) =
                    (self.mcp.get(), slot.pending_mcp.take())
                {
                    super::settle_agent_tokens(wiring.tokens.as_ref(), &minted, &previous, false)
                        .await;
                }
                slot.running = None;
                slot.retry_at = Some(now + STARTUP_WAIT);
                return false;
            }
        }
        let child_running = if healthy.is_some() {
            false
        } else {
            let slot = self.deepseek_spawn.lock().await;
            listen_target(self.deepseek_config.endpoint.as_deref())
                .ok()
                .is_some_and(|target| slot.running_on(&target.url))
        };
        match decide(
            &self.deepseek_config,
            healthy.is_some(),
            child_running,
            |name| crate::agents::tasks::find_on_path(name).is_some(),
        ) {
            Decision::Adopt => {
                let Some(endpoint) = healthy else {
                    return false;
                };
                let spawned =
                    self.deepseek_spawn.lock().await.url.as_deref() == Some(endpoint.url.as_str());
                self.attach_deepseek(
                    endpoint,
                    if spawned {
                        AgentOrigin::Spawned
                    } else {
                        AgentOrigin::Adopted
                    },
                )
                .await;
                true
            }
            Decision::Leave { warning } => {
                if let Some(warning) = warning {
                    let mut slot = self.deepseek_spawn.lock().await;
                    if !slot.backed_off(now) {
                        tracing::warn!("{warning}");
                        slot.retry_at = Some(now + MAX_START_BACKOFF);
                    }
                }
                false
            }
            Decision::Spawn(mut launch) => {
                let minted = self.prepare_deepseek_mcp(&mut launch).await;
                let retirement = self
                    .mcp
                    .get()
                    .zip(minted.as_ref())
                    .map(|(wiring, (env, _))| (wiring.tokens.clone(), env.token_id.clone()));
                match spawn_detached_with_mcp(
                    &launch,
                    minted.as_ref().map(|(env, _)| env),
                    retirement,
                )
                .await
                {
                    Ok(running) => {
                        tracing::info!(
                            url = %launch.url,
                            program = %launch.program,
                            "started DeepSeek Harness; it keeps running after the gateway stops"
                        );
                        let mut slot = self.deepseek_spawn.lock().await;
                        slot.note(launch.url, running);
                        slot.pending_mcp = minted;
                    }
                    Err(err) => {
                        if let (Some(wiring), Some((env, previous))) = (self.mcp.get(), minted) {
                            super::settle_agent_tokens(
                                wiring.tokens.as_ref(),
                                &env,
                                &previous,
                                false,
                            )
                            .await;
                        }
                        tracing::warn!(%err, "could not start DeepSeek Harness");
                        self.deepseek_spawn.lock().await.retry_at = Some(now + STARTUP_WAIT);
                    }
                }
                false
            }
        }
    }

    /// HTTP readiness can precede MCP activation. Keep both generations until
    /// the official plugin has discovered tools; never turn a slow startup
    /// into a permanent 401 by revoking its credential on the first probe.
    async fn settle_deepseek_mcp(&self) {
        let Some(wiring) = self.mcp.get() else { return };
        let url = {
            let slot = self.deepseek_spawn.lock().await;
            if slot.pending_mcp.is_none() || slot.exited() {
                return;
            }
            slot.url.clone()
        };
        let Some(endpoint) = url.and_then(|url| self.normalized_deepseek_endpoint(&url)) else {
            return;
        };
        let client = crate::agents::adapters::deepseek::client::DeepseekClient::new(endpoint);
        if !crate::agents::adapters::deepseek::mcp::connected(&client)
            .await
            .unwrap_or(false)
        {
            return;
        }
        let pending = self.deepseek_spawn.lock().await.pending_mcp.take();
        if let Some((env, previous)) = pending {
            super::settle_agent_tokens(wiring.tokens.as_ref(), &env, &previous, true).await;
        }
    }

    async fn prepare_deepseek_mcp(
        &self,
        launch: &mut Launch,
    ) -> Option<(super::McpEnv, Vec<String>)> {
        let wiring = self.mcp.get()?;
        let path = crate::state_dir().ok()?.join("mcp-deepseek.patch.yml");
        let write_path = path.clone();
        let written = tokio::task::spawn_blocking(move || {
            crate::write_secret_file(
                &write_path,
                crate::agents::adapters::deepseek::mcp::OVERLAY.as_bytes(),
            )
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            tracing::warn!("DeepSeek MCP overlay could not be written");
            return None;
        }
        let minted = match super::issue_agent_env_for(
            wiring.tokens.as_ref(),
            &wiring.url,
            "deepseek",
            "deepseek (service)",
        )
        .await
        {
            Ok(minted) => minted,
            Err(_) => {
                tracing::warn!("DeepSeek MCP credentials could not be prepared");
                return None;
            }
        };
        // Launcher options must precede app-owned flags such as --no-open.
        let Some(at) = launch.args.iter().position(|arg| arg == "web") else {
            super::settle_agent_tokens(wiring.tokens.as_ref(), &minted.0, &minted.1, false).await;
            return None;
        };
        launch.args.splice(
            at + 1..at + 1,
            ["--patch".into(), path.to_string_lossy().into_owned()],
        );
        Some(minted)
    }

    async fn healthy_deepseek_with_override(
        &self,
        env_endpoint: Option<&str>,
    ) -> Option<crate::agents::adapters::deepseek::DeepseekEndpoint> {
        use crate::agents::adapters::deepseek::DeepseekEndpoint;
        let client = super::probe_client();
        if let Some(url) = self.deepseek_config.endpoint.as_deref().or(env_endpoint) {
            return self.probe_deepseek_base(url, &client).await;
        }
        if let Some(found) = DeepseekEndpoint::discover().await {
            return Some(found);
        }
        // No explicit config/environment target remains. A harness we started
        // on a non-default port may still be outside discovery's candidates.
        let spawned = self.deepseek_spawn.lock().await.url.clone();
        if let Some(url) = spawned {
            return self.probe_deepseek_base(&url, &client).await;
        }
        None
    }

    async fn probe_deepseek_base(
        &self,
        url: &str,
        client: &reqwest::Client,
    ) -> Option<crate::agents::adapters::deepseek::DeepseekEndpoint> {
        let endpoint = self.normalized_deepseek_endpoint(url)?;
        endpoint.probe_healthy(client).await.then_some(endpoint)
    }

    pub(super) fn normalized_deepseek_endpoint(
        &self,
        url: &str,
    ) -> Option<crate::agents::adapters::deepseek::DeepseekEndpoint> {
        let target = listen_target(Some(url)).ok()?;
        Some(
            crate::agents::adapters::deepseek::DeepseekEndpoint::with_fallbacks(
                target.url,
                self.deepseek_config.token.clone(),
                self.deepseek_config.secret.clone(),
            ),
        )
    }
}

pub(crate) fn environment_endpoint(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup("DEEPSEEK_HARNESS_URL").or_else(|| lookup("DSH_URL"))
}

fn command_for(launch: &Launch) -> Command {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropping the handle must not signal the harness.
        .kill_on_drop(false);
    detach_session(&mut command);
    command
}

/// New session, so the harness is not in the gateway's process group.
/// A terminal restart sends SIGHUP to that group; systemd `KillMode=process`
/// signals only the gateway main PID. Either way the live port stays up
/// for the next gateway to adopt.
fn detach_session(command: &mut Command) {
    #[cfg(unix)]
    // SAFETY: `setsid` is async-signal-safe and runs only in the forked child,
    // before exec. The closure does not touch memory shared with the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

/// Spawn `launch` and return a flag that becomes false when the child exits.
/// Unit tests record the launch and do not exec: the user's harness must
/// not be started by `cargo test`.
#[cfg(test)]
pub(crate) async fn spawn_detached(launch: &Launch) -> std::io::Result<Arc<AtomicBool>> {
    spawn_detached_with_mcp(launch, None, None).await
}

async fn spawn_detached_with_mcp(
    launch: &Launch,
    mcp: Option<&super::McpEnv>,
    retirement: Option<(Arc<dyn crate::mcp::tokens::AssistantTokenStore>, String)>,
) -> std::io::Result<Arc<AtomicBool>> {
    // `mut` is required by `Command::spawn` in the release build. Tests never spawn.
    #[allow(unused_mut)]
    let mut command = command_for(launch);
    if let Some(mcp) = mcp {
        mcp.apply(&mut command);
    }
    #[cfg(test)]
    {
        drop(command);
        drop(retirement);
        record_test_spawn(launch);
        Ok(Arc::new(AtomicBool::new(true)))
    }
    #[cfg(not(test))]
    {
        let mut child = command.spawn()?;
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        tokio::spawn(async move {
            let _ = child.wait().await;
            if let Some((tokens, id)) = retirement {
                let _ = tokens.revoke(&id).await;
            }
            flag.store(false, Ordering::SeqCst);
        });
        Ok(running)
    }
}

#[cfg(test)]
fn record_test_spawn(launch: &Launch) {
    test_spawns()
        .lock()
        .expect("spawn log")
        .push(launch.clone());
}

#[cfg(test)]
fn test_spawns() -> &'static std::sync::Mutex<Vec<Launch>> {
    static SPAWNS: std::sync::OnceLock<std::sync::Mutex<Vec<Launch>>> = std::sync::OnceLock::new();
    SPAWNS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slow_mcp_activation_keeps_credentials_until_discovery_succeeds() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        let ready = Arc::new(AtomicBool::new(false));
        let status = ready.clone();
        let app = Router::new().route(
            "/api/pluginInventory/list",
            post(move || {
                let ready = status.load(Ordering::SeqCst);
                async move {
                    Json(json!({"result": {"ok": true, "value": {"entries": [{
                        "entryId": crate::agents::adapters::deepseek::mcp::ENTRY_ID,
                        "moduleName": crate::agents::adapters::deepseek::mcp::MODULE,
                        "enabled": true, "fiberPhase": if ready { "active" } else { "loading" }
                    }]}}}))
                }
            }),
        );
        let (client, server) =
            crate::agents::adapters::deepseek::client::tests::rpc_server(app).await;
        let state = crate::test_state("synthetic-admin", Vec::new());
        let runtime = AgentRuntime::disabled();
        runtime.enable_mcp(
            state.mcp_tokens.clone(),
            "http://127.0.0.1:23999/mcp".into(),
        );
        let tokens = state.mcp_tokens.as_ref();
        let (old, _) = super::super::issue_agent_env_for(
            tokens,
            "http://127.0.0.1:23999/mcp",
            "deepseek",
            "deepseek (service)",
        )
        .await
        .unwrap();
        let (new, previous) = super::super::issue_agent_env_for(
            tokens,
            "http://127.0.0.1:23999/mcp",
            "deepseek",
            "deepseek (service)",
        )
        .await
        .unwrap();
        let new_token = new.token.clone();
        {
            let mut slot = runtime.deepseek_spawn.lock().await;
            slot.note(client.endpoint.url.clone(), Arc::new(AtomicBool::new(true)));
            slot.pending_mcp = Some((new, previous));
        }
        runtime.settle_deepseek_mcp().await;
        assert!(runtime.deepseek_spawn.lock().await.pending_mcp.is_some());
        assert!(tokens.resolve(&old.token).await.unwrap().live().is_some());
        assert!(tokens.resolve(&new_token).await.unwrap().live().is_some());
        ready.store(true, Ordering::SeqCst);
        runtime.settle_deepseek_mcp().await;
        assert!(runtime.deepseek_spawn.lock().await.pending_mcp.is_none());
        assert!(tokens.resolve(&old.token).await.unwrap().live().is_none());
        assert!(tokens.resolve(&new_token).await.unwrap().live().is_some());
        server.abort();
    }

    #[tokio::test]
    async fn mcp_overlay_uses_child_env_and_preserves_other_agent_credentials() {
        let state = crate::test_state("synthetic-admin", Vec::new());
        let tokens = state.mcp_tokens.as_ref();
        let runtime = AgentRuntime::disabled();
        let url = "http://127.0.0.1:23999/mcp";
        runtime.enable_mcp(state.mcp_tokens.clone(), url.into());
        let (opencode, _) = super::super::issue_agent_env(tokens, url).await.unwrap();
        for runner in [Runner::Dsh, Runner::Bunx, Runner::Npx] {
            let mut launch = launch(runner, &target(None));
            let (env, previous) = runtime.prepare_deepseek_mcp(&mut launch).await.unwrap();
            assert!(!previous.contains(&opencode.token_id));
            let web = launch.args.iter().position(|arg| arg == "web").unwrap();
            assert_eq!(launch.args[web + 1], "--patch");
            assert_eq!(launch.args[web + 3], "--no-open");
            let overlay = tokio::fs::read_to_string(&launch.args[web + 2])
                .await
                .unwrap();
            assert!(!overlay.contains(&env.token));
            assert!(!launch.display().contains(&env.token));
            let mut command = command_for(&launch);
            env.apply(&mut command);
            let vars: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
            assert_eq!(
                vars.get(std::ffi::OsStr::new("MUQUN_MCP_TOKEN")),
                Some(&Some(std::ffi::OsStr::new(&env.token)))
            );
            super::super::settle_agent_tokens(tokens, &env, &previous, true).await;
            assert_eq!(
                tokens
                    .list()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|row| row.label == "deepseek (service)")
                    .count(),
                1
            );
        }
        assert!(tokens
            .resolve(&opencode.token)
            .await
            .unwrap()
            .live()
            .is_some());
        let mut off = state.config.clone();
        off.mcp.inject_into_agents = false;
        crate::mcp::inject::startup_cleanup(&off, tokens).await;
        assert!(!tokens
            .list()
            .await
            .unwrap()
            .iter()
            .any(|row| row.label == "deepseek (service)"));
    }

    #[tokio::test]
    async fn disabled_mcp_adds_no_overlay_or_credentials() {
        let runtime = AgentRuntime::disabled();
        let mut launch = launch(Runner::Dsh, &target(None));
        let before = launch.clone();
        assert!(runtime.prepare_deepseek_mcp(&mut launch).await.is_none());
        assert_eq!(launch, before);
    }

    fn lookup<'a>(present: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |name| present.contains(&name)
    }

    fn target(endpoint: Option<&str>) -> ListenTarget {
        listen_target(endpoint).expect("listen target")
    }

    fn enabled(endpoint: Option<&str>) -> DeepseekConfig {
        DeepseekConfig {
            enabled: true,
            autostart: true,
            endpoint: endpoint.map(str::to_string),
            ..DeepseekConfig::default()
        }
    }

    #[test]
    fn runner_choice_prefers_dsh_then_bunx_then_npx() {
        assert_eq!(
            choose_runner(lookup(&["dsh", "bunx", "npx"])),
            Some(Runner::Dsh)
        );
        assert_eq!(choose_runner(lookup(&["bunx", "npx"])), Some(Runner::Bunx));
        assert_eq!(choose_runner(lookup(&["npx"])), Some(Runner::Npx));
        assert_eq!(choose_runner(lookup(&[])), None);
    }

    #[test]
    fn launch_uses_host_and_port_and_never_passes_bun() {
        let cases = [
            (None, Runner::Dsh, "dsh", "127.0.0.1", 3080_u16),
            (
                Some("http://192.0.2.10:4000/ignored"),
                Runner::Bunx,
                "bunx",
                "192.0.2.10",
                4000,
            ),
            (Some("http://[::1]:3080"), Runner::Npx, "npx", "::1", 3080),
        ];
        for (endpoint, runner, program, host, port) in cases {
            let launch = launch(runner, &target(endpoint));
            assert_eq!(launch.program, program);
            assert!(
                launch.args.windows(2).any(|pair| pair == ["--host", host]),
                "{endpoint:?} {launch:?}"
            );
            assert!(
                launch
                    .args
                    .windows(2)
                    .any(|pair| pair == ["--port", port.to_string().as_str()]),
                "{endpoint:?}"
            );
            assert!(launch.args.contains(&"web".to_string()));
            assert!(launch.args.contains(&"--no-open".to_string()));
            assert!(!launch.args.iter().any(|arg| arg == "--bun"));
            assert!(!launch.display().contains("--bun"));
        }
        let bunx = launch(Runner::Bunx, &target(None));
        assert_eq!(bunx.args[0], PACKAGE);
        assert_eq!(bunx.program, "bunx");
        let npx = launch(Runner::Npx, &target(Some("http://127.0.0.1:9")));
        assert_eq!(&npx.args[..2], ["--yes", PACKAGE]);
        assert!(npx.display().contains("--port 9"));
    }

    #[test]
    fn default_listen_is_3080_and_19387_is_only_an_explicit_endpoint() {
        let default = target(None);
        assert_eq!(default.host, "127.0.0.1");
        assert_eq!(default.port, 3080);
        assert_eq!(default.url, DEFAULT_DEEPSEEK_URL);
        assert!(!default.url.contains("19387"));
        let legacy = target(Some("http://127.0.0.1:19387"));
        assert_eq!(legacy.port, 19387);
        let omitted_port = target(Some("http://127.0.0.1"));
        assert_eq!(omitted_port.port, 80);
        assert_eq!(omitted_port.url, "http://127.0.0.1");
        // Scheme-default ports are real overrides. The URL parser reports
        // them as absent; spawning on 3080 would miss a harness on :80.
        let http_default = target(Some("http://127.0.0.1:80/harness"));
        assert_eq!(http_default.port, 80);
        assert_eq!(http_default.host, "127.0.0.1");
        assert_eq!(http_default.url, "http://127.0.0.1/harness");
        let https_default = target(Some("https://[::1]:443"));
        assert_eq!(https_default.port, 443);
        assert_eq!(https_default.host, "::1");
        assert_eq!(https_default.url, "https://[::1]");
    }

    #[test]
    fn configured_endpoints_preserve_prefix_and_url_port_semantics() {
        for (url, host, port, base) in [
            (
                "http://example.test/proxy/dsh/",
                "example.test",
                80,
                "http://example.test/proxy/dsh",
            ),
            (
                "https://example.test/proxy/dsh",
                "example.test",
                443,
                "https://example.test/proxy/dsh",
            ),
            (
                "http://example.test:80/proxy/",
                "example.test",
                80,
                "http://example.test/proxy",
            ),
            (
                "https://example.test:443/proxy/",
                "example.test",
                443,
                "https://example.test/proxy",
            ),
            (
                "http://example.test:4000/proxy/dsh/",
                "example.test",
                4000,
                "http://example.test:4000/proxy/dsh",
            ),
            ("http://[::1]:80/proxy/", "::1", 80, "http://[::1]/proxy"),
            (
                "https://[::1]:8443/proxy/",
                "::1",
                8443,
                "https://[::1]:8443/proxy",
            ),
            (
                "example.test:4000/proxy/",
                "example.test",
                4000,
                "http://example.test:4000/proxy",
            ),
        ] {
            let target = target(Some(url));
            assert_eq!(target.host, host, "{url}");
            assert_eq!(target.port, port, "{url}");
            assert_eq!(target.url, base, "{url}");
        }
    }

    #[test]
    fn invalid_endpoint_is_not_a_listen_target() {
        for url in [
            "http://",
            "http://host:65536",
            "http://[::1",
            "ftp://example.test",
            "file:///path",
            "",
            "http://user:secret@127.0.0.1:80/harness",
        ] {
            assert!(listen_target(Some(url)).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn configured_probe_and_adoption_keep_reverse_proxy_prefix() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/proxy/dsh/", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/proxy/dsh/api/session/modelCatalog",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "type": "server-response", "rpcId": "probe",
                    "result": {"ok": true, "value": {
                        "default": {"provider": "deepseek", "model": "chat"},
                        "routableProviders": ["deepseek"], "groups": [], "failures": []
                    }}
                }))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = AgentRuntime::with_configs(
            super::super::OpencodeConfig::default(),
            DeepseekConfig {
                endpoint: Some(url.clone()),
                token: Some("test-token".into()),
                secret: Some("test-secret".into()),
                ..DeepseekConfig::default()
            },
            super::super::T3Config::default(),
            None,
        );
        let endpoint = runtime.healthy_deepseek_with_override(None).await;
        server.abort();
        let endpoint = endpoint.expect("configured reverse proxy is healthy");
        assert_eq!(endpoint.url, url.trim_end_matches('/'));
        assert_eq!(
            endpoint.ws_url,
            format!(
                "{}/api/remote.mux",
                url.trim_end_matches('/').replacen("http://", "ws://", 1)
            )
        );
    }

    #[tokio::test]
    async fn configured_probe_normalizes_base_and_rejects_userinfo() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let app = axum::Router::new().route(
            "/proxy/api/session/modelCatalog",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let count = Arc::clone(&count);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(headers["host"], address.to_string());
                    axum::Json(serde_json::json!({
                        "type": "server-response", "rpcId": "probe",
                        "result": {"ok": true, "value": {
                            "default": {"provider": "deepseek", "model": "chat"},
                            "routableProviders": ["deepseek"], "groups": [], "failures": []
                        }}
                    }))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for raw in [
            format!("{address}/proxy/"),
            format!("http://{address}/proxy?x=1"),
            format!("http://{address}/proxy#fragment"),
        ] {
            let runtime = AgentRuntime::with_configs(
                super::super::OpencodeConfig::default(),
                DeepseekConfig {
                    endpoint: Some(raw.clone()),
                    token: Some("test-token".into()),
                    secret: Some("test-secret".into()),
                    ..DeepseekConfig::default()
                },
                super::super::T3Config::default(),
                None,
            );
            assert!(
                runtime.supervise_deepseek_with_override(None).await,
                "{raw}"
            );
            let manager = runtime
                .manager_for_agent("deepseek")
                .await
                .expect("adopted manager");
            assert_eq!(manager.endpoint_url(), format!("http://{address}/proxy"));
            assert_eq!(*runtime.deepseek_origin.read().await, AgentOrigin::Adopted);
            manager.shutdown();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        for raw in [
            format!("http://user:password@{address}/proxy"),
            format!("ftp://{address}/proxy"),
            format!("http://{address}:invalid/proxy"),
            String::new(),
        ] {
            let runtime = AgentRuntime::with_configs(
                super::super::OpencodeConfig::default(),
                DeepseekConfig {
                    endpoint: Some(raw),
                    token: Some("test-token".into()),
                    secret: Some("test-secret".into()),
                    ..DeepseekConfig::default()
                },
                super::super::T3Config::default(),
                None,
            );
            assert!(runtime
                .healthy_deepseek_with_override(Some(&format!("http://{address}/proxy")))
                .await
                .is_none());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "invalid config must not probe raw URLs or fall back to environment"
        );
        server.abort();
    }

    #[test]
    fn environment_override_precedence_includes_invalid_values() {
        assert_eq!(
            environment_endpoint(|key| Some(
                if key == "DEEPSEEK_HARNESS_URL" {
                    ""
                } else {
                    "http://other"
                }
                .into()
            )),
            Some(String::new()),
            "a malformed explicit override must not fall back to DSH_URL"
        );
        assert_eq!(
            environment_endpoint(|key| (key == "DSH_URL").then(|| "http://dsh".into())),
            Some("http://dsh".into())
        );
    }

    #[tokio::test]
    async fn readonly_discovery_normalizes_configured_base_without_adopting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/proxy", listener.local_addr().unwrap());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let app = axum::Router::new().route(
            "/proxy/api/session/modelCatalog",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let count = Arc::clone(&count);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "type": "server-response", "rpcId": body["rpcId"],
                        "result": {"ok": true, "value": {
                            "default": {"provider": "deepseek", "model": "chat"},
                            "routableProviders": ["deepseek"], "groups": [], "failures": []
                        }}
                    }))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for raw in [
            url.trim_start_matches("http://").to_string(),
            format!("{url}/?x=1"),
            format!("{url}#fragment"),
        ] {
            let runtime = AgentRuntime::with_all_configs(
                super::super::OpencodeConfig {
                    enabled: false,
                    ..Default::default()
                },
                DeepseekConfig {
                    endpoint: Some(raw),
                    token: Some("test-token".into()),
                    secret: Some("test-secret".into()),
                    ..Default::default()
                },
                super::super::T3Config::default(),
                super::super::CodexConfig {
                    enabled: false,
                    ..Default::default()
                },
                None,
            );
            let found = runtime.probe_agents().await;
            let deepseek = found
                .agents
                .iter()
                .find(|agent| agent.id == "deepseek")
                .unwrap();
            assert_eq!(
                deepseek.status,
                crate::discovery::AgentAvailability::Reachable
            );
            assert_eq!(deepseek.endpoint.as_deref(), Some(url.as_str()));
            assert!(
                runtime.manager_for_agent("deepseek").await.is_none(),
                "discovery is read-only"
            );
            assert!(runtime.deepseek_spawn.lock().await.url.is_none());
        }
        assert!(calls.load(Ordering::SeqCst) >= 3);
        server.abort();
    }

    #[tokio::test]
    async fn readonly_environment_override_is_exclusive_and_normalized() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let app = axum::Router::new().fallback(move |uri: axum::extract::OriginalUri| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push(uri.0.to_string());
                axum::http::StatusCode::UNAUTHORIZED
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = AgentRuntime::with_all_configs(
            super::super::OpencodeConfig {
                enabled: false,
                ..Default::default()
            },
            DeepseekConfig {
                enabled: true,
                token: Some("test-token".into()),
                secret: Some("test-secret".into()),
                ..Default::default()
            },
            super::super::T3Config::default(),
            super::super::CodexConfig {
                enabled: false,
                ..Default::default()
            },
            None,
        );
        let other = format!("{base}/other");
        runtime
            .deepseek_spawn
            .lock()
            .await
            .note(other.clone(), Arc::new(AtomicBool::new(true)));
        for raw in [
            format!("{base}/explicit")
                .trim_start_matches("http://")
                .to_string(),
            format!("{base}/explicit?x=1"),
            format!("{base}/explicit#fragment"),
        ] {
            let found = runtime
                .probe_agents_with_deepseek_override(Some(&raw))
                .await;
            let deepseek = found
                .agents
                .iter()
                .find(|agent| agent.id == "deepseek")
                .unwrap();
            assert_eq!(
                deepseek.status,
                crate::discovery::AgentAvailability::Offline
            );
            assert_eq!(
                deepseek.endpoint.as_deref(),
                Some(format!("{base}/explicit").as_str())
            );
        }
        for invalid in [
            String::new(),
            "ftp://127.0.0.1".into(),
            format!(
                "http://user:password@{}/explicit",
                base.trim_start_matches("http://")
            ),
        ] {
            let found = runtime
                .probe_agents_with_deepseek_override(Some(&invalid))
                .await;
            let deepseek = found
                .agents
                .iter()
                .find(|agent| agent.id == "deepseek")
                .unwrap();
            assert_eq!(
                deepseek.status,
                crate::discovery::AgentAvailability::Offline
            );
            assert!(deepseek.endpoint.is_none());
        }
        assert_eq!(
            *calls.lock().unwrap(),
            ["/explicit/api/session/modelCatalog"; 3],
            "no credential-bearing request may target an unselected path"
        );
        assert_eq!(
            runtime.deepseek_spawn.lock().await.url.as_deref(),
            Some(other.as_str())
        );
        assert!(runtime.manager_for_agent("deepseek").await.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn failed_environment_override_never_probes_or_spawns_another_target() {
        async fn mock(
            status: axum::http::StatusCode,
        ) -> (
            String,
            Arc<std::sync::atomic::AtomicUsize>,
            tokio::task::JoinHandle<()>,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = Arc::clone(&calls);
            let app = axum::Router::new().route(
                "/api/session/modelCatalog",
                axum::routing::post(move |headers: axum::http::HeaderMap| {
                    let count = Arc::clone(&count);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(headers["authorization"], "Bearer test-token");
                        (
                            status,
                            axum::Json(serde_json::json!({
                                "type": "server-response", "rpcId": "probe",
                                "result": {"ok": true, "value": {
                                    "default": {"provider": "deepseek", "model": "chat"},
                                    "routableProviders": ["deepseek"], "groups": [], "failures": []
                                }}
                            })),
                        )
                    }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (url, calls, server)
        }
        let (explicit, explicit_calls, explicit_server) =
            mock(axum::http::StatusCode::UNAUTHORIZED).await;
        let (other, other_calls, other_server) = mock(axum::http::StatusCode::OK).await;
        let runtime = AgentRuntime::with_configs(
            super::super::OpencodeConfig::default(),
            DeepseekConfig {
                enabled: true,
                token: Some("test-token".into()),
                secret: Some("test-secret".into()),
                ..DeepseekConfig::default()
            },
            super::super::T3Config::default(),
            None,
        );
        runtime
            .deepseek_spawn
            .lock()
            .await
            .note(other.clone(), Arc::new(AtomicBool::new(true)));
        assert!(runtime
            .healthy_deepseek_with_override(Some(&explicit))
            .await
            .is_none());
        assert!(
            !runtime
                .supervise_deepseek_with_override(Some(&explicit))
                .await
        );
        for invalid in ["", "ftp://127.0.0.1", "http://user:password@127.0.0.1"] {
            assert!(
                !runtime
                    .supervise_deepseek_with_override(Some(invalid))
                    .await
            );
        }
        assert_eq!(explicit_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            other_calls.load(Ordering::SeqCst),
            0,
            "credentials must never reach a fallback target"
        );
        assert_eq!(
            runtime.deepseek_spawn.lock().await.url.as_deref(),
            Some(other.as_str()),
            "failed override must not spawn a replacement"
        );
        let adopted = runtime
            .healthy_deepseek_with_override(Some(&other))
            .await
            .unwrap();
        assert_eq!(adopted.url, other);
        assert_eq!(other_calls.load(Ordering::SeqCst), 1);
        let configured = AgentRuntime::with_configs(
            super::super::OpencodeConfig::default(),
            DeepseekConfig {
                endpoint: Some(other.clone()),
                token: Some("test-token".into()),
                secret: Some("test-secret".into()),
                ..DeepseekConfig::default()
            },
            super::super::T3Config::default(),
            None,
        );
        assert!(configured
            .healthy_deepseek_with_override(Some(&explicit))
            .await
            .is_some());
        assert_eq!(
            explicit_calls.load(Ordering::SeqCst),
            2,
            "config takes precedence over environment"
        );
        assert_eq!(other_calls.load(Ordering::SeqCst), 2);
        explicit_server.abort();
        other_server.abort();
    }

    #[test]
    fn a_healthy_endpoint_is_adopted_and_does_not_ask_for_a_runner() {
        let lookups = std::cell::Cell::new(0);
        let decision = decide(&enabled(None), true, false, |_| {
            lookups.set(lookups.get() + 1);
            true
        });
        assert_eq!(decision, Decision::Adopt);
        assert_eq!(lookups.get(), 0);
    }

    #[test]
    fn an_unhealthy_endpoint_spawns_once_and_a_live_child_blocks_a_second() {
        let spawn = decide(
            &enabled(Some("http://127.0.0.1:4000")),
            false,
            false,
            lookup(&["dsh"]),
        );
        let Decision::Spawn(launch) = spawn else {
            panic!("expected spawn, got {spawn:?}");
        };
        assert_eq!(launch.program, "dsh");
        assert!(launch.display().contains("--host 127.0.0.1"));
        assert!(launch.display().contains("--port 4000"));
        assert!(!launch.display().contains("19387"));

        let lookups = std::cell::Cell::new(0);
        let again = decide(&enabled(Some("http://127.0.0.1:4000")), false, true, |_| {
            lookups.set(lookups.get() + 1);
            true
        });
        assert_eq!(again, Decision::Leave { warning: None });
        assert_eq!(
            lookups.get(),
            0,
            "a live child must not start another runner"
        );
    }

    #[test]
    fn autostart_off_and_disabled_do_not_spawn_or_look_up_a_runner() {
        let lookups = std::cell::Cell::new(0);
        let lookup = |_: &str| {
            lookups.set(lookups.get() + 1);
            true
        };
        let mut config = enabled(None);
        config.autostart = false;
        assert_eq!(
            decide(&config, false, false, lookup),
            Decision::Leave { warning: None }
        );
        config.autostart = true;
        config.enabled = false;
        assert_eq!(
            decide(&config, false, false, lookup),
            Decision::Leave { warning: None }
        );
        assert_eq!(lookups.get(), 0);
    }

    #[test]
    fn a_missing_runner_is_not_fatal() {
        let decision = decide(&enabled(None), false, false, lookup(&[]));
        assert_eq!(
            decision,
            Decision::Leave {
                warning: Some(MISSING_RUNNER)
            }
        );
        let line = setup_manual_line(&target(None), lookup(&[]));
        assert!(line.contains("not started"));
        assert!(line.contains("keeps running"));
        assert!(!line.contains("--bun"));
    }

    #[test]
    fn setup_names_the_runner_it_would_use_and_the_endpoint_port() {
        let target = target(Some("http://127.0.0.1:3080"));
        assert_eq!(
            setup_manual_line(&target, lookup(&["dsh", "bunx", "npx"])),
            "Manual command: dsh web --no-open --host 127.0.0.1 --port 3080"
        );
        assert_eq!(
            setup_manual_line(&target, lookup(&["bunx", "npx"])),
            "Manual command: bunx @deepseek-ai/dsh web --no-open --host 127.0.0.1 --port 3080"
        );
        let npx = setup_manual_line(&target, lookup(&["npx"]));
        assert!(npx.starts_with("Manual command: npx --yes @deepseek-ai/dsh web --no-open "));
        assert!(npx.contains("--port 3080"));
        assert!(!npx.contains("--bun"));
        assert!(setup_summary(&target.url).contains("starts or joins"));
    }

    #[test]
    fn an_old_config_without_autostart_stays_off_until_enabled() {
        // Same default as OpencodeConfig::autostart. `enabled` still defaults
        // to false, so a config that never turned DeepSeek on does not spawn.
        let old: DeepseekConfig = serde_json::from_str("{}").unwrap();
        assert!(!old.enabled);
        assert!(old.autostart);
        assert!(crate::is_default_deepseek(&old));

        let turned_on: DeepseekConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(turned_on.enabled);
        assert!(turned_on.autostart);
        assert!(!crate::is_default_deepseek(&turned_on));

        let adopt_only: DeepseekConfig =
            serde_json::from_str(r#"{"enabled":true,"autostart":false}"#).unwrap();
        assert!(!adopt_only.autostart);
        assert!(!crate::is_default_deepseek(&adopt_only));
        assert!(DeepseekConfig::default().autostart);
        // A secret alone must still be written. Treating that config as the
        // default omits the whole block and drops the secret on the next save.
        let with_secret = DeepseekConfig {
            secret: Some("cookie-secret".into()),
            ..DeepseekConfig::default()
        };
        assert!(!crate::is_default_deepseek(&with_secret));
    }

    #[tokio::test]
    async fn spawn_detached_records_the_launch_and_does_not_exec() {
        let launch = launch(Runner::Dsh, &target(Some("http://127.0.0.1:3080")));
        {
            let mut log = test_spawns().lock().expect("spawn log");
            log.clear();
        }
        let running = spawn_detached(&launch).await.expect("stubbed spawn");
        assert!(running.load(Ordering::SeqCst));
        let log = test_spawns().lock().expect("spawn log");
        assert_eq!(log.as_slice(), &[launch]);
    }
}
