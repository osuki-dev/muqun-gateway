//! Platform support for embedded plugin WebViews and their helper processes.
use std::{
    path::PathBuf,
    process::Child,
    time::{Duration, Instant},
};

pub(crate) fn webview_helper() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = match std::env::var_os("MUQUN_WEBVIEW_HELPER") {
            Some(path) => PathBuf::from(path),
            None => std::env::current_exe()
                .ok()?
                .with_file_name("muqun-wry-embedded"),
        };
        (path.is_absolute()
            && path
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .then_some(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// Keep the Unix group leader reserved until its children have been signalled.
// Reaping the leader with try_wait first would allow the kernel to reuse its PID.
#[cfg(target_os = "linux")]
pub(crate) fn running(child: &mut Child) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
            && info.assume_init().si_pid() == 0
    }
}

#[cfg(not(target_os = "linux"))]
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
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
