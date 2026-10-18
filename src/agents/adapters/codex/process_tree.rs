//! Incarnation-checked ownership ledger for the private app-server tree.
//! Never adopts a PID file, process name, terminal session or shared daemon.
use std::collections::{HashMap, HashSet};

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    pid: u32,
    parent: u32,
    start: (u64, u64),
}
struct Owned {
    identity: Identity,
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
fn identity(pid: u32) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;
    let path = format!("/proc/{pid}/stat");
    // SAFETY: geteuid takes no arguments and accesses no caller memory.
    if std::fs::metadata(&path).ok()?.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    let stat = std::fs::read_to_string(path).ok()?;
    let fields: Vec<_> = stat
        .get(stat.rfind(')')? + 2..)?
        .split_whitespace()
        .collect();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    Some(Identity {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        start: (fields.get(19)?.parse().ok()?, 0),
    })
}
#[cfg(target_os = "macos")]
fn identity(pid: u32) -> Option<Identity> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: the output buffer is aligned and has the requested native size.
    if unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    } != size
    {
        return None;
    }
    // SAFETY: proc_pidinfo returned the complete initialized structure above.
    let info = unsafe { info.assume_init() };
    // SAFETY: geteuid takes no arguments and accesses no caller memory.
    if info.pbi_uid != unsafe { libc::geteuid() } || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(Identity {
        pid,
        parent: info.pbi_ppid,
        start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
    })
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn identity(_pid: u32) -> Option<Identity> {
    None
}

fn acquire(pid: u32) -> Option<Owned> {
    acquire_with(pid, identity, open_pidfd)
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open takes scalar arguments and returns a fresh fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        return None;
    }
    // SAFETY: the successful syscall returned an exclusively owned fd.
    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn open_pidfd(_pid: u32) -> Option<()> {
    Some(())
}

fn acquire_with(
    pid: u32,
    read_identity: fn(u32) -> Option<Identity>,
    #[cfg(target_os = "linux")] open: fn(u32) -> Option<std::os::fd::OwnedFd>,
    #[cfg(not(target_os = "linux"))] _open: fn(u32) -> Option<()>,
) -> Option<Owned> {
    let before = read_identity(pid)?;
    #[cfg(target_os = "linux")]
    let fd = open(pid)?;
    if read_identity(pid).as_ref() != Some(&before) {
        return None;
    }
    Some(Owned {
        identity: before,
        #[cfg(target_os = "linux")]
        fd,
    })
}
impl Owned {
    fn live(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let mut fd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: the owned pidfd and one-element output buffer are valid.
            let result = unsafe { libc::poll(&mut fd, 1, 0) };
            // Unknown liveness must not be mistaken for verified exit.
            result < 0
                || fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0
                || fd.revents & (libc::POLLIN | libc::POLLHUP) == 0
        }
        #[cfg(not(target_os = "linux"))]
        identity(self.identity.pid)
            .is_some_and(|now| now.pid == self.identity.pid && now.start == self.identity.start)
    }
    fn signal(&self, signal: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: our owned pidfd targets this incarnation even if its PID
            // is reused between the verification and the signal syscall.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            result == 0 || !self.live()
        }
        #[cfg(target_os = "macos")]
        {
            // Native start-time checks are useful observations, but macOS has
            // no pidfd-equivalent atomic signal target. Never risk signaling a
            // recycled descendant PID between a check and kill(). Upstream's
            // full grace remains available; unresolved cleanup reports failure.
            let _ = signal;
            !self.live()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = signal;
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn children(pid: u32) -> Option<Vec<u32>> {
    use std::io::Read;
    let mut out = HashSet::new();
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    for (index, task) in tasks.enumerate() {
        if index >= 1024 {
            return None;
        }
        let file = std::fs::File::open(task.ok()?.path().join("children")).ok()?;
        let mut pids = String::new();
        file.take(16 * 1024 + 1).read_to_string(&mut pids).ok()?;
        if pids.len() > 16 * 1024 {
            return None;
        }
        for pid in pids.split_whitespace() {
            out.insert(pid.parse::<u32>().ok()?);
            if out.len() > 1024 {
                return None;
            }
        }
    }
    Some(out.into_iter().collect())
}
#[cfg(target_os = "macos")]
fn children(pid: u32) -> Option<Vec<u32>> {
    let mut pids = vec![0u32; 4096];
    // SAFETY: the output buffer is writable for exactly the supplied byte size.
    let bytes = unsafe {
        libc::proc_listchildpids(
            pid as i32,
            pids.as_mut_ptr().cast(),
            (pids.len() * 4) as i32,
        )
    };
    if bytes < 0 || bytes as usize >= pids.len() * 4 {
        return None;
    }
    pids.truncate((bytes as usize / 4).min(pids.len()));
    Some(pids)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn children(_pid: u32) -> Option<Vec<u32>> {
    None
}

pub struct ProcessTree {
    root: u32,
    owned: HashMap<u32, Owned>,
    reliable: bool,
    acquire: fn(u32) -> Option<Owned>,
}
impl ProcessTree {
    pub fn new(root: u32) -> Self {
        Self::with_acquisition(root, acquire)
    }
    fn with_acquisition(root: u32, acquire: fn(u32) -> Option<Owned>) -> Self {
        let mut owned = HashMap::new();
        if let Some(node) = acquire(root) {
            owned.insert(root, node);
        }
        let reliable = owned.contains_key(&root);
        Self {
            root,
            owned,
            reliable,
            acquire,
        }
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn without_identity(root: u32) -> Self {
        Self::with_acquisition(root, |pid| acquire_with(pid, |_| None, open_pidfd))
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn without_pidfd(root: u32) -> Self {
        Self::with_acquisition(root, |pid| acquire_with(pid, identity, |_| None))
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn without_descendant_pidfds(root: u32) -> Self {
        let mut tree = Self::new(root);
        tree.acquire = |pid| acquire_with(pid, identity, |_| None);
        tree
    }
    pub fn tracking_reliable(&self) -> bool {
        self.reliable
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn kill_root_for_test(&self) {
        if let Some(root) = self.owned.get(&self.root) {
            let _ = root.signal(libc::SIGKILL);
        }
    }
    pub fn can_force(&self) -> bool {
        self.reliable && self.owned.get(&self.root).is_some_and(Owned::live)
    }
    pub fn observe(&mut self) {
        // Release dead incarnations/pidfds; the cap bounds concurrent owned
        // processes, not the lifetime number of tools this adapter has run.
        self.owned.retain(|_, node| node.live());
        let mut pending: Vec<_> = self.owned.keys().copied().collect();
        let mut seen = HashSet::new();
        while let Some(parent) = pending.pop() {
            if !seen.insert(parent) || !self.owned.get(&parent).is_some_and(Owned::live) {
                continue;
            }
            let Some(children) = children(parent) else {
                self.reliable = false;
                continue;
            };
            for pid in children {
                if self.owned.contains_key(&pid) {
                    pending.push(pid);
                    continue;
                }
                if self.owned.len() >= 1024 {
                    self.reliable = false;
                    continue;
                }
                if let Some(child) = (self.acquire)(pid) {
                    if child.identity.parent == parent
                        && self.owned.get(&parent).is_some_and(Owned::live)
                    {
                        self.owned.insert(pid, child);
                        pending.push(pid);
                    } else {
                        self.reliable = false;
                    }
                } else {
                    self.reliable = false;
                }
            }
        }
    }
    pub fn freeze_and_kill_descendants(&mut self) -> bool {
        if !self.can_force() {
            return false;
        }
        if let Some(root) = self.owned.get(&self.root) {
            if !root.signal(libc::SIGSTOP) {
                self.reliable = false;
                return false;
            }
        }
        // Freeze each observed generation before collecting its children.
        // The ongoing ledger retains observed PTY/new-session descendants even
        // if a parent exited or they were subsequently reparented.
        for _ in 0..8 {
            self.observe();
            for (pid, node) in &self.owned {
                if *pid != self.root && !node.signal(libc::SIGSTOP) {
                    self.reliable = false;
                }
            }
        }
        if !self.reliable {
            for node in self.owned.values() {
                let _ = node.signal(libc::SIGCONT);
            }
            return false;
        }
        let mut killed = true;
        for (pid, node) in &self.owned {
            if *pid != self.root {
                killed &= node.signal(libc::SIGKILL);
            }
        }
        if !killed {
            self.reliable = false;
        }
        // On failure resume any frozen processes; do not turn a failed
        // acquisition/signal into a parent-only kill or permanent tool stop.
        if !killed {
            for node in self.owned.values() {
                let _ = node.signal(libc::SIGCONT);
            }
        }
        if let Some(root) = self.owned.get(&self.root) {
            killed &= root.signal(libc::SIGCONT);
        }
        killed
    }
    pub fn kill_observed_descendants(&self) -> bool {
        let mut killed = true;
        for (pid, node) in &self.owned {
            if *pid != self.root {
                killed &= node.signal(libc::SIGKILL);
            }
        }
        killed
    }
    pub fn live_descendants(&self) -> bool {
        self.owned
            .iter()
            .any(|(pid, node)| *pid != self.root && node.live())
    }
}
