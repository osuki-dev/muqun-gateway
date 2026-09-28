//! Interactive terminal management UI: setup, status, and device/backend actions.

use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use crossterm::cursor::MoveTo;
use crossterm::event::{
    poll as poll_event, read as read_event, Event as TerminalEvent, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};
use serde_json::Value;

use crate::authority::{hash_token, DeviceRecord, PendingPairing};
use crate::backend::BackendKind;
use crate::{
    auto_public_url, backend_endpoint, backend_program_state, config_changed_since_start,
    config_dir, configured_port, ensure_pairing_transport_key, load_config, make_backend_default,
    now_unix_ms, process_running, read_devices, read_pairing_file, read_pid, revoke_device_by_id,
    start_background_inner, stop_background_inner, upsert_backend_session, validate_session_id,
    write_config, write_secret_file, SessionConfig, TransportEncryptionMode, CONFIG_FILE,
    DEFAULT_PORT, MANAGE_REFRESH_INTERVAL, PAIRING_FILE,
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
    match read_pid()? {
        Some(pid) if process_running(pid) => println!("gateway: running pid {pid}"),
        Some(pid) => println!("gateway: stale pid {pid}"),
        None => println!("gateway: stopped"),
    }
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
    print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;

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
                print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
            }
            continue;
        }

        let event = read_event()?;
        let TerminalEvent::Key(event) = event else {
            if matches!(event, TerminalEvent::Resize(_, _)) {
                print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
            }
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        let input = match event.code {
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Enter => String::new(),
            KeyCode::Esc => String::from("q"),
            _ => String::new(),
        };
        match input.as_str() {
            "s" | "start" => {
                // A refusal -- another gateway already owns this state
                // directory -- belongs on the status line. Propagating it
                // would drop the operator out of the UI on a keypress.
                message = match start_background_inner(false) {
                    Ok(()) => String::from("start requested"),
                    Err(error) => first_line(&error.to_string()),
                };
            }
            "t" | "stop" => {
                stop_background_inner(false)?;
                message = String::from("stop requested");
            }
            "p" | "pair" => {
                show_qr = true;
                message = String::from("scan to pair another device");
            }
            "x" | "revoke" => {
                if devices.is_empty() {
                    message = String::from("no paired devices to revoke");
                } else if let Some(device) = prompt_revoke_device(&devices)? {
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
            "r" | "refresh" | "" => {
                show_qr = false;
                message = String::from("refreshed");
            }
            "u" | "url" => match prompt_public_url()? {
                Some(url) => {
                    let listen = listen_for_explicit_public_url(&url, configured_port());
                    update_public_url(&url, &listen)?;
                    message = format!("url updated: {}", truncate(&url, 36));
                }
                None => {
                    message = String::from("url unchanged");
                }
            },
            "a" | "auto" => {
                let port = configured_port();
                let selection = auto_public_url(port);
                update_public_url(&selection.url, &format!("{}:{port}", selection.listen_host))?;
                message = format!("auto url: {}", truncate(&selection.url, 36));
            }
            "e" | "encryption" => {
                message = toggle_transport_encryption()?;
                show_qr = true;
            }
            "h" | "herdr" => {
                message = enable_managed_backend(BackendKind::Herdr)?;
            }
            "m" | "tmux" => {
                message = enable_managed_backend(BackendKind::Tmux)?;
            }
            "d" | "backend" => {
                message = match prompt_remove_backend()? {
                    Some(id) => remove_managed_backend(&id)?,
                    None => String::from("backend unchanged"),
                };
            }
            "f" | "default" => {
                message = match prompt_default_backend()? {
                    Some(id) => set_managed_default_backend(&id)?,
                    None => String::from("default backend unchanged"),
                };
            }
            "q" | "quit" => break,
            other => message = format!("unknown command: {other}"),
        }

        pending_pairing = fetch_pending_pairing().ok().flatten();
        match read_devices() {
            Ok(next_devices) => devices = next_devices,
            Err(error) => message = format!("could not read paired devices: {error}"),
        }
        print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
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

pub(crate) fn prompt_default_backend() -> anyhow::Result<Option<String>> {
    let config = load_config(None)?;
    prompt_backend_picker(&config.sessions, "Choose the default terminal backend")
}

pub(crate) fn prompt_remove_backend() -> anyhow::Result<Option<String>> {
    let config = load_config(None)?;
    if config.sessions.len() <= 1 {
        return Ok(None);
    }
    let Some(id) = prompt_backend_picker(&config.sessions, "Remove a terminal backend")? else {
        return Ok(None);
    };
    let session = config
        .sessions
        .iter()
        .find(|session| session.id == id)
        .context("selected backend disappeared")?;
    Ok(confirm_remove_backend(session)?.then_some(id))
}

pub(crate) fn prompt_backend_picker(
    sessions: &[SessionConfig],
    title: &str,
) -> anyhow::Result<Option<String>> {
    if sessions.is_empty() {
        return Ok(None);
    }
    let mut selected = 0_usize;
    loop {
        render_backend_picker(title, sessions, selected)?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(sessions.len() - 1),
            KeyCode::Enter => return Ok(Some(sessions[selected].id.clone())),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn render_backend_picker(
    title: &str,
    sessions: &[SessionConfig],
    selected: usize,
) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let mut lines = vec![
        title.to_owned(),
        String::new(),
        String::from("Up/Down or j/k selects | Enter continues | Esc cancels"),
        String::new(),
    ];
    for (index, session) in sessions.iter().enumerate() {
        lines.push(format!(
            "{} {}   {}   {}",
            if index == selected { ">" } else { " " },
            session.id,
            session.backend.as_str(),
            truncate(&session.label, 30)
        ));
    }
    write_centered_panel(&lines)
}

pub(crate) fn confirm_remove_backend(session: &SessionConfig) -> anyhow::Result<bool> {
    loop {
        execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        write_centered_panel(&[
            String::from("Remove terminal backend?"),
            String::new(),
            format!("{} ({})", session.label, session.backend.as_str()),
            String::from("Existing terminal sessions are not deleted."),
            String::from("The gateway must be restarted after this change."),
            String::new(),
            String::from("y remove | n or Esc cancel"),
        ])?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

pub(crate) fn prompt_public_url() -> anyhow::Result<Option<String>> {
    let current = load_config(None)
        .map(|config| config.public_url)
        .unwrap_or_else(|_| auto_public_url(configured_port()).url);
    let mut value = current;
    loop {
        render_public_url_prompt(&value)?;
        if let TerminalEvent::Key(event) = read_event()? {
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter => {
                    if let Ok(url) = validate_public_url(&value) {
                        return Ok(Some(url));
                    }
                    value = String::from("http://");
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

pub(crate) fn render_public_url_prompt(value: &str) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let lines = vec![
        String::from("Gateway URL"),
        String::from(""),
        String::from("Edit the URL encoded into the pairing QR."),
        String::from("Use a Tailscale HTTPS name if Tailscale Serve is configured."),
        format!("Otherwise use http://<tailscale-ip>:{DEFAULT_PORT}."),
        String::from(""),
        format!("url: {value}"),
        String::from(""),
        String::from("Enter saves | Esc cancels | Backspace deletes"),
    ];
    write_centered_panel(&lines)
}

pub(crate) fn prompt_revoke_device(
    devices: &[DeviceRecord],
) -> anyhow::Result<Option<DeviceRecord>> {
    let choices = devices.iter().rev().cloned().collect::<Vec<_>>();
    if choices.is_empty() {
        return Ok(None);
    }
    let mut selected = 0_usize;

    loop {
        render_revoke_device_picker(&choices, selected)?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            KeyCode::Enter => {
                let device = &choices[selected];
                if confirm_revoke_device(device)? {
                    return Ok(Some(device.clone()));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn render_revoke_device_picker(
    devices: &[DeviceRecord],
    selected: usize,
) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let mut lines = vec![
        String::from("Revoke a paired device"),
        String::from(""),
        String::from("Up/Down or j/k selects | Enter continues | Esc cancels"),
        String::from(""),
    ];
    for (index, device) in devices.iter().enumerate() {
        lines.push(format!(
            "{} {}   paired {}",
            if index == selected { ">" } else { " " },
            truncate(&device.name, 42),
            relative_since(device.paired_unix_ms)
        ));
    }
    write_centered_panel(&lines)
}

pub(crate) fn confirm_revoke_device(device: &DeviceRecord) -> anyhow::Result<bool> {
    loop {
        execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        let lines = vec![
            String::from("Revoke device?"),
            String::from(""),
            truncate(&device.name, 52),
            String::from("Its access token will stop working immediately."),
            String::from(""),
            String::from("y revoke | n or Esc cancel"),
        ];
        write_centered_panel(&lines)?;
        let TerminalEvent::Key(event) = read_event()? else {
            continue;
        };
        if event.kind != KeyEventKind::Press {
            continue;
        }
        match event.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
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
         the address again."
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
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub(crate) fn print_manage_screen(
    message: &str,
    pending_pairing: Option<&PendingPairing>,
    devices: &[DeviceRecord],
    show_qr: bool,
) -> anyhow::Result<()> {
    execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    let config = load_config(None).ok();
    let server = config
        .as_ref()
        .map(|config| truncate(&config.label, 24))
        .unwrap_or_else(|| "not configured".into());
    let url = config
        .as_ref()
        .map(|config| config.public_url.clone())
        .unwrap_or_else(|| "run setup first".into());
    let pid_on_disk = read_pid()?;
    let running_pid = pid_on_disk.filter(|&pid| process_running(pid));
    let status = match (running_pid, pid_on_disk) {
        (Some(pid), _) => format!("running ({pid})"),
        (None, Some(_)) => String::from("stale pid"),
        (None, None) => String::from("stopped"),
    };
    // `run` loads config.json once at startup into a plain value and never
    // re-reads it (see `AppState`); every setting below this line -- and the
    // encryption line, backend list, default marker, and URL further down --
    // comes straight from disk, not from what the running process actually
    // enforces. If the file was rewritten after the process started, those
    // fields are pending, not live, and the panel needs to say so instead of
    // presenting them as current.
    let restart_pending = running_pid.is_some() && config_changed_since_start();
    let pending_note = if restart_pending {
        " (pending restart -- [t] stop, [s] start to apply)"
    } else {
        ""
    };
    // Same signal, shorter: the QR panel is a narrow two-column layout where
    // the long-form hint would blow the column width, and [s]/[t] are already
    // listed as controls right there.
    let pending_note_short = if restart_pending {
        " (pending restart)"
    } else {
        ""
    };

    let mut lines = vec![
        String::from("Muqun Terminal Gateway"),
        String::from(""),
        String::from("keys   : [s] start  [t] stop  [p] pair  [x] revoke"),
        String::from("         [m] tmux  [h] Herdr  [f] default backend"),
        String::from("         [d] remove [u] url   [a] auto  [e] encryption"),
        String::from("         [r] refresh [q] close"),
        format!("server : {server}"),
        format!("status : {status}"),
    ];
    push_wrapped_field(&mut lines, "url    ", &format!("{url}{pending_note}"), 64);
    push_wrapped_field(&mut lines, "message", message, 64);
    lines.push(String::new());
    if let Some(config) = &config {
        lines.push(format!(
            "encryption: {}{}{}",
            config.transport_encryption.as_str(),
            if config.transport_encryption == TransportEncryptionMode::Disabled {
                " (token-only; unsafe on public HTTP)"
            } else {
                ""
            },
            pending_note
        ));
        lines.push(String::new());
        lines.push(format!(
            "Terminal backends ({}){pending_note}",
            config.sessions.len()
        ));
        for (index, session) in config.sessions.iter().enumerate() {
            let endpoint = backend_endpoint(session);
            lines.push(format!(
                "{} {}  {}  {}",
                if index == 0 { "*" } else { " " },
                session.id,
                session.backend.as_str(),
                truncate(&endpoint, 38)
            ));
        }
        lines.push(String::new());
    }

    // A device mid-pairing takes priority: show its name + the code to enter.
    // `url` is already on screen above this block, so the message can point
    // at it rather than repeat it -- there is no QR involved in this path.
    if let Some(pending) = pending_pairing {
        lines.extend([
            String::from("Pairing request"),
            format!("device : {}", truncate(&pending.device_name, 48)),
            format!("code   : {}", pending.code),
            String::from(""),
            String::from("In Muqun, enter the address above and this code."),
        ]);
        write_centered_panel(&lines)?;
        return Ok(());
    }

    // Once at least one device is paired, the QR is not the default view -- a
    // finished pairing should land on the device list, not another QR. `p` (or a
    // fresh install with nothing paired yet) brings the QR back to add another.
    let show_qr = show_qr || devices.is_empty();

    if !show_qr {
        lines.push(format!("Paired devices ({})", devices.len()));
        lines.push(String::from(""));
        for device in devices.iter().rev() {
            lines.push(format!(
                "  {}   paired {}",
                truncate(&device.name, 40),
                relative_since(device.paired_unix_ms)
            ));
        }
        lines.push(String::from(""));
        lines.push(String::from(
            "Press p to pair another device, or x to revoke one.",
        ));
        write_centered_panel(&lines)?;
        return Ok(());
    }

    if let (Some(config), Ok(pairing)) = (config.as_ref(), read_pairing_file()) {
        if hash_token(&pairing.payload.token) != config.token_hash {
            lines.push(String::from("Pairing identity is stale. Run setup again."));
            write_centered_panel(&lines)?;
            return Ok(());
        }
        let mut qr_controls = vec![
            String::from("Muqun Gateway"),
            String::from(""),
            String::from("[s] start  [t] stop  [p] pair"),
            String::from("[x] revoke [r] refresh [q] close"),
            String::from("[m] tmux  [h] Herdr  [f] default"),
            String::from("[d] remove [u] URL   [a] auto"),
            format!(
                "[e] encryption: {}{}",
                config.transport_encryption.as_str(),
                pending_note_short
            ),
            String::from(""),
            format!("server: {}", truncate(&server, 26)),
            format!("status: {}", truncate(&status, 25)),
        ];
        push_wrapped_field(
            &mut qr_controls,
            "url",
            &format!("{url}{pending_note_short}"),
            34,
        );
        qr_controls.push(format!("backends (* default):{pending_note_short}"));
        for (index, session) in config.sessions.iter().enumerate() {
            qr_controls.push(format!(
                " {} {} ({})",
                if index == 0 { "*" } else { " " },
                truncate(&session.label, 17),
                session.backend.as_str()
            ));
        }
        push_wrapped_field(&mut qr_controls, "message", message, 34);
        let mut qr_lines = vec![
            String::from("Scan with Muqun"),
            String::from("Code appears after scan"),
            String::from(""),
        ];
        // Config is authoritative for the advertised URL and server id. Older
        // pairing files can retain a stale URL even though their admin token is
        // still valid; rendering from that file made `p` show the wrong server.
        let encoded = pairing_qr_offer(
            &config.public_url,
            &config.server_id,
            (config.transport_encryption == TransportEncryptionMode::Required)
                .then_some(pairing.payload.transport_key.as_str()),
        );
        let code = QrCode::with_error_correction_level(encoded.as_bytes(), EcLevel::L)?;
        let image = render_qr(&code);
        for line in image.lines() {
            qr_lines.push(line.to_string());
        }
        write_two_column_panel(&qr_controls, &qr_lines)?;
        return Ok(());
    } else {
        lines.push(String::from(
            "Gateway pairing is not configured. Run setup first.",
        ));
    }
    write_centered_panel(&lines)?;
    Ok(())
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

pub(crate) fn push_line(output: &mut String, line: impl AsRef<str>) {
    output.push_str(line.as_ref());
    output.push_str("\r\n");
}

pub(crate) fn write_centered_panel(lines: &[String]) -> anyhow::Result<()> {
    let terminal_width = terminal_size()
        .map(|(width, _)| width as usize)
        .unwrap_or(110);
    let content_width = lines
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0)
        .max(56)
        .min(terminal_width.saturating_sub(4));
    let indent = terminal_width.saturating_sub(content_width) / 2;
    // Popup interiors can be a few rows shorter than the child PTY reports.
    // Keep content anchored at the top so a long QR never scrolls the controls
    // out of view on smaller laptop terminals.
    let mut output = String::new();

    for line in lines {
        let line_width = display_width(line);
        let left_padding = if line.contains(':') || line.starts_with("> ") || line.starts_with("  ")
        {
            0
        } else {
            content_width.saturating_sub(line_width) / 2
        };
        push_line(
            &mut output,
            format!("{}{}{}", " ".repeat(indent + left_padding), line, "\x1b[0m"),
        );
    }

    stdout().write_all(output.as_bytes())?;
    stdout().flush()?;
    Ok(())
}

pub(crate) fn write_two_column_panel(left: &[String], right: &[String]) -> anyhow::Result<()> {
    let terminal_width = terminal_size()
        .map(|(width, _)| width as usize)
        .unwrap_or(92);
    let left_width = left
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0);
    let right_width = right
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0);
    let gap = if left_width + right_width + 4 <= terminal_width {
        4
    } else {
        1
    };
    let total_width = left_width + gap + right_width;

    // Extremely narrow terminals cannot preserve QR geometry beside controls.
    // Keep the controls visible first, then render the code below as a fallback.
    if total_width > terminal_width {
        let mut stacked = left.to_vec();
        stacked.push(String::new());
        stacked.extend_from_slice(right);
        return write_centered_panel(&stacked);
    }

    let indent = terminal_width.saturating_sub(total_width) / 2;
    let row_count = left.len().max(right.len());
    let mut output = String::new();
    for row in 0..row_count {
        let left_line = left.get(row).map(String::as_str).unwrap_or("");
        let right_line = right.get(row).map(String::as_str).unwrap_or("");
        let left_padding = left_width.saturating_sub(display_width(left_line));
        push_line(
            &mut output,
            format!(
                "{}{}{}{}{}\x1b[0m",
                " ".repeat(indent),
                left_line,
                " ".repeat(left_padding),
                " ".repeat(gap),
                right_line
            ),
        );
    }
    stdout().write_all(output.as_bytes())?;
    stdout().flush()?;
    Ok(())
}

pub(crate) fn display_width(value: &str) -> usize {
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
            width += 1;
        }
    }
    width
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
    let prefix = format!("{label}: ");
    let continuation = " ".repeat(prefix.chars().count());
    let first_width = width.saturating_sub(prefix.chars().count()).max(1);
    let mut remaining = value.chars().peekable();
    let mut first = true;
    while remaining.peek().is_some() {
        let chunk = remaining.by_ref().take(first_width).collect::<String>();
        lines.push(format!(
            "{}{}",
            if first { &prefix } else { &continuation },
            chunk
        ));
        first = false;
    }
    if first {
        lines.push(prefix);
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
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    let request = format!(
        "GET /api/pair/pending HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        pairing.payload.token
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response)?;
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        anyhow::bail!("invalid pending response");
    };
    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return Ok(None);
    }
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
    std::io::Read::read_to_string(&mut stream, &mut response)?;
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
    use crate::*;

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
