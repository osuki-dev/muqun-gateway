//! Platform support for embedded plugin WebViews and their helper processes.
use std::{
    path::PathBuf,
    process::Child,
    time::{Duration, Instant},
};

pub(crate) fn webview_helper() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let is_exec = |path: &PathBuf| {
            path.is_absolute()
                && path
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        if let Some(path) = std::env::var_os("MUQUN_WEBVIEW_HELPER") {
            let path = PathBuf::from(path);
            return is_exec(&path).then_some(path);
        }
        let exe = std::env::current_exe().ok()?;
        let sibling = exe.with_file_name("muqun-wry-embedded");
        if is_exec(&sibling) {
            return Some(sibling);
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(helpers) = exe
                .parent()
                .and_then(|macos| macos.parent())
                .map(|contents| contents.join("Helpers").join("muqun-wry-embedded"))
            {
                if is_exec(&helpers) {
                    return Some(helpers);
                }
            }
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

// Keep the Unix group leader reserved until its children have been signalled.
// Reaping the leader with try_wait first would allow the kernel to reuse its PID.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn running(child: &mut Child) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    unsafe {
        if libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            info.assume_init().si_pid() == 0
        }
        #[cfg(target_os = "macos")]
        {
            info.assume_init().si_pid == 0
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

pub(crate) fn stop(mut child: Child) {
    // The service contract uses EOF for orderly shutdown of its own backend.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(2);
    while running(&mut child) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
