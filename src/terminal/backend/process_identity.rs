//! Native process identity, not PID identity. Unknown platforms fail closed.

/// Verify the root belongs to the reported native server. The caller obtains
/// both PIDs in the same native listing/peer exchange, not from a PID file.
pub(super) async fn pane_incarnation(server: u32, root: u32) -> Option<String> {
    tokio::task::spawn_blocking(move || verified_incarnation(server, root))
        .await
        .ok()
        .flatten()
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(u32, String)> {
    use std::os::unix::fs::MetadataExt;
    let path = format!("/proc/{pid}/stat");
    let metadata = std::fs::metadata(&path).ok()?;
    // SAFETY: geteuid has no arguments or memory requirements.
    if metadata.uid() != unsafe { libc::geteuid() } {
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
    let parent = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse::<u64>().ok()?;
    Some((parent, format!("{pid}:{start}")))
}

#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<(u32, String)> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: the kernel receives an exactly sized output buffer; it is read
    // only after proc_pidinfo reports that it initialized the complete struct.
    if unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    } != size
    {
        return None;
    }
    // SAFETY: successful full-sized proc_pidinfo initialized this struct.
    let info = unsafe { info.assume_init() };
    // SAFETY: geteuid has no arguments or memory requirements.
    if info.pbi_uid != unsafe { libc::geteuid() } || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some((
        info.pbi_ppid,
        format!("{pid}:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
    ))
}

#[cfg(target_os = "linux")]
fn boot_identity() -> Option<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    uuid::Uuid::parse_str(boot.trim()).ok()?;
    Some(format!("linux:{}", boot.trim()))
}

#[cfg(target_os = "macos")]
fn boot_identity() -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boot = std::mem::MaybeUninit::<libc::timeval>::uninit();
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: sysctl receives a two-element MIB and exactly sized output buffer.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            boot.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size != std::mem::size_of::<libc::timeval>()
    {
        return None;
    }
    // SAFETY: successful full-sized sysctl initialized this timeval.
    let boot = unsafe { boot.assume_init() };
    Some(format!("macos:{}:{}", boot.tv_sec, boot.tv_usec))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verified_incarnation(server: u32, root: u32) -> Option<String> {
    if server == 0 || root == 0 || server == root {
        return None;
    }
    let boot = boot_identity()?;
    let server_before = process(server)?;
    let root_before = process(root)?;
    let mut parent = root_before.0;
    for _ in 0..64 {
        if parent == server {
            if process(server)? != server_before || process(root)? != root_before {
                return None;
            }
            return Some(format!("{}:{}:{}", boot, server_before.1, root_before.1));
        }
        if parent <= 1 {
            return None;
        }
        parent = process(parent)?.0;
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn verified_incarnation(_server: u32, _root: u32) -> Option<String> {
    // A PID or command-line timestamp is not a substitute for kernel evidence.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_and_unrelated_processes_do_not_supply_an_incarnation() {
        assert!(pane_incarnation(0, std::process::id()).await.is_none());
        assert!(pane_incarnation(std::process::id(), std::process::id())
            .await
            .is_none());
    }
}
