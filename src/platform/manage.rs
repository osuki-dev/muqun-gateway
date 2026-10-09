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
            Section::Settings => 4,
            _ => 0,
        };
        if event.code == KeyCode::Esc {
            // One level back: pairing view -> device list -> Overview -> closed.
            if ui.section == Section::Devices && pairing.open {
                pairing.close();
                message = String::from("Pairing view closed; existing devices are unchanged.");
            } else if ui.section != Section::Overview {
                ui = ManageView::default();
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
            KeyCode::Enter => {
                primary_action(ui.section, ui.selected, item_count == 0, pairing.open)
                    .map(|ch| ch.to_string())
                    .unwrap_or_default()
            }
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
                (Section::Overview, "r") => {
                    crate::restart_gateway(false)?;
                    message = String::from("gateway restarted");
                }
                (Section::Devices, "p") => {
                    pairing.open = true;
                    pairing.paired = None;
                    ui.scroll = 0;
                    message = String::from("scan to pair another device");
                }
                (Section::Devices, "x") if !pairing.open => {
                    if devices.is_empty() {
                        message = String::from("no paired devices to revoke");
                    } else if let Some(device) = devices.iter().rev().nth(ui.selected).cloned() {
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
                (Section::Settings, "u") => match prompt_public_url()? {
                    Some(url) => {
                        let listen = listen_for_explicit_public_url(&url, configured_port());
                        update_public_url(&url, &listen)?;
                        message = format!("url updated: {}", truncate(&url, 36));
                    }
                    None => {
                        message = String::from("url unchanged");
                    }
                },
                (Section::Settings, "a") => {
                    let port = configured_port();
                    let selection = auto_public_url(port);
                    update_public_url(
                        &selection.url,
                        &format!("{}:{port}", selection.listen_host),
                    )?;
                    message = format!("auto url: {}", truncate(&selection.url, 36));
                }
                (Section::Settings, "e") => {
                    message = toggle_transport_encryption()?;
                }
                (Section::Terminals, "h") => {
                    message = enable_managed_backend(BackendKind::Herdr)?;
                }
                (Section::Terminals, "m") => {
                    message = enable_managed_backend(BackendKind::Tmux)?;
                }
                (Section::Terminals, "d") => {
                    if let Some(session) = load_config(None)?.sessions.get(ui.selected) {
                        if confirm_remove_backend(session)? {
                            message = remove_managed_backend(&session.id)?;
                        } else {
                            message = String::from("backend unchanged");
                        }
                    }
                }
                (Section::Terminals, "f") => {
                    if let Some(session) = load_config(None)?.sessions.get(ui.selected) {
                        message = set_managed_default_backend(&session.id)?;
                        ui.selected = 0;
                        ui.scroll = 0;
                    }
                }
                (Section::Terminals, "b") => {
                    if let Some(session) = load_config(None)?.sessions.get(ui.selected) {
                        message = toggle_backend_autostart(&session.id)?;
                    }
                }
                (Section::Settings, "g") => match autostart_toggle(service::state()) {
                    AutostartToggle::Refuse(reason) => message = reason,
                    toggle => {
                        let enable = toggle == AutostartToggle::Install;
                        if confirm_gateway_autostart(enable)? {
                            crate::run_service_command(if enable {
                                crate::ServiceCommand::Install
                            } else {
                                crate::ServiceCommand::Uninstall
                            })?;
                            message =
                                format!("gateway autostart {}", if enable { "on" } else { "off" });
                        } else {
                            message = String::from("gateway autostart unchanged");
                        }
                    }
                },
                (Section::Settings, "c") => {
                    message =
                        String::from("Checking official stable release (no background polling)...");
                    print_manage_screen(
                        &message,
                        pending_pairing.as_ref(),
                        &devices,
                        &pairing,
                        connectivity,
                        &mut ui,
                    )?;
                    let plan = update::in_thread(update::check)?;
                    message = plan.summary();
                }
                (Section::Settings, "i") => {
                    message =
                        String::from("Checking official stable release before confirmation...");
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

pub(crate) fn prompt_public_url() -> anyhow::Result<Option<String>> {
    let current = load_config(None)
        .map(|config| config.public_url)
        .unwrap_or_else(|_| auto_public_url(configured_port()).url);
    let mut value = current;
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
        ScreenLine::text(
            "Enter Save address | Esc Cancel | Backspace Delete",
            Tone::Accent,
        ),
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

/// What the footer's hints depend on besides the view.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FooterContext {
    no_backends: bool,
    no_devices: bool,
    /// `Some` while the pairing flow is shown instead of the device list.
    pairing: Option<PairingStage>,
}

/// The footer's hint rows: the view's actions, then navigation.
struct FooterHints {
    /// The view's own keys, one key per action, each followed by the full
    /// words for what it does.
    actions: String,
    /// Switching views, selecting, and leaving: Esc is always one level back
    /// and `q` always closes the manager.
    navigation: String,
    /// The navigation row a short terminal shows instead: the same views,
    /// Tab, Esc and `q`, in fewer words and without the selection keys.
    compact_navigation: String,
}

/// Hints are separated by " | " so narrow terminals can wrap them.
fn footer_hints(section: Section, context: &FooterContext) -> FooterHints {
    let actions = match (section, context.pairing) {
        (Section::Overview, _) => {
            String::from("s Start gateway | t Stop gateway | r Restart gateway")
        }
        (Section::Terminals, _) if context.no_backends => {
            String::from("m Add tmux backend | h Add Herdr backend")
        }
        (Section::Terminals, _) => String::from(
            "f Make selected default | b Toggle terminal-server startup | d Remove selected backend | h Add Herdr backend | m Add tmux backend",
        ),
        // Leaving the pairing flow is Esc's job, listed under navigation;
        // `r` does the same and is not listed twice.
        (Section::Devices, Some(PairingStage::Done)) => String::from("p Pair another device"),
        (Section::Devices, Some(_)) => String::new(),
        (Section::Devices, None) if context.no_devices => String::from("p Pair first device"),
        (Section::Devices, None) => {
            String::from("x Revoke selected device | p Pair new device")
        }
        (Section::Settings, _) => String::from(
            "Enter Change selected setting | g Gateway login autostart | u Edit address | a Detect address | e Toggle encryption | c Check updates | i Install update",
        ),
    };
    let views = (section == Section::Overview)
        .then_some("1 Overview · 2 Terminals · 3 Devices · 4 Settings");
    let select = (section != Section::Overview && context.pairing.is_none())
        .then_some("Up/Down Select | PageUp/PageDown Scroll");
    let (back, compact_back) = match (section, context.pairing) {
        (Section::Overview, _) => ("Esc or q Close manager", "Esc or q Close manager"),
        (Section::Devices, Some(_)) => (
            "Esc Back to device list | q Close manager",
            "Esc Back | q Close manager",
        ),
        _ => (
            "Esc Back to Overview | q Close manager",
            "Esc Back | q Close manager",
        ),
    };
    let join = |hints: &[Option<&str>]| {
        hints
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join(" | ")
    };
    FooterHints {
        actions,
        navigation: join(&[views, Some("Tab Next view"), select, Some(back)]),
        compact_navigation: join(&[views, Some("Tab Views"), Some(compact_back)]),
    }
}

/// The key Enter stands in for on a view. Overview has none: its actions
/// start, stop and restart the gateway, and none of those belongs on the key
/// an operator leans on to look around.
fn primary_action(
    section: Section,
    selected: usize,
    empty_items: bool,
    pairing_open: bool,
) -> Option<char> {
    match section {
        Section::Overview => None,
        Section::Terminals => Some(if empty_items { 'm' } else { 'f' }),
        // The pairing view (QR, code, or done) returns to the list on Enter.
        Section::Devices if pairing_open => Some('r'),
        Section::Devices => Some(if empty_items { 'p' } else { 'x' }),
        Section::Settings => ['g', 'u', 'e', 'c'].get(selected).copied(),
    }
}

#[derive(Default)]
struct ManageView {
    section: Section,
    selected: usize,
    scroll: usize,
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
    let data = ManageData {
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
            row.spans.insert(0, ("│ ".into(), Tone::Accent));
            // Clip only the content before appending the right border.
            let mut budget = width.saturating_sub(4);
            for (text, _) in row.spans.iter_mut().skip(1) {
                *text = clip_cells(text, budget).to_owned();
                budget = budget.saturating_sub(display_width(text));
            }
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

/// Hints wrap at " | "; a hint wider than the row on its own, such as the
/// views list, wraps at " · " rather than being clipped mid-word.
fn shortcut_rows(hints: &str, width: usize, tone: Tone) -> Vec<ScreenLine> {
    let mut rows = Vec::new();
    let mut line = String::new();
    for hint in hints.split(" | ") {
        if !line.is_empty() && display_width(&line) + 3 + display_width(hint) > width {
            rows.push(ScreenLine::text(&line, tone));
            line.clear();
        }
        if line.is_empty() && display_width(hint) > width && hint.contains(" · ") {
            for part in hint.split(" · ") {
                if !line.is_empty() && display_width(&line) + 3 + display_width(part) > width {
                    rows.push(ScreenLine::text(&line, tone));
                    line.clear();
                }
                if !line.is_empty() {
                    line.push_str(" · ");
                }
                line.push_str(part);
            }
            continue;
        }
        if !line.is_empty() {
            line.push_str(" | ");
        }
        line.push_str(hint);
    }
    if !line.is_empty() {
        rows.push(ScreenLine::text(line, tone));
    }
    rows
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
        Section::Settings => 3,
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
    let mut tabs = ScreenLine {
        spans: Vec::new(),
        qr: None,
    };
    for (index, section) in Section::ALL.iter().enumerate() {
        if content < 60 && *section != ui.section {
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
    let hints = footer_hints(
        ui.section,
        &FooterContext {
            no_backends: config.sessions.is_empty(),
            no_devices: data.devices.is_empty(),
            pairing: show_pairing.then_some(pairing_stage),
        },
    );
    let mut footer = shortcut_rows(&hints.actions, content, Tone::Accent);
    footer.extend(shortcut_rows(&hints.navigation, content, Tone::Quiet));
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
    // Small windows keep one primary action and one status row, not a wall of shortcuts.
    if height < 14 {
        footer = hints
            .actions
            .split(" | ")
            .next()
            .filter(|hint| !hint.is_empty())
            .map(|hint| vec![ScreenLine::text(hint, Tone::Accent)])
            .unwrap_or_default();
        footer.extend(shortcut_rows(
            &hints.compact_navigation,
            content,
            Tone::Quiet,
        ));
        footer.push(ScreenLine::plain(format!("Status: {}", data.message)));
    }
    let pinned_backend =
        ui.section == Section::Terminals && height >= 14 && config.sessions.len() + 12 >= height;
    if pinned_backend {
        if let Some(session) = config.sessions.get(ui.selected) {
            let mut context = vec![
                ScreenLine::text(
                    format!("Selected: {} ({})", session.label, session.backend.as_str()),
                    Tone::Title,
                ),
                ScreenLine::plain(format!(
                    "Terminal-server startup: {} (on Gateway start)",
                    if config.autostart_backends.contains(&session.id) {
                        "On"
                    } else {
                        "Off"
                    }
                )),
                ScreenLine::text(
                    format!("Endpoint: {}", backend_endpoint(session)),
                    Tone::Quiet,
                ),
            ];
            context.extend(footer);
            footer = context;
        }
    }
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
            if data.changed {
                rows.push(ScreenLine::text(
                    "Settings changed: restart Gateway to apply.",
                    Tone::Warning,
                ));
            }
            panel(&mut body, "Runtime", rows, content, framed);
            let mut rows = wrapped_rows("Address", &config.public_url, inner);
            rows.push(ScreenLine::plain(format!("Listener: {}", config.listen)));
            rows.push(ScreenLine::plain(format!(
                "Devices: {} paired",
                data.devices.len()
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
                        "{}{}: {}",
                        session.id,
                        if index == 0 { " [default]" } else { "" },
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
            let mut rows = config
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    ScreenLine::text(
                        format!(
                            "{} {}{}  {}",
                            if index == ui.selected { ">" } else { " " },
                            session.id,
                            if index == 0 { " [default]" } else { "" },
                            session.label
                        ),
                        if index == ui.selected {
                            Tone::Selected
                        } else {
                            Tone::Normal
                        },
                    )
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(ScreenLine::plain(
                    "No terminal backends. Enter Add tmux or h Add Herdr.",
                ));
            }
            panel(
                &mut body,
                "Terminal backends - select with arrows",
                rows,
                content,
                framed,
            );
            if let Some(session) = config.sessions.get(ui.selected).filter(|_| !pinned_backend) {
                let mut details = vec![
                    ScreenLine::plain(format!(
                        "Selected: {} ({})",
                        session.label,
                        session.backend.as_str()
                    )),
                    ScreenLine::plain(format!(
                        "Terminal-server startup: {} (on Gateway start)",
                        if config.autostart_backends.contains(&session.id) {
                            "On"
                        } else {
                            "Off"
                        }
                    )),
                ];
                details.extend(wrapped_rows("Endpoint", &backend_endpoint(session), inner));
                panel(&mut body, "Selected backend", details, content, framed);
            }
            body.push(ScreenLine::plain(
                "Enter makes the selected backend the default for the App.",
            ));
            body.push(ScreenLine::text(
                "Removal never closes terminals. Restart Gateway after changes.",
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
                            format!(
                                "Waiting for the phone… {} is asking for this code.",
                                pending.device_name
                            ),
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
                let mut rows = data
                    .devices
                    .iter()
                    .rev()
                    .enumerate()
                    .map(|(index, device)| {
                        ScreenLine::text(
                            format!(
                                "{} {}  (paired {})",
                                if index == ui.selected { ">" } else { " " },
                                device.name,
                                relative_since(device.paired_unix_ms)
                            ),
                            if index == ui.selected {
                                Tone::Selected
                            } else {
                                Tone::Normal
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    rows.push(ScreenLine::plain("No devices paired yet."));
                    rows.push(ScreenLine::text(
                        "Press Enter to pair your first device with Muqun.",
                        Tone::Accent,
                    ));
                }
                panel(&mut body, "Paired devices", rows, content, framed);
                if let Some(pending) = data.pending {
                    body.push(ScreenLine::text(
                        format!(
                            "{} is waiting for its pairing code: press p to show it.",
                            pending.device_name
                        ),
                        Tone::Warning,
                    ));
                }
                if !data.devices.is_empty() {
                    body.push(ScreenLine::plain(
                        "Enter reviews revocation; the selected device will lose access.",
                    ));
                }
            }
        }
        Section::Settings => {
            ui.selected = ui.selected.min(3);
            let settings = [
                format!("Gateway login autostart: {}", data.autostart),
                format!("Gateway address: {}", config.public_url),
                format!(
                    "Transport encryption: {}",
                    config.transport_encryption.as_str()
                ),
                String::from("Stable Gateway updates: Enter checks; i installs with confirmation"),
            ];
            panel(
                &mut body,
                "Gateway settings - Enter changes selected",
                settings
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        ScreenLine::text(
                            format!("{} {text}", if index == ui.selected { ">" } else { " " }),
                            if index == ui.selected {
                                Tone::Selected
                            } else {
                                Tone::Normal
                            },
                        )
                    })
                    .collect(),
                content,
                framed,
            );
            let mut rows = vec![
                ScreenLine::plain("Gateway login autostart starts Gateway when you log in."),
                ScreenLine::plain("Terminal-server startup is separate: 2 Terminals, b."),
                ScreenLine::plain("That starts opted-in terminal servers on Gateway start."),
            ];
            if config.transport_encryption == TransportEncryptionMode::Disabled {
                rows.push(ScreenLine::text(
                    "Warning: token-only mode. A leaked token can call the API.",
                    Tone::Warning,
                ));
            }
            rows.push(ScreenLine::plain(
                "Address/encryption edits need a Gateway restart.",
            ));
            rows.push(ScreenLine::text(
                "Encryption edits affect new pairings, not existing devices.",
                Tone::Quiet,
            ));
            rows.push(ScreenLine::plain(
                "Updates are manual only; never background-installed or downgraded.",
            ));
            panel(&mut body, "What these settings do", rows, content, framed);
        }
    }
    let selected_line = body.iter().position(ScreenLine::selected);
    // Pin only complete QR rows, never the scrollable address fallback.
    if body.iter().any(|line| line.qr.is_some()) {
        ui.scroll = 0;
    }
    ui.scroll = viewport_start(ui.scroll, selected_line, body.len(), body_height);
    let mut lines = header;
    lines.extend(body.into_iter().skip(ui.scroll).take(body_height));
    while lines.len() + footer.len() < height {
        lines.push(ScreenLine::plain(""));
    }
    lines.extend(footer);
    lines.truncate(height);
    for line in &mut lines {
        if line.qr.is_none() {
            let mut budget = content;
            for (text, _) in &mut line.spans {
                *text = clip_cells(text, budget).to_owned();
                budget = budget.saturating_sub(display_width(text));
            }
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
        lines.push(String::from(
            "Arrows/Tab Choose | Enter Confirm selection | Esc Cancel",
        ));
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
        rows.extend(wrapped_rows("", line, inner));
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
        install_id: value
            .get("install_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
        clip_cells, manage_lines, primary_action, safe_text, viewport_start, Confirmation, KeyCode,
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
    fn footer_hints_name_every_action_and_always_offer_esc_and_q() {
        use super::{footer_hints, FooterContext, PairingStage};
        let base = FooterContext::default();
        let cases: [(Section, FooterContext, &[&str]); 6] = [
            (
                Section::Overview,
                base,
                &[
                    "s Start gateway",
                    "t Stop gateway",
                    "1 Overview · 2 Terminals · 3 Devices · 4 Settings",
                    "Esc or q Close manager",
                ],
            ),
            (
                Section::Terminals,
                FooterContext {
                    no_backends: true,
                    ..base
                },
                &[
                    "m Add tmux backend",
                    "h Add Herdr backend",
                    "Esc Back to Overview",
                    "q Close manager",
                ],
            ),
            (
                Section::Terminals,
                base,
                &[
                    "f Make selected default",
                    "d Remove selected backend",
                    "Esc Back to Overview",
                    "q Close manager",
                ],
            ),
            (
                Section::Devices,
                FooterContext {
                    pairing: Some(PairingStage::Scan),
                    ..base
                },
                &["Esc Back to device list", "q Close manager"],
            ),
            (
                Section::Devices,
                FooterContext {
                    pairing: Some(PairingStage::Done),
                    ..base
                },
                &[
                    "p Pair another device",
                    "Esc Back to device list",
                    "q Close manager",
                ],
            ),
            (
                Section::Settings,
                base,
                &[
                    "Enter Change selected setting",
                    "i Install update",
                    "Esc Back to Overview",
                    "q Close manager",
                ],
            ),
        ];
        for (section, context, expected) in cases {
            let hints = footer_hints(section, &context);
            let footer = format!("{} | {}", hints.actions, hints.navigation);
            for text in expected {
                assert!(
                    footer.contains(text),
                    "{section:?} {context:?}: missing {text:?} in {footer:?}"
                );
            }
            if section == Section::Overview {
                assert!(!footer.contains("Enter"), "Overview offers Enter");
            }
            // No action is listed under two keys.
            let mut described: Vec<&str> = footer
                .split(" | ")
                .filter_map(|hint| hint.split_once(' ').map(|(_, what)| what))
                .collect();
            described.sort_unstable();
            let before = described.len();
            described.dedup();
            assert_eq!(
                described.len(),
                before,
                "{section:?}: an action is listed twice"
            );
            for row in [&hints.navigation, &hints.compact_navigation] {
                assert!(row.contains("Esc"), "{section:?}: Esc missing in {row:?}");
                assert!(
                    row.contains("q Close manager"),
                    "{section:?}: q missing in {row:?}"
                );
                assert!(row.contains("Tab"), "{section:?}: Tab missing in {row:?}");
                assert_eq!(
                    row.contains("1 Overview · 2 Terminals · 3 Devices · 4 Settings"),
                    section == Section::Overview,
                    "{section:?}: views explainer in {row:?}"
                );
            }
            assert!(!hints.compact_navigation.contains("Select"));
        }
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
        let context = super::FooterContext::default();
        assert!(!super::footer_hints(Section::Terminals, &context)
            .actions
            .contains("Gateway login autostart"));
        assert!(super::footer_hints(Section::Settings, &context)
            .actions
            .contains("Gateway login autostart"));
        ui.navigate(KeyCode::Tab, 50);
        assert_eq!(ui.selected, 0);
        assert_eq!(ui.scroll, 0);
    }

    #[test]
    fn manager_enter_actions_and_confirmation_cancel_are_explicit() {
        // Enter never starts or stops the gateway from the landing view.
        assert_eq!(primary_action(Section::Overview, 0, true, false), None);
        assert_eq!(
            primary_action(Section::Terminals, 3, false, false),
            Some('f')
        );
        assert_eq!(
            primary_action(Section::Terminals, 0, true, false),
            Some('m')
        );
        assert_eq!(primary_action(Section::Devices, 0, true, false), Some('p'));
        assert_eq!(primary_action(Section::Devices, 1, false, false), Some('x'));
        assert_eq!(primary_action(Section::Devices, 0, true, true), Some('r'));
        for (index, action) in ['g', 'u', 'e'].iter().enumerate() {
            assert_eq!(
                primary_action(Section::Settings, index, true, false),
                Some(*action)
            );
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
                scroll: 0,
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
                            .any(|line| line.selected() && line.render(100).contains("> qa-39")));
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
            scroll: 0,
        };
        let lines = manage_lines(&data(&config), &mut ui, 100, 30);
        let selected = lines
            .iter()
            .find(|line| line.selected() && line.render(100).contains("qa-01"))
            .unwrap()
            .render(100);
        assert!(selected.contains("> qa-01"));
        assert!(!selected.contains("[default]"));
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("qa-00 [default]")));
        let mut empty_config = layout_config();
        empty_config.sessions.clear();
        let lines = manage_lines(&data(&empty_config), &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter Add tmux or h Add Herdr")));
        ui.section = Section::Devices;
        let lines = manage_lines(&data(&empty_config), &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter to pair your first device")));
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("p Pair first device")));
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
