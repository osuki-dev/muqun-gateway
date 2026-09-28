//! Command-line interface: arguments, subcommands, and dispatch.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    auto_import_skip_reason, command_catalog, configure_backend, default_herdr_plugin_config_dir,
    default_herdr_plugin_state_dir, import_herdr_plugin, list_devices, manage, read_devices_at,
    revoke_device, run, run_service_command, setup, standalone_config_dir, standalone_state_dir,
    start_background, status, stop_background, SetupBackend, TransportEncryptionMode, CONFIG_FILE,
    DEFAULT_PORT, DEVICES_FILE, HERDR_PLUGIN_IMPORT_MARKER, PAIRING_FILE,
};

#[derive(Parser)]
#[command(name = "gateway", about = "Mobile gateway for terminal workspaces")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Setup {
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        socket_path: Option<String>,
        /// Terminal workspace backend to configure. Omit it to leave an
        /// existing install's backend alone (a bare `setup` never switches
        /// what an install already runs); a genuinely fresh install with
        /// nothing configured yet defaults to tmux.
        #[arg(long, value_enum)]
        backend: Option<SetupBackend>,
        /// Application transport encryption for newly paired devices.
        #[arg(long, value_enum)]
        transport_encryption: Option<TransportEncryptionMode>,
    },
    Run {
        #[arg(long)]
        config: Option<String>,
    },
    Start,
    Stop,
    Status,
    Manage,
    /// Keep the gateway running across a logout or a reboot.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Add, remove, or inspect terminal backends in this gateway.
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
    /// Refresh the locally cached agent slash-command catalog.
    Commands {
        #[command(subcommand)]
        command: CommandsCommand,
    },
    /// Adopt an existing Herdr plugin pairing into the standalone gateway.
    ImportHerdrPlugin {
        /// Installer mode: skip absent or already imported plugin state, but
        /// never ignore malformed pairing files.
        #[arg(long)]
        if_present: bool,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        target_config_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        target_state_dir: Option<PathBuf>,
    },
    /// List devices that hold a gateway token.
    Devices,
    /// Revoke one device's token, or every device token with --all.
    Revoke {
        /// Device id from `gateway devices`.
        device_id: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ServiceCommand {
    /// Register the gateway with this user's init system so it starts at login
    /// and comes back after a crash or a reboot.
    Install,
    /// Remove that registration. Pairings, devices and config are untouched.
    Uninstall,
    /// Whether an init system is currently managing the gateway.
    Status,
}

#[derive(Subcommand)]
pub(crate) enum CommandsCommand {
    /// Download the latest supported command snapshots once, on demand.
    Update {
        /// Fetch only when the initial local cache is missing or invalid.
        #[arg(long)]
        if_missing: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum BackendCommand {
    List,
    /// Opt a configured backend in/out of startup with the gateway.
    Autostart {
        id: String,
        #[arg(value_enum)]
        mode: BackendAutostartMode,
    },
    Add {
        #[arg(value_enum)]
        backend: SetupBackend,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        socket_path: Option<String>,
    },
    Remove {
        id: String,
    },
    /// Make one configured backend the first session returned to clients.
    Default {
        id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum BackendAutostartMode {
    On,
    Off,
}

/// Structured logging for the parts of the gateway that run unattended -- the
/// OpenCode SSE reader, the event mapper and the HTTP client. Everything goes
/// to stderr, which is the journal under systemd. `MUQUN_LOG` (or `RUST_LOG`)
/// overrides the default of `info`.
pub(crate) fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = std::env::var("MUQUN_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|raw| EnvFilter::try_new(raw).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    // A second initialisation is not an error worth failing a start over: it
    // only happens in tests that call into `run` more than once.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

pub(crate) async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Setup {
            public_url,
            port,
            socket_path,
            backend,
            transport_encryption,
        } => setup(
            public_url,
            port,
            socket_path,
            backend.map(Into::into),
            transport_encryption,
        )?,
        Command::Run { config } => run(config).await?,
        Command::Start => start_background()?,
        Command::Stop => stop_background()?,
        Command::Status => status()?,
        Command::Manage => manage()?,
        Command::Service { command } => run_service_command(command)?,
        Command::Backend { command } => configure_backend(command)?,
        Command::Commands { command } => match command {
            CommandsCommand::Update { if_missing } => {
                if if_missing && command_catalog::is_cached() {
                    println!("Agent command catalogs are already cached.");
                } else {
                    let count = command_catalog::update().await?;
                    println!("Updated {count} agent command catalogs.");
                }
            }
        },
        Command::ImportHerdrPlugin {
            if_present,
            config_dir,
            state_dir,
            target_config_dir,
            target_state_dir,
        } => {
            let source = config_dir.unwrap_or(default_herdr_plugin_config_dir()?);
            let target = target_config_dir.unwrap_or(standalone_config_dir()?);
            let absent = !source.join(CONFIG_FILE).try_exists()?
                && !source.join(PAIRING_FILE).try_exists()?;
            let imported = target.join(HERDR_PLUGIN_IMPORT_MARKER).try_exists()?;
            if !if_present {
                import_herdr_plugin(Some(source), state_dir, Some(target), target_state_dir)?;
            } else if !absent && !imported {
                let target_state = match &target_state_dir {
                    Some(path) => path.clone(),
                    None => standalone_state_dir()?,
                };
                match auto_import_skip_reason(&source, &target, &target_state) {
                    Some(reason) => {
                        let source_state = match &state_dir {
                            Some(path) => path.clone(),
                            None => default_herdr_plugin_state_dir()?,
                        };
                        let plugin_devices = read_devices_at(&source_state.join(DEVICES_FILE))
                            .map_or_else(
                                |_| String::from("an unreadable list of"),
                                |devices| devices.len().to_string(),
                            );
                        println!(
                            "==> Left the Herdr plugin pairing at {} alone: {reason}.\n    \
                             The plugin pairing has {plugin_devices} paired device(s). If those \
                             are the devices you use, stop the gateway\n    \
                             (`muqun-gateway service uninstall`, then `muqun-gateway stop`), \
                             run `muqun-gateway import-herdr-plugin`,\n    \
                             and re-run this installer. If it is a leftover, move that directory \
                             aside and this notice goes away.",
                            source.display()
                        );
                    }
                    None => import_herdr_plugin(
                        Some(source),
                        state_dir,
                        Some(target),
                        target_state_dir,
                    )?,
                }
            }
        }
        Command::Devices => list_devices()?,
        Command::Revoke { device_id, all } => revoke_device(device_id, all)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn event_filter_matches_dot_and_underscore() {
        assert_eq!(normalize_event_name("pane.updated"), "pane_updated");
        assert_eq!(normalize_event_name(" pane_updated "), "pane_updated");
    }
}
