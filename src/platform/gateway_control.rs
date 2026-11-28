//! Host-facing management API for this computer's local Muqun Gateway:
//! reads/edits `config.json` in the service directories, inspects running and
//! login-service status, manages terminal backends, and delegates process /
//! init-service actions to the installed `muqun-gateway` CLI binary.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::Context as _;

use super::service;
pub use super::service::ServiceState;
use super::state_lock;
use super::store::{service_config_dir, service_state_dir};
use crate::backend::BackendKind;
use crate::terminal::factory::backend_endpoint;
use crate::{
    auto_public_url, listen_for_explicit_public_url, make_backend_default, upsert_backend_session,
    validate_public_url, validate_session_id, write_config, Config, TransportEncryptionMode,
    CONFIG_FILE, DEFAULT_PORT, MAX_WORKSPACE_LABEL_CHARS, PID_FILE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayBackendInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub endpoint: String,
    pub is_default: bool,
    pub autostart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayControlSnapshot {
    pub configured: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub service_state: ServiceState,
    pub config_changed_since_start: bool,
    pub cli_available: bool,
    pub server_id: String,
    pub label: String,
    pub public_url: String,
    pub listen: String,
    pub port: u16,
    pub transport_encryption: bool,
    pub allow_remote_auto_approve: bool,
    pub agent_trust_folders: bool,
    pub rich_agent_pushes: bool,
    pub backends: Vec<GatewayBackendInfo>,
    pub config_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayControlAction {
    SetLabel(String),
    SetPublicUrl(String),
    AutoDetectPublicUrl,
    SetPort(u16),
    SetTransportEncryption(bool),
    SetAllowRemoteAutoApprove(bool),
    SetAgentTrustFolders(bool),
    SetRichAgentPushes(bool),
    SetDefaultBackend(String),
    SetBackendAutostart { id: String, enabled: bool },
    AddBackend(String),
    RemoveBackend(String),
    InstallService,
    UninstallService,
    StartGateway,
    StopGateway,
    RestartGateway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayControlOutcome {
    pub snapshot: GatewayControlSnapshot,
    pub message: String,
    pub restart_recommended: bool,
}

fn parse_port(listen: &str) -> u16 {
    listen
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn replace_listen_port(listen: &str, port: u16) -> String {
    if let Some((host, _)) = listen.rsplit_once(':') {
        format!("{host}:{port}")
    } else {
        format!("0.0.0.0:{port}")
    }
}

fn replace_public_url_port(public_url: &str, old_port: u16, new_port: u16) -> String {
    let Ok(mut url) = reqwest::Url::parse(public_url) else {
        return public_url.to_string();
    };
    if url.port() == Some(old_port) && url.set_port(Some(new_port)).is_ok() {
        return url.to_string().trim_end_matches('/').to_string();
    }
    public_url.to_string()
}

fn config_changed_since_start_at(config_path: &Path, state_dir: &Path, running: bool) -> bool {
    if !running {
        return false;
    }
    let Ok(cfg_mtime) = std::fs::metadata(config_path).and_then(|m| m.modified()) else {
        return false;
    };
    let pid_file = state_dir.join(PID_FILE);
    let Ok(pid_mtime) = std::fs::metadata(pid_file).and_then(|m| m.modified()) else {
        return false;
    };
    cfg_mtime > pid_mtime
}

/// Resolve the installed `muqun-gateway` executable for lifecycle/service commands.
pub fn resolve_gateway_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("muqun-gateway"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/muqun-gateway"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/muqun-gateway"));
    candidates.push(PathBuf::from("/usr/local/bin/muqun-gateway"));
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("muqun-gateway"))
            .find(|p| p.is_file())
    })
}

fn load_config_at(config_path: &Path) -> anyhow::Result<Option<Config>> {
    match std::fs::read(config_path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("failed to parse {}", config_path.display())
        })?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", config_path.display())),
    }
}

pub(crate) fn load_snapshot_at(
    config_dir: &Path,
    state_dir: &Path,
    service_state: ServiceState,
    cli_available: bool,
) -> anyhow::Result<GatewayControlSnapshot> {
    let config_path = config_dir.join(CONFIG_FILE);
    let pid = state_lock::running_owner(state_dir).unwrap_or(None);
    let running = pid.is_some();
    let Some(config) = load_config_at(&config_path)? else {
        return Ok(GatewayControlSnapshot {
            configured: false,
            running,
            pid,
            service_state,
            config_changed_since_start: false,
            cli_available,
            server_id: String::new(),
            label: String::new(),
            public_url: String::new(),
            listen: format!("127.0.0.1:{DEFAULT_PORT}"),
            port: DEFAULT_PORT,
            transport_encryption: true,
            allow_remote_auto_approve: true,
            agent_trust_folders: true,
            rich_agent_pushes: false,
            backends: Vec::new(),
            config_path: config_path.display().to_string(),
        });
    };
    let port = parse_port(&config.listen);
    let backends = config
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, session)| GatewayBackendInfo {
            id: session.id.clone(),
            label: session.label.clone(),
            kind: if session.is_desktop_terminal() {
                "desktop-terminal".into()
            } else {
                session.backend.as_str().into()
            },
            endpoint: backend_endpoint(session),
            is_default: idx == 0,
            autostart: config
                .autostart_backends
                .iter()
                .any(|item| item == &session.id),
        })
        .collect();
    Ok(GatewayControlSnapshot {
        configured: true,
        running,
        pid,
        service_state,
        config_changed_since_start: config_changed_since_start_at(&config_path, state_dir, running),
        cli_available,
        server_id: config.server_id,
        label: config.label,
        public_url: config.public_url,
        listen: config.listen,
        port,
        transport_encryption: config.transport_encryption == TransportEncryptionMode::Required,
        allow_remote_auto_approve: config.allow_remote_auto_approve,
        agent_trust_folders: config.agent_trust_folders,
        rich_agent_pushes: config.rich_agent_pushes,
        backends,
        config_path: config_path.display().to_string(),
    })
}

/// Read the local Gateway service's live status and configuration snapshot.
pub fn load_snapshot() -> anyhow::Result<GatewayControlSnapshot> {
    let config_dir = service_config_dir()?;
    let state_dir = service_state_dir()?;
    let svc = service::state().unwrap_or(ServiceState::NotInstalled);
    let cli_available = resolve_gateway_binary().is_some();
    load_snapshot_at(&config_dir, &state_dir, svc, cli_available)
}

fn apply_config_edit_at(
    config_dir: &Path,
    action: &GatewayControlAction,
) -> anyhow::Result<(String, bool)> {
    let config_path = config_dir.join(CONFIG_FILE);
    let mut config = load_config_at(&config_path)?
        .ok_or_else(|| anyhow::anyhow!("Local Gateway is not configured yet"))?;
    let (message, restart_recommended) = match action {
        GatewayControlAction::SetLabel(raw) => {
            let trimmed = raw.trim();
            anyhow::ensure!(!trimmed.is_empty(), "Computer name cannot be empty");
            anyhow::ensure!(
                trimmed.chars().count() <= MAX_WORKSPACE_LABEL_CHARS,
                "Computer name is too long (max {MAX_WORKSPACE_LABEL_CHARS} characters)"
            );
            config.label = trimmed.to_string();
            ("Computer name saved".to_string(), true)
        }
        GatewayControlAction::SetPublicUrl(raw) => {
            let url = validate_public_url(raw)?;
            let port = parse_port(&config.listen);
            config.listen = listen_for_explicit_public_url(&url, port);
            config.public_url = url;
            ("Public URL saved".to_string(), true)
        }
        GatewayControlAction::AutoDetectPublicUrl => {
            let port = parse_port(&config.listen);
            let selection = auto_public_url(port);
            config.public_url = selection.url;
            config.listen = format!("{}:{port}", selection.listen_host);
            (format!("Detected address ({})", selection.source), true)
        }
        GatewayControlAction::SetPort(port) => {
            anyhow::ensure!(*port >= 1, "Port must be between 1 and 65535");
            let old_port = parse_port(&config.listen);
            config.listen = replace_listen_port(&config.listen, *port);
            config.public_url = replace_public_url_port(&config.public_url, old_port, *port);
            (format!("Listen port updated to {port}"), true)
        }
        GatewayControlAction::SetTransportEncryption(required) => {
            config.transport_encryption = if *required {
                TransportEncryptionMode::Required
            } else {
                TransportEncryptionMode::Disabled
            };
            (
                if *required {
                    "Transport encryption enabled".to_string()
                } else {
                    "Transport encryption disabled".to_string()
                },
                true,
            )
        }
        GatewayControlAction::SetAllowRemoteAutoApprove(enabled) => {
            config.allow_remote_auto_approve = *enabled;
            (
                if *enabled {
                    "Remote auto-approve enabled".to_string()
                } else {
                    "Remote auto-approve disabled".to_string()
                },
                true,
            )
        }
        GatewayControlAction::SetAgentTrustFolders(enabled) => {
            config.agent_trust_folders = *enabled;
            (
                if *enabled {
                    "Automatic folder trust enabled".to_string()
                } else {
                    "Automatic folder trust disabled".to_string()
                },
                true,
            )
        }
        GatewayControlAction::SetRichAgentPushes(enabled) => {
            config.rich_agent_pushes = *enabled;
            (
                if *enabled {
                    "Detailed push notifications enabled".to_string()
                } else {
                    "Detailed push notifications disabled".to_string()
                },
                true,
            )
        }
        GatewayControlAction::SetDefaultBackend(id) => {
            make_backend_default(&mut config, id)?;
            (format!("Default backend set to {id}"), true)
        }
        GatewayControlAction::SetBackendAutostart { id, enabled } => {
            validate_session_id(id)?;
            anyhow::ensure!(
                config.sessions.iter().any(|s| s.id == *id),
                "Backend {id} not found"
            );
            config.autostart_backends.retain(|item| item != id);
            if *enabled {
                config.autostart_backends.push(id.clone());
            }
            (
                if *enabled {
                    format!("Autostart enabled for {id}")
                } else {
                    format!("Autostart disabled for {id}")
                },
                true,
            )
        }
        GatewayControlAction::AddBackend(kind_name) => {
            let backend = match kind_name.trim().to_ascii_lowercase().as_str() {
                "tmux" => BackendKind::Tmux,
                "herdr" => BackendKind::Herdr,
                other => anyhow::bail!("Unsupported backend: {other}"),
            };
            let id = upsert_backend_session(&mut config, backend, None, None, None)?;
            (format!("Added backend {id}"), true)
        }
        GatewayControlAction::RemoveBackend(id) => {
            validate_session_id(id)?;
            anyhow::ensure!(config.sessions.len() > 1, "Cannot remove the only backend");
            let prev = config.sessions.len();
            config.sessions.retain(|s| s.id != *id);
            config.autostart_backends.retain(|item| item != id);
            anyhow::ensure!(config.sessions.len() != prev, "Backend {id} not found");
            (format!("Removed backend {id}"), true)
        }
        GatewayControlAction::InstallService
        | GatewayControlAction::UninstallService
        | GatewayControlAction::StartGateway
        | GatewayControlAction::StopGateway
        | GatewayControlAction::RestartGateway => unreachable!(),
    };
    write_config(&config_path, &config)?;
    Ok((message, restart_recommended))
}

fn run_gateway_cli(args: &[&str]) -> anyhow::Result<String> {
    let bin = resolve_gateway_binary()
        .ok_or_else(|| anyhow::anyhow!("muqun-gateway binary was not found on this computer"))?;
    let output = ProcessCommand::new(&bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exited with {}", output.status)
        };
        anyhow::bail!("{detail}");
    }
    Ok(stdout.lines().last().unwrap_or("Done").to_string())
}

/// Apply a Gateway configuration or lifecycle action and return the refreshed snapshot.
pub fn apply(action: GatewayControlAction) -> anyhow::Result<GatewayControlOutcome> {
    let config_dir = service_config_dir()?;
    let (message, restart_recommended) = match &action {
        GatewayControlAction::InstallService => {
            let out = run_gateway_cli(&["service", "install"])?;
            (
                if out.is_empty() {
                    "Login service installed".into()
                } else {
                    out
                },
                false,
            )
        }
        GatewayControlAction::UninstallService => {
            let out = run_gateway_cli(&["service", "uninstall"])?;
            (
                if out.is_empty() {
                    "Login service removed".into()
                } else {
                    out
                },
                false,
            )
        }
        GatewayControlAction::StartGateway => {
            let out = run_gateway_cli(&["start"])?;
            (
                if out.is_empty() {
                    "Gateway started".into()
                } else {
                    out
                },
                false,
            )
        }
        GatewayControlAction::StopGateway => {
            let out = run_gateway_cli(&["stop"])?;
            (
                if out.is_empty() {
                    "Gateway stopped".into()
                } else {
                    out
                },
                false,
            )
        }
        GatewayControlAction::RestartGateway => {
            let out = run_gateway_cli(&["restart"])?;
            (
                if out.is_empty() {
                    "Gateway restarted".into()
                } else {
                    out
                },
                false,
            )
        }
        _ => apply_config_edit_at(&config_dir, &action)?,
    };
    let snapshot = load_snapshot()?;
    Ok(GatewayControlOutcome {
        snapshot,
        message,
        restart_recommended,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        serde_json::from_value(serde_json::json!({
            "server_id": "srv-test",
            "label": "studio",
            "listen": "0.0.0.0:23847",
            "public_url": "http://100.64.0.10:23847",
            "token_hash": "hash",
            "transport_encryption": "required",
            "sessions": [
                {
                    "id": "tmux",
                    "label": "tmux",
                    "backend": "tmux",
                    "socket_path": ""
                },
                {
                    "id": "herdr",
                    "label": "Herdr",
                    "backend": "herdr",
                    "socket_path": "/tmp/herdr.sock"
                }
            ]
        }))
        .expect("sample gateway config")
    }

    #[test]
    fn config_edits_persist_and_validate_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("cfg");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        write_config(&cfg_dir.join(CONFIG_FILE), &sample_config()).unwrap();

        let snap = load_snapshot_at(&cfg_dir, &state_dir, ServiceState::Installed, true).unwrap();
        assert!(snap.configured);
        assert_eq!(snap.label, "studio");
        assert_eq!(snap.port, 23847);
        assert!(snap.transport_encryption);
        assert_eq!(snap.backends.len(), 2);
        assert!(snap.backends[0].is_default);

        // Label validation & update
        assert!(
            apply_config_edit_at(&cfg_dir, &GatewayControlAction::SetLabel("   ".into())).is_err()
        );
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::SetLabel("workstation".into()),
        )
        .unwrap();

        // Port update updates both listen and matching public_url port
        apply_config_edit_at(&cfg_dir, &GatewayControlAction::SetPort(24000)).unwrap();

        // Security & push flags
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::SetTransportEncryption(false),
        )
        .unwrap();
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::SetAllowRemoteAutoApprove(false),
        )
        .unwrap();
        apply_config_edit_at(&cfg_dir, &GatewayControlAction::SetAgentTrustFolders(false)).unwrap();
        apply_config_edit_at(&cfg_dir, &GatewayControlAction::SetRichAgentPushes(true)).unwrap();

        // Backend default, autostart, remove
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::SetDefaultBackend("herdr".into()),
        )
        .unwrap();
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::SetBackendAutostart {
                id: "herdr".into(),
                enabled: true,
            },
        )
        .unwrap();
        apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::RemoveBackend("tmux".into()),
        )
        .unwrap();
        // Cannot remove the only remaining backend
        assert!(apply_config_edit_at(
            &cfg_dir,
            &GatewayControlAction::RemoveBackend("herdr".into())
        )
        .is_err());

        let updated =
            load_snapshot_at(&cfg_dir, &state_dir, ServiceState::Installed, true).unwrap();
        assert_eq!(updated.label, "workstation");
        assert_eq!(updated.port, 24000);
        assert_eq!(updated.listen, "0.0.0.0:24000");
        assert_eq!(updated.public_url, "http://100.64.0.10:24000");
        assert!(!updated.transport_encryption);
        assert!(!updated.allow_remote_auto_approve);
        assert!(!updated.agent_trust_folders);
        assert!(updated.rich_agent_pushes);
        assert_eq!(updated.backends.len(), 1);
        assert_eq!(updated.backends[0].id, "herdr");
        assert!(updated.backends[0].is_default);
        assert!(updated.backends[0].autostart);
    }
}
