//! Keeping an OpenCode engine attached for the life of the gateway.
//!
//! Discovery used to run once at startup and the result was final: if OpenCode
//! was not running at that moment every agent route answered 503 for the life
//! of the process, and if OpenCode restarted on a new port -- its port is
//! ephemeral -- the gateway kept the dead URL forever. Restarting OpenCode
//! meant restarting the gateway.
//!
//! This supervises instead. It adopts a healthy service, starts one when there
//! is none and the owner has left autostart on, re-discovers whenever the
//! stream or the health probe says the engine has gone, and hands every route
//! whatever manager is current.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};

use super::adapters::opencode::OpencodeEndpoint;
use super::domain::AgentDomainEvent;
use super::manager::AgentManager;

/// How often the supervisor checks a healthy engine.
const HEALTHY_POLL: Duration = Duration::from_secs(15);
/// How soon it retries after finding none.
const UNHEALTHY_POLL: Duration = Duration::from_secs(3);
/// How long a freshly spawned `opencode serve --service` is given to register.
const STARTUP_WAIT: Duration = Duration::from_secs(20);
const STARTUP_POLL: Duration = Duration::from_millis(400);
/// Backoff ceiling between failed start attempts.
const MAX_START_BACKOFF: Duration = Duration::from_secs(120);
/// How often the supervisor glances at the event stream while the engine is
/// otherwise healthy. A lost stream *is* the engine going away, and waiting
/// for the next health poll to notice it cost thirteen seconds of silence in
/// the app for no reason -- the flag is an atomic read, so this is nearly free.
const STREAM_WATCH_TICK: Duration = Duration::from_secs(1);
/// The wait before re-discovering after the stream dropped. Nothing the first
/// time: look at once. Then doubling, so a stream that flaps cannot spin the
/// supervisor.
const STREAM_LOSS_FIRST_WAIT: Duration = Duration::ZERO;
const STREAM_LOSS_MAX_WAIT: Duration = Duration::from_secs(30);
/// The engine major version this gateway speaks. v1 is a different API, and
/// half-working with it is worse than saying so.
const MIN_OPENCODE_MAJOR: u64 = 2;

/// `opencode` in `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeConfig {
    /// Start `opencode serve --service` when no healthy service is found.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// The binary to start, when `PATH` is not the right answer.
    ///
    /// Absent, the gateway runs whatever `opencode` `PATH` resolves to, and
    /// says which file that turned out to be. It does not go looking in an
    /// install directory of its own: where OpenCode lives differs per OS and
    /// per install, and a gateway guessing at it would quietly run a different
    /// binary than the one the owner's shell does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            binary: None,
        }
    }
}

/// How the engine currently attached was obtained, for the status route and
/// the log line an operator reads after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOrigin {
    /// A service that was already running.
    Adopted,
    /// One this gateway started.
    Spawned,
    /// Nothing attached.
    None,
}

/// Whether this gateway can establish that OpenCode is installed locally.
///
/// This deliberately says nothing about whether a service is currently
/// reachable. An externally configured endpoint is not necessarily local, so
/// its installation cannot be determined from this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineInstallation {
    Installed,
    NotFound,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub available: bool,
    pub installation: EngineInstallation,
    pub origin: EngineOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub stream_connected: bool,
    pub autostart: bool,
}

pub struct AgentRuntime {
    manager: RwLock<Option<Arc<AgentManager>>>,
    /// The event channel belongs to the runtime, not to a manager, so a
    /// subscriber keeps its stream across a reconnect.
    events_tx: broadcast::Sender<AgentDomainEvent>,
    config: OpencodeConfig,
    origin: RwLock<EngineOrigin>,
    /// The child this gateway started, if any. Held so a second one is never
    /// spawned while the first is alive.
    child: Mutex<Option<tokio::process::Child>>,
    supervising: AtomicBool,
}

impl AgentRuntime {
    pub fn new(config: OpencodeConfig) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            manager: RwLock::new(None),
            events_tx,
            config,
            origin: RwLock::new(EngineOrigin::None),
            child: Mutex::new(None),
            supervising: AtomicBool::new(false),
        })
    }

    /// A runtime that will never attach an engine, for tests and for a build
    /// of `AppState` that has no business starting anything.
    pub fn disabled() -> Arc<Self> {
        Self::new(OpencodeConfig {
            autostart: false,
            binary: None,
        })
    }

    /// The engine as it stands, or `None` while nothing is attached. Every
    /// agent route asks here rather than holding a manager of its own.
    pub async fn manager(&self) -> Option<Arc<AgentManager>> {
        self.manager.read().await.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events_tx.subscribe()
    }

    pub async fn status(&self) -> EngineStatus {
        let manager = self.manager.read().await.clone();
        EngineStatus {
            available: manager.is_some(),
            installation: if manager.is_some() {
                EngineInstallation::Installed
            } else {
                local_installation_status(&self.config)
            },
            origin: *self.origin.read().await,
            url: manager.as_ref().map(|m| m.endpoint_url().to_string()),
            version: manager
                .as_ref()
                .and_then(|m| m.driver().client().endpoint.version.clone()),
            stream_connected: manager
                .as_ref()
                .map(|m| m.stream_connected())
                .unwrap_or(false),
            autostart: self.config.autostart,
        }
    }

    /// Attach an engine now, and keep one attached. Safe to call once.
    pub fn spawn_supervisor(self: &Arc<Self>) {
        if self.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut start_backoff = Duration::from_secs(2);
            let mut stream_loss_wait = STREAM_LOSS_FIRST_WAIT;
            loop {
                let healthy = runtime.check_and_repair(&mut start_backoff).await;
                if healthy {
                    runtime.watch_while_healthy(&mut stream_loss_wait).await;
                } else {
                    tokio::time::sleep(UNHEALTHY_POLL).await;
                }
            }
        });
    }

    /// Hold until the engine is worth checking again.
    ///
    /// Normally that is the next health poll. But the event stream dropping is
    /// the engine telling us it has gone, and that should not wait: this
    /// returns within a tick of the stream going down, so the gap between
    /// OpenCode dying and the gateway re-attaching is about a second rather
    /// than however much of the poll interval was left.
    async fn watch_while_healthy(&self, stream_loss_wait: &mut Duration) {
        let deadline = tokio::time::Instant::now() + HEALTHY_POLL;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // A full interval with the stream up: whatever flapping there
                // was has settled, so the next loss is looked at immediately.
                *stream_loss_wait = STREAM_LOSS_FIRST_WAIT;
                return;
            }
            tokio::time::sleep(STREAM_WATCH_TICK.min(remaining)).await;
            if self.stream_down().await {
                tracing::warn!(
                    wait_s = stream_loss_wait.as_secs(),
                    "opencode event stream is down, re-discovering"
                );
                if !stream_loss_wait.is_zero() {
                    tokio::time::sleep(*stream_loss_wait).await;
                }
                *stream_loss_wait = next_stream_loss_wait(*stream_loss_wait);
                return;
            }
        }
    }

    /// True when an engine is attached but its event stream has dropped.
    async fn stream_down(&self) -> bool {
        self.manager
            .read()
            .await
            .as_ref()
            .map(|m| !m.stream_connected())
            .unwrap_or(false)
    }

    /// One supervision pass. Returns whether an engine is attached and well.
    async fn check_and_repair(&self, start_backoff: &mut Duration) -> bool {
        let current = self.manager.read().await.clone();

        // Whatever the registration file says now wins: OpenCode's port is
        // ephemeral, so a restart moves it and the old URL is dead.
        let discovered = OpencodeEndpoint::discover().await;

        if let Some(manager) = current {
            let same_endpoint = discovered
                .as_ref()
                .map(|e| e.url == manager.endpoint_url())
                .unwrap_or(false);
            if same_endpoint && self.probe(manager.endpoint_url()).await {
                return true;
            }
            tracing::warn!(
                url = manager.endpoint_url(),
                "opencode engine unhealthy or moved, re-discovering"
            );
            manager.shutdown();
            *self.manager.write().await = None;
            *self.origin.write().await = EngineOrigin::None;
        }

        // Adopt anything already healthy before starting anything -- but not
        // a v1 service. Attaching to one used to look like success and then
        // fail on every route, which is a worse answer than refusing here.
        if let Some(endpoint) = discovered {
            if endpoint.probe_healthy(&probe_client()).await {
                if let Err(refusal) = check_version(endpoint.version.as_deref()) {
                    tracing::error!(
                        "refusing to use the OpenCode service at {}: {refusal}",
                        endpoint.url
                    );
                    return false;
                }
                self.attach(endpoint, EngineOrigin::Adopted).await;
                *start_backoff = Duration::from_secs(2);
                return true;
            }
        }

        if !self.config.autostart {
            tracing::debug!("no opencode service and autostart is off");
            return false;
        }

        match self.start_service().await {
            Ok(endpoint) => {
                self.attach(endpoint, EngineOrigin::Spawned).await;
                *start_backoff = Duration::from_secs(2);
                true
            }
            Err(err) => {
                tracing::warn!(%err, backoff_s = start_backoff.as_secs(), "could not start opencode");
                tokio::time::sleep(*start_backoff).await;
                *start_backoff = (*start_backoff * 2).min(MAX_START_BACKOFF);
                false
            }
        }
    }

    async fn attach(&self, endpoint: OpencodeEndpoint, origin: EngineOrigin) {
        let url = endpoint.url.clone();
        let version = endpoint.version.clone();
        // Which file is actually serving this. For a service this gateway
        // started that is the path it resolved; for one it adopted it is read
        // off the running process, because "which opencode am I talking to" is
        // the question an operator has after a restart and a bare `opencode`
        // does not answer it.
        let path = endpoint
            .pid
            .and_then(running_binary_path)
            .map(|p| p.display().to_string());
        let manager = Arc::new(AgentManager::connect(endpoint, self.events_tx.clone()));
        *self.manager.write().await = Some(manager);
        *self.origin.write().await = origin;
        let path = path.unwrap_or_else(|| "unknown".to_string());
        match origin {
            EngineOrigin::Adopted => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                binary = %path,
                "adopted the running OpenCode service"
            ),
            EngineOrigin::Spawned => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                binary = %path,
                "started an OpenCode service and attached to it"
            ),
            EngineOrigin::None => {}
        }
    }

    async fn probe(&self, url: &str) -> bool {
        let Some(manager) = self.manager.read().await.clone() else {
            return false;
        };
        let endpoint = manager.driver().client().endpoint.clone();
        if endpoint.url != url {
            return false;
        }
        endpoint.probe_healthy(&probe_client()).await
    }

    /// Start `opencode serve --service`, detached, and wait for it to register.
    ///
    /// Never more than one: a child this gateway started and has not reaped is
    /// given the benefit of the doubt, and a service someone else is running is
    /// adopted before this is ever reached.
    async fn start_service(&self) -> anyhow::Result<OpencodeEndpoint> {
        let mut slot = self.child.lock().await;
        if let Some(ref mut child) = *slot {
            match child.try_wait() {
                // Still running: it simply has not registered yet.
                Ok(None) => {
                    drop(slot);
                    return wait_for_service().await;
                }
                Ok(Some(status)) => {
                    tracing::warn!(%status, "the OpenCode service this gateway started has exited");
                }
                Err(err) => {
                    tracing::warn!(%err, "could not check the spawned OpenCode service");
                }
            }
            *slot = None;
        }

        // `opencode.binary` if the owner set one, otherwise whatever `opencode`
        // means on PATH -- and then the file that resolved to, so the log names
        // a path rather than a word.
        let binary = match resolve_binary(self.config.binary.as_deref()) {
            Ok(path) => path,
            Err(err) => {
                anyhow::bail!("{err}");
            }
        };
        let version = binary_version(&binary);
        if let Err(refusal) = check_version(version.as_deref()) {
            // One line, naming the file and what it said, because the reader
            // has to go and fix an install.
            tracing::error!("refusing to start {}: {refusal}", binary.display());
            anyhow::bail!("{} is not OpenCode 2.x", binary.display());
        }
        tracing::info!(
            binary = %binary.display(),
            version = version.as_deref().unwrap_or("unknown"),
            "no OpenCode service found, starting one"
        );

        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("serve")
            .arg("--service")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Its own process group, so it survives this gateway and does not
            // take a terminal signal meant for it.
            .process_group(0)
            .kill_on_drop(false);

        let child = command.spawn().map_err(|e| {
            anyhow::anyhow!("could not run `{} serve --service`: {e}", binary.display())
        })?;
        tracing::info!(pid = child.id(), "OpenCode service starting");
        *slot = Some(child);
        drop(slot);

        wait_for_service().await
    }
}

/// The wait after a stream loss, given the last one.
///
/// The first loss is looked at immediately, because that is the common case
/// and the whole point. Repeated losses without a settled interval in between
/// mean something is flapping, and the supervisor backs off rather than
/// re-discovering once a second forever.
fn next_stream_loss_wait(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_secs(1)
    } else {
        (current * 2).min(STREAM_LOSS_MAX_WAIT)
    }
}

/// The major version out of whatever `--version` or a registration said:
/// `opencode v2.0.1`, `2.0.1`, `v2.0.1-beta.3`.
fn parse_major(version: &str) -> Option<u64> {
    version
        .split_whitespace()
        .filter_map(|word| {
            let word = word.trim_start_matches(['v', 'V']);
            let head = word.split(['.', '-', '+']).next()?;
            head.parse::<u64>().ok().map(|major| (major, word))
        })
        // A bare `2` is not a version string; take the first word that looks
        // like one, so `opencode v2.0.1` is not read as the `opencode` in it.
        .find(|(_, word)| word.contains('.'))
        .map(|(major, _)| major)
        .or_else(|| {
            version
                .trim()
                .trim_start_matches(['v', 'V'])
                .parse::<u64>()
                .ok()
        })
}

/// Whether a version is one this gateway will talk to.
///
/// An unreadable or absent version is allowed through: it cannot be shown to
/// be too old, and refusing on silence would break an install that simply does
/// not report one. Only a version that is legible *and* below 2.0 is refused.
fn check_version(version: Option<&str>) -> Result<(), String> {
    let Some(version) = version.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let Some(major) = parse_major(version) else {
        return Ok(());
    };
    if major >= MIN_OPENCODE_MAJOR {
        return Ok(());
    }
    Err(format!(
        "it reports version {version}, and this gateway speaks OpenCode \
         {MIN_OPENCODE_MAJOR}.x only. Install OpenCode 2, or point the gateway \
         at the right one by setting `opencode.binary` to its absolute path in \
         config.json"
    ))
}

/// `opencode.binary` if the owner set one, else `opencode` as `PATH` resolves
/// it -- and in both cases the file it actually is.
///
/// No install directory is guessed at. Where OpenCode lives differs per OS and
/// per install, and a gateway reaching into one of its own would quietly run a
/// different binary than the owner's shell does, which is the confusion this
/// is here to end.
fn resolve_binary(configured: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    resolve_binary_in(configured, std::env::var_os("PATH").as_deref())
}

/// The lookup itself, with `PATH` passed in rather than read, so a test can
/// exercise the order without reaching into the process environment that every
/// other thread is also using.
fn resolve_binary_in(
    configured: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
) -> anyhow::Result<std::path::PathBuf> {
    let name = binary_name(configured);

    // Anything with a separator is a path the owner meant literally, and a
    // missing one is an error naming it rather than a quiet fall back to PATH:
    // they asked for that file.
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let path = std::path::PathBuf::from(name);
        if !path.is_file() {
            anyhow::bail!("`opencode.binary` is set to {name}, and there is no file there");
        }
        return Ok(absolute(path));
    }

    let path_var = path_var
        .ok_or_else(|| anyhow::anyhow!("PATH is not set, so `{name}` cannot be resolved"))?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(absolute(candidate));
        }
    }
    anyhow::bail!(
        "`{name}` is not on PATH. Install OpenCode 2, or set `opencode.binary` \
         to its absolute path in config.json"
    )
}

fn binary_name(configured: Option<&str>) -> &str {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("opencode")
}

/// Classify installation without starting OpenCode or contacting an endpoint.
/// The inputs are injected to keep lookup tests independent of global process
/// environment.
fn installation_status_in(
    configured: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
    external_endpoint_configured: bool,
) -> EngineInstallation {
    if external_endpoint_configured {
        return EngineInstallation::Unknown;
    }

    let name = binary_name(configured);
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        return match std::fs::metadata(name) {
            Ok(metadata) if metadata_is_executable(&metadata) => EngineInstallation::Installed,
            Ok(_) => EngineInstallation::NotFound,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => EngineInstallation::NotFound,
            Err(_) => EngineInstallation::Unknown,
        };
    }

    let Some(path_var) = path_var else {
        return EngineInstallation::Unknown;
    };
    let mut ambiguous = false;
    for directory in std::env::split_paths(path_var) {
        let candidate = directory.join(name);
        match std::fs::metadata(&candidate) {
            Ok(metadata) if metadata_is_executable(&metadata) => {
                return EngineInstallation::Installed;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => ambiguous = true,
        }
    }
    if ambiguous {
        EngineInstallation::Unknown
    } else {
        EngineInstallation::NotFound
    }
}

fn local_installation_status(config: &OpencodeConfig) -> EngineInstallation {
    let Ok(external_endpoint_configured) =
        external_endpoint_configured_with(|name| std::env::var(name))
    else {
        return EngineInstallation::Unknown;
    };
    installation_status_in(
        config.binary.as_deref(),
        std::env::var_os("PATH").as_deref(),
        external_endpoint_configured,
    )
}

fn external_endpoint_configured_with(
    mut read: impl FnMut(&str) -> Result<String, std::env::VarError>,
) -> Result<bool, ()> {
    for name in ["OPENCODE_URL", "HERDR_GATEWAY_OPENCODE_URL"] {
        match read(name) {
            Ok(url) => return Ok(!url.trim().is_empty()),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => return Err(()),
        }
    }
    Ok(false)
}

fn absolute(path: std::path::PathBuf) -> std::path::PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata_is_executable(&metadata))
        .unwrap_or(false)
}

/// What `<binary> --version` says, or `None` if it cannot be asked.
fn binary_version(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        text.to_string()
    };
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// The executable behind a running pid, where the platform will say.
#[cfg(target_os = "linux")]
fn running_binary_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn running_binary_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
}

/// Poll the registration file until the service answers its health probe, or
/// give up after `STARTUP_WAIT`. Bounded on purpose: the caller backs off and
/// tries again rather than blocking the supervisor forever.
async fn wait_for_service() -> anyhow::Result<OpencodeEndpoint> {
    let client = probe_client();
    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    loop {
        if let Some(endpoint) = OpencodeEndpoint::discover().await {
            if endpoint.probe_healthy(&client).await {
                return Ok(endpoint);
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the OpenCode service did not become healthy within {}s",
                STARTUP_WAIT.as_secs()
            );
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_defaults_to_on_and_the_binary_is_optional() {
        let config: OpencodeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(config.autostart);
        assert!(config.binary.is_none());
    }

    #[test]
    fn autostart_can_be_turned_off() {
        let config: OpencodeConfig =
            serde_json::from_str(r#"{"autostart":false,"binary":"/opt/opencode"}"#)
                .expect("config parses");
        assert!(!config.autostart);
        assert_eq!(config.binary.as_deref(), Some("/opt/opencode"));
    }

    /// The version gate is about the major number, and the string it reads
    /// comes from three different places in three different shapes.
    #[test]
    fn a_version_is_read_out_of_whatever_shape_it_arrives_in() {
        assert_eq!(parse_major("opencode v2.0.1"), Some(2));
        assert_eq!(parse_major("2.0.1"), Some(2));
        assert_eq!(parse_major("v2.0.1-beta.3"), Some(2));
        assert_eq!(parse_major("opencode 1.18.4"), Some(1));
        assert_eq!(parse_major("v10.2.0"), Some(10));
        assert_eq!(parse_major("2"), Some(2));
        assert_eq!(parse_major("not a version"), None);
        assert_eq!(parse_major(""), None);
    }

    /// v1 is a different API. Attaching to it looked like success and then
    /// failed on every route, which is a worse answer than refusing.
    #[test]
    fn a_v1_engine_is_refused_and_the_refusal_says_what_to_do() {
        let refusal = check_version(Some("opencode 1.18.4")).expect_err("v1 is refused");
        assert!(
            refusal.contains("1.18.4"),
            "it names what it found: {refusal}"
        );
        assert!(
            refusal.contains("opencode.binary"),
            "and how to point it elsewhere: {refusal}"
        );

        assert!(check_version(Some("opencode v2.0.1")).is_ok());
        assert!(check_version(Some("v3.0.0")).is_ok());
    }

    /// Silence is not evidence of being old. An install that reports no
    /// version, or one this cannot parse, is allowed through rather than
    /// refused on a guess.
    #[test]
    fn an_unreadable_version_is_not_treated_as_too_old() {
        assert!(check_version(None).is_ok());
        assert!(check_version(Some("")).is_ok());
        assert!(check_version(Some("   ")).is_ok());
        assert!(check_version(Some("unknown build")).is_ok());
    }

    /// `opencode.binary` first, then `opencode` as PATH resolves it. No
    /// install directory is guessed at, so this is the whole order.
    /// `opencode.binary` first, then `opencode` as PATH resolves it. No
    /// install directory is guessed at, so this is the whole order.
    #[test]
    fn the_binary_is_the_configured_one_or_whatever_path_says() {
        let dir =
            std::env::temp_dir().join(format!("muqun-resolve-{}", uuid::Uuid::new_v4().simple()));
        let other = dir.join("elsewhere");
        std::fs::create_dir_all(&other).expect("temp dir");

        let on_path = dir.join("opencode");
        let configured = other.join("opencode-2");
        for file in [&on_path, &configured] {
            std::fs::write(file, "#!/bin/sh\nexit 0\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        let path_var = std::ffi::OsString::from(&dir);
        let empty_path = std::ffi::OsString::from(&other);

        // An absolute `opencode.binary` is taken literally, whatever PATH says.
        assert_eq!(
            resolve_binary_in(Some(configured.to_str().unwrap()), Some(&path_var))
                .expect("configured"),
            std::fs::canonicalize(&configured).unwrap()
        );

        // A configured path that is not there names itself rather than
        // silently falling back to PATH.
        let missing = other.join("not-here");
        let err = resolve_binary_in(Some(missing.to_str().unwrap()), Some(&path_var))
            .expect_err("missing");
        assert!(err.to_string().contains("not-here"), "got {err}");

        // Absent, it is `opencode` on PATH -- and the answer is the file.
        assert_eq!(
            resolve_binary_in(None, Some(&path_var)).expect("found on PATH"),
            std::fs::canonicalize(&on_path).unwrap()
        );
        // A bare name is looked up the same way.
        assert_eq!(
            resolve_binary_in(Some("opencode"), Some(&path_var)).expect("bare name"),
            std::fs::canonicalize(&on_path).unwrap()
        );

        // Nothing named `opencode` anywhere on PATH: an error that says how to
        // fix it, not a guess at an install directory.
        let err = resolve_binary_in(None, Some(&empty_path)).expect_err("not there");
        assert!(err.to_string().contains("not on PATH"), "got {err}");
        assert!(err.to_string().contains("opencode.binary"), "got {err}");

        let err = resolve_binary_in(None, None).expect_err("no PATH at all");
        assert!(err.to_string().contains("PATH is not set"), "got {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installation_status_distinguishes_local_lookup_from_service_readiness() {
        let dir = std::env::temp_dir().join(format!(
            "muqun-installation-status-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let binary = dir.join("opencode");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let path_var = std::ffi::OsString::from(&dir);
        let empty_path = std::ffi::OsString::from(dir.join("empty"));
        let missing = dir.join("missing-opencode");
        let non_executable = dir.join("not-executable");
        std::fs::write(&non_executable, "not a program\n").expect("write non-executable");

        assert_eq!(
            installation_status_in(None, Some(&path_var), false),
            EngineInstallation::Installed,
            "a local executable is installed even while its service is down"
        );
        assert_eq!(
            installation_status_in(Some(missing.to_str().unwrap()), Some(&path_var), false),
            EngineInstallation::NotFound,
            "a broken explicit path does not fall back to PATH"
        );
        #[cfg(unix)]
        assert_eq!(
            installation_status_in(
                Some(non_executable.to_str().unwrap()),
                Some(&path_var),
                false
            ),
            EngineInstallation::NotFound,
            "an explicit regular file must also be executable"
        );
        assert_eq!(
            installation_status_in(None, Some(&empty_path), false),
            EngineInstallation::NotFound
        );
        assert_eq!(
            installation_status_in(None, None, false),
            EngineInstallation::Unknown,
            "without PATH the local lookup is inconclusive"
        );
        assert_eq!(
            installation_status_in(None, Some(&path_var), true),
            EngineInstallation::Unknown,
            "the local PATH cannot establish installation for an external endpoint"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_environment_errors_make_installation_inconclusive() {
        let unreadable = std::ffi::OsString::from("unreadable");
        assert_eq!(
            external_endpoint_configured_with(|name| match name {
                "OPENCODE_URL" => Err(std::env::VarError::NotUnicode(unreadable.clone())),
                _ => Err(std::env::VarError::NotPresent),
            }),
            Err(())
        );
        assert_eq!(
            external_endpoint_configured_with(|name| match name {
                "OPENCODE_URL" => Err(std::env::VarError::NotPresent),
                "HERDR_GATEWAY_OPENCODE_URL" => Ok(" http://127.0.0.1:4096 ".to_string()),
                _ => unreachable!(),
            }),
            Ok(true)
        );
    }

    #[test]
    fn engine_status_serializes_the_additive_installation_field() {
        let status = EngineStatus {
            available: false,
            installation: EngineInstallation::NotFound,
            origin: EngineOrigin::None,
            url: None,
            version: None,
            stream_connected: false,
            autostart: true,
        };
        let value = serde_json::to_value(status).expect("status serializes");
        assert_eq!(value["installation"], "not_found");
        assert_eq!(value["available"], false);
        assert_eq!(value["origin"], "none");
        assert_eq!(value["autostart"], true);
        assert!(value.get("url").is_none());
        assert!(value.get("version").is_none());
    }

    /// A lost stream is the engine going away, so the first one is looked at
    /// at once; a stream that keeps dropping must not spin the supervisor.
    #[test]
    fn a_flapping_stream_backs_off_but_the_first_loss_does_not_wait() {
        assert_eq!(STREAM_LOSS_FIRST_WAIT, Duration::ZERO);
        let mut wait = STREAM_LOSS_FIRST_WAIT;
        wait = next_stream_loss_wait(wait);
        assert_eq!(wait, Duration::from_secs(1));
        let mut seen = vec![wait];
        for _ in 0..8 {
            wait = next_stream_loss_wait(wait);
            seen.push(wait);
        }
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "it only grows: {seen:?}"
        );
        assert_eq!(*seen.last().unwrap(), STREAM_LOSS_MAX_WAIT, "and it stops");
        assert!(
            STREAM_WATCH_TICK < HEALTHY_POLL,
            "the stream is watched more often than the engine is polled"
        );
    }

    #[tokio::test]
    async fn a_disabled_runtime_attaches_nothing_and_never_spawns() {
        let runtime = AgentRuntime::disabled();
        assert!(runtime.manager().await.is_none());
        let status = runtime.status().await;
        assert!(!status.available);
        assert!(!status.autostart);
        assert_eq!(status.origin, EngineOrigin::None);
        // A subscriber works with no engine attached, which is what lets a
        // stream outlive a reconnect.
        let _rx = runtime.subscribe_events();
    }
}
