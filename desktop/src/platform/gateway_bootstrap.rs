//! First-launch Gateway provisioning delegates to the packaged distribution
//! installer. It never downloads binaries or repairs an existing installation.
use crate::model::inbox::{Inbox, UiAction, Update};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const ATTEMPT: &str = "gateway-bootstrap.attempt";
const DEADLINE: Duration = Duration::from_secs(480);
static CONTEXT: OnceLock<Arc<Context>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Unavailable,
    Checking,
    Existing,
    NeedsConsent,
    Installing,
    Downloading { received: u64, total: u64 },
    Verifying,
    Activating,
    Starting,
    Ready,
    Failed,
    Interrupted,
}

impl Status {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Unavailable => "Gateway setup is available in packaged Desktop installations.",
            Self::Checking => "Checking for a local Gateway…",
            Self::Existing => "An existing Gateway installation was left unchanged.",
            Self::NeedsConsent => "Install Gateway to connect your devices to this computer.",
            Self::Installing => "Preparing Gateway setup…",
            Self::Downloading { .. } => "Downloading Gateway…",
            Self::Verifying => "Verifying the Gateway download…",
            Self::Activating => "Installing Gateway…",
            Self::Starting => "Starting Gateway…",
            Self::Ready => "Local Gateway is ready. Connect when your current work is finished.",
            Self::Failed => "Gateway setup failed or timed out. Retry to continue.",
            Self::Interrupted => {
                "A previous Gateway setup did not finish. It will only run again when you retry."
            }
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(self, Self::NeedsConsent | Self::Failed | Self::Interrupted)
    }
}

struct Context {
    changed: Condvar,
    state: PathBuf,
    installer: Option<PathBuf>,
    inbox: Arc<Inbox>,
    running: AtomicBool,
    cancel: AtomicBool,
    status: Mutex<Status>,
}

pub fn status() -> Status {
    CONTEXT.get().map_or(Status::Unavailable, |context| {
        context.status.lock().unwrap().clone()
    })
}

fn packaged_installer(executable: &Path) -> Option<PathBuf> {
    let prefix = executable.parent()?.parent()?;
    [
        prefix.join("Resources/installers/install-desktop.sh"),
        prefix.join("share/muqun-desktop/installers/install-desktop.sh"),
    ]
    .into_iter()
    .find(|path| path.is_file() && path.with_file_name("install.sh").is_file())
}

pub fn initialize(state_dir: PathBuf, inbox: Arc<Inbox>) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let installer = packaged_installer(&executable);
    // An unpackaged Cargo build runs directly from its build profile directory.
    // A damaged installed package still has to pass the Gateway check.
    if installer.is_none()
        && !executable
            .parent()
            .is_some_and(|dir| dir.ends_with("bin") || dir.ends_with("Contents/MacOS"))
    {
        return;
    }
    let context = Arc::new(Context {
        changed: Condvar::new(),
        state: state_dir,
        installer,
        inbox,
        running: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
        status: Mutex::new(Status::Checking),
    });
    if CONTEXT.set(context.clone()).is_ok() {
        begin(context, false);
    }
}

/// Keep core startup on its own worker until the local Gateway is available.
/// The main event loop and the installer continue independently.
pub fn wait_for_setup() {
    if let Some(context) = CONTEXT.get() {
        drop(
            context
                .changed
                .wait_while(context.status.lock().unwrap(), |status| {
                    !matches!(status, Status::Ready | Status::Existing)
                })
                .unwrap(),
        );
    }
}

pub fn shutdown() {
    if let Some(context) = CONTEXT.get() {
        context.cancel.store(true, Ordering::Release);
        drop(
            context
                .changed
                .wait_timeout_while(
                    context.status.lock().unwrap(),
                    Duration::from_secs(1),
                    |_| context.running.load(Ordering::Acquire),
                )
                .unwrap(),
        );
    }
}

pub fn retry() {
    if let Some(context) = CONTEXT.get() {
        begin(context.clone(), true);
    }
}

fn begin(context: Arc<Context>, consent: bool) {
    if context.running.swap(true, Ordering::AcqRel) {
        return;
    }
    context.cancel.store(false, Ordering::Release);
    std::thread::spawn(move || {
        let result = provision(&context, consent, DEADLINE);
        {
            let mut status = context.status.lock().unwrap();
            *status = result.clone();
            context.running.store(false, Ordering::Release);
            context.changed.notify_all();
        }
        context
            .inbox
            .push(Update::Ui(UiAction::LocalGatewayStatus(result.clone())));
        if result == Status::Ready {
            context.inbox.push(Update::Ui(UiAction::LocalGatewayReady));
        }
    });
}

pub(super) fn search_path() -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local/bin"));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    paths
}

fn existing_evidence(
    paths: &[PathBuf],
    home: Option<&Path>,
    config: Option<&Path>,
    data: Option<&Path>,
) -> std::io::Result<bool> {
    let mut evidence = Vec::new();
    for path in paths {
        evidence.extend([path.join("muqun-gateway"), path.join("herdr-gateway")]);
    }
    let mut configuration = Vec::new();
    for root in [config, data].into_iter().flatten() {
        configuration.extend([root.join("muqun-gateway"), root.join("herdr-gateway")]);
    }
    for name in [
        "MUQUN_GATEWAY_CONFIG_DIR",
        "MUQUN_GATEWAY_STATE_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_STATE_DIR",
    ] {
        if let Some(path) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            configuration.push(path.into());
        }
    }
    for root in configuration {
        evidence.extend([root.join("config.json"), root.join("local-client.json")]);
    }
    if let Some(path) = std::env::var_os("MUQUN_LOCAL_GATEWAY").filter(|value| !value.is_empty()) {
        evidence.push(path.into());
    }
    if let Some(root) = config {
        evidence.push(root.join("systemd/user/dev.osuki.muqun-gateway.service"));
    }
    if let Some(home) = home {
        evidence.push(home.join("Library/LaunchAgents/dev.osuki.muqun-gateway.plist"));
    }
    evidence.extend([
        PathBuf::from("/etc/systemd/system/dev.osuki.muqun-gateway.service"),
        PathBuf::from("/usr/lib/systemd/user/dev.osuki.muqun-gateway.service"),
    ]);
    for path in evidence {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn running_gateway_named(pattern: &str) -> Option<bool> {
    let mut command = Command::new("/usr/bin/pgrep");
    #[cfg(unix)]
    {
        // SAFETY: geteuid only reads this process's identity.
        command.args(["-u", &unsafe { libc::geteuid() }.to_string()]);
    }
    let Ok(mut probe) = command
        .args(["-x", pattern])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return None;
    }; // An unknown process inventory never authorizes installation.
    let started = Instant::now();
    loop {
        match probe.try_wait() {
            Ok(Some(exit)) => {
                return match exit.code() {
                    Some(0) => Some(true),
                    Some(1) => Some(false),
                    _ => None,
                }
            }
            Ok(None) if started.elapsed() < Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = probe.kill();
                let _ = probe.wait();
                return None;
            }
        }
    }
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn provision(context: &Context, consent: bool, deadline: Duration) -> Status {
    if context.cancel.load(Ordering::Acquire) {
        return Status::Interrupted;
    }
    let mut paths = search_path();
    if let Some(bin) =
        std::env::var_os("MUQUN_GATEWAY_INSTALL_DIR").filter(|value| !value.is_empty())
    {
        paths.push(bin.into());
    }
    match existing_evidence(
        &paths,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
        dirs::data_dir().as_deref(),
    ) {
        Ok(true) => return Status::Existing,
        Err(_) => return Status::Failed,
        Ok(false) => {}
    }
    use super::coexist::{self, GatewayPresence};
    match coexist::detect(&coexist::gateway_state_dirs()) {
        GatewayPresence::Running { .. } => return Status::Existing,
        GatewayPresence::Unknown { .. } => return Status::Failed,
        GatewayPresence::NotRunning => {}
    }
    match running_gateway_named("muqun-gateway|herdr-gateway") {
        Some(false) => {}
        Some(true) => return Status::Existing,
        None => return Status::Failed,
    }
    let Some(installer) = &context.installer else {
        return Status::Failed;
    };
    install_once(context, installer, &paths, consent, deadline, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()
            .is_some_and(|runtime| {
                runtime
                    .block_on(crate::hub::discover_local_gateway())
                    .is_some()
            })
    })
}

/// The Desktop instance owns an *attempt*. An unrelated subprocess can
/// inherit the file description across fork, but must not extend the attempt.
#[cfg(unix)]
struct AttemptLock(fs::File);

#[cfg(unix)]
impl AttemptLock {
    fn acquire(file: fs::File) -> Result<Self, Status> {
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(Status::Installing),
            Err(std::fs::TryLockError::Error(_)) => Err(Status::Failed),
        }
    }
}

#[cfg(unix)]
impl Drop for AttemptLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn install_once(
    context: &Context,
    installer: &Path,
    paths: &[PathBuf],
    consent: bool,
    deadline: Duration,
    ready: impl FnOnce() -> bool,
) -> Status {
    if context.cancel.load(Ordering::Acquire) {
        return Status::Interrupted;
    }
    if !consent {
        return if context.state.join(ATTEMPT).exists() {
            Status::Interrupted
        } else {
            Status::NeedsConsent
        };
    }
    if fs::create_dir_all(&context.state).is_err() {
        return Status::Failed;
    }
    let Ok(owner) = private_file(&context.state.join("gateway-bootstrap.lock")) else {
        return Status::Failed;
    };
    #[cfg(unix)]
    let _attempt_lock = match AttemptLock::acquire(owner) {
        Ok(lock) => lock,
        Err(status) => return status,
    };
    let attempt = context.state.join(ATTEMPT);
    // Persist before execution. Even an interrupted app or ambiguous install
    // result requires an explicit retry on the next launch.
    let Ok(mut record) = private_file(&attempt) else {
        return Status::Failed;
    };
    if record
        .write_all(b"started\n")
        .and_then(|_| record.sync_all())
        .is_err()
    {
        return Status::Failed;
    }
    let progress = |status: Status| {
        let mut current = context.status.lock().unwrap();
        if *current != status && !context.cancel.load(Ordering::Acquire) {
            *current = status.clone();
            context
                .inbox
                .push(Update::Ui(UiAction::LocalGatewayStatus(status)));
        }
    };
    progress(Status::Installing);
    let outcome = run_installer(installer, paths, &context.cancel, deadline, progress);
    if outcome != Status::Ready {
        return outcome;
    }
    if ready() {
        Status::Ready
    } else {
        Status::Failed
    }
}

fn run_installer(
    installer: &Path,
    paths: &[PathBuf],
    cancel: &AtomicBool,
    deadline: Duration,
    progress: impl Fn(Status),
) -> Status {
    use super::installer::{self, Phase};
    let mut command = Command::new("/bin/sh");
    command.arg(installer).arg("--gateway-only");
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    let result = installer::run(command, cancel, Some(deadline), |phase| {
        progress(match phase {
            Phase::Downloading { received, total } => Status::Downloading { received, total },
            Phase::Verifying => Status::Verifying,
            Phase::Installing => Status::Activating,
            Phase::Starting => Status::Starting,
        })
    });
    match result {
        Ok(()) => Status::Ready,
        Err(_) if cancel.load(Ordering::Acquire) => Status::Interrupted,
        Err(_) => Status::Failed,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        context: Context,
    }
    impl Fixture {
        fn new(script: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("muqun gateway QA {}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let installer = root.join("install-desktop.sh");
            fs::write(
                &installer,
                format!(
                    r#"set -eu
[ "$#" = 1 ] && [ "$1" = --gateway-only ] || exit 71
if (exec 3< /dev/tty) 2>/dev/null; then exit 72; fi
printf 'attempt\n' >> "${{0%/*}}/attempts"
{script}
"#
                ),
            )
            .unwrap();
            let context = Context {
                changed: Condvar::new(),
                state: root.join("state"),
                installer: Some(installer),
                inbox: Arc::new(Inbox::new()),
                running: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
                status: Mutex::new(Status::Unavailable),
            };
            Self { root, context }
        }
        fn attempt(&self, retry: bool) -> Status {
            install_once(
                &self.context,
                self.context.installer.as_deref().unwrap(),
                &[PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
                retry,
                Duration::from_secs(2),
                || true,
            )
        }
        fn count(&self) -> usize {
            fs::read_to_string(self.root.join("attempts"))
                .unwrap_or_default()
                .lines()
                .count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn first_launch_is_once_and_failure_requires_explicit_retry_without_a_terminal() {
        let fixture = Fixture::new("exit 9");
        assert_eq!(fixture.attempt(false), Status::NeedsConsent);
        assert_eq!(fixture.count(), 0);
        assert_eq!(fixture.attempt(true), Status::Failed);
        assert_eq!(fixture.count(), 1);
        assert_eq!(fixture.attempt(false), Status::Interrupted);
        assert_eq!(fixture.count(), 1);
        fs::write(
            fixture.context.installer.as_ref().unwrap(),
            r#"set -eu
[ "$1" = --gateway-only ] || exit 71
if (exec 3< /dev/tty) 2>/dev/null; then exit 72; fi
printf 'attempt\n' >> "${0%/*}/attempts"
"#,
        )
        .unwrap();
        assert_eq!(fixture.attempt(true), Status::Ready);
        assert_eq!(fixture.count(), 2);
        assert_eq!(fixture.attempt(false), Status::Interrupted);
        assert_eq!(fixture.count(), 2);
    }

    #[test]
    fn consented_install_streams_actual_download_bytes_and_activation_stages() {
        let fixture = Fixture::new(
            r#"
[ "$MUQUN_INSTALL_PROGRESS" = 1 ]
printf 'ordinary installer output\nMUQUN_PROGRESS download 20' >&2
sleep 0.15
printf ' 100\nMUQUN_PROGRESS download 100 100\nMUQUN_PROGRESS verify\nMUQUN_PROGRESS install\nMUQUN_PROGRESS start\n' >&2
"#,
        );
        assert_eq!(fixture.attempt(false), Status::NeedsConsent);
        assert!(!fixture.context.state.exists());
        assert_eq!(fixture.attempt(true), Status::Ready);
        let statuses: Vec<_> = fixture
            .context
            .inbox
            .take()
            .into_iter()
            .filter_map(|update| match update {
                Update::Ui(UiAction::LocalGatewayStatus(status)) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [
                Status::Installing,
                Status::Downloading {
                    received: 20,
                    total: 100
                },
                Status::Downloading {
                    received: 100,
                    total: 100
                },
                Status::Verifying,
                Status::Activating,
                Status::Starting
            ]
        );
    }

    #[test]
    fn inherited_lock_descriptor_cannot_extend_a_finished_install_attempt() {
        let fixture = Fixture::new("exit 0");
        let path = fixture.root.join("gateway-bootstrap.lock");
        let file = private_file(&path).unwrap();
        let inherited = file.try_clone().unwrap();
        let owner = AttemptLock::acquire(file).unwrap();
        // The child keeps the same open file description as a pre-exec fork.
        // A pipe controls its lifetime independently from the install attempt.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read x"])
            .stdin(Stdio::piped())
            .stdout(inherited)
            .spawn()
            .unwrap();
        let acquire = || AttemptLock::acquire(private_file(&path).unwrap());
        assert!(matches!(acquire(), Err(Status::Installing)));
        drop(owner);
        assert!(child.try_wait().unwrap().is_none());
        let next = acquire();
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(next.is_ok(), "another Desktop must be able to retry");
    }

    #[test]
    fn blocked_download_is_bounded_and_cancelled_without_touching_real_installers() {
        let fixture = Fixture::new("sleep 30");
        let paths = [PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
        let started = Instant::now();
        assert_eq!(
            run_installer(
                fixture.context.installer.as_deref().unwrap(),
                &paths,
                &fixture.context.cancel,
                Duration::from_millis(100),
                |_| {}
            ),
            Status::Failed
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        fixture.context.cancel.store(true, Ordering::Release);
        assert_eq!(
            run_installer(
                fixture.context.installer.as_deref().unwrap(),
                &paths,
                &fixture.context.cancel,
                Duration::from_secs(2),
                |_| {}
            ),
            Status::Interrupted
        );
        assert_eq!(
            fixture.count(),
            1,
            "cancellation before launch must not execute an installer"
        );
    }

    #[test]
    fn binary_configuration_and_unhealthy_service_evidence_prevent_new_installation() {
        let fixture = Fixture::new("exit 0");
        let bin = fixture.root.join("bin");
        let config = fixture.root.join("config");
        let data = fixture.root.join("data");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(data.join("muqun-gateway")).unwrap();
        assert!(!existing_evidence(
            std::slice::from_ref(&bin),
            Some(&fixture.root),
            Some(&config),
            Some(&data)
        )
        .unwrap());
        let binary = bin.join("muqun-gateway");
        fs::write(&binary, "old or custom install").unwrap();
        assert!(existing_evidence(
            std::slice::from_ref(&bin),
            Some(&fixture.root),
            Some(&config),
            Some(&data)
        )
        .unwrap());
        fs::remove_file(binary).unwrap();
        fs::create_dir_all(config.join("systemd/user")).unwrap();
        fs::write(
            config.join("systemd/user/dev.osuki.muqun-gateway.service"),
            "broken unit",
        )
        .unwrap();
        assert!(existing_evidence(
            std::slice::from_ref(&bin),
            Some(&fixture.root),
            Some(&config),
            Some(&data)
        )
        .unwrap());
    }

    #[test]
    fn an_external_process_outside_search_paths_is_detected_without_invoking_it() {
        let fixture = Fixture::new("exit 0");
        let name = format!("muqunqa{}", &uuid::Uuid::new_v4().simple().to_string()[..7]);
        let binary = fixture.root.join(&name);
        // No writable executable is created: parallel fork/exec tests can
        // briefly inherit a copy's write fd and make its exec fail ETXTBSY.
        std::os::unix::fs::symlink("/bin/sleep", &binary).unwrap();
        let mut external = Command::new(binary).arg("10").spawn().unwrap();
        let detected = running_gateway_named(&name);
        let _ = external.kill();
        let _ = external.wait();
        assert_eq!(detected, Some(true));
        assert_eq!(running_gateway_named(&name), Some(false));
    }

    #[test]
    fn normal_linux_and_standalone_mac_packages_find_the_same_adjacent_installer_pair() {
        let fixture = Fixture::new("exit 0");
        for relative in [
            "linux/share/muqun-desktop/installers",
            "Muqun.app/Contents/Resources/installers",
        ] {
            let installer = fixture.root.join(relative);
            fs::create_dir_all(&installer).unwrap();
            fs::write(installer.join("install-desktop.sh"), "wrapper").unwrap();
            fs::write(installer.join("install.sh"), "official installer").unwrap();
            let executable = if relative.starts_with("linux") {
                fixture.root.join("linux/bin/muqun-desktop")
            } else {
                fixture.root.join("Muqun.app/Contents/MacOS/muqun-desktop")
            };
            assert_eq!(
                packaged_installer(&executable),
                Some(installer.join("install-desktop.sh"))
            );
        }
        assert!(packaged_installer(&fixture.root.join("target/debug/muqun-desktop")).is_none());
    }
}
