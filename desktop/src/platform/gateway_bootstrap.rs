//! First-launch Gateway provisioning delegates to the packaged distribution
//! installer. It never downloads binaries or repairs an existing installation.
use crate::{
    config::DesktopConfig,
    model::inbox::{Inbox, UiAction, Update},
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

pub const OPT_OUT: &str = "gateway-auto-install.disabled";
const ATTEMPT: &str = "gateway-bootstrap.attempt";
const DEADLINE: Duration = Duration::from_secs(480);
static CONTEXT: OnceLock<Arc<Context>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Unavailable,
    Disabled,
    Existing,
    Remote,
    Installing,
    Ready,
    Failed,
    Interrupted,
}

impl Status {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Unavailable => "Gateway setup is available in packaged Desktop installations.",
            Self::Disabled => "Local-only mode: automatic Gateway installation is disabled.",
            Self::Existing => "An existing Gateway installation was left unchanged.",
            Self::Remote => "Remote computer selected; local Gateway setup was skipped.",
            Self::Installing => {
                "Installing local Gateway in the background. Local mode remains available."
            }
            Self::Ready => "Local Gateway is ready. Connect when your current work is finished.",
            Self::Failed => {
                "Gateway setup failed or timed out. Local mode remains available; retry when ready."
            }
            Self::Interrupted => {
                "A previous Gateway setup did not finish. It will only run again when you retry."
            }
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Failed | Self::Interrupted | Self::Disabled)
    }
}

struct Context {
    config: PathBuf,
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

fn local(computer: &str) -> bool {
    matches!(computer, "" | "local" | "self")
}

pub fn initialize(
    config: &DesktopConfig,
    config_dir: PathBuf,
    state_dir: PathBuf,
    inbox: Arc<Inbox>,
) {
    let installer = std::env::current_exe()
        .ok()
        .and_then(|path| packaged_installer(&path));
    let context = Arc::new(Context {
        config: config_dir,
        state: state_dir,
        installer,
        inbox,
        running: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
        status: Mutex::new(Status::Unavailable),
    });
    if CONTEXT.set(context.clone()).is_err() {
        return;
    }
    if config.general.gateway_local_only || context.config.join(OPT_OUT).exists() {
        *context.status.lock().unwrap() = Status::Disabled;
    } else if !local(&config.current_computer) {
        *context.status.lock().unwrap() = Status::Remote;
    } else {
        begin(context, false);
    }
}

pub fn set_local_only(disabled: bool) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    if disabled {
        context.cancel.store(true, Ordering::Release);
        if fs::create_dir_all(&context.config).is_ok() {
            let _ = private_file(&context.config.join(OPT_OUT), false);
        }
        *context.status.lock().unwrap() = Status::Disabled;
    } else {
        let _ = fs::remove_file(context.config.join(OPT_OUT));
        if !context.running.load(Ordering::Acquire) {
            *context.status.lock().unwrap() = Status::Interrupted;
        }
    }
}

pub fn retry(config: &DesktopConfig) {
    if config.general.gateway_local_only || !local(&config.current_computer) {
        return;
    }
    if let Some(context) = CONTEXT.get() {
        if context.config.join(OPT_OUT).exists() {
            return;
        }
        begin(context.clone(), true);
    }
}

fn begin(context: Arc<Context>, retry: bool) {
    if context.running.swap(true, Ordering::AcqRel) {
        return;
    }
    context.cancel.store(false, Ordering::Release);
    std::thread::spawn(move || {
        let result = provision(&context, retry, DEADLINE);
        *context.status.lock().unwrap() = result.clone();
        context.running.store(false, Ordering::Release);
        context
            .inbox
            .push(Update::Ui(UiAction::LocalGatewayStatus(result.clone())));
        if result == Status::Ready {
            context.inbox.push(Update::Ui(UiAction::LocalGatewayReady));
        }
    });
}

fn path_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok() || path.try_exists().is_err()
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
) -> bool {
    if paths.iter().any(|path| {
        ["muqun-gateway", "herdr-gateway"]
            .iter()
            .any(|name| path_present(&path.join(name)))
    }) {
        return true;
    }
    let mut evidence = Vec::new();
    for root in [config, data].into_iter().flatten() {
        evidence.extend([root.join("muqun-gateway"), root.join("herdr-gateway")]);
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
    evidence.iter().any(|path| path_present(path))
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

fn private_file(path: &Path, exclusive: bool) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn provision(context: &Context, retry: bool, deadline: Duration) -> Status {
    if context.cancel.load(Ordering::Acquire) || context.config.join(OPT_OUT).exists() {
        return Status::Disabled;
    }
    if [
        "MUQUN_LOCAL_GATEWAY",
        "MUQUN_GATEWAY_CONFIG_DIR",
        "MUQUN_GATEWAY_STATE_DIR",
        "MUQUN_GATEWAY_INSTALL_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_STATE_DIR",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return Status::Existing;
    }
    let paths = search_path();
    if existing_evidence(
        &paths,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
        dirs::data_dir().as_deref(),
    ) || super::coexist::detect(&super::coexist::gateway_state_dirs()).blocks_autostart()
    {
        return Status::Existing;
    }
    match running_gateway_named("muqun-gateway|herdr-gateway") {
        Some(false) => {}
        Some(true) => return Status::Existing,
        None => return Status::Failed,
    }
    let Some(installer) = &context.installer else {
        return Status::Unavailable;
    };
    install_once(context, installer, &paths, retry, deadline, || {
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
    retry: bool,
    deadline: Duration,
    ready: impl FnOnce() -> bool,
) -> Status {
    if context.cancel.load(Ordering::Acquire) || context.config.join(OPT_OUT).exists() {
        return Status::Disabled;
    }
    if fs::create_dir_all(&context.state).is_err() {
        return Status::Failed;
    }
    let Ok(owner) = private_file(&context.state.join("gateway-bootstrap.lock"), false) else {
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
    let mut record = match private_file(&attempt, !retry) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Status::Interrupted
        }
        Err(_) => return Status::Failed,
    };
    if record
        .write_all(b"started\n")
        .and_then(|_| record.sync_all())
        .is_err()
    {
        return Status::Failed;
    }
    *context.status.lock().unwrap() = Status::Installing;
    context
        .inbox
        .push(Update::Ui(UiAction::LocalGatewayStatus(Status::Installing)));
    let outcome = run_installer(installer, paths, &context.cancel, deadline);
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
) -> Status {
    if cancel.load(Ordering::Acquire) {
        return Status::Disabled;
    }
    let mut command = Command::new("/bin/bash");
    command
        .arg(installer)
        .arg("--gateway-only")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is the only post-fork operation; no allocation or
        // locking. Optional installer prompts must not read Desktop's TTY.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let Ok(mut child) = command.spawn() else {
        return Status::Failed;
    };
    let started = Instant::now();
    loop {
        if let Ok(Some(exit)) = child.try_wait() {
            return if exit.success() {
                Status::Ready
            } else {
                Status::Failed
            };
        }
        if cancel.load(Ordering::Acquire) || started.elapsed() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return if cancel.load(Ordering::Acquire) {
                Status::Disabled
            } else {
                Status::Failed
            };
        }
        std::thread::sleep(Duration::from_millis(100));
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
                config: root.join("config"),
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
        assert_eq!(fixture.attempt(false), Status::Failed);
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
    fn inherited_lock_descriptor_cannot_extend_a_finished_install_attempt() {
        let fixture = Fixture::new("exit 0");
        let path = fixture.root.join("gateway-bootstrap.lock");
        let file = private_file(&path, false).unwrap();
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
        let acquire = || AttemptLock::acquire(private_file(&path, false).unwrap());
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
                Duration::from_millis(100)
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
                Duration::from_secs(2)
            ),
            Status::Disabled
        );
        assert_eq!(
            fixture.count(),
            1,
            "cancellation before launch must not execute an installer"
        );
    }

    #[test]
    fn local_only_marker_prevents_first_attempt_without_creating_state() {
        let fixture = Fixture::new("exit 0");
        fs::create_dir_all(&fixture.context.config).unwrap();
        fs::write(fixture.context.config.join(OPT_OUT), "").unwrap();
        assert_eq!(fixture.attempt(false), Status::Disabled);
        assert_eq!(fixture.count(), 0);
        assert!(!fixture.context.state.join(ATTEMPT).exists());
    }

    #[test]
    fn binary_configuration_and_unhealthy_service_evidence_prevent_new_installation() {
        let fixture = Fixture::new("exit 0");
        let bin = fixture.root.join("bin");
        let config = fixture.root.join("config");
        let data = fixture.root.join("data");
        fs::create_dir_all(&bin).unwrap();
        assert!(!existing_evidence(
            std::slice::from_ref(&bin),
            Some(&fixture.root),
            Some(&config),
            Some(&data)
        ));
        let binary = bin.join("muqun-gateway");
        fs::write(&binary, "old or custom install").unwrap();
        assert!(existing_evidence(
            std::slice::from_ref(&bin),
            Some(&fixture.root),
            Some(&config),
            Some(&data)
        ));
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
        ));
        assert!(!local("remote-computer"));
        assert!(!local("all"));
        assert!(local("local"));
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
