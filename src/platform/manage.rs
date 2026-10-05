//! Interactive terminal management UI: setup, status, and device/backend actions.

use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::time::Duration;

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

use super::{lifecycle, service};
use crate::authority::{hash_token, DeviceRecord, PendingPairing};
use crate::backend::BackendKind;
use crate::{
    auto_public_url, backend_endpoint, backend_program_state, config_changed_since_start,
    config_dir, configured_port, ensure_pairing_transport_key, load_config, make_backend_default,
    now_unix_ms, read_devices, read_pairing_file, revoke_device_by_id, start_background_inner,
    stop_background_inner, upsert_backend_session, validate_session_id, write_config,
    write_secret_file, SessionConfig, TransportEncryptionMode, CONFIG_FILE, DEFAULT_PORT,
    MANAGE_REFRESH_INTERVAL, PAIRING_FILE,
};

pub(crate) fn status() -> anyhow::Result<()> {
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
            backend_program_state(&session)
        );
    }
    println!("gateway: {}", lifecycle::summary()?);
    println!("gateway autostart: {:?}", service::state()?);
    Ok(())
}

pub(crate) fn manage() -> anyhow::Result<()> {
    ensure_pairing_transport_key()?;
    let _terminal = TerminalModeGuard::enter()?;
    let mut message = auto_upgrade_local_public_url()?.unwrap_or_else(|| String::from("ready"));
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
    // False by default so a finished pairing lands on the device list; `p` flips
    // it on to add another device.
    let mut show_qr = false;
    let mut ui = ManageView::default();
    print_manage_screen(
        &message,
        pending_pairing.as_ref(),
        &devices,
        show_qr,
        &mut ui,
    )?;

    loop {
        if !poll_event(MANAGE_REFRESH_INTERVAL)? {
            let next_pending_pairing = fetch_pending_pairing().ok().flatten();
            // A refresh that cannot read the file keeps showing the last list
            // it could read, rather than blanking the screen mid-write.
            let Ok(next_devices) = read_devices() else {
                continue;
            };
            if next_pending_pairing != pending_pairing || next_devices != devices {
                // A newly paired device flips off the QR so the screen settles on
                // the device list instead of showing a fresh code.
                if next_devices.len() > devices.len() {
                    show_qr = false;
                    message = String::from("device paired");
                }
                pending_pairing = next_pending_pairing;
                devices = next_devices;
            }
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                show_qr,
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
                    show_qr,
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
            Section::Devices if !show_qr && pending_pairing.is_none() => devices.len(),
            Section::Settings => 3,
            _ => 0,
        };
        if event.code == KeyCode::Esc {
            if ui.section == Section::Devices && show_qr {
                show_qr = false;
                message = String::from("Pairing view closed; existing devices are unchanged.");
            } else if ui.section != Section::Overview {
                ui = ManageView::default();
            } else {
                break;
            }
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                show_qr,
                &mut ui,
            )?;
            continue;
        }
        if ui.navigate(event.code, item_count) {
            print_manage_screen(
                &message,
                pending_pairing.as_ref(),
                &devices,
                show_qr,
                &mut ui,
            )?;
            continue;
        }
        let input = match event.code {
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Enter => primary_action(
                ui.section,
                ui.selected,
                item_count == 0,
                show_qr,
                pending_pairing.is_some(),
                lifecycle::running_pid().is_ok_and(|pid| pid.is_some()),
            )
            .map(|ch| ch.to_string())
            .unwrap_or_default(),
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
                    show_qr = true;
                    ui.scroll = 0;
                    message = String::from("scan to pair another device");
                }
                (Section::Devices, "x") if !show_qr && pending_pairing.is_none() => {
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
                                show_qr = true;
                            }
                            Ok(false) => message = String::from("device was already revoked"),
                            Err(error) => message = first_line(&format!("{error:#}")),
                        }
                    } else {
                        message = String::from("revoke cancelled");
                    }
                }
                (Section::Devices, "r") => {
                    show_qr = false;
                    ui.scroll = 0;
                    if pending_pairing.is_some() {
                        ui.section = Section::Overview;
                    }
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
                (Section::Settings, "g") => {
                    let enabled = service::state()? != service::ServiceState::NotInstalled;
                    if confirm_gateway_autostart(!enabled)? {
                        crate::run_service_command(if enabled {
                            crate::ServiceCommand::Uninstall
                        } else {
                            crate::ServiceCommand::Install
                        })?;
                        message =
                            format!("gateway autostart {}", if enabled { "off" } else { "on" });
                    } else {
                        message = String::from("gateway autostart unchanged");
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
            show_qr,
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
    fn hints(self) -> &'static str {
        match self {
            Self::Overview => "s Start gateway | t Stop gateway | r Restart gateway",
            Self::Terminals => "Enter Make selected default | b Toggle terminal-server startup | d Remove selected backend | h Add Herdr | m Add tmux",
            Self::Devices => "Enter Revoke selected device | p Pair new device",
            Self::Settings => "Enter Change selected setting | g Gateway login autostart | u Edit address | a Detect address | e Toggle encryption",
        }
    }
}

fn primary_action(
    section: Section,
    selected: usize,
    empty_items: bool,
    show_qr: bool,
    pending: bool,
    running: bool,
) -> Option<char> {
    match section {
        Section::Overview => Some(if running { 't' } else { 's' }),
        Section::Terminals => Some(if empty_items { 'm' } else { 'f' }),
        Section::Devices if show_qr || pending => Some('r'),
        Section::Devices => Some(if empty_items { 'p' } else { 'x' }),
        Section::Settings => ['g', 'u', 'e'].get(selected).copied(),
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
    show_qr: bool,
    ui: &mut ManageView,
) -> anyhow::Result<()> {
    let config = load_config(None)?;
    let (width, height) = terminal_size().unwrap_or((100, 30));
    let owner = lifecycle::running_pid();
    let running = matches!(owner, Ok(Some(_)));
    let runtime = match owner {
        Ok(Some(pid)) => format!("Running - process {pid}"),
        Ok(None) => String::from("Stopped - press Enter to start"),
        Err(error) => format!("Unavailable: {}", first_line(&error.to_string())),
    };
    let autostart = match service::state() {
        Ok(service::ServiceState::Installed) => "On - user service registered",
        Ok(service::ServiceState::FileOnly) => "Incomplete - service file only",
        Ok(service::ServiceState::NotInstalled) => "Off - start Gateway manually",
        Err(_) => "Unavailable - cannot read service registration",
    };
    let backend_states = config
        .sessions
        .iter()
        .map(|session| match session.backend {
            BackendKind::Tmux if backend_program_state(session).contains("NOT FOUND") => {
                String::from("tmux executable not found")
            }
            BackendKind::Tmux => String::from("tmux executable available"),
            BackendKind::Herdr => String::from("Herdr socket configured"),
        })
        .collect::<Vec<_>>();
    let qr_result =
        if ui.section == Section::Devices && show_qr && pending_pairing.is_none() {
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
    };
    let lines = manage_lines(&data, show_qr, ui, width, height);
    write_viewport(&lines, width, height)
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
        if matches!(self, Self::Title | Self::Selected) {
            style.attributes.set(Attribute::Bold);
        }
        if self == Self::Selected {
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

fn shortcut_rows(hints: &str, width: usize) -> Vec<ScreenLine> {
    let mut rows = Vec::new();
    let mut line = String::new();
    for hint in hints.split(" | ") {
        if !line.is_empty() && display_width(&line) + 3 + display_width(hint) > width {
            rows.push(ScreenLine::text(&line, Tone::Accent));
            line.clear();
        }
        if !line.is_empty() {
            line.push_str(" | ");
        }
        line.push_str(hint);
    }
    if !line.is_empty() {
        rows.push(ScreenLine::text(line, Tone::Accent));
    }
    rows
}

fn manage_lines(
    data: &ManageData<'_>,
    show_qr: bool,
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
        Section::Settings => 2,
        Section::Overview => 0,
    });
    let mut header = vec![ScreenLine {
        spans: vec![
            ("Muqun Gateway".into(), Tone::Title),
            (format!("  v{}", env!("CARGO_PKG_VERSION")), Tone::Quiet),
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
    let hints = match ui.section {
        Section::Terminals if config.sessions.is_empty() => {
            "Enter Add tmux backend | h Add Herdr backend"
        }
        Section::Overview => {
            if data.running {
                "Enter Stop gateway | s Start gateway | r Restart gateway"
            } else {
                "Enter Start gateway | t Stop gateway | r Restart gateway"
            }
        }
        Section::Devices if data.pending.is_some() => {
            "Enter Back to Overview | Esc Back to Overview"
        }
        Section::Devices if show_qr => "Enter Back to device list | Esc Cancel pairing view",
        Section::Devices if data.devices.is_empty() => {
            "Enter Pair first device | p Pair new device"
        }
        _ => ui.section.hints(),
    };
    let mut footer = shortcut_rows(hints, content);
    footer.push(ScreenLine::text(
        if ui.section == Section::Overview {
            "Tab Views | Arrows Select | PageUp/Down Scroll | Esc/q Close"
        } else {
            "Tab Views | Arrows Select | Esc Back | q Close"
        },
        Tone::Quiet,
    ));
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
        footer.truncate(1);
        footer[0] = ScreenLine::text(hints.split(" | ").next().unwrap_or(hints), Tone::Accent);
        footer.push(ScreenLine::text(
            "Tab Views | Esc Back | q Close",
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
            if let Some(pending) = data.pending {
                let mut rows = vec![
                    ScreenLine::plain(format!("Device: {}", pending.device_name)),
                    ScreenLine::text(format!("Pairing code: {}", pending.code), Tone::Title),
                ];
                rows.extend(wrapped_rows("Address", &config.public_url, inner));
                rows.push(ScreenLine::plain(
                    "Enter this code in Muqun to finish pairing.",
                ));
                panel(
                    &mut body,
                    "Finish pairing in the App",
                    rows,
                    content,
                    framed,
                );
            } else if show_qr {
                body.push(ScreenLine::text("Pair a device", Tone::Title));
                if data.qr.is_none() {
                    body.push(ScreenLine::text(
                        "Pairing unavailable. See the status message below.",
                        Tone::Error,
                    ));
                } else {
                    if let Some(qr) = data.qr.filter(|qr| {
                        qr.lines().count() + 2 <= body_height
                            && qr.lines().all(|row| display_width(row) <= content)
                    }) {
                        body.push(ScreenLine::plain(
                            "Scan with Muqun; code appears after scan.",
                        ));
                        body.extend(qr.lines().map(|row| ScreenLine {
                            spans: Vec::new(),
                            qr: Some(row.to_owned()),
                        }));
                    } else {
                        body.push(ScreenLine::text(
                            "Resize for the full QR; Down for URL.",
                            Tone::Warning,
                        ));
                        body.extend(wrapped_rows("Address", &config.public_url, inner));
                        body.push(ScreenLine::plain(
                            "In Muqun: add Gateway, enter this address, then the code.",
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
                if !data.devices.is_empty() {
                    body.push(ScreenLine::plain(
                        "Enter reviews revocation; the selected device will lose access.",
                    ));
                }
            }
        }
        Section::Settings => {
            ui.selected = ui.selected.min(2);
            let settings = [
                format!("Gateway login autostart: {}", data.autostart),
                format!("Gateway address: {}", config.public_url),
                format!(
                    "Transport encryption: {}",
                    config.transport_encryption.as_str()
                ),
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
    let pairing = read_pairing_file()?;
    let config = load_config(None)?;
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let host_port = local_management_addr(listen).to_string();
    let address = local_management_addr(listen);
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let request = format!(
        "GET /api/pair/pending HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        pairing.payload.token
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(&mut stream, 64 * 1024),
        &mut response,
    )?;
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        anyhow::bail!("invalid pending response");
    };
    anyhow::ensure!(
        headers.starts_with("HTTP/1.1 200 ") || headers.starts_with("HTTP/1.0 200 "),
        "gateway refused local management request"
    );
    let value: Value = serde_json::from_str(body)?;
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
    let pairing = read_pairing_file()?;
    let config = load_config(None)?;
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let address = local_management_addr(listen);
    let mut stream = match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) =>
        {
            // With no running gateway there is no in-memory token list to
            // invalidate, so updating the persisted records is sufficient.
            return revoke_device_by_id(device_id);
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = format!(
        "DELETE /api/pairings/{device_id} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        pairing.payload.token
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(&mut stream, 64 * 1024),
        &mut response,
    )?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.contains(" 200 ") {
        return Ok(true);
    }
    if status_line.contains(" 404 ") {
        return Ok(false);
    }
    anyhow::bail!("gateway refused device revocation: {status_line}")
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
    use super::{
        clip_cells, manage_lines, primary_action, safe_text, viewport_start, Confirmation, KeyCode,
        ManageData, ManageView, ScreenLine, Section, Tone,
    };
    use crate::*;

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
        assert!(!Section::Terminals
            .hints()
            .contains("Gateway login autostart"));
        assert!(Section::Settings
            .hints()
            .contains("Gateway login autostart"));
        ui.navigate(KeyCode::Tab, 50);
        assert_eq!(ui.selected, 0);
        assert_eq!(ui.scroll, 0);
    }

    #[test]
    fn manager_enter_actions_and_confirmation_cancel_are_explicit() {
        assert_eq!(
            primary_action(Section::Overview, 0, true, false, false, false),
            Some('s')
        );
        assert_eq!(
            primary_action(Section::Overview, 0, true, false, false, true),
            Some('t')
        );
        assert_eq!(
            primary_action(Section::Terminals, 3, false, false, false, false),
            Some('f')
        );
        assert_eq!(
            primary_action(Section::Terminals, 0, true, false, false, false),
            Some('m')
        );
        assert_eq!(
            primary_action(Section::Devices, 0, true, false, false, false),
            Some('p')
        );
        assert_eq!(
            primary_action(Section::Devices, 1, false, false, false, false),
            Some('x')
        );
        assert_eq!(
            primary_action(Section::Devices, 0, true, true, false, false),
            Some('r')
        );
        for (index, action) in ['g', 'u', 'e'].iter().enumerate() {
            assert_eq!(
                primary_action(Section::Settings, index, true, false, false, false),
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
        };
        for section in Section::ALL {
            let mut ui = ManageView {
                section,
                selected: if section == Section::Terminals { 39 } else { 0 },
                scroll: 0,
            };
            for width in [0_u16, 1, 2, 8, 24, 40, 60, 80, 100, 200] {
                for height in [0_u16, 1, 5, 8, 18, 30] {
                    let lines = manage_lines(&data, false, &mut ui, width, height);
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
        };
        let mut ui = ManageView {
            section: Section::Terminals,
            selected: 1,
            scroll: 0,
        };
        let lines = manage_lines(&data(&config), false, &mut ui, 100, 30);
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
        let lines = manage_lines(&data(&empty_config), false, &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter Add tmux or h Add Herdr")));
        ui.section = Section::Devices;
        let lines = manage_lines(&data(&empty_config), false, &mut ui, 100, 30);
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter to pair your first device")));
        assert!(lines
            .iter()
            .any(|line| line.render(100).contains("Enter Pair first device")));
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
        };
        let mut ui = ManageView {
            section: Section::Devices,
            selected: 0,
            scroll: 99,
        };
        let lines = manage_lines(&data, true, &mut ui, 100, 50);
        assert_eq!(
            lines.iter().filter(|line| line.qr.is_some()).count(),
            qr.lines().count()
        );
        assert_eq!(ui.scroll, 0);
        let lines = manage_lines(&data, true, &mut ui, 40, 8);
        assert!(lines.iter().all(|line| line.qr.is_none()));
        assert!(lines
            .iter()
            .any(|line| line.render(39).contains("Resize for the full QR")));
        assert!(lines
            .iter()
            .any(|line| line.render(39).contains("Down for URL")));
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
            let lines = manage_lines(&data, true, &mut ui, 40, 8);
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
