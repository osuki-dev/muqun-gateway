//! Small per-OS behaviours the UI needs but must not branch on itself.

use std::path::Path;

/// Whether a native file dialog must be opened from the main (UI) thread.
/// macOS requires it; elsewhere it runs on its own thread so frames keep
/// coming while it is up.
pub fn dialogs_need_main_thread() -> bool {
    cfg!(target_os = "macos")
}

/// Show `folder` in the OS file manager. Fire and forget: the opener runs
/// detached and its outcome is only logged.
pub fn open_folder(folder: &Path) {
    open_path(folder);
}

/// Open a file or folder with the OS default handler (images, video, …).
pub fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let opener = "xdg-open";
    match std::process::Command::new(opener)
        .arg(path)
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
        Err(err) => tracing::warn!(opener, "could not open {}: {err}", path.display()),
    }
}
