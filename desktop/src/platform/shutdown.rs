//! Clean shutdown on SIGTERM / SIGINT (Ctrl+C on Windows), so a task
//! manager, `systemd` or a CI smoke test sees the process end with status 0
//! instead of being killed mid-frame.

// Available to the UI; wired as those surfaces land.
#![allow(dead_code)]

use std::time::Duration;

/// How long the window gets to close before the process leaves anyway
/// (it may not exist yet, or the compositor may be slow to answer).
pub const GRACE: Duration = Duration::from_secs(3);

/// Resolves when the process is asked to stop. Must run inside a tokio
/// runtime. Never resolves if the signal handlers cannot be installed (the
/// default behaviour of the signals then stays in force).
pub async fn requested() -> &'static str {
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
