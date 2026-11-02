//! Small per-OS behaviours the UI needs but must not branch on itself.

use std::path::Path;

/// How the platform's primary shortcut modifier is written in hints:
/// `⌘` on macOS, `Ctrl+` elsewhere. Prefix it to a key name.
pub fn primary_modifier() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+"
    }
}

/// Whether a native file dialog must be opened from the main (UI) thread.
/// macOS requires it; elsewhere it runs on its own thread so frames keep
/// coming while it is up.
pub fn dialogs_need_main_thread() -> bool {
    cfg!(target_os = "macos")
}

/// The local time zone's offset from UTC right now, in seconds (0 when the
/// OS will not say).
pub fn local_utc_offset() -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: `localtime_r` writes only into `tm`, which we own.
        unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&now, &mut tm).is_null() {
                return tm.tm_gmtoff;
            }
        }
    }
    0
}

/// Show `folder` in the OS file manager. Fire and forget: the opener runs
/// detached and its outcome is only logged.
pub fn open_folder(folder: &Path) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let opener = "xdg-open";
    match std::process::Command::new(opener)
        .arg(folder)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap it off-thread so it never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(opener, "could not open {}: {err}", folder.display()),
    }
}
