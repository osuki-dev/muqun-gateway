//! Clean shutdown on SIGTERM / SIGINT (Ctrl+C on Windows), so a task
//! manager, `systemd` or a CI smoke test sees the process end with status 0
//! instead of a crash.
//!
//! A stop request only asks the event loop to quit; the window and its GPU
//! device go away on the UI thread, and the process then leaves through
//! [`leave`]. Calling `exit` from another thread instead ran the libraries'
//! destructors (the NVIDIA Vulkan driver's among them) under a frame still
//! being drawn, and the UI thread segfaulted inside the driver.

use std::time::Duration;

/// How long the app gets to wind down once asked to stop before the process
/// leaves anyway (the window may not exist yet, or teardown may hang).
pub const GRACE: Duration = Duration::from_secs(3);

/// Watch for SIGTERM / SIGINT on a thread of its own: the first one calls
/// `on_stop` (ask the event loop to quit), and the process leaves with status
/// 0 after [`GRACE`] if it has not by then.
pub fn on_request(on_stop: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("muqun-signals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::warn!("no stop-signal watcher: {err}");
                    return;
                }
            };
            let signal = runtime.block_on(requested());
            tracing::info!(signal, "stop requested");
            on_stop();
            std::thread::sleep(GRACE);
            tracing::warn!("shutdown took longer than {GRACE:?}; leaving now");
            leave(0);
        });
    if let Err(err) = spawned {
        tracing::warn!("no stop-signal watcher: {err}");
    }
}

/// Leave after [`GRACE`] from now unless the process has left by then: the
/// bound on teardown once the event loop has returned.
pub fn deadline() {
    let _ = std::thread::Builder::new()
        .name("muqun-deadline".into())
        .spawn(|| {
            std::thread::sleep(GRACE);
            tracing::warn!("shutdown took longer than {GRACE:?}; leaving now");
            leave(0);
        });
}

/// End the process with `code`, once the app's own state is saved.
///
/// On Unix this skips `atexit` handlers and library destructors (`_exit`):
/// they would run while the core runtime's threads, the PTY readers and the
/// audio and D-Bus threads are still live, which is the race that crashed
/// the app at every stop. Nothing of the app's needs them; its config is
/// flushed by then and the session store's SQLite journal is durable.
///
/// Standard output is not flushed: it is line-buffered (the log writes whole
/// lines), and its lock may be held by a thread that will never let go.
pub fn leave(code: i32) -> ! {
    #[cfg(unix)]
    // SAFETY: `_exit` is async-signal-safe and takes no locks; the process
    // ends here.
    unsafe {
        libc::_exit(code)
    }
    // Windows' ExitProcess stops the other threads before DLLs detach.
    #[cfg(not(unix))]
    std::process::exit(code)
}

/// Resolves when the process is asked to stop. Must run inside a tokio
/// runtime. Never resolves if the signal handlers cannot be installed (the
/// default behaviour of the signals then stays in force).
async fn requested() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("could not install the SIGTERM/SIGINT handlers");
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "Ctrl+C",
            Err(_) => std::future::pending().await,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD: &str = "MUQUN_SHUTDOWN_TEST_CHILD";

    /// The child half: an "event loop" that ends when `on_stop` says so
    /// ("quits"), or never ("hangs"), then leaves as `main` does.
    fn child(mode: &str) -> ! {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let quits = mode == "quits";
        super::on_request(move || {
            if quits {
                let _ = tx.send(());
            } else {
                std::mem::forget(tx);
            }
        });
        // Let the watcher install its handlers before saying so.
        std::thread::sleep(Duration::from_millis(300));
        // Straight to fd 1, past libtest's capture of `println!` (it lands
        // after libtest's "test … " on the same line).
        // SAFETY: write(2) of a static buffer.
        unsafe { libc::write(1, b"ready\n".as_ptr().cast(), 6) };
        let _ = rx.recv();
        super::deadline();
        super::leave(0);
    }

    #[test]
    fn sigterm_and_sigint_end_the_process_with_status_zero() {
        if let Ok(mode) = std::env::var(CHILD) {
            child(&mode);
        }
        let name = concat!(
            module_path!(),
            "::sigterm_and_sigint_end_the_process_with_status_zero"
        );
        let name = name.split_once("::").map_or(name, |(_, rest)| rest);
        for (mode, signal, within) in [
            ("quits", libc::SIGTERM, Duration::from_secs(2)),
            ("quits", libc::SIGINT, Duration::from_secs(2)),
            (
                "hangs",
                libc::SIGTERM,
                super::GRACE + Duration::from_secs(2),
            ),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--test-threads=1"])
                .env(CHILD, mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            assert!(
                lines.any(|l| l.is_ok_and(|l| l.ends_with("ready"))),
                "{mode}: child never got ready"
            );
            let sent = Instant::now();
            // SAFETY: kill(2) on our own child.
            unsafe { libc::kill(child.id() as libc::pid_t, signal) };
            let status = child.wait().unwrap();
            assert_eq!(status.code(), Some(0), "{mode}/{signal}: {status}");
            assert!(
                sent.elapsed() < within,
                "{mode}/{signal}: {:?}",
                sent.elapsed()
            );
        }
    }
}
