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
                tracing::warn!("backend {} autostart failed: {error}; start it manually or restart the gateway to retry", session.id);
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

/// The transient unit an autostarted tmux server is given when the gateway
/// itself runs under systemd.
///
/// The server is a persistent terminal server, not a gateway worker: left in
/// the gateway's cgroup, systemd reports it as a leftover process on every
/// restart and folds its memory into the gateway's figure. Its own unit keeps
/// it just as independent while the gateway's accounting stays its own. The
/// name is derived from the socket, so two sessions never collide.
fn tmux_scope_unit(socket: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(socket.as_bytes());
    format!("muqun-tmux-{}", crate::hex(&digest[..8]))
}

/// Whether this process is running as a systemd service. `INVOCATION_ID` is
/// set by systemd for every service it starts and by nothing else, which is
/// exactly the case the wrapper exists for.
fn systemd_service_context() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

/// Run `direct` inside a transient systemd unit when `under_systemd`; hand it
/// back untouched everywhere else.
///
/// A foreground `muqun-gateway run`, macOS, or a Linux session without
/// `systemd-run` all keep the old path, and autostart never depends on the
/// wrapper: the unit is `RemainAfterExit`, so the tmux server outlives the
/// client exactly as it did before.
fn systemd_scoped(direct: Command, unit: &str, under_systemd: bool) -> Command {
    let path = std::env::var("PATH").unwrap_or_default();
    if !under_systemd || crate::login_env::lookup("systemd-run", &path).is_none() {
        return direct;
    }
    let mut wrapped = Command::new("systemd-run");
    wrapped.args([
        "--user",
        "--quiet",
        "--collect",
        &format!("--unit={unit}"),
        "--property=Type=oneshot",
        "--property=RemainAfterExit=yes",
        "--",
    ]);
    wrapped.arg(direct.as_std().get_program());
    wrapped.args(direct.as_std().get_args());
    for (name, value) in direct.as_std().get_envs() {
        match value {
            Some(value) => {
                wrapped.env(name, value);
            }
            None => {
                wrapped.env_remove(name);
            }
        }
    }
    if let Some(dir) = direct.as_std().get_current_dir() {
        wrapped.current_dir(dir);
    }
    wrapped
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    wrapped
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
            cmd.args(["new-session", "-d", "-s", "muqun"]);
            let mut cmd = systemd_scoped(
                cmd,
                &tmux_scope_unit(&session.socket_path),
                systemd_service_context(),
            );
            let status = tokio::time::timeout(Duration::from_secs(8), cmd.status()).await??;
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
    fn the_tmux_scope_unit_name_is_stable_and_systemd_safe() {
        let name = tmux_scope_unit("/tmp/a.sock");
        assert_eq!(name, tmux_scope_unit("/tmp/a.sock"));
        assert_ne!(name, tmux_scope_unit("/tmp/b.sock"));
        assert!(name.starts_with("muqun-tmux-"));
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'));
    }

    #[test]
    fn without_a_systemd_session_the_command_is_untouched() {
        // Tests do not run under a systemd service, so the wrapper must hand
        // the command back exactly as it was.
        let direct = tmux_command("/tmp/scope-test.sock");
        let scoped = systemd_scoped(direct, "muqun-tmux-test", false);
        assert_eq!(
            scoped.as_std().get_program(),
            std::ffi::OsStr::new(crate::backend::TMUX_PROGRAM)
        );
        let args: Vec<_> = scoped.as_std().get_args().collect();
        assert_eq!(args, vec!["-S", "/tmp/scope-test.sock"]);
    }

    #[test]
    fn a_systemd_session_wraps_the_command_and_keeps_its_arguments() {
        let path = std::env::var("PATH").unwrap_or_default();
        if crate::login_env::lookup("systemd-run", &path).is_none() {
            return;
        }
        let mut direct = tmux_command("/tmp/scope-test.sock");
        direct.args(["new-session", "-d", "-s", "muqun"]);
        let scoped = systemd_scoped(direct, "muqun-tmux-test", true);
        assert_eq!(scoped.as_std().get_program(), "systemd-run");
        let args: Vec<String> = scoped
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--unit=muqun-tmux-test"));
        assert!(args.iter().any(|a| a == "--property=RemainAfterExit=yes"));
        let after_separator: Vec<&String> = args.iter().skip_while(|a| *a != "--").collect();
        assert_eq!(
            after_separator,
            vec![
                "--",
                crate::backend::TMUX_PROGRAM,
                "-S",
                "/tmp/scope-test.sock",
                "new-session",
                "-d",
                "-s",
                "muqun"
            ]
        );
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
