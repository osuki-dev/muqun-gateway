//! Sharing OpenCode with a gateway service on the same machine.
//!
//! There is one OpenCode service per user (`~/.local/state/opencode/
//! service.json`), and whoever starts it decides its environment -- the
//! gateway starts it with its `muqun` MCP tools wired in. So the rule
//! (architecture §4): when a gateway is running here, Muqun Desktop only
//! adopts an OpenCode that is already up and never starts one; with no
//! gateway it honours its own `opencode.autostart`.
//!
//! "A gateway is running" is answered by the gateway's own state lock
//! (`platform/state_lock.rs` in the gateway): a running gateway holds an
//! exclusive `flock` on `<its state dir>/gateway.lock` for its whole life,
//! and the kernel drops it whatever way the process ends. The probe here
//! asks for a *shared* lock, non-blocking, on a descriptor opened read-only,
//! exactly as the gateway's own `running_owner` probe does and for the reason
//! its docs give: an exclusive probe that lands while a gateway is starting
//! could win the lock from it. A shared probe can only ever share with other
//! probes. The file is never created and never written.

use std::path::{Path, PathBuf};

/// The gateway's lock file name (`state_lock::LOCK_FILE`).
pub const GATEWAY_LOCK_FILE: &str = "gateway.lock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayPresence {
    /// Someone holds the lock: a gateway owns that state directory.
    Running { lock: PathBuf, pid: Option<u32> },
    /// No lock file, or nobody holds it.
    NotRunning,
    /// The lock could not be inspected; treated as running, the safe side.
    Unknown { lock: PathBuf, error: String },
}

impl GatewayPresence {
    pub fn blocks_autostart(&self) -> bool {
        !matches!(self, Self::NotRunning)
    }
}

/// The state directories a gateway for this user may be using, most specific
/// first: its environment override, then its standalone directory.
pub fn gateway_state_dirs() -> Vec<PathBuf> {
    let mut dirs_found = Vec::new();
    for var in ["MUQUN_GATEWAY_STATE_DIR", "HERDR_PLUGIN_STATE_DIR"] {
        if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            dirs_found.push(PathBuf::from(dir));
        }
    }
    if let Some(parent) = dirs::data_dir().or_else(dirs::config_dir) {
        dirs_found.push(parent.join("muqun-gateway"));
    }
    dirs_found.dedup();
    dirs_found
}

/// Whether a gateway is running against any of `state_dirs`.
pub fn detect(state_dirs: &[PathBuf]) -> GatewayPresence {
    for dir in state_dirs {
        match probe(&dir.join(GATEWAY_LOCK_FILE)) {
            GatewayPresence::NotRunning => continue,
            found => return found,
        }
    }
    GatewayPresence::NotRunning
}

#[cfg(unix)]
pub fn probe(lock: &Path) -> GatewayPresence {
    use std::os::fd::AsRawFd as _;
    let file = match std::fs::File::open(lock) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return GatewayPresence::NotRunning
        }
        Err(err) => {
            return GatewayPresence::Unknown {
                lock: lock.to_owned(),
                error: err.to_string(),
            }
        }
    };
    // SAFETY: `file` owns the descriptor for the whole call; dropping it at
    // the end of this function releases the shared probe lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return GatewayPresence::NotRunning;
    }
    let err = std::io::Error::last_os_error();
    if err.kind() != std::io::ErrorKind::WouldBlock {
        return GatewayPresence::Unknown {
            lock: lock.to_owned(),
            error: err.to_string(),
        };
    }
    let pid = std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    GatewayPresence::Running {
        lock: lock.to_owned(),
        pid,
    }
}

/// The gateway service runs on macOS and Linux only.
#[cfg(not(unix))]
pub fn probe(_lock: &Path) -> GatewayPresence {
    GatewayPresence::NotRunning
}

/// Apply the ownership rule to `opencode` for this run. Returns why
/// autostart was turned off, when it was.
pub fn apply(
    opencode: &mut muqun_gateway::agents::OpencodeConfig,
    presence: &GatewayPresence,
) -> Option<String> {
    if !opencode.autostart || !presence.blocks_autostart() {
        return None;
    }
    opencode.autostart = false;
    Some(match presence {
        GatewayPresence::Running { lock, pid } => format!(
            "a gateway service is running (lock {}{}); OpenCode will only be adopted, \
             never started, so the gateway keeps its OpenCode environment",
            lock.display(),
            pid.map(|pid| format!(", pid {pid}")).unwrap_or_default()
        ),
        GatewayPresence::Unknown { lock, error } => format!(
            "cannot tell whether a gateway service is running ({}: {error}); \
             OpenCode will only be adopted, never started",
            lock.display()
        ),
        GatewayPresence::NotRunning => unreachable!("checked above"),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd as _;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-desktop-coexist-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_lock_file_means_no_gateway_and_nothing_is_created() {
        let dir = scratch("absent");
        assert_eq!(
            detect(std::slice::from_ref(&dir)),
            GatewayPresence::NotRunning
        );
        assert!(!dir.join(GATEWAY_LOCK_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_released_lock_means_no_gateway() {
        let dir = scratch("released");
        std::fs::write(dir.join(GATEWAY_LOCK_FILE), "4242\n").unwrap();
        assert_eq!(
            probe(&dir.join(GATEWAY_LOCK_FILE)),
            GatewayPresence::NotRunning
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_lock_means_a_gateway_and_turns_autostart_off() {
        let dir = scratch("held");
        let path = dir.join(GATEWAY_LOCK_FILE);
        std::fs::write(&path, "4242\n").unwrap();
        // Held the way a gateway holds it: exclusive, on its own description.
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let presence = detect(std::slice::from_ref(&dir));
        assert_eq!(
            presence,
            GatewayPresence::Running {
                lock: path.clone(),
                pid: Some(4242)
            }
        );
        let mut opencode = muqun_gateway::agents::OpencodeConfig::default();
        let why = apply(&mut opencode, &presence).expect("autostart turned off");
        assert!(!opencode.autostart);
        assert!(why.contains("pid 4242"), "{why}");
        // The probe must not have stolen or broken the holder's lock.
        let second = std::fs::File::open(&path).unwrap();
        assert_ne!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(holder);
        assert_eq!(probe(&path), GatewayPresence::NotRunning);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_gateway_the_configured_autostart_is_kept() {
        let mut opencode = muqun_gateway::agents::OpencodeConfig::default();
        assert!(apply(&mut opencode, &GatewayPresence::NotRunning).is_none());
        assert!(opencode.autostart);
        opencode.autostart = false;
        let running = GatewayPresence::Running {
            lock: PathBuf::from("/x"),
            pid: None,
        };
        assert!(apply(&mut opencode, &running).is_none(), "already off");
    }
}
