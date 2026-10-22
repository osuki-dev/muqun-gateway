//! Interactive terminal management UI: setup, status, and device/backend actions.

use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossterm::cursor::MoveTo;
use crossterm::event::{
    poll as poll_event, read as read_event, Event as TerminalEvent, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::style::{Attribute, Color, ContentStyle};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};
use serde_json::Value;

use super::{lifecycle, service, update};
use crate::authority::{hash_token, DeviceRecord, PendingPairing};
use crate::backend::BackendKind;
use crate::{
    auto_public_url, backend_endpoint, config_changed_since_start, config_dir, configured_port,
    ensure_pairing_transport_key, load_config, make_backend_default, now_unix_ms, read_devices,
    read_pairing_file, revoke_device_by_id, start_background_inner, stop_background_inner,
    upsert_backend_session, validate_session_id, write_config, write_secret_file, SessionConfig,
    TransportEncryptionMode, CONFIG_FILE, DEFAULT_PORT, MANAGE_REFRESH_INTERVAL, PAIRING_FILE,
};

/// What `status` prints after a session's endpoint: tmux's availability,
/// judged and labelled the way manage's Overview does.
fn backend_program_state(session: &SessionConfig, service_path: Option<&str>) -> String {
    if session.backend != BackendKind::Tmux {
        return String::new();
    }
    tmux_availability(&std::env::var("PATH").unwrap_or_default(), service_path)
}

pub(crate) fn status() -> anyhow::Result<()> {
    let service_path = service::installed_path_env();
    let config = load_config(None)?;
    println!("server_id: {}", config.server_id);
    println!("label: {}", config.label);
    println!("listen: {}", config.listen);
    println!("public_url: {}", config.public_url);
    println!(
        "transport_encryption: {}",
        config.transport_encryption.as_str()
    );
    for session in config.sessions {
        let endpoint = backend_endpoint(&session);
        println!(
            "session {}: backend={} endpoint={endpoint} {}",
            session.id,
            session.backend.as_str(),
            backend_program_state(&session, service_path.as_deref())
        );
    }
    println!("gateway: {}", lifecycle::summary()?);
    println!("gateway autostart: {:?}", service::state()?);
    Ok(())
}

/// The status line when nothing has happened on the current view yet.
const IDLE_STATUS: &str = "ready";

pub(crate) fn manage() -> anyhow::Result<()> {
    ensure_pairing_transport_key()?;
    let _terminal = TerminalModeGuard::enter()?;
    let mut message = auto_upgrade_local_public_url()?.unwrap_or_else(|| String::from(IDLE_STATUS));
    let mut pending_pairing = fetch_pending_pairing().ok().flatten();
    let mut devices = match read_devices() {
        Ok(devices) => devices,
        Err(error) => {
            // An unreadable device file is not an empty one. Reporting it as
            // "no paired devices" is how an owner is talked into re-pairing,
            // and re-pairing is the write that makes the loss permanent.
            message = format!("could not read paired devices: {error}");
            Vec::new()
        }
    };
    // The pairing view is open while a phone is being paired: `p` opens it, a
    // phone that has already asked for a code opens it on its own, and Esc,
    // Enter or `r` return to the device list.
    let mut pairing = PairingView {
        open: pending_pairing.is_some(),
        paired: None,
    };
    let links = spawn_link_poller();
    let mut connectivity = Connectivity::default();
    let mut ui = ManageView::default();
    print_manage_screen(
        &message,
        pending_pairing.as_ref(),
        &devices,
        &pairing,
        connectivity,
        &mut ui,
    )?;

    loop {
        if let Some(latest) = links.try_iter().last() {
            connectivity = latest;
        }
        if !poll_event(MANAGE_REFRESH_INTERVAL)? {
            let next_pending_pairing = fetch_pending_pairing().ok().flatten();
            // A refresh that cannot read the file keeps showing the last list
            // it could read, rather than blanking the screen mid-write.
            let Ok(next_devices) = read_devices() else {
                continue;
            };
            if next_pending_pairing != pending_pairing || next_devices != devices {
                // A phone that scanned the QR (or typed the address) opens the
                // pairing view on its code step; a finished pairing keeps the
                // view open on its "Done" step, naming the new device.
                if next_devices.len() > devices.len() {
                    let name = next_devices
                        .last()
                        .map(|device| device.name.clone())
                        .unwrap_or_default();
                    message = format!("paired as {}", truncate(&name, 32));
                    pairing.open = true;
                    pairing.paired = Some(name);
                } else if next_pending_pairing.is_some()
                    && next_pending_pairing.as_ref().map(|p| &p.request_id)
                        != pending_pairing.as_ref().map(|p| &p.request_id)
                {
                    pairing.open = true;
                    pairing.paired = None;
                    ui.scroll = 0;
                    if ui.section != Section::Devices {
                        message = String::from("A phone is waiting for its code: open 3 Devices.");
                    }
                }
                pending_pairing = next_pending_pairing;
                devices = next_devices;
            }
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                &pairing,
                connectivity,
                &mut ui,
            )?;
            continue;
        }

        let event = read_event()?;
        let TerminalEvent::Key(event) = event else {
            if matches!(event, TerminalEvent::Resize(_, _)) {
                print_manage_screen(
                    &message,
                    pending_pairing.as_ref(),
                    &devices,
                    &pairing,
                    connectivity,
                    &mut ui,
                )?;
            }
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
            break;
        }
        let item_count = match ui.section {
            Section::Terminals => load_config(None)?.sessions.len(),
            Section::Devices if !pairing.open => devices.len(),
            Section::Settings => Setting::ALL.len(),
            _ => 0,
        };
        if event.code == KeyCode::Esc {
            // One level back: pairing view -> device list -> Overview -> closed.
            if ui.section == Section::Devices && pairing.open {
                pairing.close();
                message = String::from("Pairing view closed; existing devices are unchanged.");
            } else if ui.section != Section::Overview {
                ui = ManageView {
                    restart: std::mem::take(&mut ui.restart),
                    ..ManageView::default()
                };
                message = String::from(IDLE_STATUS);
            } else {
                break;
            }
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                &pairing,
                connectivity,
                &mut ui,
            )?;
            continue;
        }
        let section = ui.section;
        if ui.navigate(event.code, item_count) {
            // A status line describes the last action on the view it was
            // taken on; carried to another view it reads as that view's.
            if ui.section != section {
                message = String::from(IDLE_STATUS);
            }
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                &pairing,
                connectivity,
                &mut ui,
            )?;
            continue;
        }
        let input = match event.code {
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Enter => enter_action(ui.section, item_count == 0, pairing.open)
                .unwrap_or_default()
                .to_owned(),
            _ => String::new(),
        };
        if input == "q" {
            break;
        }
        // Only the active view's keys may change state. Every failed action is
        // shown in the status line, never an unexpected exit from raw mode.
        let outcome = (|| -> anyhow::Result<()> {
            match (ui.section, input.as_str()) {
                (Section::Overview, "s") => {
                    // A refusal -- another gateway already owns this state
                    // directory -- belongs on the status line. Propagating it
                    // would drop the operator out of the UI on a keypress.
                    start_background_inner(false)?;
                    message = String::from("Gateway started. Terminal tasks are unchanged.");
                }
                (Section::Overview, "t") => {
                    if !confirm_action(
                        "Stop gateway?",
                        &[
                            String::from(
                                "Phones will lose Gateway access until you start it again.",
                            ),
                            String::from(
                                "Terminal servers, sessions, and running tasks stay running.",
                            ),
                            String::from("Login autostart stays registered; pairing is unchanged."),
                        ],
                        "stop gateway",
                    )? {
                        message = String::from("Gateway stop cancelled.");
                        return Ok(());
                    }
                    stop_background_inner(false)?;
                    message = String::from("Gateway stopped. Terminal tasks are still running.");
                }
                (Section::Overview | Section::Settings, "r") => {
                    crate::restart_gateway(false)?;
                    ui.restart.clear();
                    message = String::from("gateway restarted");
                }
                (Section::Devices, "p") => {
                    pairing.open = true;
                    pairing.paired = None;
                    ui.scroll = 0;
                    message = String::from("scan to pair another device");
                }
                (Section::Devices, "x" | "enter") if !pairing.open => {
                    if devices.is_empty() {
                        message = String::from("no paired devices to revoke");
                    } else if let Some(device) = devices.iter().rev().nth(ui.selected).cloned() {
                        if input == "enter" && !show_device(&device)? {
                            message = String::from(IDLE_STATUS);
                            return Ok(());
                        }
                        if !confirm_revoke_device(&device)? {
                            message = String::from("revoke cancelled");
                            return Ok(());
                        }
                        // A revocation that could not be carried out is a status
                        // line, not an exit: dropping the operator out of the UI
                        // mid-revoke tells them nothing about what happened.
                        match revoke_managed_device(&device.id) {
                            Ok(true) => {
                                message = format!(
                                    "revoked {}; scan to pair again",
                                    truncate(&device.name, 32)
                                );
                                // Revocation usually means the user is replacing
                                // this app's credential. Return straight to the
                                // pairing QR instead of leaving them on the
                                // remaining device list.
                                pairing.open = true;
                                pairing.paired = None;
                            }
                            Ok(false) => message = String::from("device was already revoked"),
                            Err(error) => message = first_line(&format!("{error:#}")),
                        }
                    } else {
                        message = String::from("revoke cancelled");
                    }
                }
                (Section::Devices, "r") => {
                    pairing.close();
                    ui.scroll = 0;
                    message = String::from("Pairing view closed; paired devices are unchanged.");
                }
                (Section::Terminals, "h") => {
                    message = enable_managed_backend(BackendKind::Herdr)?;
                }
                (Section::Terminals, "m") => {
                    message = enable_managed_backend(BackendKind::Tmux)?;
                }
                (Section::Terminals, "enter") => {
                    message = backend_actions(ui.selected)?;
                    if message.starts_with("default is now") {
                        ui.selected = 0;
                        ui.scroll = 0;
                    }
                }
                (Section::Settings, "enter") => {
                    let setting = Setting::ALL[ui.selected.min(Setting::ALL.len() - 1)];
                    let before = load_config(None)?;
                    match setting {
                        Setting::Autostart => match autostart_toggle(service::state()) {
                            AutostartToggle::Refuse(reason) => message = reason,
                            toggle => {
                                let enable = toggle == AutostartToggle::Install;
                                if confirm_gateway_autostart(enable)? {
                                    crate::run_service_command(if enable {
                                        crate::ServiceCommand::Install
                                    } else {
                                        crate::ServiceCommand::Uninstall
                                    })?;
                                    message = format!(
                                        "gateway autostart {}",
                                        if enable { "on" } else { "off" }
                                    );
                                } else {
                                    message = String::from("gateway autostart unchanged");
                                }
                            }
                        },
                        Setting::Address => match prompt_public_url(None)? {
                            Some(url) => {
                                let listen =
                                    listen_for_explicit_public_url(&url, configured_port());
                                update_public_url(&url, &listen)?;
                                message = format!("url updated: {}", truncate(&url, 36));
                            }
                            None => {
                                message = String::from("url unchanged");
                            }
                        },
                        Setting::DetectAddress => {
                            let port = configured_port();
                            let selection = auto_public_url(port);
                            match prompt_public_url(Some(&selection.url))? {
                                Some(url) => {
                                    let listen = if url == selection.url {
                                        format!("{}:{port}", selection.listen_host)
                                    } else {
                                        listen_for_explicit_public_url(&url, port)
                                    };
                                    update_public_url(&url, &listen)?;
                                    message = format!("url updated: {}", truncate(&url, 36));
                                }
                                None => message = String::from("url unchanged"),
                            }
                        }
                        Setting::TransportEncryption => message = toggle_transport_encryption()?,
                        Setting::Updates => {
                            message = String::from(
                                "Checking official stable release before confirmation...",
                            );
                            print_manage_screen(
                                &message,
                                pending_pairing.as_ref(),
                                &devices,
                                &pairing,
                                connectivity,
                                &mut ui,
                            )?;
                            let plan = update::in_thread(update::check)?;
                            if !plan.available() {
                        message = plan.summary();
                    } else if confirm_action("Install stable Gateway update?", &[
                        plan.summary(),
                        String::from("Download, SHA256, platform and version are checked before downtime."),
                        String::from("Running Gateway restarts through its owner; stopped Gateway stays stopped."),
                        String::from("Phone access pauses briefly. Pairings, config and terminal tasks stay unchanged."),
                        String::from("This manager stays open on its old image; reopen it to load the new UI."),
                    ], "update Gateway")? {
                        message = String::from("Downloading/verifying, then updating. Please wait; do not close this manager.");
                        print_manage_screen(&message, pending_pairing.as_ref(), &devices, &pairing, connectivity, &mut ui)?;
                        message = update::in_thread(move || update::apply(plan))?;
                    } else {
                        message = String::from("Gateway update cancelled; no binary downloaded or restart.");
                    }
                        }
                        Setting::HistoryStorage => match choose_history_storage()? {
                            Some(changed) => message = changed,
                            None => message = String::from("History checkpoints unchanged."),
                        },
                        Setting::AgentBypass => message = toggle_agent_bypass()?,
                        Setting::AgentTrust => message = toggle_agent_trust()?,
                        Setting::Mcp => message = toggle_mcp()?,
                        Setting::McpInject => message = toggle_mcp_inject()?,
                        Setting::StoreEncryption | Setting::AgentEnv | Setting::McpLoopback => {
                            message = setting.read_only().unwrap_or(IDLE_STATUS).to_owned();
                        }
                    }
                    // A change that needs a restart says so on its row.
                    let after = load_config(None)?;
                    let changed = serde_json::to_value(&before)? != serde_json::to_value(&after)?;
                    if changed && setting.needs_restart() && !ui.restart.contains(&setting) {
                        ui.restart.push(setting);
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            message = format!("Action failed: {}", first_line(&error.to_string()));
        }

        pending_pairing = fetch_pending_pairing().ok().flatten();
        match read_devices() {
            Ok(next_devices) => devices = next_devices,
            Err(error) => message = format!("could not read paired devices: {error}"),
        }
        print_manage_screen(
            &message,
            pending_pairing.as_ref(),
            &devices,
            &pairing,
            connectivity,
            &mut ui,
        )?;
    }
    Ok(())
}

pub(crate) fn enable_managed_backend(backend: BackendKind) -> anyhow::Result<String> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    if let Some(session) = config
        .sessions
        .iter()
        .find(|session| session.backend == backend)
    {
        return Ok(format!(
            "{} backend already configured as {}",
            backend.as_str(),
            session.id
        ));
    }
    let id = upsert_backend_session(&mut config, backend, None, None, None)?;
    write_config(&path, &config)?;
    Ok(format!("added {id}; restart gateway to apply"))
}

pub(crate) fn toggle_transport_encryption() -> anyhow::Result<String> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.transport_encryption = match config.transport_encryption {
        TransportEncryptionMode::Required => TransportEncryptionMode::Disabled,
        TransportEncryptionMode::Disabled => TransportEncryptionMode::Required,
    };
    let mode = config.transport_encryption;
    write_config(&path, &config)?;
    let warning = if mode == TransportEncryptionMode::Disabled {
        "; warning: leaked bearer tokens can call the API"
    } else {
        ""
    };
    Ok(format!(
        "encryption {}{warning}; restart gateway to apply",
        mode.as_str()
    ))
}

pub(crate) fn remove_managed_backend(id: &str) -> anyhow::Result<String> {
    validate_session_id(id)?;
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    anyhow::ensure!(config.sessions.len() > 1, "cannot remove the only backend");
    let previous_len = config.sessions.len();
    config.sessions.retain(|session| session.id != id);
    config.autostart_backends.retain(|item| item != id);
    anyhow::ensure!(
        config.sessions.len() != previous_len,
        "backend {id} not found"
    );
    write_config(&path, &config)?;
    Ok(format!("removed {id}; restart gateway to apply"))
}

pub(crate) fn set_managed_default_backend(id: &str) -> anyhow::Result<String> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    make_backend_default(&mut config, id)?;
    write_config(&path, &config)?;
    Ok(format!("default is now {id}; restart gateway to apply"))
}

pub(crate) fn confirm_remove_backend(session: &SessionConfig) -> anyhow::Result<bool> {
    confirm_action(
        "Remove terminal backend?",
        &[
            format!("Selected: {} ({})", session.label, session.backend.as_str()),
            String::from("Only this Gateway's backend configuration is removed."),
            String::from("Terminal servers, sessions, and running tasks stay running."),
            String::from("Restart Gateway after this change."),
        ],
        "remove backend",
    )
}

pub(crate) fn prompt_public_url(offered: Option<&str>) -> anyhow::Result<Option<String>> {
    let current = load_config(None)
        .map(|config| config.public_url)
        .unwrap_or_else(|_| auto_public_url(configured_port()).url);
    // An offered address (Detect address) is shown for Enter to apply.
    let mut value = offered.map_or(current, str::to_owned);
    let mut invalid = false;
    loop {
        render_public_url_prompt(&value, invalid)?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter => {
                    if let Ok(url) = validate_public_url(&value) {
                        return Ok(Some(url));
                    }
                    invalid = true;
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(ch) if !ch.is_control() => value.push(ch),
                _ => {}
            }
        }
    }
}

fn render_public_url_prompt(value: &str, invalid: bool) -> anyhow::Result<()> {
    let mut lines = vec![
        String::from("This address is shared with your phone for pairing."),
        format!("Use HTTPS with Tailscale Serve, or http://<tailscale-ip>:{DEFAULT_PORT}."),
        format!("Address: {value}"),
    ];
    if invalid {
        lines.push(String::from(
            "Invalid address: use http:// or https:// without credentials or a query.",
        ));
    }
    write_dialog(
        "Edit Gateway address",
        &lines,
        ScreenLine::text("Enter save · Esc cancel · Backspace delete", Tone::Accent),
    )
}

pub(crate) fn confirm_revoke_device(device: &DeviceRecord) -> anyhow::Result<bool> {
    confirm_action(
        "Revoke selected device?",
        &[
            format!("Selected: {}", device.name),
            String::from("This device loses Gateway access immediately; it must pair again."),
            String::from("Other devices and terminal tasks are unchanged."),
        ],
        "revoke device",
    )
}

/// Point the gateway at a new address -- and move its listener with it.
///
/// The listener is not optional here. This used to write `public_url` alone,
/// which is how an install that was set up before Tailscale was running ended
/// up advertising a tailnet name while still bound to 127.0.0.1: the startup
/// auto-upgrade rewrote the URL, left the socket on loopback, and the gateway
/// came up clean announcing an address that nothing anywhere was listening on.
/// A URL and the socket that answers it are one decision, so they are one
/// write.
pub(crate) fn update_public_url(public_url: &str, listen: &str) -> anyhow::Result<()> {
    let public_url = validate_public_url(public_url)?;
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.public_url = public_url.clone();
    config.listen = listen.to_owned();
    write_secret_file(&config_path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    let pairing_path = config_dir()?.join(PAIRING_FILE);
    let mut pairing = read_pairing_file()?;
    pairing.payload.url = public_url;
    write_secret_file(&pairing_path, &serde_json::to_vec_pretty(&pairing)?)?;
    Ok(())
}

pub(crate) fn auto_upgrade_local_public_url() -> anyhow::Result<Option<String>> {
    let Ok(config) = load_config(None) else {
        return Ok(None);
    };
    if !is_local_public_url(&config.public_url) {
        return Ok(None);
    }
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let selection = auto_public_url(listen.port());
    if is_local_public_url(&selection.url) || selection.url == config.public_url {
        return Ok(None);
    }
    update_public_url(
        &selection.url,
        &format!("{}:{}", selection.listen_host, listen.port()),
    )?;
    Ok(Some(format!("auto url: {}", truncate(&selection.url, 36))))
}

/// A listener bound to loopback under an address no one outside can use.
///
/// This combination starts cleanly and serves nobody: the QR carries a name
/// that resolves to a real interface, and the socket is only on 127.0.0.1, so
/// every phone gets a connection refused and the app -- which cannot tell that
/// apart from a bad code -- reports the code as refused. It is silent, it is
/// automatic (see `update_public_url`), and it cost a day of looking in the
/// wrong place, so it says so now.
///
/// Loopback with an `https://` address is left alone: that is what Tailscale
/// Serve looks like, and Serve is *supposed* to terminate TLS outside and
/// proxy in over loopback.
pub(crate) fn unreachable_listen_warning(listen: &str, public_url: &str) -> Option<String> {
    let host = listen.rsplit_once(':').map(|(host, _)| host)?;
    let bound_to_loopback = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    if !bound_to_loopback || public_url.starts_with("https://") || is_local_public_url(public_url) {
        return None;
    }
    Some(format!(
        "warning: this gateway answers only on {listen}, but tells devices to reach it at \
         {public_url}. Nothing outside this machine can connect, and a phone will report the \
         pairing code as refused. Fix it with: muqun-gateway manage, then press [a] to detect \
          the address again in [4] Settings."
    ))
}

pub(crate) fn is_local_public_url(url: &str) -> bool {
    url.contains("127.0.0.1") || url.contains("localhost")
}

pub(crate) struct TerminalModeGuard;

impl TerminalModeGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )
        .inspect_err(|_| {
            let _ = disable_raw_mode();
        })?;
        Ok(Self)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Section {
    #[default]
    Overview,
    Terminals,
    Devices,
    Settings,
}

impl Section {
    const ALL: [Self; 4] = [
        Self::Overview,
        Self::Terminals,
        Self::Devices,
        Self::Settings,
    ];
}

/// The Devices view's pairing flow. `open` shows it instead of the device
/// list; `paired` is the device a just-finished pairing produced, which keeps
/// the flow on its "Done" step until the operator leaves or pairs another.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PairingView {
    open: bool,
    paired: Option<String>,
}

impl PairingView {
    fn close(&mut self) {
        self.open = false;
        self.paired = None;
    }
    /// Which of the three steps the operator is on, given whether a phone has
    /// asked for its code.
    fn stage(&self, pending: bool) -> PairingStage {
        if self.paired.is_some() {
            PairingStage::Done
        } else if pending {
            PairingStage::Code
        } else {
            PairingStage::Scan
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingStage {
    Scan,
    Code,
    Done,
}

/// One row of the pairing flow: "1 Scan the QR in Muqun · 2 Type this code on
/// the phone · 3 Done", with the current step highlighted. Where that row
/// does not fit in `width` cells, only the current step is shown, as
/// "Step N of 3: <text>", rather than a row clipped mid-step.
fn pairing_steps(stage: PairingStage, width: usize) -> ScreenLine {
    let steps = [
        (PairingStage::Scan, "Scan the QR in Muqun"),
        (PairingStage::Code, "Type this code on the phone"),
        (PairingStage::Done, "Done"),
    ];
    let mut spans = Vec::new();
    for (index, (step, text)) in steps.iter().enumerate() {
        if index > 0 {
            spans.push((" · ".into(), Tone::Quiet));
        }
        spans.push((
            format!("{} {text}", index + 1),
            if *step == stage {
                Tone::Badge
            } else {
                Tone::Quiet
            },
        ));
    }
    let full = ScreenLine { spans, qr: None };
    if full.width() <= width {
        return full;
    }
    let (index, (_, text)) = steps
        .iter()
        .enumerate()
        .find(|(_, (step, _))| *step == stage)
        .expect("every stage is a step");
    ScreenLine {
        spans: vec![
            (
                format!("Step {} of {}: ", index + 1, steps.len()),
                Tone::Quiet,
            ),
            ((*text).into(), Tone::Badge),
        ],
        qr: None,
    }
}

/// Whether one of the gateway's services answers, as the header shows it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// Nothing could be asked: the gateway is stopped or has not answered yet.
    #[default]
    Unknown,
    Connected,
    Offline,
    NotConfigured,
}

impl Link {
    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Connected => "connected",
            Self::Offline => "offline",
            Self::NotConfigured => "none",
        }
    }
}

/// What the running gateway reports about its backends and agents.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Connectivity {
    herdr: Link,
    agents: Link,
}

/// Everything the one-line header says.
struct HeaderStatus<'a> {
    version: &'a str,
    listen: &'a str,
    running: bool,
    links: Connectivity,
    devices: usize,
}

/// The persistent header: version, where the gateway listens, whether Herdr
/// and the agent service answer, and how many phones are paired. Falls back
/// to a shorter form when the full one does not fit the width.
fn header_line(status: &HeaderStatus<'_>, width: usize) -> String {
    let devices = format!(
        "{} device{}",
        status.devices,
        if status.devices == 1 { "" } else { "s" }
    );
    let links = format!(
        "Herdr {} · Agents {}",
        status.links.herdr.label(),
        status.links.agents.label()
    );
    let full = if status.running {
        format!(
            "Muqun Gateway v{} · Running {} · {links} · {devices}",
            status.version, status.listen
        )
    } else {
        format!(
            "Muqun Gateway v{} · Stopped · listens on {} when started · {devices}",
            status.version, status.listen
        )
    };
    if display_width(&full) <= width {
        return full;
    }
    let state = if status.running { "Running" } else { "Stopped" };
    let compact = if status.running {
        format!(
            "v{} · {state} {} · {links} · {devices}",
            status.version, status.listen
        )
    } else {
        format!(
            "v{} · {state} · {} · {devices}",
            status.version, status.listen
        )
    };
    if display_width(&compact) <= width {
        return compact;
    }
    format!("v{} · {state} · {}", status.version, status.listen)
}

/// Read `GET /api/capabilities` into the header's two links: the Herdr
/// backend's `connected` flag and whether any agent answers.
fn connectivity_from_discovery(value: &Value) -> Connectivity {
    let planes = &value["planes"];
    let herdr: Vec<bool> = planes["terminal"]["backends"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|backend| backend["kind"] == "herdr")
        .map(|backend| backend["connected"] == true)
        .collect();
    let agents: Vec<&str> = planes["agents"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|agent| agent["status"].as_str())
        .collect();
    let link = |answering: bool, configured: bool| {
        if answering {
            Link::Connected
        } else if configured {
            Link::Offline
        } else {
            Link::NotConfigured
        }
    };
    Connectivity {
        herdr: link(herdr.contains(&true), !herdr.is_empty()),
        agents: link(
            agents
                .iter()
                .any(|status| matches!(*status, "connected" | "reachable")),
            agents
                .iter()
                .any(|status| matches!(*status, "offline" | "unsupported")),
        ),
    }
}

fn fetch_connectivity() -> Connectivity {
    let Ok(config) = load_config(None) else {
        return Connectivity::default();
    };
    // The gateway's own backend and agent probes take up to a few seconds
    // when their caches are cold; this runs off the UI thread, so waiting is
    // fine, and a gateway that is not running leaves both links unknown.
    match crate::cli::local_api::admin_request_with_timeouts(
        &config,
        "GET",
        "/api/capabilities",
        None,
        Duration::from_millis(250),
        Duration::from_secs(8),
    ) {
        Ok((200, value)) => connectivity_from_discovery(&value),
        _ => Connectivity::default(),
    }
}

/// The gap between two link probes. `/api/capabilities` is the only answer
/// that names both links, and it carries every agent's model catalogue, so
/// it is not asked for more often than the header needs.
const CONNECTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(8);
/// How often the poller looks at whether the gateway is running at all.
const RUNNING_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Ask the running gateway about its links every few seconds, off the UI
/// thread, so the header is current without a key press and a slow probe
/// never stalls key handling. A stopped gateway is not asked: its links are
/// unknown, and the first probe after it starts is immediate. The thread
/// ends when the manager drops the receiver.
fn spawn_link_poller() -> std::sync::mpsc::Receiver<Connectivity> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut next_probe = Instant::now();
        loop {
            let running = lifecycle::running_pid().is_ok_and(|pid| pid.is_some());
            let links = if !running {
                next_probe = Instant::now();
                Some(Connectivity::default())
            } else if Instant::now() >= next_probe {
                let links = fetch_connectivity();
                next_probe = Instant::now() + CONNECTIVITY_POLL_INTERVAL;
                Some(links)
            } else {
                None
            };
            if links.is_some_and(|links| sender.send(links).is_err()) {
                break;
            }
            std::thread::sleep(RUNNING_CHECK_INTERVAL);
        }
    });
    receiver
}

/// What the footer depends on besides the view.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FooterContext {
    no_backends: bool,
    no_devices: bool,
    /// `Some` while the pairing flow is shown instead of the device list.
    pairing: Option<PairingStage>,
}

/// The footer: one navigation line, the same shape on every view, and at
/// most one group of the view's own keys -- the actions that are not a row.
/// Everything a row does is its Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Footer {
    navigation: String,
    actions: Option<String>,
}

fn footer(section: Section, context: &FooterContext) -> Footer {
    let enter = match (section, context.pairing) {
        (Section::Devices, Some(_)) => "Enter back",
        (Section::Terminals, _) if context.no_backends => "Enter add tmux",
        (Section::Terminals, _) => "Enter actions",
        (Section::Devices, None) if context.no_devices => "Enter pair",
        (Section::Devices, None) => "Enter details",
        _ => "Enter change",
    };
    let navigation = if section == Section::Overview {
        String::from("Tab next view · q quit")
    } else {
        format!("↑↓ select · {enter} · Tab next view · q quit")
    };
    let actions = match (section, context.pairing) {
        (Section::Overview, _) => Some("s start · t stop · r restart"),
        (Section::Terminals, _) => Some("h add Herdr · m add tmux"),
        (Section::Devices, Some(PairingStage::Done)) => Some("p pair another · Esc back"),
        (Section::Devices, Some(_)) => Some("Esc back"),
        (Section::Devices, None) if context.no_devices => Some("p pair a device"),
        (Section::Devices, None) => Some("p pair a device · x revoke"),
        (Section::Settings, _) => Some("PgUp/PgDn page · r restart"),
    };
    Footer {
        navigation,
        actions: actions.map(str::to_owned),
    }
}

/// The footer's lines in `width` cells: navigation and actions on one line
/// when they fit, the actions right-aligned; else on two.
fn footer_lines(footer: &Footer, width: usize) -> Vec<ScreenLine> {
    let shorter;
    let footer = if display_width(&footer.navigation) > width {
        shorter = Footer {
            navigation: footer.navigation.replace("Tab next view", "Tab view"),
            actions: footer.actions.clone(),
        };
        &shorter
    } else {
        footer
    };
    let Some(actions) = &footer.actions else {
        return vec![ScreenLine::text(&footer.navigation, Tone::Quiet)];
    };
    let used = display_width(&footer.navigation) + display_width(actions);
    if used + 3 <= width {
        return vec![ScreenLine {
            spans: vec![
                (footer.navigation.clone(), Tone::Quiet),
                (" ".repeat(width - used), Tone::Normal),
                (actions.clone(), Tone::Accent),
            ],
            qr: None,
        }];
    }
    vec![
        ScreenLine::text(&footer.navigation, Tone::Quiet),
        ScreenLine::text(actions, Tone::Accent),
    ]
}

/// What Enter does on a view, as the key or action it stands for: `None` on
/// Overview, whose actions start, stop and restart the gateway -- none of
/// them belongs on the key an operator leans on to look around.
fn enter_action(section: Section, empty_items: bool, pairing_open: bool) -> Option<&'static str> {
    match section {
        Section::Overview => None,
        Section::Terminals if empty_items => Some("m"),
        // The pairing view (QR, code, or done) returns to the list on Enter.
        Section::Devices if pairing_open => Some("r"),
        Section::Devices if empty_items => Some("p"),
        _ => Some("enter"),
    }
}

/// One row of the Settings view, in the order shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    Autostart,
    Address,
    DetectAddress,
    TransportEncryption,
    Updates,
    HistoryStorage,
    StoreEncryption,
    AgentBypass,
    AgentTrust,
    AgentEnv,
    Mcp,
    McpInject,
    McpLoopback,
}

impl Setting {
    const ALL: [Self; 13] = [
        Self::Autostart,
        Self::Address,
        Self::DetectAddress,
        Self::TransportEncryption,
        Self::Updates,
        Self::HistoryStorage,
        Self::StoreEncryption,
        Self::AgentBypass,
        Self::AgentTrust,
        Self::AgentEnv,
        Self::Mcp,
        Self::McpInject,
        Self::McpLoopback,
    ];

    fn group(self) -> &'static str {
        match self {
            Self::Autostart
            | Self::Address
            | Self::DetectAddress
            | Self::TransportEncryption
            | Self::Updates => "GATEWAY",
            Self::HistoryStorage | Self::StoreEncryption => "STORAGE",
            Self::AgentBypass | Self::AgentTrust | Self::AgentEnv => {
                "AGENTS STARTED FROM THE PHONE"
            }
            Self::Mcp | Self::McpInject | Self::McpLoopback => "ASSISTANT ACCESS",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Autostart => "Login autostart",
            Self::Address => "Gateway address",
            Self::DetectAddress => "Detect address",
            Self::TransportEncryption => "Transport encryption",
            Self::Updates => "Updates",
            Self::HistoryStorage => "History checkpoints",
            Self::StoreEncryption => "Store encryption",
            Self::AgentBypass => "Skip the agent's own approval prompts",
            Self::AgentTrust => "Trust the task folder automatically",
            Self::AgentEnv => "Environment passed to agents",
            Self::Mcp => "MCP server",
            Self::McpInject => "Give started agents the muqun tools",
            Self::McpLoopback => "Loopback only",
        }
    }

    /// The feature the row switches, in one line.
    fn explanation(self) -> &'static str {
        match self {
            Self::Autostart => "Starts Gateway when you log in.",
            Self::Address => "What the App dials. Enter edits it.",
            Self::DetectAddress => {
                "Finds this computer's LAN or tailnet address and offers it as the address."
            }
            Self::TransportEncryption => "Encrypts new pairings; paired devices keep theirs.",
            Self::Updates => "Enter checks for a stable release. Installing always asks first.",
            Self::HistoryStorage => "Keeps agent panes' scrollback across restarts.",
            Self::StoreEncryption => "How tokens and history are encrypted at rest.",
            Self::AgentBypass => "Starts agents with bypass flags; tasks never stall.",
            Self::AgentTrust => "Pre-accepts the folder trust dialog for the task.",
            Self::AgentEnv => "Variables every agent the gateway starts receives.",
            Self::Mcp => "Lets agents on this computer use the muqun tools (tasks, fleet).",
            Self::McpInject => "Started agents get the tools; agent configs untouched.",
            Self::McpLoopback => "The MCP endpoint answers this computer only.",
        }
    }

    /// The explanation where the full one does not fit.
    fn short_explanation(self) -> &'static str {
        match self {
            Self::Autostart => "Starts Gateway at login.",
            Self::Address => "What the App dials.",
            Self::DetectAddress => "Offers this computer's LAN or tailnet address.",
            Self::TransportEncryption => "Encrypts new pairings only.",
            Self::Updates => "Enter checks; installing asks.",
            Self::HistoryStorage => "Keeps scrollback across restarts.",
            Self::StoreEncryption => "How secrets are encrypted at rest.",
            Self::AgentBypass => "Agents never stall on a prompt.",
            Self::AgentTrust => "Pre-accepts the folder trust dialog.",
            Self::AgentEnv => "Variables agents receive.",
            Self::Mcp => "Agents here use the muqun tools.",
            Self::McpInject => "Started agents get the tools at launch.",
            Self::McpLoopback => "Answers this computer only.",
        }
    }

    /// The label where the full one would hide the value.
    fn short_label(self) -> &'static str {
        match self {
            Self::AgentBypass => "Skip approval prompts",
            Self::AgentTrust => "Trust task folder",
            Self::AgentEnv => "Agent environment",
            Self::McpInject => "Tools for started agents",
            other => other.label(),
        }
    }

    /// Where the setting lives, shown in grey beside its explanation.
    fn source(self) -> Option<&'static str> {
        match self {
            Self::Autostart => Some("user service"),
            Self::Address => Some("config.json › public_url"),
            Self::TransportEncryption => Some("config.json › transport_encryption"),
            Self::HistoryStorage => Some("config.json › history.storage"),
            Self::StoreEncryption => Some("config.json › store.encryption"),
            Self::AgentBypass => Some("config.json › agent_launch_args"),
            Self::AgentTrust => Some("config.json › agent_trust_folders"),
            Self::AgentEnv => Some("config.json › agent_env"),
            Self::Mcp => Some("config.json › mcp.enabled"),
            Self::McpInject => Some("config.json › mcp.inject_into_agents"),
            Self::DetectAddress | Self::Updates | Self::McpLoopback => None,
        }
    }

    /// Whether a change takes effect only once Gateway restarts: every
    /// setting the running gateway read from config.json when it started.
    fn needs_restart(self) -> bool {
        matches!(
            self,
            Self::Address
                | Self::DetectAddress
                | Self::TransportEncryption
                | Self::HistoryStorage
                | Self::AgentBypass
                | Self::AgentTrust
                | Self::Mcp
                | Self::McpInject
        )
    }

    /// The status line Enter gives a row it cannot change.
    fn read_only(self) -> Option<&'static str> {
        match self {
            Self::StoreEncryption => Some(
                "Store encryption is chosen when the store is created; it is not changed here.",
            ),
            Self::AgentEnv => Some("Edit agent_env in config.json; values are never shown here."),
            Self::McpLoopback => Some("The MCP endpoint always answers this computer only."),
            _ => None,
        }
    }
}

/// Whether each built-in kind starts with its bypass flags: "On" with the
/// defaults (or an override that keeps the flag), "Off" with an empty
/// override, "custom" with any other.
fn bypass_states(
    overrides: &std::collections::BTreeMap<String, Vec<String>>,
) -> [(&'static str, &'static str); 2] {
    ["codex", "claude"].map(|kind| {
        let state = match overrides.get(kind) {
            None => "On",
            Some(args) if args.is_empty() => "Off",
            Some(args)
                if args.iter().any(|arg| {
                    arg == crate::agents::tasks::CODEX_BYPASS_FLAG
                        || arg == crate::agents::tasks::CLAUDE_BYPASS_FLAG
                }) =>
            {
                "On"
            }
            Some(_) => "custom",
        };
        (kind, state)
    })
}

fn on_off(on: bool) -> &'static str {
    if on {
        "On"
    } else {
        "Off"
    }
}

/// The value a Settings row shows.
fn setting_value(setting: Setting, config: &crate::Config, autostart: &str) -> String {
    match setting {
        Setting::Autostart => autostart
            .split_once(" - ")
            .map_or(autostart, |(state, _)| state)
            .to_owned(),
        Setting::Address => config.public_url.clone(),
        Setting::DetectAddress => String::from("Enter"),
        Setting::TransportEncryption => {
            on_off(config.transport_encryption == TransportEncryptionMode::Required).to_owned()
        }
        Setting::Updates => env!("CARGO_PKG_VERSION").to_owned(),
        Setting::HistoryStorage => match config.history.storage {
            crate::HistoryStorage::Memory => String::from("memory"),
            crate::HistoryStorage::Sqlite => String::from("SQLite"),
        },
        Setting::StoreEncryption => match config.store.encryption {
            crate::StoreEncryption::File => String::from("key in store.key · read-only"),
            crate::StoreEncryption::Off => String::from("off · read-only"),
        },
        Setting::AgentBypass => bypass_states(&config.agent_launch_args)
            .map(|(kind, state)| format!("{kind} {state}"))
            .join(" · "),
        Setting::AgentTrust => on_off(config.agent_trust_folders).to_owned(),
        Setting::AgentEnv => {
            let keys: Vec<&str> = config.agent_env.0.keys().map(String::as_str).collect();
            if keys.is_empty() {
                String::from("none")
            } else {
                format!("{} · values hidden", keys.join(", "))
            }
        }
        Setting::Mcp => on_off(config.mcp.enabled).to_owned(),
        Setting::McpInject if !config.mcp.enabled => {
            format!("{} · MCP server off", on_off(config.mcp.inject_into_agents))
        }
        Setting::McpInject => on_off(config.mcp.inject_into_agents).to_owned(),
        Setting::McpLoopback => String::from("On · read-only"),
    }
}

/// `text` broken at spaces into lines of at most `width` cells; a word wider
/// than that (an address) is broken across lines rather than lost.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && display_width(&line) + 1 + display_width(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        let mut rest = word;
        while display_width(&line) + display_width(rest) > width {
            let mut head = clip_cells(rest, width.saturating_sub(display_width(&line)));
            if head.is_empty() && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            if head.is_empty() {
                // A character wider than the whole line goes on its own.
                let first = rest.chars().next().map_or(0, char::len_utf8);
                head = &rest[..first];
            }
            line.push_str(head);
            lines.push(std::mem::take(&mut line));
            rest = &rest[head.len()..];
        }
        line.push_str(rest);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `text` in at most `cells` cells, cut with `…` when it does not fit.
fn fit(text: &str, cells: usize) -> String {
    if display_width(text) <= cells {
        return text.to_owned();
    }
    if cells == 0 {
        return String::new();
    }
    format!("{}…", clip_cells(text, cells - 1))
}

/// A row with its label on the left and its value on the right, in `width`
/// cells; the value is cut with `…` before the label is.
fn value_row(label: &str, value: &str, selected: bool, width: usize) -> ScreenLine {
    value_row_or(label, label, value, selected, width)
}

/// [`value_row`] that falls back to `short` for the label when the full one
/// would leave the value cut.
fn value_row_or(label: &str, short: &str, value: &str, selected: bool, width: usize) -> ScreenLine {
    let marker = if selected { "▌ " } else { "  " };
    let label = if display_width(marker) + display_width(label) + 2 + display_width(value) > width {
        short
    } else {
        label
    };
    let left = safe_text(&format!("{marker}{label}"));
    let room = width.saturating_sub(display_width(&left) + 2);
    let value = fit(&safe_text(value), room);
    let pad = width.saturating_sub(display_width(&left) + display_width(&value));
    let (label_tone, value_tone) = if selected {
        (Tone::Selected, Tone::Selected)
    } else {
        (Tone::Normal, Tone::Accent)
    };
    ScreenLine {
        spans: vec![
            (left, label_tone),
            (" ".repeat(pad), label_tone),
            (value, value_tone),
        ],
        qr: None,
    }
}

/// A row's explanation, indented, with where it lives right-aligned in grey
/// when both fit.
fn help_row(explanation: &str, short: &str, source: Option<&str>, width: usize) -> ScreenLine {
    let text = if 3 + display_width(explanation) <= width {
        format!("   {explanation}")
    } else {
        format!("   {short}")
    };
    match source {
        Some(source) if display_width(&text) + 2 + display_width(source) <= width => {
            let pad = width - display_width(&text) - display_width(source);
            ScreenLine {
                spans: vec![
                    (text, Tone::Quiet),
                    (" ".repeat(pad), Tone::Quiet),
                    (source.to_owned(), Tone::Quiet),
                ],
                qr: None,
            }
        }
        _ => ScreenLine::text(text, Tone::Quiet),
    }
}

/// Set (or, with `None`, remove) the key at `path` in the config file
/// `file`, leaving every other key as it was written -- unknown keys, keys
/// that hold defaults, and the top-level order -- then check the result
/// still loads before it replaces the file.
fn set_config_key(
    file: &std::path::Path,
    path: &[&str],
    value: Option<Value>,
) -> anyhow::Result<crate::Config> {
    set_config_keys(file, &[(path, value)])
}

/// [`set_config_key`] for several keys, in one replacement of the file.
fn set_config_keys(
    file: &std::path::Path,
    edits: &[(&[&str], Option<Value>)],
) -> anyhow::Result<crate::Config> {
    let text = std::fs::read_to_string(file)?;
    // Read in written order at every depth (serde_json's own maps sort).
    let mut doc: Ordered = serde_json::from_str(&text)?;
    for (path, value) in edits {
        let (last, parents) = path.split_last().context("an empty config key")?;
        let mut at = &mut doc;
        for key in parents {
            let Ordered::Object(object) = at else {
                anyhow::bail!("config key is not an object");
            };
            at = object
                .entry((*key).to_owned())
                .or_insert_with(|| Ordered::Object(indexmap::IndexMap::new()));
        }
        let Ordered::Object(object) = at else {
            anyhow::bail!("config key is not an object");
        };
        match value {
            Some(value) => {
                object.insert((*last).to_owned(), serde_json::from_value(value.clone())?);
            }
            None => {
                object.shift_remove(*last);
            }
        }
        // A one-level object the removal left empty goes too, as if it had
        // never been written (`"agent_launch_args": {}` reads as noise).
        if let ([parent], None, Ordered::Object(top)) = (parents, value, &mut doc) {
            if matches!(top.get(*parent), Some(Ordered::Object(left)) if left.is_empty()) {
                top.shift_remove(*parent);
            }
        }
    }
    let rendered = serde_json::to_vec_pretty(&doc)?;
    let config: crate::Config =
        serde_json::from_slice(&rendered).context("the change would not load")?;
    write_secret_file(file, &rendered)?;
    Ok(config)
}

/// A JSON document that keeps every object's keys in written order.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
enum Ordered {
    Object(indexmap::IndexMap<String, Ordered>),
    Array(Vec<Ordered>),
    Other(Value),
}

fn write_config_key(path: &[&str], value: Option<Value>) -> anyhow::Result<crate::Config> {
    set_config_key(&config_dir()?.join(CONFIG_FILE), path, value)
}

/// Enter on "Skip the agent's own approval prompts": off by an empty
/// override for both kinds; on by removing those overrides, after a
/// confirmation, since it lets the agents act without asking. Custom launch
/// arguments are left for config.json.
fn toggle_agent_bypass() -> anyhow::Result<String> {
    let config = load_config(None)?;
    let states = bypass_states(&config.agent_launch_args);
    if states.iter().any(|(_, state)| *state == "custom") {
        return Ok(String::from(
            "Custom launch arguments are set; edit agent_launch_args in config.json.",
        ));
    }
    if states.iter().all(|(_, state)| *state == "On") {
        write_launch_args(Some(Vec::new()))?;
        return Ok(String::from(
            "Approval prompts on; press r to restart Gateway and apply. A task may then wait for you.",
        ));
    }
    if !confirm_action(
        "Skip the agents' own approval prompts?",
        &[
            String::from("Codex and Claude Code started from the phone run without asking."),
            String::from("Only for agents the gateway starts; your own terminals are unchanged."),
        ],
        "skip prompts",
    )? {
        return Ok(String::from("Approval prompts unchanged."));
    }
    write_launch_args(None)?;
    Ok(String::from(
        "Approval prompts skipped; press r to restart Gateway and apply.",
    ))
}

/// `agent_launch_args` for codex and claude set to `args` (`None` removes
/// them), in one replacement that keeps every other key, and the order of
/// the kinds, as written.
fn write_launch_args(args: Option<Vec<String>>) -> anyhow::Result<()> {
    let value = args.map(Value::from);
    let codex: &[&str] = &["agent_launch_args", "codex"];
    let claude: &[&str] = &["agent_launch_args", "claude"];
    set_config_keys(
        &config_dir()?.join(CONFIG_FILE),
        &[(codex, value.clone()), (claude, value)],
    )?;
    Ok(())
}

fn toggle_agent_trust() -> anyhow::Result<String> {
    let trust = !load_config(None)?.agent_trust_folders;
    write_config_key(&["agent_trust_folders"], Some(Value::Bool(trust)))?;
    Ok(format!(
        "Trust the task folder automatically: {}; press r to restart Gateway and apply.",
        on_off(trust)
    ))
}

fn toggle_mcp_inject() -> anyhow::Result<String> {
    let inject = !load_config(None)?.mcp.inject_into_agents;
    write_config_key(&["mcp", "inject_into_agents"], Some(Value::Bool(inject)))?;
    Ok(format!(
        "Give started agents the muqun tools: {}; press r to restart Gateway and apply.",
        on_off(inject)
    ))
}

fn toggle_mcp() -> anyhow::Result<String> {
    let enabled = !load_config(None)?.mcp.enabled;
    write_config_key(&["mcp", "enabled"], Some(Value::Bool(enabled)))?;
    Ok(format!(
        "MCP server: {}; press r to restart Gateway and apply.",
        on_off(enabled)
    ))
}

/// What the running Gateway's SQLite history store holds, for the caption
/// beside its setting: "12 checkpoints · WAL 1.2 MiB". `None` when the
/// Gateway does not answer at once.
fn history_storage_caption(config: &crate::Config) -> Option<String> {
    let (status, value) = crate::cli::local_api::admin_request_with_timeouts(
        config,
        "GET",
        "/api/history/storage",
        None,
        Duration::from_millis(250),
        Duration::from_millis(500),
    )
    .ok()?;
    if status != 200 || value["storage"] != "sqlite" {
        return None;
    }
    if value["capture"] == false {
        return Some("capture off (config.json › history.capture)".into());
    }
    let count = value["checkpoints"].as_u64()?;
    let wal = value["wal_bytes"].as_u64()?;
    let wal = if wal < 1024 * 1024 {
        format!("{} KiB", wal.div_ceil(1024))
    } else {
        format!("{:.1} MiB", wal as f64 / (1024.0 * 1024.0))
    };
    let failing = if value["healthy"] == true {
        ""
    } else {
        " · failing, retrying"
    };
    Some(format!(
        "{count} checkpoint{} · WAL {wal}{failing}",
        if count == 1 { "" } else { "s" }
    ))
}

/// Enter on "History checkpoints": a chooser; the pick is written and needs
/// a restart. `None` when nothing changed.
fn choose_history_storage() -> anyhow::Result<Option<String>> {
    let config = load_config(None)?;
    let current = config.history.storage;
    // True to the store this Gateway keeps: encrypted unless it was turned off.
    let sqlite = match config.store.encryption {
        crate::StoreEncryption::File => "survives restarts · encrypted store",
        crate::StoreEncryption::Off => "survives restarts · plaintext store",
    };
    let options = [
        (
            crate::HistoryStorage::Memory,
            "memory",
            "only while Gateway runs",
        ),
        (crate::HistoryStorage::Sqlite, "SQLite", sqlite),
    ];
    let start = options
        .iter()
        .position(|(storage, _, _)| *storage == current)
        .unwrap_or(0);
    let Some(pick) = choose(
        "History checkpoints",
        &[String::from(
            "Where captured scrollback of agent panes is kept.",
        )],
        &options.map(|(_, label, detail)| (label.to_owned(), detail.to_owned())),
        start,
        Some("Changing this needs a restart. Enter picks, then r restarts."),
    )?
    else {
        return Ok(None);
    };
    let (storage, label, _) = options[pick];
    if storage == current {
        return Ok(None);
    }
    write_config_key(
        &["history", "storage"],
        Some(serde_json::to_value(storage)?),
    )?;
    Ok(Some(format!(
        "History checkpoints: {label}; press r to restart Gateway and apply."
    )))
}

/// A small list to pick one entry from, Enter picks and Esc cancels.
fn choose(
    title: &str,
    about: &[String],
    options: &[(String, String)],
    start: usize,
    note: Option<&str>,
) -> anyhow::Result<Option<usize>> {
    let mut selected = start.min(options.len().saturating_sub(1));
    loop {
        let (width, height) = terminal_size().unwrap_or((100, 30));
        let budget = (width.saturating_sub(1) as usize).min(92);
        let framed = budget >= 48 && height >= 14;
        let inner = if framed {
            budget.saturating_sub(4)
        } else {
            budget
        };
        let label_width = options
            .iter()
            .map(|(label, _)| display_width(label))
            .max()
            .unwrap_or(0)
            + 4;
        let mut rows = Vec::new();
        for line in about {
            rows.extend(word_wrap(line, inner).into_iter().map(ScreenLine::plain));
        }
        rows.push(ScreenLine::plain(""));
        for (index, (label, detail)) in options.iter().enumerate() {
            let marker = if index == selected { "▌ " } else { "  " };
            let pad = label_width.saturating_sub(display_width(label));
            rows.push(ScreenLine::text(
                fit(
                    &format!("{marker}{label}{}{detail}", " ".repeat(pad)),
                    inner,
                ),
                if index == selected {
                    Tone::Selected
                } else {
                    Tone::Normal
                },
            ));
        }
        if let Some(note) = note {
            rows.push(ScreenLine::plain(""));
            rows.extend(word_wrap(note, inner).into_iter().map(ScreenLine::plain));
        }
        let mut lines = Vec::new();
        panel(&mut lines, title, rows, budget, framed);
        lines.truncate((height as usize).saturating_sub(2));
        lines.push(ScreenLine::text(
            fit("↑↓ choose · Enter pick · Esc cancel", budget),
            Tone::Quiet,
        ));
        lines.push(ScreenLine::plain(fit(
            &format!("Status: choosing {title}"),
            budget,
        )));
        write_viewport(&lines, width, height)?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(options.len().saturating_sub(1))
                }
                KeyCode::Enter => return Ok(Some(selected)),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                _ => {}
            }
        }
    }
}

/// Enter on a terminal backend: its three actions in a chooser.
fn backend_actions(index: usize) -> anyhow::Result<String> {
    let config = load_config(None)?;
    let Some(session) = config.sessions.get(index).cloned() else {
        return Ok(String::from(IDLE_STATUS));
    };
    let starts = config.autostart_backends.contains(&session.id);
    let options = [
        (
            String::from("Make default"),
            String::from(if index == 0 {
                "already the default"
            } else {
                "the App opens it first"
            }),
        ),
        (
            String::from("Start with Gateway"),
            format!("{} → {}", on_off(starts), on_off(!starts)),
        ),
        (
            String::from("Remove backend"),
            String::from(if config.sessions.len() > 1 {
                "asks first"
            } else {
                "the only backend stays"
            }),
        ),
    ];
    let pick = choose(
        &format!("{} · {}", session.id, session.label),
        &[String::from(
            "Removing a backend never closes its terminals. Changes apply after a Gateway restart.",
        )],
        &options,
        0,
        None,
    )?;
    Ok(match pick {
        None => String::from("backend unchanged"),
        Some(0) if index == 0 => format!("{} is already the default", session.id),
        Some(0) => set_managed_default_backend(&session.id)?,
        Some(1) => toggle_backend_autostart(&session.id)?,
        Some(_) if confirm_remove_backend(&session)? => remove_managed_backend(&session.id)?,
        Some(_) => String::from("backend unchanged"),
    })
}

fn device_kind(device: &DeviceRecord) -> &'static str {
    match device.kind {
        crate::authority::DeviceKind::App => "phone",
        crate::authority::DeviceKind::Gateway => "gateway",
    }
}

fn last_seen(device: &DeviceRecord) -> String {
    if device.last_seen_unix_ms == 0 {
        String::from("—")
    } else {
        relative_since(device.last_seen_unix_ms)
    }
}

/// Enter on a paired device: what is known about it. Whether `x` was pressed
/// there to revoke it.
fn show_device(device: &DeviceRecord) -> anyhow::Result<bool> {
    let details = [
        format!("Kind: {}", device_kind(device)),
        format!("Paired: {}", relative_since(device.paired_unix_ms)),
        format!("Last seen: {}", last_seen(device)),
        String::from(if device.kind == crate::authority::DeviceKind::Gateway {
            "Another of your computers; revoking it stops that computer sending work here."
        } else {
            "Revoking it removes its access at once; it can pair again."
        }),
    ];
    loop {
        write_dialog(
            &device.name,
            &details,
            ScreenLine::text("x revoke · Esc back", Tone::Quiet),
        )?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Char('x') => return Ok(true),
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
        }
    }
}

#[derive(Default)]
struct ManageView {
    section: Section,
    selected: usize,
    scroll: usize,
    /// Settings changed here that apply only after a restart; their rows say
    /// so until Gateway restarts.
    restart: Vec<Setting>,
}

impl ManageView {
    fn navigate(&mut self, key: KeyCode, item_count: usize) -> bool {
        let index = Section::ALL
            .iter()
            .position(|section| *section == self.section)
            .unwrap();
        let section = match key {
            KeyCode::Tab | KeyCode::Right => Some((index + 1) % 4),
            KeyCode::BackTab | KeyCode::Left => Some((index + 3) % 4),
            KeyCode::Char(ch @ '1'..='4') => Some(ch as usize - '1' as usize),
            _ => None,
        };
        if let Some(index) = section {
            self.section = Section::ALL[index];
            self.selected = 0;
            self.scroll = 0;
            return true;
        }
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if item_count > 0 {
                    self.selected = (self.selected + 1).min(item_count - 1);
                } else {
                    self.scroll = self.scroll.saturating_add(1);
                }
            }
            KeyCode::PageUp if item_count > 0 => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown if item_count > 0 => {
                self.selected = self.selected.saturating_add(10).min(item_count - 1)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Home => {
                self.selected = 0;
                self.scroll = 0;
            }
            KeyCode::End if item_count > 0 => self.selected = item_count - 1,
            _ => return false,
        }
        true
    }
}

/// Keep the selected item visible after navigation, deletion, and resizing.
fn viewport_start(scroll: usize, selected: Option<usize>, rows: usize, height: usize) -> usize {
    let height = height.max(1);
    let mut start = scroll.min(rows.saturating_sub(height));
    if let Some(selected) = selected {
        let selected = selected.min(rows.saturating_sub(1));
        if selected < start {
            start = selected;
        }
        if selected >= start + height {
            start = selected + 1 - height;
        }
    }
    start
}

fn print_manage_screen(
    message: &str,
    pending_pairing: Option<&PendingPairing>,
    devices: &[DeviceRecord],
    pairing: &PairingView,
    connectivity: Connectivity,
    ui: &mut ManageView,
) -> anyhow::Result<()> {
    let config = load_config(None)?;
    let (width, height) = terminal_size().unwrap_or((100, 30));
    let owner = lifecycle::running_pid();
    let running = matches!(owner, Ok(Some(_)));
    let runtime = match owner {
        Ok(Some(pid)) => format!("Running - process {pid}"),
        Ok(None) => String::from("Stopped - press s to start"),
        Err(error) => format!("Unavailable: {}", first_line(&error.to_string())),
    };
    let autostart = autostart_label(&service::state());
    let service_path = service::installed_path_env();
    let backend_states = config
        .sessions
        .iter()
        .map(|session| match session.backend {
            BackendKind::Tmux => tmux_availability(
                &std::env::var("PATH").unwrap_or_default(),
                service_path.as_deref(),
            ),
            BackendKind::Herdr => String::from("Herdr socket configured"),
        })
        .collect::<Vec<_>>();
    let qr_result =
        if ui.section == Section::Devices
            && pairing.open
            && pairing.stage(pending_pairing.is_some()) == PairingStage::Scan
        {
            Some((|| -> anyhow::Result<String> {
                let pairing = read_pairing_file()?;
                anyhow::ensure!(hash_token(&pairing.payload.token) == config.token_hash,
            "Pairing identity is inconsistent; check configuration with `muqun-gateway setup`.");
                let encoded = pairing_qr_offer(
                    &config.public_url,
                    &config.server_id,
                    (config.transport_encryption == TransportEncryptionMode::Required)
                        .then_some(pairing.payload.transport_key.as_str()),
                );
                Ok(render_qr(&QrCode::with_error_correction_level(
                    encoded.as_bytes(),
                    EcLevel::L,
                )?))
            })())
        } else {
            None
        };
    let qr_error = qr_result
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(|error| format!("Action failed: {}", first_line(&error.to_string())));
    let qr = qr_result.as_ref().and_then(|result| result.as_ref().ok());
    let history_caption = (ui.section == Section::Settings
        && running
        && config.history.storage == crate::HistoryStorage::Sqlite)
        .then(|| history_storage_caption(&config))
        .flatten();
    let data = ManageData {
        history_caption: history_caption.as_deref(),
        config: &config,
        message: qr_error.as_deref().unwrap_or(message),
        pending: pending_pairing,
        devices,
        runtime: &runtime,
        running,
        autostart,
        backend_states: &backend_states,
        qr: qr.map(String::as_str),
        changed: running && config_changed_since_start(),
        connectivity,
        pairing,
    };
    let lines = manage_lines(&data, ui, width, height);
    write_viewport(&lines, width, height)
}

/// Whether tmux can be found by the gateway that will run it, and on which
/// PATH that was judged. With a service installed that is the unit's pinned
/// `PATH`, not this shell's: a Homebrew tmux on the CLI's `PATH` but not the
/// unit's is what leaves the service's tmux backend unavailable while
/// `tmux -V` works here. `status` and manage's Overview both print this.
fn tmux_availability(cli_path: &str, service_path: Option<&str>) -> String {
    let lookup =
        |path: &str| crate::terminal::login_env::lookup(crate::backend::TMUX_PROGRAM, path);
    let cli = lookup(cli_path);
    let Some(service_path) = service_path else {
        return match cli {
            Some(found) => format!("tmux: found on CLI PATH ({})", found.display()),
            None => format!("tmux: NOT FOUND on CLI PATH ({cli_path})"),
        };
    };
    match (lookup(service_path), cli) {
        (Some(found), _) => format!("tmux: found on service PATH ({})", found.display()),
        (None, Some(found)) => format!(
            "tmux: NOT on service PATH ({service_path}); found on CLI PATH ({}) -- reinstall the service from this shell",
            found.display()
        ),
        (None, None) => format!(
            "tmux: NOT FOUND on service PATH ({service_path}) or CLI PATH ({cli_path})"
        ),
    }
}

fn safe_text(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Quiet,
    Accent,
    Good,
    Warning,
    Error,
    Selected,
    Title,
    /// Looks like `Selected` (bold, reversed) but is not a list selection, so
    /// the viewport does not pin it: the current pairing step and the code.
    Badge,
}

impl Tone {
    fn style(self) -> ContentStyle {
        let mut style = ContentStyle::new();
        style.foreground_color = match self {
            Self::Quiet => Some(Color::DarkGrey),
            Self::Accent => Some(Color::DarkCyan),
            Self::Good => Some(Color::DarkGreen),
            Self::Warning => Some(Color::DarkYellow),
            Self::Error => Some(Color::DarkRed),
            _ => None,
        };
        if matches!(self, Self::Title | Self::Selected | Self::Badge) {
            style.attributes.set(Attribute::Bold);
        }
        if matches!(self, Self::Selected | Self::Badge) {
            style.attributes.set(Attribute::Reverse);
        }
        style
    }
}

#[derive(Clone)]
struct ScreenLine {
    spans: Vec<(String, Tone)>,
    qr: Option<String>,
}

impl ScreenLine {
    fn text(text: impl AsRef<str>, tone: Tone) -> Self {
        Self {
            spans: vec![(safe_text(text.as_ref()), tone)],
            qr: None,
        }
    }
    fn plain(text: impl AsRef<str>) -> Self {
        Self::text(text, Tone::Normal)
    }
    fn width(&self) -> usize {
        self.qr
            .as_deref()
            .map(display_width)
            .unwrap_or_else(|| self.spans.iter().map(|(text, _)| display_width(text)).sum())
    }
    fn selected(&self) -> bool {
        self.spans.iter().any(|(_, tone)| *tone == Tone::Selected)
    }
    fn render(&self, budget: usize) -> String {
        if let Some(qr) = &self.qr {
            // QR rows are internally generated, never clipped or sanitized.
            return if self.width() <= budget {
                qr.clone()
            } else {
                String::new()
            };
        }
        let mut output = String::new();
        let mut remaining = budget;
        for (text, tone) in &self.spans {
            let clipped = clip_cells(text, remaining);
            output.push_str(&tone.style().apply(clipped).to_string());
            remaining = remaining.saturating_sub(display_width(clipped));
            if clipped.len() != text.len() {
                break;
            }
        }
        output
    }
}

struct ManageData<'a> {
    /// What the SQLite history store holds now, shown beside its setting.
    history_caption: Option<&'a str>,
    config: &'a crate::Config,
    message: &'a str,
    pending: Option<&'a PendingPairing>,
    devices: &'a [DeviceRecord],
    runtime: &'a str,
    running: bool,
    autostart: &'a str,
    backend_states: &'a [String],
    qr: Option<&'a str>,
    changed: bool,
    connectivity: Connectivity,
    pairing: &'a PairingView,
}

fn panel(
    body: &mut Vec<ScreenLine>,
    title: &str,
    rows: Vec<ScreenLine>,
    width: usize,
    framed: bool,
) {
    if framed {
        let heading = format!("─ {} ", safe_text(title));
        let heading = clip_cells(&heading, width.saturating_sub(2));
        body.push(ScreenLine::text(
            format!(
                "┌{}{}┐",
                heading,
                "─".repeat(width.saturating_sub(2 + display_width(heading)))
            ),
            Tone::Accent,
        ));
        for mut row in rows {
            // Cut only the content, with `…`, before adding the borders.
            fit_line(&mut row, width.saturating_sub(4));
            row.spans.insert(0, ("│ ".into(), Tone::Accent));
            let padding = width.saturating_sub(2 + row.width());
            row.spans
                .push((format!("{} │", " ".repeat(padding)), Tone::Accent));
            body.push(row);
        }
        body.push(ScreenLine::text(
            format!("└{}┘", "─".repeat(width.saturating_sub(2))),
            Tone::Accent,
        ));
        body.push(ScreenLine::plain(""));
    } else {
        body.push(ScreenLine::text(title, Tone::Title));
        body.extend(rows);
        body.push(ScreenLine::plain(""));
    }
}

fn wrapped_rows(label: &str, value: &str, width: usize) -> Vec<ScreenLine> {
    let mut lines = Vec::new();
    push_wrapped_field(&mut lines, label, value, width);
    lines.into_iter().map(ScreenLine::plain).collect()
}

fn manage_lines(
    data: &ManageData<'_>,
    ui: &mut ManageView,
    width: u16,
    height: u16,
) -> Vec<ScreenLine> {
    let width = width.saturating_sub(1) as usize;
    let height = height as usize;
    let margin = if width >= 50 { 2 } else { 0 };
    let content = width.saturating_sub(margin * 2).min(96);
    let framed = content >= 48 && height >= 18;
    let inner = if framed {
        content.saturating_sub(4)
    } else {
        content
    };
    let config = data.config;
    ui.selected = ui.selected.min(match ui.section {
        Section::Terminals => config.sessions.len().saturating_sub(1),
        Section::Devices => data.devices.len().saturating_sub(1),
        Section::Settings => Setting::ALL.len() - 1,
        Section::Overview => 0,
    });
    let status = header_line(
        &HeaderStatus {
            version: env!("CARGO_PKG_VERSION"),
            listen: &config.listen,
            running: data.running,
            links: data.connectivity,
            devices: data.devices.len(),
        },
        content,
    );
    // The product name is the bold part; the rest reads as status.
    let (title, rest) = status
        .split_once(" · ")
        .map(|(title, rest)| (title.to_owned(), format!(" · {rest}")))
        .unwrap_or((status.clone(), String::new()));
    let mut header = vec![ScreenLine {
        spans: vec![
            (title, Tone::Title),
            (
                rest,
                if data.running {
                    Tone::Normal
                } else {
                    Tone::Warning
                },
            ),
        ],
        qr: None,
    }];
    if data.changed {
        let badge = "Restart needed";
        if header[0].width() + 3 + display_width(badge) <= content {
            let pad = content - header[0].width() - display_width(badge);
            header[0].spans.push((" ".repeat(pad), Tone::Normal));
            header[0].spans.push((badge.into(), Tone::Warning));
        } else {
            header.push(ScreenLine::text(badge, Tone::Warning));
        }
    }
    let mut tabs = ScreenLine {
        spans: Vec::new(),
        qr: None,
    };
    let all_tabs: usize = Section::ALL
        .iter()
        .enumerate()
        .map(|(index, section)| display_width(&format!(" {} {:?}  ", index + 1, section)))
        .sum();
    for (index, section) in Section::ALL.iter().enumerate() {
        if all_tabs > content && *section != ui.section {
            continue;
        }
        tabs.spans.push((
            format!(" {} {:?} ", index + 1, section),
            if *section == ui.section {
                Tone::Selected
            } else {
                Tone::Quiet
            },
        ));
        tabs.spans.push((" ".into(), Tone::Normal));
    }
    header.push(tabs);
    if framed {
        header.push(ScreenLine::plain(""));
    }
    let pairing_stage = data.pairing.stage(data.pending.is_some());
    let show_pairing = data.pairing.open;
    let shape = footer(
        ui.section,
        &FooterContext {
            no_backends: config.sessions.is_empty(),
            no_devices: data.devices.is_empty(),
            pairing: show_pairing.then_some(pairing_stage),
        },
    );
    let mut footer = footer_lines(&shape, content);
    // A short window keeps the navigation line and the status line only.
    if height < 14 {
        footer.truncate(1);
    }
    footer.push(ScreenLine::text(
        format!(
            "{}{}",
            if data.message.starts_with("Action failed:") {
                ""
            } else {
                "Status: "
            },
            data.message
        ),
        if data.message.starts_with("Action failed:") {
            Tone::Error
        } else {
            Tone::Normal
        },
    ));
    let body_height = height.saturating_sub(header.len() + footer.len());
    let mut body = Vec::new();
    match ui.section {
        Section::Overview => {
            let mut rows = vec![
                ScreenLine::text(
                    format!("Gateway: {}", data.runtime),
                    if data.running {
                        Tone::Good
                    } else {
                        Tone::Warning
                    },
                ),
                ScreenLine::plain(format!("Login autostart: {}", data.autostart)),
            ];
            if framed {
                rows.insert(0, ScreenLine::text(&config.label, Tone::Title));
            }
            panel(&mut body, "Runtime", rows, content, framed);
            let mut rows = wrapped_rows("Address", &config.public_url, inner);
            rows.push(ScreenLine::plain(format!("Listener: {}", config.listen)));
            if config.mcp.enabled {
                // Where this computer's agents reach /mcp: the loopback
                // listener a non-loopback gateway runs beside its own.
                let at = crate::mcp::listening_line(&config.listen);
                rows.push(ScreenLine::plain(format!("MCP: {at}")));
            }
            let gateways = data
                .devices
                .iter()
                .filter(|device| device.kind == crate::authority::DeviceKind::Gateway)
                .count();
            rows.push(ScreenLine::plain(format!(
                "Devices: {} paired{}",
                data.devices.len(),
                if gateways > 0 {
                    format!(
                        " · {gateways} gateway{}",
                        if gateways == 1 { "" } else { "s" }
                    )
                } else {
                    String::new()
                }
            )));
            if data.pending.is_some() {
                rows.push(ScreenLine::text(
                    "Pairing request waiting: open 3 Devices for the code.",
                    Tone::Warning,
                ));
            }
            panel(&mut body, "Connectivity", rows, content, framed);
            let mut rows = config
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    ScreenLine::plain(format!(
                        "{} · {}{} · {}",
                        session.id,
                        session.label,
                        if index == 0 { " · default" } else { "" },
                        data.backend_states
                            .get(index)
                            .map(String::as_str)
                            .unwrap_or("unavailable")
                    ))
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(ScreenLine::plain(
                    "No backends. Open 2 Terminals to add Herdr or tmux.",
                ));
            }
            panel(&mut body, "Terminal servers", rows, content, framed);
            body.push(ScreenLine::text(
                "Server connections are checked when the App opens a backend.",
                Tone::Quiet,
            ));
            body.push(ScreenLine::text(
                "Stopping Gateway leaves terminal sessions and tasks running.",
                Tone::Quiet,
            ));
        }
        Section::Terminals => {
            ui.selected = ui.selected.min(config.sessions.len().saturating_sub(1));
            body.push(ScreenLine::text("TERMINAL BACKENDS", Tone::Title));
            for (index, session) in config.sessions.iter().enumerate() {
                let starts = config.autostart_backends.contains(&session.id);
                let value = match (index == 0, starts) {
                    (true, true) => "default · starts with Gateway",
                    (true, false) => "default · on demand",
                    (false, true) => "starts with Gateway",
                    (false, false) => "on demand",
                };
                let name = format!("{:<12} {}", session.id, session.label);
                body.push(value_row(&name, value, index == ui.selected, content));
                body.push(ScreenLine::text(
                    format!(
                        "   {}",
                        data.backend_states
                            .get(index)
                            .filter(|state| !state.is_empty())
                            .cloned()
                            .unwrap_or_else(|| backend_endpoint(session))
                    ),
                    Tone::Quiet,
                ));
            }
            if config.sessions.is_empty() {
                body.push(ScreenLine::plain(
                    "No terminal backends. Enter adds tmux; h adds Herdr.",
                ));
            }
            body.push(ScreenLine::plain(""));
            body.extend(
                word_wrap(
                    "Enter opens the backend's actions. Removing a backend never closes its terminals.",
                    content,
                )
                .into_iter()
                .map(|line| ScreenLine::text(line, Tone::Quiet)),
            );
            body.push(ScreenLine::text(
                "Changes apply after a Gateway restart.",
                Tone::Quiet,
            ));
        }
        Section::Devices => {
            if show_pairing {
                // The QR with its step line and state line, if it fits at all.
                let qr = data.qr.filter(|qr| {
                    qr.lines().count() + 2 <= body_height
                        && qr.lines().all(|row| display_width(row) <= content)
                });
                // The step line names the task on its own; the title and a
                // spacer are only added where there is room for them without
                // costing the QR, so a short terminal keeps the QR or the
                // next instruction on screen.
                let roomy =
                    body_height >= 10 && qr.is_none_or(|qr| qr.lines().count() + 4 <= body_height);
                if roomy {
                    body.push(ScreenLine::text("Pair a device", Tone::Title));
                }
                body.push(pairing_steps(pairing_stage, content));
                if roomy {
                    body.push(ScreenLine::plain(""));
                }
                match (pairing_stage, data.pending, data.pairing.paired.as_deref()) {
                    (PairingStage::Code, Some(pending), _) => {
                        // The code is the one thing to read from across the
                        // room: spaced out on a bold reversed badge.
                        let spaced = pending
                            .code
                            .chars()
                            .map(|ch| ch.to_string())
                            .collect::<Vec<_>>()
                            .join(" ");
                        let badge = " ".repeat(display_width(&spaced) + 8);
                        body.push(ScreenLine::text(&badge, Tone::Badge));
                        body.push(ScreenLine::text(format!("    {spaced}    "), Tone::Badge));
                        body.push(ScreenLine::text(&badge, Tone::Badge));
                        body.push(ScreenLine::plain(""));
                        body.push(ScreenLine::text(
                            match pending.kind {
                                crate::connectivity::authority::DeviceKind::App => format!(
                                    "Waiting for the phone… {} is asking for this code.",
                                    pending.device_name
                                ),
                                crate::connectivity::authority::DeviceKind::Gateway => format!(
                                    "Waiting for the computer… {} (gateway) is asking for this code to link with this one.",
                                    pending.device_name
                                ),
                            },
                            Tone::Accent,
                        ));
                        body.extend(wrapped_rows("Address", &config.public_url, inner));
                    }
                    (PairingStage::Done, _, paired) => {
                        body.push(ScreenLine::text(
                            format!("Paired as {}.", paired.unwrap_or("new device")),
                            Tone::Good,
                        ));
                        body.push(ScreenLine::plain(
                            "The phone can open this Gateway now. Press p to pair another device.",
                        ));
                    }
                    _ if data.qr.is_none() => {
                        body.push(ScreenLine::text(
                            "Pairing unavailable. See the status message below.",
                            Tone::Error,
                        ));
                    }
                    _ => {
                        if let Some(qr) = qr {
                            body.extend(qr.lines().map(|row| ScreenLine {
                                spans: Vec::new(),
                                qr: Some(row.to_owned()),
                            }));
                        } else {
                            body.push(ScreenLine::text(
                                "Resize for the full QR, or type this address into Muqun:",
                                Tone::Warning,
                            ));
                            body.extend(wrapped_rows("Address", &config.public_url, inner));
                        }
                        body.push(ScreenLine::text(
                            "Waiting for the phone… the code appears here after the scan.",
                            Tone::Accent,
                        ));
                    }
                }
            } else {
                ui.selected = ui.selected.min(data.devices.len().saturating_sub(1));
                // Columns: name, then kind, paired and last seen while they fit.
                let columns: &[(&str, usize)] = if content >= 64 {
                    &[("kind", 10), ("paired", 14), ("last seen", 12)]
                } else if content >= 50 {
                    &[("kind", 8), ("paired", 9), ("seen", 9)]
                } else if content >= 40 {
                    &[("kind", 10)]
                } else {
                    &[]
                };
                let name_width =
                    content.saturating_sub(columns.iter().map(|(_, w)| w).sum::<usize>());
                let row = |name: String, cells: &[String], tone: Tone| {
                    let mut text = format!(
                        "{}{}",
                        fit(&name, name_width),
                        " ".repeat(
                            name_width.saturating_sub(display_width(&fit(&name, name_width)))
                        )
                    );
                    for ((_, width), cell) in columns.iter().zip(cells) {
                        let cell = fit(cell, width - 1);
                        text.push_str(&cell);
                        text.push_str(&" ".repeat(width.saturating_sub(display_width(&cell))));
                    }
                    ScreenLine::text(text.trim_end(), tone)
                };
                body.push(row(
                    String::from("PAIRED DEVICES"),
                    &columns
                        .iter()
                        .map(|(title, _)| (*title).to_owned())
                        .collect::<Vec<_>>(),
                    Tone::Title,
                ));
                for (index, device) in data.devices.iter().rev().enumerate() {
                    let selected = index == ui.selected;
                    body.push(row(
                        format!("{}{}", if selected { "▌ " } else { "  " }, device.name),
                        &[
                            device_kind(device).to_owned(),
                            relative_since(device.paired_unix_ms),
                            last_seen(device),
                        ],
                        if selected {
                            Tone::Selected
                        } else {
                            Tone::Normal
                        },
                    ));
                }
                if data.devices.is_empty() {
                    body.push(ScreenLine::plain("No devices paired yet."));
                    body.push(ScreenLine::text(
                        "Press Enter to pair your first device with Muqun.",
                        Tone::Accent,
                    ));
                }
                body.push(ScreenLine::plain(""));
                if let Some(pending) = data.pending {
                    body.push(ScreenLine::text(
                        format!(
                            "{}{} is waiting for its pairing code: press p to show it.",
                            pending.device_name,
                            if pending.kind == crate::connectivity::authority::DeviceKind::Gateway {
                                " (gateway)"
                            } else {
                                ""
                            }
                        ),
                        Tone::Warning,
                    ));
                }
                if !data.devices.is_empty() {
                    for text in [
                        "Enter shows the device; x revokes it after a confirmation. A revoked device loses access at once.",
                        "A gateway device is another of your computers; revoking it stops that computer sending work here.",
                    ] {
                        body.extend(
                            word_wrap(text, content)
                                .into_iter()
                                .map(|line| ScreenLine::text(line, Tone::Quiet)),
                        );
                    }
                }
            }
        }
        Section::Settings => {
            ui.selected = ui.selected.min(Setting::ALL.len() - 1);
            let mut group = "";
            for (index, setting) in Setting::ALL.iter().enumerate() {
                if setting.group() != group {
                    if !group.is_empty() {
                        body.push(ScreenLine::plain(""));
                    }
                    group = setting.group();
                    body.push(ScreenLine::text(group, Tone::Title));
                }
                let mut value = setting_value(*setting, config, data.autostart);
                if let (Setting::HistoryStorage, Some(caption)) = (setting, data.history_caption) {
                    value.push_str(" · ");
                    value.push_str(caption);
                }
                if data.changed && ui.restart.contains(setting) {
                    value.push_str(" · restart needed");
                }
                body.push(value_row_or(
                    setting.label(),
                    setting.short_label(),
                    &value,
                    index == ui.selected,
                    content,
                ));
                body.push(help_row(
                    setting.explanation(),
                    setting.short_explanation(),
                    setting.source(),
                    content,
                ));
            }
            // Linking this computer with your others gets its own row here.
            body.push(ScreenLine::plain(""));
            body.push(ScreenLine::text("OTHER COMPUTERS", Tone::Title));
            body.extend(
                word_wrap(
                    "Linking between your computers appears here once it is available.",
                    content.saturating_sub(2),
                )
                .into_iter()
                .map(|line| ScreenLine::text(format!("  {line}"), Tone::Quiet)),
            );
        }
    }
    let selected_line = body.iter().position(ScreenLine::selected);
    // A setting is its value row and its help row; the last one shows
    // everything below it too, to the end of the view.
    let selected_end = match (ui.section, selected_line) {
        (Section::Settings, Some(_)) if ui.selected + 1 == Setting::ALL.len() => {
            Some(body.len().saturating_sub(1))
        }
        (Section::Settings, Some(line)) => Some(line + 1),
        _ => selected_line,
    };
    // Pin only complete QR rows, never the scrollable address fallback.
    if body.iter().any(|line| line.qr.is_some()) {
        ui.scroll = 0;
    }
    ui.scroll = viewport_start(ui.scroll, selected_end, body.len(), body_height);
    if let Some(line) = selected_line.filter(|line| *line < ui.scroll) {
        ui.scroll = line;
    }
    let mut lines = header;
    lines.extend(body.into_iter().skip(ui.scroll).take(body_height));
    while lines.len() + footer.len() < height {
        lines.push(ScreenLine::plain(""));
    }
    lines.extend(footer);
    lines.truncate(height);
    for line in &mut lines {
        if line.qr.is_none() {
            fit_line(line, content);
        }
    }
    if margin > 0 {
        for line in &mut lines {
            if let Some(qr) = &mut line.qr {
                qr.insert_str(0, &" ".repeat(margin));
            } else {
                line.spans.insert(0, (" ".repeat(margin), Tone::Normal));
            }
        }
    }
    lines
}

/// `line` in at most `width` cells; one that is longer ends in `…`.
fn fit_line(line: &mut ScreenLine, width: usize) {
    if line.width() <= width {
        return;
    }
    let mut budget = width.saturating_sub(1);
    for (text, _) in &mut line.spans {
        *text = clip_cells(text, budget).to_owned();
        budget = budget.saturating_sub(display_width(text));
    }
    if width > 0 {
        if let Some((text, _)) = line
            .spans
            .iter_mut()
            .rev()
            .find(|(text, _)| !text.is_empty())
        {
            text.push('…');
        }
    }
}

fn write_viewport(lines: &[ScreenLine], width: u16, height: u16) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All))?;
    for (row, line) in lines.iter().take(height as usize).enumerate() {
        execute!(stdout(), MoveTo(0, row as u16))?;
        stdout().write_all(line.render(width.saturating_sub(1) as usize).as_bytes())?;
    }
    stdout().flush()?;
    Ok(())
}

fn toggle_backend_autostart(id: &str) -> anyhow::Result<String> {
    let mut config = load_config(None)?;
    let session = config
        .sessions
        .iter()
        .find(|session| session.id == id)
        .context("selected backend disappeared")?;
    if config.autostart_backends.contains(&session.id) {
        config.autostart_backends.retain(|item| item != id);
    } else {
        crate::backend_startup::validate(session)?;
        config.autostart_backends.push(id.to_owned());
    }
    write_config(&config_dir()?.join(CONFIG_FILE), &config)?;
    Ok(format!(
        "backend {id} autostart {}; applies on next gateway start",
        if config.autostart_backends.iter().any(|item| item == id) {
            "on"
        } else {
            "off"
        }
    ))
}

/// What `g` may do from what the service reader reports. Uninstalling needs
/// the reader to confirm a registered, loaded service; installing needs it to
/// confirm there is none (or only a file that would not autostart, which
/// install repairs). A stopped-but-installed LaunchAgent is neither: `g` there
/// would uninstall the very registration the owner just saw as "Installed",
/// so it explains instead of acting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutostartToggle {
    Install,
    Uninstall,
    Refuse(String),
}

fn autostart_toggle(state: anyhow::Result<service::ServiceState>) -> AutostartToggle {
    match state {
        Ok(service::ServiceState::Installed) => AutostartToggle::Uninstall,
        Ok(service::ServiceState::NotInstalled | service::ServiceState::FileOnly) => {
            AutostartToggle::Install
        }
        Ok(service::ServiceState::Stopped) => AutostartToggle::Refuse(String::from(
            "autostart is on and Gateway is stopped; press s on Overview to start it, \
             or run `muqun-gateway service uninstall` to turn autostart off",
        )),
        Err(error) => AutostartToggle::Refuse(format!(
            "autostart unchanged: cannot read service registration: {}",
            first_line(&error.to_string())
        )),
    }
}

fn autostart_label(state: &anyhow::Result<service::ServiceState>) -> &'static str {
    match state {
        Ok(service::ServiceState::Installed) => "On - user service registered",
        Ok(service::ServiceState::Stopped) => "On - Installed · stopped (starts at next login)",
        Ok(service::ServiceState::FileOnly) => "Incomplete - service file only",
        Ok(service::ServiceState::NotInstalled) => "Off - start Gateway manually",
        Err(_) => "Unavailable - cannot read service registration",
    }
}

fn confirm_gateway_autostart(enabled: bool) -> anyhow::Result<bool> {
    confirm_action(&format!("Turn Gateway login autostart {}?", if enabled { "on" } else { "off" }), &[
        String::from("This registers/removes only Gateway's user service."),
        String::from("Enabling starts Gateway now and at login; disabling keeps its current running/stopped state."),
        String::from("Terminal-server startup and paired devices are unchanged."),
    ], "change login autostart")
}

#[derive(Default)]
struct Confirmation {
    yes: bool,
}

impl Confirmation {
    fn key(&mut self, key: KeyCode) -> Option<bool> {
        match key {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab => {
                self.yes = !self.yes;
                None
            }
            KeyCode::Enter => Some(self.yes),
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        }
    }
}

fn confirm_action(title: &str, scope: &[String], verb: &str) -> anyhow::Result<bool> {
    let mut confirmation = Confirmation::default();
    loop {
        let choice = ScreenLine {
            spans: vec![
                (
                    " No, cancel ".into(),
                    if confirmation.yes {
                        Tone::Normal
                    } else {
                        Tone::Selected
                    },
                ),
                (
                    format!("  Yes, {verb} "),
                    if confirmation.yes {
                        Tone::Selected
                    } else {
                        Tone::Normal
                    },
                ),
            ],
            qr: None,
        };
        let mut lines = scope.to_vec();
        lines.push(String::from("←→ choose · Enter confirm · Esc cancel"));
        write_dialog(title, &lines, choice)?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(answer) = confirmation.key(event.code) {
                return Ok(answer);
            }
        }
    }
}

fn write_dialog(title: &str, details: &[String], choices: ScreenLine) -> anyhow::Result<()> {
    let (width, height) = terminal_size().unwrap_or((100, 30));
    let budget = (width.saturating_sub(1) as usize).min(92);
    let framed = budget >= 48 && height >= 14;
    let inner = if framed {
        budget.saturating_sub(4)
    } else {
        budget
    };
    let mut rows = Vec::new();
    for line in details {
        rows.extend(word_wrap(line, inner).into_iter().map(ScreenLine::plain));
    }
    let mut lines = Vec::new();
    panel(&mut lines, title, rows, budget, framed);
    lines.truncate((height as usize).saturating_sub(2));
    lines.push(choices);
    write_viewport(&lines, width, height)
}

/// A compact "3m ago" / "2h ago" / "5d ago" for the manage device list. Falls
/// back to "just now" for anything under a minute and "recently" if the clock
/// looks off (a future timestamp).
pub(crate) fn relative_since(then_unix_ms: u128) -> String {
    let now = now_unix_ms();
    if then_unix_ms > now {
        return String::from("recently");
    }
    let secs = (now - then_unix_ms) / 1000;
    if secs < 60 {
        String::from("just now")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

pub(crate) fn display_width(value: &str) -> usize {
    with_width_locale(|| {
        let mut chars = value.chars().peekable();
        let mut width = 0;
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for control in chars.by_ref() {
                    if ('@'..='~').contains(&control) {
                        break;
                    }
                }
            } else {
                width += character_cells(ch);
            }
        }
        width
    })
}

// POSIX wcwidth supplies the host's Unicode cell-width tables without another
// dependency. Use a cached *thread-local* UTF-8 locale, never setlocale: the
// gateway runtime is multithreaded and changing its global locale is unsafe.
#[cfg(unix)]
struct WidthLocale(libc::locale_t);

#[cfg(unix)]
impl WidthLocale {
    fn new() -> Self {
        for name in [b"C.UTF-8\0".as_slice(), b"en_US.UTF-8\0", b"UTF-8\0"] {
            // SAFETY: names are NUL-terminated; a fresh locale is privately owned.
            let locale = unsafe {
                libc::newlocale(
                    libc::LC_CTYPE_MASK,
                    name.as_ptr().cast(),
                    std::ptr::null_mut(),
                )
            };
            if !locale.is_null() {
                return Self(locale);
            }
        }
        Self(std::ptr::null_mut())
    }
}

#[cfg(unix)]
impl Drop for WidthLocale {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this thread owns the locale; every measurement restored
            // its prior locale before the thread-local object can be dropped.
            unsafe {
                libc::freelocale(self.0);
            }
        }
    }
}

#[cfg(unix)]
fn with_width_locale<T>(measure: impl FnOnce() -> T) -> T {
    thread_local! { static LOCALE: WidthLocale = WidthLocale::new(); }
    struct RestoreLocale(libc::locale_t);
    impl Drop for RestoreLocale {
        fn drop(&mut self) {
            // SAFETY: this is the still-live previous locale of this thread.
            unsafe {
                libc::uselocale(self.0);
            }
        }
    }
    LOCALE.with(|locale| {
        let _restore = if locale.0.is_null() {
            None
        } else {
            // SAFETY: immutable locale remains owned by this thread-local object.
            let previous = unsafe { libc::uselocale(locale.0) };
            (!previous.is_null()).then_some(RestoreLocale(previous))
        };
        measure()
    })
}

#[cfg(not(unix))]
fn with_width_locale<T>(measure: impl FnOnce() -> T) -> T {
    measure()
}

fn character_cells(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn wcwidth(ch: libc::wchar_t) -> libc::c_int;
        }
        // SAFETY: Rust char is a valid scalar fitting POSIX wchar_t. Callers
        // measure under with_width_locale, which restores the prior locale.
        let cells = unsafe { wcwidth(ch as libc::wchar_t) };
        if cells >= 0 {
            return cells as usize;
        }
    }
    // Unknown/unavailable Unicode tables: conservatively budget two cells, so
    // an unfamiliar wide scalar cannot overflow a row. ASCII always occupies one.
    if ch.is_ascii() {
        1
    } else {
        2
    }
}

fn clip_cells(value: &str, max_cells: usize) -> &str {
    with_width_locale(|| {
        let mut cells = 0;
        let mut end = 0;
        for (index, ch) in value.char_indices() {
            let next = character_cells(ch);
            if cells + next > max_cells {
                break;
            }
            cells += next;
            end = index + ch.len_utf8();
        }
        &value[..end]
    })
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

/// The first line of a multi-line error, for the manage screen's one-line
/// status field. Errors written for a terminal put the essential sentence
/// first and the remedy underneath; only the first fits here.
pub(crate) fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or(value).to_string()
}

pub(crate) fn push_wrapped_field(lines: &mut Vec<String>, label: &str, value: &str, width: usize) {
    if width == 0 {
        lines.push(String::new());
        return;
    }
    let label = if label.is_empty() {
        String::new()
    } else {
        format!("{}: ", safe_text(label))
    };
    // Leave room for a wide scalar even on a narrow viewport. The final writer
    // applies the same display-cell clipping to every row.
    let prefix = clip_cells(&label, width.saturating_sub(2));
    let continuation = " ".repeat(display_width(prefix));
    let first_width = width.saturating_sub(display_width(prefix));
    let text = safe_text(value);
    let mut remaining = text.as_str();
    let mut first = true;
    while !remaining.is_empty() {
        let chunk = clip_cells(remaining, first_width);
        // A one-cell terminal cannot display a wide scalar. Consume it with a
        // visible placeholder rather than wrapping it or looping forever.
        let (chunk, consumed) = if chunk.is_empty() {
            ("?", remaining.chars().next().unwrap().len_utf8())
        } else {
            (chunk, chunk.len())
        };
        lines.push(format!(
            "{}{}",
            if first { prefix } else { &continuation },
            chunk
        ));
        remaining = &remaining[consumed..];
        first = false;
    }
    if first {
        lines.push(prefix.to_owned());
    }
}

pub(crate) fn fetch_pending_pairing() -> anyhow::Result<Option<PendingPairing>> {
    let config = load_config(None)?;
    let (status, value) = crate::cli::local_api::admin_request_with_timeouts(
        &config,
        "GET",
        "/api/pair/pending",
        None,
        Duration::from_millis(250),
        Duration::from_millis(500),
    )?;
    pending_from_answer(status, &value)
}

/// `setup` and `lifecycle` use the pending probe as a readiness check, so a
/// 200 that is not the gateway's JSON object (a stray listener on the port)
/// is an error, not "nothing pending".
fn pending_from_answer(status: u16, value: &Value) -> anyhow::Result<Option<PendingPairing>> {
    anyhow::ensure!(status == 200, "gateway refused local management request");
    anyhow::ensure!(value.is_object(), "unexpected answer to the pending probe");
    if value.get("pending").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    Ok(Some(PendingPairing {
        request_id: value
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        device_name: value
            .get("device_name")
            .and_then(Value::as_str)
            .unwrap_or("Muqun app")
            .into(),
        kind: serde_json::from_value(value["kind"].clone()).unwrap_or_default(),
        install_id: value
            .get("install_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        server_id: None,
        code: value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        code_hash: String::new(),
        created_unix_ms: value
            .get("created_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .unwrap_or_default(),
        failed_attempts: 0,
        minted_by: None,
    }))
}

pub(crate) fn revoke_managed_device(device_id: &str) -> anyhow::Result<bool> {
    validate_session_id(device_id).context("invalid device id")?;
    let config = load_config(None)?;
    match crate::cli::local_api::admin_request(
        &config,
        "DELETE",
        &format!("/api/pairings/{device_id}"),
        None,
    ) {
        Ok((200, _)) => Ok(true),
        Ok((404, _)) => Ok(false),
        Ok((status, _)) => anyhow::bail!("gateway refused device revocation: HTTP {status}"),
        // With no running gateway there is no in-memory token list to
        // invalidate, so updating the persisted records is sufficient.
        Err(error) if error.is::<crate::cli::local_api::GatewayNotRunning>() => {
            revoke_device_by_id(device_id)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn local_management_addr(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        if listen.is_ipv6() {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], listen.port()))
        } else {
            SocketAddr::from(([127, 0, 0, 1], listen.port()))
        }
    } else {
        listen
    }
}

pub(crate) fn validate_public_url(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    let parsed = reqwest::Url::parse(value).context("invalid gateway URL")?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "gateway URL must use http:// or https://"
    );
    anyhow::ensure!(parsed.host().is_some(), "gateway URL must include a host");
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "gateway URL cannot contain credentials"
    );
    anyhow::ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "gateway URL cannot contain a query or fragment"
    );
    Ok(value.trim_end_matches('/').to_string())
}

pub(crate) fn listen_for_explicit_public_url(public_url: &str, port: u16) -> String {
    let host = reqwest::Url::parse(public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    match host.as_deref() {
        Some("localhost") => format!("127.0.0.1:{port}"),
        Some(host) => match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
        {
            Ok(std::net::IpAddr::V4(ip)) if ip.is_loopback() => format!("{ip}:{port}"),
            Ok(std::net::IpAddr::V6(ip)) if ip.is_loopback() => format!("[{ip}]:{port}"),
            _ => format!("0.0.0.0:{port}"),
        },
        None => format!("0.0.0.0:{port}"),
    }
}

pub(crate) fn pairing_qr_offer(url: &str, server_id: &str, transport_key: Option<&str>) -> String {
    let mut offer = format!(
        "muqun://pair?u={}&s={}",
        url_component(url),
        url_component(server_id)
    );
    if let Some(transport_key) = transport_key {
        offer.push_str("&k=");
        offer.push_str(&url_component(transport_key));
    }
    offer
}

pub(crate) fn url_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub(crate) fn render_qr(code: &QrCode) -> String {
    let image = code.render::<unicode::Dense1x2>().quiet_zone(true).build();
    // Force the standard dark-on-light polarity. Relying on the terminal's
    // foreground/background colors can invert the code under some Herdr themes
    // and native camera scanners do not consistently recover from that.
    image
        .lines()
        .map(|line| format!("\x1b[30;47m{line}\x1b[0m"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_pending_probe_needs_a_json_object() {
        use serde_json::json;
        assert!(super::pending_from_answer(200, &json!("ok")).is_err());
        assert!(super::pending_from_answer(200, &json!(null)).is_err());
        assert!(super::pending_from_answer(500, &json!({})).is_err());
        assert!(super::pending_from_answer(200, &json!({"pending": false}))
            .unwrap()
            .is_none());
    }

    use super::{
        clip_cells, enter_action, manage_lines, safe_text, viewport_start, Confirmation, KeyCode,
        ManageData, ManageView, ScreenLine, Section, Tone,
    };
    use crate::*;

    #[cfg(unix)]
    #[test]
    fn tmux_availability_is_judged_on_the_service_path_and_labelled() {
        use super::tmux_availability;
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("manage-tmux-{}", uuid::Uuid::new_v4()));
        let with = root.join("brew/bin");
        let without = root.join("usr/bin");
        std::fs::create_dir_all(&with).unwrap();
        std::fs::create_dir_all(&without).unwrap();
        let tmux = with.join(crate::backend::TMUX_PROGRAM);
        std::fs::write(&tmux, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (with, without) = (with.display().to_string(), without.display().to_string());

        let tmux = tmux.display().to_string();
        assert_eq!(
            tmux_availability(&with, None),
            format!("tmux: found on CLI PATH ({tmux})")
        );
        assert_eq!(
            tmux_availability(&without, None),
            format!("tmux: NOT FOUND on CLI PATH ({without})")
        );
        assert_eq!(
            tmux_availability(&without, Some(&with)),
            format!("tmux: found on service PATH ({tmux})")
        );
        // The case the CLI alone cannot see: found here, not by the service.
        let state = tmux_availability(&with, Some(&without));
        assert!(
            state.starts_with(&format!(
                "tmux: NOT on service PATH ({without}); found on CLI PATH ({tmux})"
            )),
            "{state}"
        );
        assert!(tmux_availability(&without, Some(&without))
            .starts_with("tmux: NOT FOUND on service PATH"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn autostart_toggle_never_uninstalls_a_stopped_or_unreadable_service() {
        use super::{autostart_label, autostart_toggle, AutostartToggle};
        use crate::platform::service::ServiceState::*;
        assert_eq!(autostart_toggle(Ok(Installed)), AutostartToggle::Uninstall);
        assert_eq!(autostart_toggle(Ok(NotInstalled)), AutostartToggle::Install);
        assert_eq!(autostart_toggle(Ok(FileOnly)), AutostartToggle::Install);
        for state in [Ok(Stopped), Err(anyhow::anyhow!("launchctl failed"))] {
            assert!(matches!(
                autostart_toggle(state),
                AutostartToggle::Refuse(_)
            ));
        }
        assert_eq!(
            autostart_label(&Ok(Stopped)),
            "On - Installed · stopped (starts at next login)"
        );
        assert!(!autostart_label(&Ok(Stopped)).contains("Incomplete"));
    }

    #[test]
    fn header_line_reports_status_and_shrinks_to_fit() {
        use super::{header_line, Connectivity, HeaderStatus, Link};
        let links = Connectivity {
            herdr: Link::Connected,
            agents: Link::Offline,
        };
        let cases = [
            (
                HeaderStatus { version: "1.2.3", listen: "0.0.0.0:23100", running: true, links, devices: 2 },
                96,
                "Muqun Gateway v1.2.3 · Running 0.0.0.0:23100 · Herdr connected · Agents offline · 2 devices",
            ),
            (
                HeaderStatus { version: "1.2.3", listen: "0.0.0.0:23100", running: true, links, devices: 1 },
                80,
                "v1.2.3 · Running 0.0.0.0:23100 · Herdr connected · Agents offline · 1 device",
            ),
            (
                HeaderStatus { version: "1.2.3", listen: "127.0.0.1:23100", running: false, links: Connectivity::default(), devices: 0 },
                96,
                "Muqun Gateway v1.2.3 · Stopped · listens on 127.0.0.1:23100 when started · 0 devices",
            ),
            (
                HeaderStatus { version: "1.2.3", listen: "127.0.0.1:23100", running: false, links: Connectivity::default(), devices: 0 },
                40,
                "v1.2.3 · Stopped · 127.0.0.1:23100",
            ),
        ];
        for (status, width, expected) in &cases {
            assert_eq!(header_line(status, *width), *expected, "width {width}");
        }
        let discovered = super::connectivity_from_discovery(&serde_json::json!({
            "planes": {
                "terminal": { "backends": [
                    { "kind": "tmux", "connected": false },
                    { "kind": "herdr", "connected": true },
                ]},
                "agents": { "agents": [ { "status": "offline" }, { "status": "unconfigured" } ] },
            }
        }));
        assert_eq!(discovered, links);
    }

    #[test]
    fn the_footer_is_one_navigation_line_and_the_views_own_keys() {
        use super::{footer, footer_lines, FooterContext, PairingStage};
        let base = FooterContext::default();
        let cases = [
            (
                Section::Overview,
                base,
                "",
                Some("s start · t stop · r restart"),
            ),
            (
                Section::Terminals,
                FooterContext {
                    no_backends: true,
                    ..base
                },
                "Enter add tmux",
                Some("h add Herdr · m add tmux"),
            ),
            (
                Section::Terminals,
                base,
                "Enter actions",
                Some("h add Herdr · m add tmux"),
            ),
            (
                Section::Devices,
                base,
                "Enter details",
                Some("p pair a device · x revoke"),
            ),
            (
                Section::Devices,
                FooterContext {
                    pairing: Some(PairingStage::Done),
                    ..base
                },
                "Enter back",
                Some("p pair another · Esc back"),
            ),
            (
                Section::Settings,
                base,
                "Enter change",
                Some("PgUp/PgDn page · r restart"),
            ),
        ];
        for (section, context, enter, actions) in cases {
            let shape = footer(section, &context);
            // Overview has no rows, so no Enter.
            let navigation = if enter.is_empty() {
                String::from("Tab next view · q quit")
            } else {
                format!("↑↓ select · {enter} · Tab next view · q quit")
            };
            assert_eq!(shape.navigation, navigation, "{section:?}");
            assert_eq!(shape.actions.as_deref(), actions, "{section:?}");
            // One line where it fits, two where it does not; never clipped.
            assert_eq!(footer_lines(&shape, 110).len(), 1, "{section:?}");
            for line in footer_lines(&shape, 56) {
                assert!(line.width() <= 56, "{section:?}: {}", line.render(200));
            }
        }
        // Notes break at spaces; an address longer than the line is kept whole.
        for (text, width, lines) in [
            ("one two three", 7, vec!["one two", "three"]),
            (
                "see https://example.test/abc",
                12,
                vec!["see", "https://exam", "ple.test/abc"],
            ),
            ("中文", 1, vec!["中", "文"]),
        ] {
            assert_eq!(super::word_wrap(text, width), lines, "{text}");
        }
    }

    #[test]
    fn every_setting_is_a_row_with_its_value_explanation_and_source() {
        use super::{setting_value, Setting};
        let config = |change: serde_json::Value| {
            let mut value = serde_json::to_value(layout_config()).unwrap();
            for (key, field) in change.as_object().unwrap() {
                value[key] = field.clone();
            }
            serde_json::from_value::<crate::Config>(value).unwrap()
        };
        let cases = [
            (Setting::HistoryStorage, serde_json::json!({}), "memory"),
            (
                Setting::HistoryStorage,
                serde_json::json!({ "history": { "storage": "sqlite" } }),
                "SQLite",
            ),
            (
                Setting::StoreEncryption,
                serde_json::json!({}),
                "key in store.key · read-only",
            ),
            (
                Setting::StoreEncryption,
                serde_json::json!({ "store": { "encryption": "off" } }),
                "off · read-only",
            ),
            (
                Setting::AgentBypass,
                serde_json::json!({}),
                "codex On · claude On",
            ),
            (
                Setting::AgentBypass,
                serde_json::json!({ "agent_launch_args": { "codex": [], "claude": ["--model", "x"] } }),
                "codex Off · claude custom",
            ),
            (
                Setting::AgentTrust,
                serde_json::json!({ "agent_trust_folders": false }),
                "Off",
            ),
            (
                Setting::AgentEnv,
                serde_json::json!({ "agent_env": { "HTTPS_PROXY": "http://secret" } }),
                "HTTPS_PROXY · values hidden",
            ),
            (
                Setting::Mcp,
                serde_json::json!({ "mcp": { "enabled": false } }),
                "Off",
            ),
            (Setting::Autostart, serde_json::json!({}), "Off"),
            (
                Setting::McpInject,
                serde_json::json!({ "mcp": { "inject_into_agents": false } }),
                "Off",
            ),
        ];
        for (setting, change, value) in cases {
            assert_eq!(
                setting_value(setting, &config(change), "Off - start Gateway manually"),
                value,
                "{setting:?}"
            );
            assert!(!setting.explanation().is_empty());
        }
        // A change applies after a restart for every key the running
        // gateway read at startup.
        for setting in Setting::ALL {
            // Every config.json key a row changes is read at startup; Detect
            // address changes one too.
            let writes_config = setting
                .source()
                .is_some_and(|source| source.starts_with("config.json ›"))
                && setting.read_only().is_none();
            let restarts = writes_config || setting == Setting::DetectAddress;
            assert_eq!(setting.needs_restart(), restarts, "{setting:?}");
        }
        // The view lists every row in order, each under its group, and fits
        // a 60-column terminal, cutting with … rather than mid-word.
        let config = config(serde_json::json!({}));
        let data = ManageData {
            history_caption: None,
            config: &config,
            message: "ready",
            pending: None,
            devices: &[],
            runtime: "stopped",
            running: false,
            autostart: "Off - start Gateway manually",
            backend_states: &[],
            qr: None,
            changed: false,
            connectivity: super::Connectivity::default(),
            pairing: &super::PairingView::default(),
        };
        for width in [110_u16, 61] {
            let mut text = String::new();
            for selected in [0, Setting::ALL.len() - 1] {
                let mut ui = ManageView {
                    section: Section::Settings,
                    selected,
                    ..Default::default()
                };
                for line in manage_lines(&data, &mut ui, width, 200) {
                    assert!(line.width() <= width as usize, "{}", line.render(300));
                    text.push_str(&line.render(300));
                    text.push('\n');
                }
            }
            let mut at = 0;
            for setting in Setting::ALL {
                let found = [setting.label(), setting.short_label()]
                    .iter()
                    .filter_map(|label| text[at..].find(label))
                    .min()
                    .unwrap_or_else(|| panic!("{setting:?} missing at {width}: {text}"));
                at += found;
            }
            assert!(text.contains("OTHER COMPUTERS"));
            if width == 110 {
                assert!(text.contains("config.json › history.storage"));
            }
        }
        // At a real terminal's height the last row, its explanation and the
        // view's end are reachable; a changed row says it needs a restart.
        let changed = ManageData {
            history_caption: None,
            changed: true,
            ..data
        };
        for (width, height) in [(110_u16, 32_u16), (61, 24)] {
            let mut ui = ManageView {
                section: Section::Settings,
                selected: Setting::ALL.len() - 1,
                restart: vec![Setting::HistoryStorage],
                ..Default::default()
            };
            let text: String = manage_lines(&changed, &mut ui, width, height)
                .iter()
                .map(|line| line.render(300) + "\n")
                .collect();
            assert!(text.contains("OTHER COMPUTERS"), "{width}x{height}: {text}");
            assert!(
                text.contains("Answers this computer only")
                    || text.contains("The MCP endpoint answers")
            );
            ui.selected = 5;
            let text: String = manage_lines(&changed, &mut ui, width, height)
                .iter()
                .map(|line| line.render(300) + "\n")
                .collect();
            assert!(
                text.contains("memory · restart needed"),
                "{width}x{height}: {text}"
            );
        }
    }

    #[test]
    fn a_settings_change_keeps_the_rest_of_config_json() {
        let dir = std::env::temp_dir().join(format!("muqun-config-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        let mut original = serde_json::to_value(layout_config()).unwrap();
        original["zz_unknown"] = serde_json::json!({ "zz": 1, "aa": 2 });
        original["agent_trust_folders"] = serde_json::json!(true);
        // An order of the owner's own: unknown key first.
        let mut ordered = indexmap::IndexMap::new();
        ordered.insert("zz_unknown".to_owned(), original["zz_unknown"].clone());
        for (key, value) in original.as_object().unwrap() {
            ordered.entry(key.clone()).or_insert_with(|| value.clone());
        }
        let keys: Vec<String> = ordered.keys().cloned().collect();
        for (path, value) in [
            (
                vec!["history", "storage"],
                Some(serde_json::json!("sqlite")),
            ),
            (vec!["mcp", "enabled"], Some(serde_json::json!(false))),
            (vec!["agent_trust_folders"], Some(serde_json::json!(false))),
            (
                vec!["agent_launch_args", "codex"],
                Some(serde_json::json!([])),
            ),
            (vec!["agent_launch_args", "codex"], None),
        ] {
            std::fs::write(&file, serde_json::to_vec_pretty(&ordered).unwrap()).unwrap();
            super::set_config_key(&file, &path, value).unwrap();
            let after: indexmap::IndexMap<String, serde_json::Value> =
                serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            let after_keys: Vec<String> = after.keys().cloned().collect();
            let added: Vec<&String> = after_keys
                .iter()
                .filter(|key| !keys.contains(key))
                .collect();
            let kept: Vec<&String> = after_keys.iter().filter(|key| keys.contains(key)).collect();
            assert_eq!(kept, keys.iter().collect::<Vec<_>>(), "{path:?}: order");
            for key in &keys {
                if key != path[0] {
                    // Bytes, not values: a reordered nested object fails.
                    assert_eq!(
                        serde_json::to_string(&after[key]).unwrap(),
                        serde_json::to_string(&ordered[key]).unwrap(),
                        "{path:?}: {key} changed"
                    );
                }
            }
            assert!(
                added.iter().all(|key| *key == path[0]),
                "{path:?}: {added:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manager_navigation_is_scoped_and_bounded() {
        let mut ui = ManageView::default();
        assert!(ui.navigate(KeyCode::BackTab, 0));
        assert_eq!(ui.section, Section::Settings);
        ui.navigate(KeyCode::Char('2'), 0);
        assert_eq!(ui.section, Section::Terminals);
        ui.navigate(KeyCode::End, 50);
        assert_eq!(ui.selected, 49);
        ui.navigate(KeyCode::Down, 50);
        assert_eq!(ui.selected, 49);
        ui.navigate(KeyCode::PageUp, 50);
        assert_eq!(ui.selected, 39);
        assert!(!ui.navigate(KeyCode::Char('g'), 50));
        ui.navigate(KeyCode::Tab, 50);
        assert_eq!(ui.selected, 0);
        assert_eq!(ui.scroll, 0);
    }

    #[test]
    fn manager_enter_actions_and_confirmation_cancel_are_explicit() {
        // Enter never starts or stops the gateway from the landing view; on
        // every other view it is the selected row's action.
        for (section, empty, pairing, action) in [
            (Section::Overview, true, false, None),
            (Section::Terminals, false, false, Some("enter")),
            (Section::Terminals, true, false, Some("m")),
            (Section::Devices, true, false, Some("p")),
            (Section::Devices, false, false, Some("enter")),
            (Section::Devices, true, true, Some("r")),
            (Section::Settings, false, false, Some("enter")),
        ] {
            assert_eq!(enter_action(section, empty, pairing), action, "{section:?}");
        }
        let mut confirmation = Confirmation::default();
        assert_eq!(confirmation.key(KeyCode::Enter), Some(false));
        assert_eq!(confirmation.key(KeyCode::Right), None);
        assert_eq!(confirmation.key(KeyCode::Enter), Some(true));
        assert_eq!(confirmation.key(KeyCode::Esc), Some(false));
    }

    fn layout_config() -> crate::Config {
        serde_json::from_value(serde_json::json!({
            "server_id": "layout", "label": "工作站 e\u{301}\u{1b}[2J", "token_hash": "not-a-token",
            "listen": "127.0.0.1:23100", "public_url": "http://localhost:23100",
            "sessions": (0..40).map(|index| serde_json::json!({"id": format!("qa-{index:02}"),
                "label": "中文 e\u{301}\u{1b}[2J", "backend": "tmux", "socket_path": "/isolated/absent.sock"})).collect::<Vec<_>>()
        })).unwrap()
    }

    #[test]
    fn manager_styled_layout_is_cell_bounded_through_resize_and_keeps_selection_visible() {
        let config = layout_config();
        let data = ManageData {
            history_caption: None,
            config: &config,
            message: "Ready 中文e\u{301}",
            pending: None,
            devices: &[],
            runtime: "stopped",
            running: false,
            autostart: "Off",
            backend_states: &[],
            qr: None,
            changed: false,
            connectivity: super::Connectivity::default(),
            pairing: &super::PairingView::default(),
        };
        for section in Section::ALL {
            let mut ui = ManageView {
                section,
                selected: if section == Section::Terminals { 39 } else { 0 },
                ..Default::default()
            };
            for width in [0_u16, 1, 2, 8, 24, 40, 60, 80, 100, 200] {
                for height in [0_u16, 1, 5, 8, 18, 30] {
                    let lines = manage_lines(&data, &mut ui, width, height);
                    assert!(lines.len() <= height as usize);
                    for line in &lines {
                        let output = line.render(width.saturating_sub(1) as usize);
                        assert!(
                            display_width(&output) <= width.saturating_sub(1) as usize,
                            "{output:?}"
                        );
                        assert!(
                            !output.contains("\x1b[2J"),
                            "untrusted label escaped: {output:?}"
                        );
                        assert!(
                            !output.contains("\x1b[48;"),
                            "forced background: {output:?}"
                        );
                        assert!(line.width() <= 98);
                    }
                    if section == Section::Terminals && width >= 40 && height >= 8 {
                        assert!(lines
                            .iter()
                            .any(|line| line.selected() && line.render(100).contains("▌ qa-39")));
                    }
                }
            }
        }
        let line = ScreenLine::text("中文e\u{301}X", Tone::Selected);
        assert_eq!(display_width(&line.render(5)), 5);
        assert!(line.render(5).contains("\x1b[7m"));
        assert!(line.render(5).contains("中文e\u{301}"));
    }

    #[test]
    fn manager_empty_states_and_default_marker_explain_next_actions() {
        let config = layout_config();
        let pairing = super::PairingView::default();
        let data = |config| ManageData {
            history_caption: None,
            config,
            message: "ready",
            pending: None,
            devices: &[],
            runtime: "stopped",
            running: false,
            autostart: "Off",
            backend_states: &[],
            qr: None,
            changed: false,
            connectivity: super::Connectivity::default(),
            pairing: &pairing,
        };
        let mut ui = ManageView {
            section: Section::Terminals,
            selected: 1,
            ..Default::default()
        };
        let lines = manage_lines(&data(&config), &mut ui, 100, 30);
        let selected = lines
            .iter()
            .find(|line| line.selected() && line.render(100).contains("qa-01"))
            .unwrap()
            .render(100);
        assert!(selected.contains("▌ qa-01"));
        assert!(!selected.contains("default"));
        assert!(lines.iter().any(|line| {
            let text = line.render(100);
            text.contains("qa-00") && text.contains("default · on demand")
        }));
        let mut empty_config = layout_config();
        empty_config.sessions.clear();
        let lines = manage_lines(&data(&empty_config), &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter adds tmux; h adds Herdr")));
        ui.section = Section::Devices;
        let lines = manage_lines(&data(&empty_config), &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter to pair your first device")));
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("p pair a device")));
    }

    #[test]
    fn manager_selection_scrolls_and_survives_resize_or_deletion() {
        assert_eq!(viewport_start(0, Some(49), 50, 10), 40);
        assert_eq!(viewport_start(40, Some(49), 50, 3), 47);
        assert_eq!(viewport_start(47, Some(49), 2, 10), 0);
        assert_eq!(viewport_start(99, None, 20, 5), 15);
        assert_eq!(viewport_start(99, None, 0, 0), 0);
        assert_eq!(safe_text("phone\x1b[2J\r\n"), "phone[2J");
    }

    #[test]
    fn manager_fields_wrap_without_losing_url_or_message_text() {
        let value = "http://osk.taila90692.ts.net:23847/a-long-path";
        let mut lines = Vec::new();
        push_wrapped_field(&mut lines, "url", value, 24);
        let reconstructed = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    line.strip_prefix("url: ").unwrap()
                } else {
                    line.trim_start()
                }
            })
            .collect::<String>();
        assert_eq!(reconstructed, value);
        assert!(lines.iter().all(|line| display_width(line) <= 24));
    }

    #[test]
    fn manager_clips_viewport_in_cells_and_keeps_combining_marks_with_their_base() {
        assert_eq!(display_width("中文e\u{301}"), 5);
        assert_eq!(display_width("\x1b[30;47m中文e\u{301}\x1b[0m"), 5);
        assert_eq!(clip_cells("中文e\u{301}X", 5), "中文e\u{301}");
        assert_eq!(clip_cells("e\u{301}中", 1), "e\u{301}");
        assert_eq!(clip_cells("中", 1), "");
        assert_eq!(clip_cells("中文", 0), "");
        // The viewport reserves the final cell to avoid terminal autowrap.
        for width in [0_u16, 1, 2, 3, 5, 20] {
            let budget = width.saturating_sub(1) as usize;
            let text = safe_text("中文e\u{301}\r\n\x1b[2J");
            assert!(display_width(clip_cells(&text, budget)) <= budget);
        }
    }

    #[test]
    fn manager_wide_fields_wrap_in_cells_without_losing_combining_or_cjk_text() {
        let value = "中文e\u{301}界a\u{308}中文";
        for width in [8, 10, 20] {
            let mut lines = Vec::new();
            push_wrapped_field(&mut lines, "名称", value, width);
            assert!(lines.iter().all(|line| display_width(line) <= width));
            let reconstructed = lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    if index == 0 {
                        line.strip_prefix("名称: ").unwrap()
                    } else {
                        line.trim_start()
                    }
                })
                .collect::<String>();
            assert_eq!(reconstructed, value);
            assert!(lines
                .iter()
                .all(|line| !line.trim_start().starts_with(['\u{301}', '\u{308}'])));
        }
        for width in 0..=3 {
            let mut lines = Vec::new();
            push_wrapped_field(&mut lines, "名称", value, width);
            assert!(!lines.is_empty());
            assert!(lines.iter().all(|line| display_width(line) <= width));
        }
    }

    #[test]
    fn manager_qr_uses_the_current_config_fields() {
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1", Some("key_1")),
            "muqun://pair?u=http%3A%2F%2F100.1.2.3%3A23847&s=server-1&k=key_1"
        );
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1", None),
            "muqun://pair?u=http%3A%2F%2F100.1.2.3%3A23847&s=server-1"
        );
    }

    #[test]
    fn terminal_qr_has_explicit_standard_colors_and_measurable_width() {
        let code = QrCode::with_error_correction_level(
            b"muqun://pair?u=http%3A%2F%2Fhost&s=id",
            EcLevel::L,
        )
        .unwrap();
        let image = render_qr(&code);
        let expected_width = code.width() + 8;
        assert!(image
            .lines()
            .all(|line| line.starts_with("\x1b[30;47m") && line.ends_with("\x1b[0m")));
        assert!(image
            .lines()
            .all(|line| display_width(line) == expected_width));
        assert_eq!(display_width("\x1b[30;47m█▀ \x1b[0m"), 3);
    }

    #[test]
    fn manager_qr_layout_shows_a_whole_qr_or_an_address_fallback() {
        let mut config = layout_config();
        config.public_url =
            "https://gateway.example.test/a-long-address-for-isolated-pairing".into();
        let qr = render_qr(
            &QrCode::with_error_correction_level(b"isolated-qr-layout", EcLevel::L).unwrap(),
        );
        let data = ManageData {
            history_caption: None,
            config: &config,
            message: "ready",
            pending: None,
            devices: &[],
            runtime: "stopped",
            running: false,
            autostart: "Off",
            backend_states: &[],
            qr: Some(&qr),
            changed: false,
            connectivity: super::Connectivity::default(),
            pairing: &super::PairingView {
                open: true,
                paired: None,
            },
        };
        let mut ui = ManageView {
            section: Section::Devices,
            selected: 0,
            scroll: 99,
            ..Default::default()
        };
        let lines = manage_lines(&data, &mut ui, 100, 50);
        assert_eq!(
            lines.iter().filter(|line| line.qr.is_some()).count(),
            qr.lines().count()
        );
        assert_eq!(ui.scroll, 0);
        let lines = manage_lines(&data, &mut ui, 40, 8);
        assert!(lines.iter().all(|line| line.qr.is_none()));
        assert!(lines
            .iter()
            .any(|line| line.render(39).contains("Resize for the full QR")));
        let plain = |line: &ScreenLine| {
            line.spans
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>()
        };
        let expected = super::wrapped_rows("Address", &config.public_url, 39)
            .iter()
            .map(plain)
            .collect::<Vec<_>>();
        assert!(expected.len() > 1, "regression requires a wrapped address");
        let mut visible = lines
            .iter()
            .map(plain)
            .collect::<std::collections::HashSet<_>>();
        assert!(!expected.iter().all(|row| visible.contains(row)));
        // Stay at 40x8: arrows must reveal every address segment without
        // permitting a partial QR or making the operator resize the terminal.
        for _ in 0..10 {
            ui.navigate(KeyCode::Down, 0);
            let lines = manage_lines(&data, &mut ui, 40, 8);
            assert!(lines.iter().all(|line| line.qr.is_none()));
            assert!(lines
                .iter()
                .all(|line| display_width(&line.render(39)) <= 39));
            visible.extend(lines.iter().map(plain));
        }
        assert!(ui.scroll > 0);
        assert!(
            expected.iter().all(|row| visible.contains(row)),
            "address segments are unreachable: {expected:?}"
        );
        assert!(visible
            .iter()
            .any(|row| row.contains("Waiting for the phone")));
    }

    #[test]
    fn public_url_validation_allows_http_without_allowing_url_injection() {
        assert_eq!(
            validate_public_url("http://100.100.100.100:23100/").unwrap(),
            "http://100.100.100.100:23100"
        );
        assert!(validate_public_url("ftp://100.100.100.100/file").is_err());
        assert!(validate_public_url("http://user:secret@100.100.100.100:23100").is_err());
        assert!(validate_public_url("http://100.100.100.100:23100?token=secret").is_err());
    }

    #[test]
    fn management_connection_uses_the_actual_safe_listener() {
        assert_eq!(
            local_management_addr("0.0.0.0:23100".parse().unwrap()),
            "127.0.0.1:23100".parse().unwrap()
        );
        assert_eq!(
            local_management_addr("100.100.100.100:23100".parse().unwrap()),
            "100.100.100.100:23100".parse().unwrap()
        );
    }

    #[test]
    fn an_explicit_loopback_url_never_opens_the_listener_to_the_lan() {
        assert_eq!(
            listen_for_explicit_public_url("http://localhost:23847", 23847),
            "127.0.0.1:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("http://127.0.0.1:23847", 23847),
            "127.0.0.1:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("http://[::1]:23847", 23847),
            "[::1]:23847"
        );
        assert_eq!(
            listen_for_explicit_public_url("https://host.tailnet.ts.net", 23847),
            "0.0.0.0:23847"
        );
    }

    /// The shape that shipped: a tailnet name in the QR, a socket on loopback.
    ///
    /// It is reached without anyone choosing it -- install before Tailscale is
    /// up, and the next start rewrites the URL and leaves the socket behind --
    /// so the gateway has to say so rather than come up looking healthy.
    #[test]
    fn a_loopback_socket_under_a_tailnet_name_is_warned_about() {
        let warning = unreachable_listen_warning("127.0.0.1:23847", "http://y.ts.net:23847")
            .expect("a loopback socket cannot serve a tailnet name");
        assert!(warning.contains("127.0.0.1:23847"));
        assert!(warning.contains("http://y.ts.net:23847"));
    }

    #[test]
    fn a_reachable_listener_is_not_warned_about() {
        assert!(unreachable_listen_warning("0.0.0.0:23847", "http://y.ts.net:23847").is_none());
        assert!(
            unreachable_listen_warning("100.99.165.54:23847", "http://y.ts.net:23847").is_none()
        );
    }

    /// Loopback is correct under a local URL, and correct under Tailscale
    /// Serve -- which terminates TLS outside and proxies in over 127.0.0.1.
    /// Warning about either would train people to ignore the warning.
    #[test]
    fn loopback_is_left_alone_where_loopback_is_the_answer() {
        assert!(unreachable_listen_warning("127.0.0.1:23847", "http://127.0.0.1:23847").is_none());
        assert!(unreachable_listen_warning("127.0.0.1:23847", "http://localhost:23847").is_none());
        assert!(unreachable_listen_warning("127.0.0.1:23847", "https://y.ts.net").is_none());
    }
}
