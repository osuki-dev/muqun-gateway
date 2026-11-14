//! Small per-OS behaviours the UI needs but must not branch on itself.

// Available to the UI; wired as those surfaces land.
#![allow(dead_code)]

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

/// Show `file` selected in the OS file manager: Finder's `open -R`,
/// Explorer's `/select,`, and on Linux the file manager's
/// `org.freedesktop.FileManager1.ShowItems` (Nautilus, Dolphin, Nemo, Thunar…),
/// falling back to opening the folder it is in.
pub fn reveal(file: &Path) {
    #[cfg(target_os = "macos")]
    detached("open", &["-R".as_ref(), file.as_os_str()]);
    #[cfg(target_os = "windows")]
    {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(file.as_os_str());
        detached("explorer", &[select.as_os_str()]);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let file = file.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("muqun-reveal".into())
            .spawn(move || {
                if !show_items(&file) {
                    if let Some(folder) = file.parent() {
                        open_path(folder);
                    }
                }
            });
        if let Err(err) = spawned {
            tracing::warn!("could not start the file manager: {err}");
        }
    }
}

#[cfg(target_os = "linux")]
fn show_items(file: &Path) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let uri = format!("file://{}", percent_path(file));
    runtime.block_on(async move {
        let call = async {
            let conn = zbus::Connection::session().await?;
            let proxy = zbus::Proxy::new(
                &conn,
                "org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
            )
            .await?;
            proxy
                .call_method("ShowItems", &(vec![uri.as_str()], ""))
                .await
                .map(|_| ())
        };
        match tokio::time::timeout(std::time::Duration::from_secs(5), call).await {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::debug!("no file manager answered ShowItems: {err}");
                false
            }
            Err(_) => false,
        }
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn show_items(_: &Path) -> bool {
    false
}

/// A path as a `file://` URI's path: everything but unreserved bytes and `/`
/// percent-encoded.
#[cfg(target_os = "linux")]
fn percent_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn detached(program: &str, args: &[&std::ffi::OsStr]) {
    match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(program, "could not start: {err}"),
    }
}

/// Start a fresh process of this binary, then return. The caller should
/// close the current window so config and agents are picked up again.
pub fn relaunch_self() {
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("could not resolve current executable for relaunch");
        return;
    };
    match std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(
            exe = %exe.display(),
            "could not relaunch Muqun: {err}"
        ),
    }
}

/// Open a file or folder with the OS default handler (images, video, …).
pub fn open_path(path: &Path) {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if path.is_file() && super::editors::is_text_or_code(path) {
            if let Ok(true) = super::editors::try_open_editor(path) {
                return;
            }
        }
    }
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

/// Open an HTTP(S) URL in the user's default browser.
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let opener = "xdg-open";
    match std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(opener, "could not open url {url}: {err}"),
    }
}
