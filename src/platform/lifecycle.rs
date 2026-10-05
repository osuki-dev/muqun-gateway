//! One lifecycle for CLI and manager; supervisors own supervised processes.
//! Commands serialize independently of the gateway's lifetime state lock.
use std::time::{Duration, Instant};

use anyhow::Context as _;

use super::{service, setup, state_lock};
use crate::{config_dir, load_config, process_matches_name, state_dir, supervision, CONFIG_FILE};

pub(crate) use service::ServiceAction as Action;

/// Where a supervised gateway's own diagnostics go besides its log file.
#[cfg(target_os = "macos")]
const SERVICE_LOGS: &str =
    "launchd's view of it (`log show --last 10m --predicate 'process == \"launchd\"' | grep muqun`)";
#[cfg(not(target_os = "macos"))]
const SERVICE_LOGS: &str = "the service journal (`journalctl --user -u dev.osuki.muqun-gateway`)";

pub(crate) fn running_pid() -> anyhow::Result<Option<u32>> {
    let owner = state_lock::running_owner(&state_dir()?)?;
    if let Some(pid) = owner {
        anyhow::ensure!(process_matches_name(pid, crate::GATEWAY_PROCESS_NAME),
            "state directory is owned by an unrecognized process (pid {pid}); refusing lifecycle actions");
    }
    Ok(owner)
}

pub(crate) fn summary() -> anyhow::Result<String> {
    let pid = running_pid()?;
    let service = service::state()?;
    let mode = if service != service::ServiceState::NotInstalled {
        "user service"
    } else if pid
        .map(supervision::gateway_supervisor)
        .transpose()?
        .flatten()
        .is_some()
    {
        "external systemd service"
    } else {
        "ordinary background/foreground"
    };
    Ok(match pid {
        Some(pid) => format!("running pid {pid} ({mode})"),
        None => format!("stopped ({mode})"),
    })
}

pub(crate) fn control(action: Action, verbose: bool) -> anyhow::Result<()> {
    let controller = Controller::acquire(action)?;
    controller.control(action)?;
    if verbose {
        println!("gateway: {}", summary()?);
        println!("config: {}", config_dir()?.join(CONFIG_FILE).display());
    }
    Ok(())
}

enum Owner {
    Installed,
    External(String),
    Detached,
}

/// Retains the supervisor and executable across a stop/replace/start transaction.
/// The command lock is held once; never call `control` while holding this guard.
pub(crate) struct Controller {
    _command_lock: state_lock::StateLock,
    owner: Owner,
    paths: service::ServicePaths,
    pub(crate) was_running: bool,
}

impl Controller {
    pub(crate) fn acquire(action: Action) -> anyhow::Result<Self> {
        // Validate configuration before creating state or changing any process.
        load_config(None)?;
        let state = state_dir()?;
        let command_lock = state_lock::StateLock::acquire_strict(&state.join("lifecycle"))
            .context("another lifecycle command is in progress; retry after it finishes")?;
        let pid = running_pid()?;
        let external = pid
            .map(supervision::gateway_supervisor)
            .transpose()?
            .flatten();
        let paths = setup::service_paths()?;
        let owner = if service::state()? != service::ServiceState::NotInstalled {
            service::ensure_current_install(&paths)?;
            if let Some(pid) = pid {
                #[cfg(target_os = "linux")]
                anyhow::ensure!(external.as_ref().is_some_and(|unit|
                unit.user_manager && unit.unit == format!("{}.service", service::SERVICE_LABEL)),
                "gateway pid {pid} is not owned by the installed user service; stop it before transferring ownership");
                #[cfg(target_os = "macos")]
                anyhow::ensure!(
                    service::owns_pid(pid),
                    "gateway pid {pid} is not owned by the installed LaunchAgent"
                );
            }
            Owner::Installed
        } else if let Some(unit) = external {
            anyhow::ensure!(
                unit.user_manager,
                "gateway is managed by a system service; use `{}`",
                unit.systemctl(action.verb())
            );
            #[cfg(not(target_os = "macos"))]
            service::ensure_effective_unit(&unit.unit, &paths)?;
            Owner::External(unit.unit)
        } else {
            Owner::Detached
        };
        Ok(Self {
            _command_lock: command_lock,
            owner,
            paths,
            was_running: pid.is_some(),
        })
    }

    pub(crate) fn exe(&self) -> &std::path::Path {
        &self.paths.exe
    }

    pub(crate) fn control(&self, action: Action) -> anyhow::Result<()> {
        match &self.owner {
            Owner::Installed => service::control(action)?,
            Owner::External(unit) => {
                service::checked_command("systemctl", &["--user", action.verb(), unit])?
            }
            Owner::Detached => {
                if matches!(action, Action::Stop | Action::Restart) {
                    setup::stop_detached()?;
                }
                if action != Action::Stop {
                    setup::start_detached_at(&self.paths.exe)?;
                }
            }
        }
        let state = &self.paths.state;
        if action == Action::Stop {
            wait_for(Duration::from_secs(15), || {
                Ok(state_lock::running_owner(state)?.is_none())
            })
            .context("gateway did not stop; no replacement was started")?;
        } else {
            wait_for(Duration::from_secs(15), || {
                Ok(running_pid()?.is_some() && crate::fetch_pending_pairing().is_ok())
            })
            .with_context(|| {
                format!(
                    "gateway did not become ready; inspect {} and {SERVICE_LOGS}",
                    state.join(crate::LOG_FILE).display()
                )
            })?;
        }
        Ok(())
    }
}

pub(crate) fn wait_for(
    limit: Duration,
    mut ready: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        match ready() {
            Ok(true) => return Ok(()),
            // Lock acquisition precedes PID publication. Only this typed,
            // empty-record state is retryable; invalid/unrecognized owners and
            // unrelated probe failures still abort without authorizing signals.
            Err(error) if error.is::<state_lock::UnpublishedOwner>() => {}
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for gateway lifecycle transition"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_wait_reports_timeout_and_probe_failure() {
        assert!(wait_for(Duration::ZERO, || Ok(false)).is_err());
        assert!(wait_for(Duration::ZERO, || anyhow::bail!("probe failed"))
            .unwrap_err()
            .to_string()
            .contains("probe failed"));
        assert!(wait_for(Duration::ZERO, || Ok(true)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_wait_retries_a_lock_held_unpublished_owner_until_publication_or_deadline() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;
        let dir = std::env::temp_dir().join(format!(
            "gateway-owner-publication-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(state_lock::LOCK_FILE);
        let mut held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // SAFETY: this is an owned descriptor for an isolated fixture file.
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(state_lock::running_owner(&dir)
            .unwrap_err()
            .is::<state_lock::UnpublishedOwner>());
        let error = wait_for(Duration::ZERO, || {
            Ok(state_lock::running_owner(&dir)?.is_some())
        })
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));

        let mut probes = 0;
        wait_for(Duration::from_secs(1), || {
            probes += 1;
            let owner = state_lock::running_owner(&dir);
            if probes == 1 {
                assert!(owner
                    .as_ref()
                    .unwrap_err()
                    .is::<state_lock::UnpublishedOwner>());
                // Publish only after observing the exact empty-file window,
                // not by relying on fork/scheduling or a timing-based sleep.
                writeln!(held, "{}", std::process::id()).unwrap();
                held.flush().unwrap();
            }
            Ok(owner? == Some(std::process::id()))
        })
        .unwrap();
        assert_eq!(probes, 2);

        // Nonempty malformed records are not transitional publication.
        held.set_len(0).unwrap();
        use std::io::Seek as _;
        held.rewind().unwrap();
        held.write_all(b"not-a-pid").unwrap();
        let mut probes = 0;
        let error = wait_for(Duration::from_secs(1), || {
            probes += 1;
            Ok(state_lock::running_owner(&dir)?.is_some())
        })
        .unwrap_err();
        assert!(!error.is::<state_lock::UnpublishedOwner>());
        assert_eq!(probes, 1);
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
