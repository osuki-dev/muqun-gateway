//! Explicit, one-shot backend startup. Never supervise or replace user sessions.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::process::Command;

use crate::{backend::BackendKind, Config, SessionConfig};

/// Only standard Herdr session paths can safely identify the persistence store.
/// A custom socket alone does not identify that store: do not guess and restore
/// the default session into a second server.
fn herdr_session(socket: &Path, roots: &[PathBuf]) -> anyhow::Result<Option<String>> {
    for root in roots {
        if socket == root.join("herdr.sock") {
            return Ok(None);
        }
        if let Ok(relative) = socket.strip_prefix(root.join("sessions")) {
            let parts: Vec<_> = relative.components().collect();
            if parts.len() == 2 && parts[1].as_os_str() == "herdr.sock" {
                let name = parts[0].as_os_str().to_str().unwrap_or_default();
                anyhow::ensure!(
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                    "invalid Herdr session name"
                );
                return Ok(Some(name.to_owned()));
            }
        }
    }
    anyhow::bail!(
        "custom Herdr socket: start it manually; autostart requires a standard Herdr session path"
    )
}

fn herdr_roots() -> Vec<PathBuf> {
    // Herdr uses XDG paths on macOS too, not ~/Library/Application Support.
    vec![PathBuf::from(crate::default_socket_path())
        .parent()
        .unwrap()
        .to_path_buf()]
}

pub(crate) fn validate(session: &SessionConfig) -> anyhow::Result<()> {
    if session.backend == BackendKind::Herdr {
        herdr_session(Path::new(&session.socket_path), &herdr_roots())?;
    }
    Ok(())
}

pub(crate) fn spawn(config: &Config) {
    let mut started = std::collections::HashSet::new();
    for session in &config.sessions {
        if !config.autostart_backends.contains(&session.id) {
            continue;
        }
        if !started.insert((session.backend.as_str(), session.socket_path.clone())) {
            continue;
        }
        let session = session.clone();
        tokio::spawn(async move {
            if let Err(error) = start(&session).await {
                eprintln!("backend {} autostart failed: {error}; start it manually or restart the gateway to retry", session.id);
            }
        });
    }
}

fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No pane context may leak from a manually launched gateway into a new server.
    for name in [
        "TMUX",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_ENV",
    ] {
        cmd.env_remove(name);
    }
    if let Some(home) = dirs::home_dir() {
        cmd.current_dir(home);
    }
    cmd
}

fn tmux_command(socket: &str) -> Command {
    let mut cmd = command(crate::backend::TMUX_PROGRAM);
    if !socket.is_empty() {
        cmd.arg("-S").arg(socket);
    }
    cmd.kill_on_drop(true);
    cmd
}

async fn start(session: &SessionConfig) -> anyhow::Result<()> {
    validate(session)?;
    match session.backend {
        BackendKind::Tmux => {
            let mut probe = tmux_command(&session.socket_path);
            // list-sessions never creates a server. Reuse any existing sessions.
            let status =
                tokio::time::timeout(Duration::from_secs(3), probe.arg("list-sessions").status())
                    .await??;
            if status.success() {
                return Ok(());
            }
            let mut cmd = tmux_command(&session.socket_path);
            let status = tokio::time::timeout(
                Duration::from_secs(8),
                cmd.args(["new-session", "-d", "-s", "muqun"]).status(),
            )
            .await??;
            anyhow::ensure!(status.success(), "tmux did not create its initial session");
        }
        BackendKind::Herdr => {
            #[cfg(unix)]
            if tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::UnixStream::connect(&session.socket_path),
            )
            .await
            .is_ok_and(|result| result.is_ok())
            {
                return Ok(());
            }
            let mut cmd = command("herdr");
            if let Some(name) = herdr_session(Path::new(&session.socket_path), &herdr_roots())? {
                cmd.env("HERDR_SESSION", name);
            }
            cmd.env("HERDR_SOCKET_PATH", &session.socket_path)
                .arg("server");
            // It is an independent persistent terminal server, not a gateway
            // worker. Dropping/restarting the gateway must not terminate it.
            let mut child = cmd.spawn().context("could not start Herdr")?;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            #[cfg(unix)]
            {
                let ready = async {
                    for _ in 0..40 {
                        if tokio::net::UnixStream::connect(&session.socket_path)
                            .await
                            .is_ok()
                        {
                            return true;
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    false
                };
                anyhow::ensure!(
                    tokio::time::timeout(Duration::from_secs(10), ready)
                        .await
                        .unwrap_or(false),
                    "Herdr did not become reachable within 10 seconds"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_paths_identify_only_known_persistence_locations() {
        let roots = [PathBuf::from("/qa/herdr")];
        assert_eq!(
            herdr_session(Path::new("/qa/herdr/herdr.sock"), &roots).unwrap(),
            None
        );
        assert_eq!(
            herdr_session(Path::new("/qa/herdr/sessions/test-1/herdr.sock"), &roots).unwrap(),
            Some("test-1".into())
        );
        for path in [
            "/tmp/custom.sock",
            "/qa/herdr/sessions/../herdr.sock",
            "/qa/herdr/sessions/test/nested/herdr.sock",
        ] {
            assert!(herdr_session(Path::new(path), &roots).is_err());
        }
    }

    #[test]
    fn tmux_socket_is_a_single_argument_not_shell_text() {
        let command = tmux_command("/tmp/a socket;echo bad");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, ["-S", "/tmp/a socket;echo bad"]);
    }

    #[tokio::test]
    #[ignore = "requires tmux; uses an isolated socket and closes only its own server"]
    async fn real_tmux_startup_reuses_the_server_and_pane() {
        // macOS's long per-user temp prefix exceeds sockaddr_un.sun_path here.
        let dir = PathBuf::from("/tmp").join(format!("muqun-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let session = SessionConfig {
            id: "qa".into(),
            label: "QA".into(),
            backend: BackendKind::Tmux,
            socket_path: dir.join("tmux.sock").to_string_lossy().into_owned(),
        };
        let result = async {
            let backend = crate::backend_registry().connect(session.backend, &session.socket_path);
            anyhow::ensure!(!backend.probe_reachable().await.unwrap());
            start(&session).await?;
            anyhow::ensure!(backend.probe_reachable().await.unwrap());
            let first = tmux_command(&session.socket_path)
                .args(["list-panes", "-a", "-F", "#{pid}:#{pane_id}"])
                .output()
                .await?;
            start(&session).await?;
            let second = tmux_command(&session.socket_path)
                .args(["list-panes", "-a", "-F", "#{pid}:#{pane_id}"])
                .output()
                .await?;
            anyhow::ensure!(first.status.success() && !first.stdout.is_empty());
            anyhow::ensure!(
                first.stdout == second.stdout,
                "startup replaced a running terminal"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = tmux_command(&session.socket_path)
            .arg("kill-server")
            .status()
            .await;
        assert!(!crate::backend_registry()
            .connect(session.backend, &session.socket_path)
            .probe_reachable()
            .await
            .unwrap());
        let _ = std::fs::remove_file(dir.join("tmux.sock"));
        let _ = std::fs::remove_dir(&dir);
        result.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires Herdr; creates a unique named QA session and stops only that session"]
    async fn real_herdr_startup_reuses_the_server() {
        let name = format!("muqun-startup-{}", uuid::Uuid::new_v4().simple());
        let socket = PathBuf::from(crate::default_socket_path())
            .parent()
            .unwrap()
            .join("sessions")
            .join(&name)
            .join("herdr.sock");
        let session = SessionConfig {
            id: "qa".into(),
            label: "QA".into(),
            backend: BackendKind::Herdr,
            socket_path: socket.to_string_lossy().into_owned(),
        };
        let result = async {
            start(&session).await?;
            let registry = crate::backend_registry();
            let backend = registry.connect(session.backend, &session.socket_path);
            backend
                .metadata()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            #[cfg(unix)]
            let inode = {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&socket)?.ino()
            };
            start(&session).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                anyhow::ensure!(
                    inode == std::fs::metadata(&socket)?.ino(),
                    "startup replaced the socket"
                );
            }
            backend
                .metadata()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = command("herdr")
            .args(["--session", &name, "server", "stop"])
            .status()
            .await;
        result.unwrap();
    }
}
