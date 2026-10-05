//! One owner at a time for a gateway state directory.
//!
//! `devices.json` is not edited in place by anyone. A gateway loads the whole
//! paired-device list into memory at startup and writes the whole list back on
//! every change, so two gateways sharing one state directory do not interleave
//! -- they overwrite. Gateway A starts and reads two devices; B pairs a third
//! and writes three; A pairs a fourth and writes *its* list, which never had
//! B's device in it. B's pairing is gone, with no error anywhere, and the app
//! that holds that token is simply unpaired the next time it asks for
//! anything. Atomic writes do not help: both writes are individually perfect.
//!
//! So the file needs an owner, and the owner is whoever holds an exclusive
//! `flock` on a lock file in the directory. A second gateway refuses to start
//! rather than start and quietly compete, and the rare code path that edits
//! the list on disk without a gateway running takes the same lock across its
//! whole read-modify-write, because serialising only the write would still let
//! a change land in the window between a read and the write that erases it.
//!
//! `flock` is the right primitive here specifically because the kernel holds
//! the lock against an open file description, not against a process's promise
//! to clean up: every way a gateway can die -- `SIGKILL`, a panic, the OOM
//! killer, a yanked power cord -- closes its descriptors and releases the
//! lock. There is no stale lock to break by hand and so no need for a `--force`
//! that would reintroduce exactly the corruption this prevents. Two rules keep
//! that property: the lock file is opened and never unlinked (unlinking and
//! recreating it would leave two processes locking two different inodes, each
//! believing it was alone), and the descriptor is held in a live binding for
//! as long as the lock is meant to last (dropping it releases the lock early).
//!
//! Releasing is not instantaneous *as seen by another acquirer* when the
//! releasing process is also spawning children, and that is worth knowing
//! before it is mistaken for a bug. `fork` copies the descriptor table, so a
//! child that has been forked but has not yet reached `exec` holds a copy of
//! the lock's descriptor, and the lock lives until the last copy is gone. A
//! gateway spawns subprocesses constantly (tmux, Herdr, agents), so for a few
//! microseconds after it exits, a child of its that was mid-spawn can still be
//! holding the directory. The consequence is bounded and fail-safe: an
//! acquirer in that window is *refused*, never wrongly let in, and the next
//! attempt succeeds. It cannot corrupt anything, which is why this is
//! documented rather than designed around -- the alternative primitives
//! trade this window for worse properties (POSIX record locks are not
//! inherited across `fork`, but they are dropped when the process closes *any*
//! descriptor to the file, and they do not contend between threads of one
//! process, which would make this module untestable).
//!
//! Assumption: the state directory is on a local filesystem. `flock` over
//! NFS is emulated by the client and only coordinates between hosts on newer
//! kernels with NFSv4; on filesystems that cannot lock at all this degrades to
//! a warning and no protection rather than an unstartable gateway, because a
//! lone gateway needs no lock to be correct.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// The lock file's name inside the state directory. Created once and never
/// removed -- see the module docs on why unlinking it would defeat the lock.
pub const LOCK_FILE: &str = "gateway.lock";

/// The owner has taken the lock but has not finished publishing its PID.
/// Transition waits may retry this exact state; it never authorizes a signal.
#[derive(Debug)]
pub(crate) struct UnpublishedOwner;

impl std::fmt::Display for UnpublishedOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("state directory lock is held but its owner PID is not published yet")
    }
}

impl std::error::Error for UnpublishedOwner {}

/// Ownership of one state directory, for as long as this value is alive.
///
/// There is no explicit release, and deliberately so. `flock` is held against
/// an open file description and dropped when the *last* descriptor referring
/// to it closes, so letting this value's `File` close is both necessary and
/// sufficient -- and it is the same path the kernel takes for a process that
/// dies without running any destructor. An explicit `LOCK_UN` would be worse
/// than redundant: it releases the lock for every descriptor sharing that open
/// file description, including ones a forked child inherited, so a process
/// that released early could unlock a directory another process was relying on.
#[derive(Debug)]
pub struct StateLock {
    /// Never read: this is held open purely so that closing it releases the
    /// directory. See the type's documentation.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl StateLock {
    /// Transactions must fail closed on filesystems without working locks.
    #[cfg(unix)]
    pub(crate) fn acquire_strict(state_dir: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd as _;
        std::fs::create_dir_all(state_dir)?;
        let file = open_lock_file(&state_dir.join(LOCK_FILE))?;
        // SAFETY: file owns the descriptor until this guard is dropped.
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another lifecycle command is in progress, or this filesystem cannot lock safely"
        );
        record_holder(&file);
        Ok(Self { file })
    }

    /// Take exclusive ownership of `state_dir`, or fail naming the holder.
    pub fn acquire(state_dir: &Path) -> anyhow::Result<Self> {
        Self::acquire_with(state_dir, 1)
    }

    /// The gateway's own claim on its state directory at startup: retried
    /// `ACQUIRE_ATTEMPTS` times, so a probe or a fork window holding the lock
    /// for an instant does not make a freshly started gateway exit.
    pub fn acquire_owner(state_dir: &Path) -> anyhow::Result<Self> {
        Self::acquire_with(state_dir, ACQUIRE_ATTEMPTS)
    }

    fn acquire_with(state_dir: &Path, attempts: u32) -> anyhow::Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("failed to create state dir {}", state_dir.display()))?;
        let path = state_dir.join(LOCK_FILE);
        let file = open_lock_file(&path)?;
        lock_exclusive(file, path, state_dir, attempts)
    }
}

/// The pid recorded by whoever holds `path`, when it can be read.
///
/// Only meaningful while the lock is actually held: after a holder dies its
/// pid stays in the file, and the lock does not.
pub fn holder_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Inspect an existing lock without stamping our PID or creating state. The
/// recorded PID is meaningful only while another open description holds it.
///
/// The probe takes a *shared* lock, read-only, for the instant it needs: it
/// never holds what a gateway starting at that moment wants exclusively, only
/// shares it with other probes. `manage` redraws and the lifecycle readiness
/// wait (every 50 ms) both probe; with an exclusive probe a restart's own
/// readiness check could win the lock from the gateway it had just started,
/// which then exited and waited out launchd's 10 s respawn throttle. A probe
/// that lands in the gateway's attempt still delays it by microseconds, which
/// `ACQUIRE_ATTEMPTS` absorbs.
#[cfg(unix)]
pub fn running_owner(state_dir: &Path) -> anyhow::Result<Option<u32>> {
    use std::os::unix::io::AsRawFd as _;
    let path = state_dir.join(LOCK_FILE);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // SAFETY: the file owns the descriptor; dropping it releases a probe lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return Ok(None);
    }
    let error = std::io::Error::last_os_error();
    anyhow::ensure!(
        error.kind() == std::io::ErrorKind::WouldBlock,
        "cannot inspect state lock: {error}"
    );
    let contents = std::fs::read_to_string(&path)
        .context("state directory is owned but its PID is unreadable")?;
    if contents.is_empty() {
        return Err(UnpublishedOwner.into());
    }
    contents
        .trim()
        .parse()
        .map(Some)
        .context("state directory is owned but its PID is unreadable")
}

#[cfg(not(unix))]
pub fn running_owner(_state_dir: &Path) -> anyhow::Result<Option<u32>> {
    anyhow::bail!("gateway lifecycle is supported on macOS and Linux only")
}

fn contended_message(state_dir: &Path, lock_path: &Path) -> String {
    // A holder that systemd manages deserves a different refusal entirely.
    // "Stop it with `muqun-gateway stop`" is a trap there: the supervisor
    // restarts it within seconds, and if this process wins the lock race in
    // that window the unit then fails its restart every few seconds for as
    // long as nobody notices -- which, in the incident that motivated this,
    // was six days and 179k restarts.
    if let Some(unit) = holder_pid(lock_path).and_then(crate::supervision::managing_gateway_unit) {
        return format!(
            "another muqun-gateway already owns the state directory {}, and it is managed \
             by systemd as {}.\n\
             Do not start a second gateway by hand next to a supervised one -- manage that \
             one instead:\n\
             restart it with `{}`, or stop it for good with `{}`.",
            state_dir.display(),
            unit.unit,
            unit.systemctl("restart"),
            unit.systemctl("stop")
        );
    }
    let holder = match holder_pid(lock_path) {
        Some(pid) => format!("pid {pid}"),
        None => String::from("its pid could not be read from the lock file"),
    };
    let how_to_find = match holder_pid(lock_path) {
        Some(pid) => format!("`ps -p {pid} -o pid,lstart,args`"),
        None => format!("`fuser {}`", lock_path.display()),
    };
    format!(
        "another muqun-gateway already owns the state directory {} ({holder})\n\
         Two gateways cannot share one state directory: each keeps the whole paired-device \
         list in memory and rewrites the whole file, so the second one to write erases the \
         devices the first one paired.\n\
         Find the other one with {how_to_find}, stop it with `muqun-gateway stop`, and try \
         again -- or point this one at a different state directory. Killing it releases the \
         directory immediately; there is no lock left behind to clear.",
        state_dir.display()
    )
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open the state lock {}", path.display()))
}

#[cfg(not(unix))]
fn open_lock_file(path: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open the state lock {}", path.display()))
}

/// How many times a gateway tries for its state directory at startup,
/// `ACQUIRE_RETRY` apart, before it refuses to start. A probe (`running_owner`) or a
/// fork-to-exec window (module docs) holds the lock for microseconds; a second
/// gateway holds it for as long as it runs. One second tells them apart, and
/// costs a refused second start that second.
pub(crate) const ACQUIRE_ATTEMPTS: u32 = 5;
pub(crate) const ACQUIRE_RETRY: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(unix)]
fn lock_exclusive(
    file: std::fs::File,
    path: PathBuf,
    state_dir: &Path,
    attempts: u32,
) -> anyhow::Result<StateLock> {
    use std::os::unix::io::AsRawFd as _;

    let mut attempt = 1;
    let error = loop {
        // SAFETY: `file` owns this descriptor for the whole call and outlives it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            record_holder(&file);
            return Ok(StateLock { file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock || attempt >= attempts {
            break error;
        }
        attempt += 1;
        std::thread::sleep(ACQUIRE_RETRY);
    };
    if error.kind() == std::io::ErrorKind::WouldBlock {
        anyhow::bail!(contended_message(state_dir, &path));
    }
    // A filesystem that cannot take advisory locks must not make the gateway
    // unstartable. The lock guards against a *second* instance; the first one
    // is correct without it. Say what was lost and carry on.
    tracing::warn!(
        "could not lock {} ({error}); this gateway cannot detect a second one sharing its \
         state directory",
        path.display()
    );
    Ok(StateLock { file })
}

#[cfg(not(unix))]
fn lock_exclusive(
    file: std::fs::File,
    _path: PathBuf,
    _state_dir: &Path,
    _attempts: u32,
) -> anyhow::Result<StateLock> {
    Ok(StateLock { file })
}

/// Stamp the holder's pid into the lock file so a refused start can name it.
///
/// Written into the file that is already open and locked -- never by replacing
/// the file, which would move the lock to a new inode.
fn record_holder(file: &std::fs::File) {
    use std::io::{Seek as _, Write as _};
    let mut file = file;
    // Best effort: an unpublished PID prevents proving lifecycle ownership.
    // Readiness waits retry the empty publication window; signals fail closed.
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
    let _ = file.flush();
}

/// How long a test waits for a released directory to become acquirable.
///
/// Sized against the two things it has to tell apart: the `fork`-to-`exec`
/// window described in the module docs, which is microseconds, and a genuine
/// leak, which holds the directory for as long as the leaking process lives.
/// Anything in between does not happen.
#[cfg(test)]
pub const RELEASE_VISIBLE_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// Take the directory once it is free, rather than on the very next
/// instruction after someone released it.
///
/// Asserting that a release is visible immediately asserts something this
/// design does not promise: another thread of this process may be between
/// `fork` and `exec` while holding an inherited copy of the descriptor (see
/// the module docs), and a test binary that runs hundreds of tests in
/// parallel is doing that almost continuously. Waiting for the condition is
/// not a slack sleep -- it returns the instant the directory is free, and it
/// still fails if anything holds the lock for longer than the window can
/// possibly last.
#[cfg(test)]
pub fn acquire_within(dir: &Path, limit: std::time::Duration) -> anyhow::Result<StateLock> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match StateLock::acquire(dir) {
            Ok(lock) => return Ok(lock),
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
}

/// Retry an operation that takes the state-directory lock until the directory
/// stops being someone else's. Same reasoning as `acquire_within`.
#[cfg(test)]
pub fn retry_while_directory_is_busy<T>(
    mut attempt: impl FnMut() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let deadline = std::time::Instant::now() + RELEASE_VISIBLE_WITHIN;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-gateway-lock-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn owner_inspection_does_not_create_or_trust_stale_lock_files() {
        let dir = scratch_dir("inspect");
        assert_eq!(running_owner(&dir).unwrap(), None);
        assert!(!dir.join(LOCK_FILE).exists());
        std::fs::write(dir.join(LOCK_FILE), "12345\n").unwrap();
        assert_eq!(running_owner(&dir).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap(),
            "12345\n"
        );
        let held = StateLock::acquire(&dir).unwrap();
        assert_eq!(running_owner(&dir).unwrap(), Some(std::process::id()));
        drop(held);
        retry_while_directory_is_busy(|| {
            anyhow::ensure!(running_owner(&dir)?.is_none(), "probe still held");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The loss this file exists to prevent: two gateways against one state
    /// directory, each rewriting the whole device list over the other's.
    #[cfg(unix)]
    #[test]
    fn probing_a_held_directory_never_disturbs_its_holder() {
        let dir = scratch_dir("probe-held");
        let holder = StateLock::acquire(&dir).unwrap();
        for _ in 0..100 {
            assert_eq!(running_owner(&dir).unwrap(), Some(std::process::id()));
        }
        // Still exclusively held: neither another owner nor a lifecycle
        // command can take it after all those probes.
        assert!(StateLock::acquire_strict(&dir).is_err());
        assert!(StateLock::acquire_owner(&dir).is_err());
        assert_eq!(holder_pid(&dir.join(LOCK_FILE)), Some(std::process::id()));
        drop(holder);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_gateway_starting_under_a_probe_waits_it_out_instead_of_exiting() {
        use std::os::unix::io::AsRawFd as _;
        let dir = scratch_dir("probe-start");
        std::fs::create_dir_all(&dir).unwrap();
        let probe = std::fs::File::create(dir.join(LOCK_FILE)).unwrap();
        // SAFETY: an owned descriptor for an isolated fixture file.
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        // A probe of the empty directory sees no owner and does not block.
        assert_eq!(running_owner(&dir).unwrap(), None);
        let release = std::thread::spawn(move || {
            std::thread::sleep(ACQUIRE_RETRY / 2);
            drop(probe);
        });
        let lock = StateLock::acquire_owner(&dir).expect("the gateway retried past the probe");
        release.join().unwrap();
        assert_eq!(running_owner(&dir).unwrap(), Some(std::process::id()));
        drop(lock);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_gateway_cannot_take_a_held_state_directory() {
        let dir = scratch_dir("second");

        let first = acquire_within(&dir, RELEASE_VISIBLE_WITHIN)
            .expect("the first gateway could not take the lock");

        // Exact, not waited on: while the first holds it, a second must be
        // refused every single time.
        let second = StateLock::acquire(&dir);
        assert!(
            second.is_err(),
            "a second gateway took a state directory another one already owned"
        );

        // Releasing hands it over; nothing has to be cleaned up first.
        drop(first);
        assert!(
            acquire_within(&dir, RELEASE_VISIBLE_WITHIN).is_ok(),
            "the directory stayed locked after its owner released it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A refusal that does not say who is holding the directory leaves the
    /// owner with an unstartable gateway and nothing to act on.
    #[test]
    fn a_refused_start_names_the_process_holding_the_directory() {
        let dir = scratch_dir("names-holder");
        let _held = acquire_within(&dir, RELEASE_VISIBLE_WITHIN).unwrap();

        let error = StateLock::acquire(&dir).unwrap_err().to_string();
        // Who the refusal names depends on where this test runs, because the
        // holder is this very process. From a plain shell it is a pid. From a
        // terminal the gateway itself spawned -- a developer working over
        // Muqun, which is the normal way to work on Muqun -- the process sits
        // in the gateway unit's cgroup, is correctly read as supervised, and
        // is named by its unit with the systemctl commands instead. Both are
        // "names the holder"; asserting only the first made the suite fail on
        // the machine it is developed on.
        match crate::supervision::managing_gateway_unit(std::process::id()) {
            Some(unit) => assert!(
                error.contains(&unit.unit) && error.contains(&unit.systemctl("stop")),
                "the refusal did not name the supervising unit: {error}"
            ),
            None => assert!(
                error.contains(&format!("pid {}", std::process::id())),
                "the refusal did not name the holding pid: {error}"
            ),
        }
        assert!(
            error.contains(&dir.display().to_string()),
            "the refusal did not name the state directory: {error}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A gateway killed outright -- `SIGKILL`, the OOM killer, a panic, a
    /// yanked power cord -- must not leave the state directory unstartable
    /// for ever. Nothing in this process releases the lock below; the kernel
    /// does it when the holder dies, and that property is the entire reason
    /// `flock` was chosen over a pid file or a lock the owner has to remove.
    #[cfg(unix)]
    #[test]
    fn a_killed_holder_releases_the_state_directory() {
        let dir = scratch_dir("killed-holder");
        let lock_path = std::ffi::CString::new(dir.join(LOCK_FILE).as_os_str().as_encoded_bytes())
            .expect("lock path had an interior nul");

        // The child takes the lock itself rather than inheriting a descriptor
        // from here, so this really tests a separate process's lock dying with
        // it. It reports readiness down a pipe: without that the parent could
        // check before the child had locked anything and prove nothing.
        let mut ends = [0_i32; 2];
        // SAFETY: `ends` is two writable ints, which is what `pipe` wants.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe failed");
        let (read_end, write_end) = (ends[0], ends[1]);

        // SAFETY: the child calls only async-signal-safe functions -- dup2,
        // close, open, flock, write, alarm, pause, _exit -- before dying.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            unsafe {
                libc::dup2(write_end, 1);
                // Everything else inherited from the test runner goes,
                // including its captured output pipes: an orphan holding one
                // of those open hangs the whole test run rather than failing
                // it. This is also what keeps the child from holding a lock
                // another test thread happened to have open at fork time.
                libc::close(0);
                libc::close(2);
                // In one syscall where the kernel has it. Every descriptor
                // still open here is one this child is briefly holding on
                // another test thread's behalf, so the loop fallback -- a
                // thousand syscalls -- is a window worth closing quickly.
                #[cfg(target_os = "linux")]
                if libc::close_range(3, libc::c_uint::MAX, 0) != 0 {
                    for fd in 3..1024 {
                        libc::close(fd);
                    }
                }
                #[cfg(not(target_os = "linux"))]
                for fd in 3..1024 {
                    libc::close(fd);
                }
                let fd = libc::open(lock_path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600);
                if fd < 0 || libc::flock(fd, libc::LOCK_EX) != 0 {
                    libc::_exit(1);
                }
                libc::write(1, b"k".as_ptr().cast(), 1);
                libc::close(1);
                // A parent that panics before the kill below must not leave
                // this process behind for ever.
                libc::alarm(30);
                libc::pause();
                libc::_exit(0);
            }
        }
        // SAFETY: the parent's copy of the write end, which must go so that
        // the read below cannot block for ever if the child dies early.
        unsafe { libc::close(write_end) };
        let mut ready = [0_u8; 1];
        // SAFETY: `ready` is one writable byte and `read_end` is open here.
        let got = unsafe { libc::read(read_end, ready.as_mut_ptr().cast(), 1) };
        // SAFETY: the parent is done with the pipe either way.
        unsafe { libc::close(read_end) };
        assert_eq!(got, 1, "the child never took the lock");

        // Both checks run before anything can panic, so the child is always
        // reaped: a leaked `pause()`-ing orphan is worse than a failed test.
        // Exact, not waited on: the child is alive and holding it.
        let refused_while_alive = StateLock::acquire(&dir).is_err();
        // SAFETY: `child` is this process's own child and is not yet reaped.
        // `waitpid` returning is what makes the kill observable -- the lock
        // is gone once the process is, not once the signal was sent.
        unsafe {
            libc::kill(child, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
        }
        let reclaimed_after_kill = acquire_within(&dir, RELEASE_VISIBLE_WITHIN).is_ok();

        assert!(
            refused_while_alive,
            "another process held the directory and this one took it anyway"
        );
        assert!(
            reclaimed_after_kill,
            "a killed gateway left the state directory permanently locked"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The gateway spawns long-lived children (tmux, Herdr). If one of them
    /// inherited the lock descriptor *across `exec`*, it would keep the state
    /// directory locked for its whole life, with no gateway left to blame for
    /// it -- and unlike the brief pre-`exec` window in the module docs, that
    /// would be unbounded. `O_CLOEXEC` on the lock file is what prevents it.
    ///
    /// The child here lives 30 seconds and the wait below gives up after 5, so
    /// a descriptor that really did survive `exec` fails this test rather than
    /// being waited out.
    #[cfg(unix)]
    #[test]
    fn a_spawned_child_process_does_not_keep_the_directory_locked() {
        let dir = scratch_dir("cloexec");

        let lock = acquire_within(&dir, RELEASE_VISIBLE_WITHIN).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn a child process");
        drop(lock);

        let reclaimed = acquire_within(&dir, RELEASE_VISIBLE_WITHIN);
        let outlived = child.try_wait().ok().flatten().is_none();
        child.kill().ok();
        child.wait().ok();

        assert!(outlived, "the child exited before the check could run");
        assert!(
            reclaimed.is_ok(),
            "a child process the gateway spawned held the state lock open past the gateway"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Unlinking and recreating the lock file is the classic way to defeat
    /// `flock`: the next process locks a different inode and both believe they
    /// are alone. Releasing must leave the file exactly where it was.
    #[test]
    fn releasing_the_directory_leaves_the_lock_file_in_place() {
        let dir = scratch_dir("keeps-file");

        let path = dir.join(LOCK_FILE);
        let lock = acquire_within(&dir, RELEASE_VISIBLE_WITHIN).unwrap();
        assert!(path.exists());
        drop(lock);

        assert!(
            path.exists(),
            "the lock file was removed on release, so the next lock would be on a new inode"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Separate directories are separate installs and must not block each
    /// other -- this is the escape hatch a refused start points people at.
    #[test]
    fn separate_state_directories_do_not_block_each_other() {
        let first_dir = scratch_dir("separate-a");
        let second_dir = scratch_dir("separate-b");

        let _first = acquire_within(&first_dir, RELEASE_VISIBLE_WITHIN).unwrap();
        assert!(acquire_within(&second_dir, RELEASE_VISIBLE_WITHIN).is_ok());

        std::fs::remove_dir_all(&first_dir).ok();
        std::fs::remove_dir_all(&second_dir).ok();
    }
}
