//! Setup, backend configuration, Herdr plugin import, and service lifecycle.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::Context as _;
use serde::Serialize;
use serde_json::Value;

use super::{lifecycle, service, state_lock};
use crate::authority::DeviceRecord;
use crate::backend::BackendKind;
use crate::platform::metadata::transport_protection;
use crate::terminal::factory::{
    backend_endpoint, backend_registry, default_backend_label, default_backend_socket,
};
use crate::{
    agents, backend_startup, config_dir, default_herdr_plugin_config_dir,
    default_herdr_plugin_state_dir, gateway_listener_pids, generate_token, hash_token,
    hostname_label, listen_for_explicit_public_url, load_config, login_env, read_devices_at,
    remove_pid_file, standalone_config_dir, standalone_state_dir, state_dir, stop_pid, transport,
    validate_public_url, write_config, write_secret_file, BackendAutostartMode, BackendCommand,
    Config, PairingFile, PairingPayload, PublicUrlSelection, PushTokenRecord, ServiceCommand,
    SessionConfig, TransportEncryptionMode, CONFIG_FILE, DEFAULT_PORT, DEVICES_FILE,
    HERDR_PLUGIN_IMPORT_MARKER, LOG_FILE, MAX_WORKSPACE_LABEL_CHARS, PAIRING_FILE, PID_FILE,
    PUSH_TOKENS_FILE,
};

/// What `setup --backend` configures when the flag is left off.
///
/// An omitted `--backend` must never flip what an existing install already
/// runs -- that is the whole complaint this default used to earn ("Herdr
/// wins whenever it is installed, regardless of ... what the reader wants").
/// So "no backend named" means "keep doing what this install already does"
/// whenever there is an existing install to keep doing it: the same rule a
/// bare `herdr plugin action invoke *.setup` (no CLI flags available to it)
/// relies on to stay a no-op on every Herdr-plugin update. Only a genuinely
/// fresh install -- nothing configured yet -- falls back to a hard default,
/// and tmux is the honest one: it is the primary backend everywhere else
/// (the app, the website, the store listing), and it needs nothing beyond
/// tmux itself.
pub(crate) fn resolve_setup_backend(
    explicit: Option<BackendKind>,
    existing: Option<&Config>,
) -> BackendKind {
    explicit.unwrap_or_else(|| {
        existing
            .and_then(|config| config.sessions.first())
            .map(|session| session.backend)
            .unwrap_or(BackendKind::Tmux)
    })
}

pub(crate) fn setup(
    public_url: Option<String>,
    port: u16,
    socket_path: Option<String>,
    backend: Option<BackendKind>,
    transport_encryption: Option<TransportEncryptionMode>,
) -> anyhow::Result<()> {
    let config_dir = config_dir()?;
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create config dir {}", config_dir.display()))?;

    // Reuse an existing install's identity so re-running setup (after an update
    // or a retry) refreshes settings without minting a new server id or token --
    // that would orphan every already-paired device. Only a consistent config +
    // pairing pair is trusted; a half-written state falls back to a fresh mint.
    let existing = load_existing_install(
        &config_dir.join(CONFIG_FILE),
        &config_dir.join(PAIRING_FILE),
    );

    let backend = resolve_setup_backend(backend, existing.as_ref().map(|install| &install.config));
    ensure_backend_available(backend)?;

    let (server_id, token, token_hash) = match &existing {
        Some(install) => (
            install.config.server_id.clone(),
            install.pairing.payload.token.clone(),
            install.config.token_hash.clone(),
        ),
        None => {
            let token = generate_token();
            let token_hash = hash_token(&token);
            (uuid::Uuid::new_v4().to_string(), token, token_hash)
        }
    };

    // An explicit --public-url always wins. Otherwise a returning install keeps
    // the URL and listen address it already has (including one set from the
    // manage panel); only a fresh install auto-detects.
    let (public_url, listen, url_source) = match (public_url, &existing) {
        (Some(url), _) => {
            let url = validate_public_url(&url)?;
            let listen = listen_for_explicit_public_url(&url, port);
            (url, listen, String::from("manual --public-url"))
        }
        (None, Some(install)) => (
            install.config.public_url.clone(),
            install.config.listen.clone(),
            String::from("existing config"),
        ),
        (None, None) => {
            let selection = auto_public_url(port);
            (
                selection.url,
                format!("{}:{port}", selection.listen_host),
                selection.source,
            )
        }
    };
    let transport_key = existing
        .as_ref()
        .map(|install| install.pairing.payload.transport_key.clone())
        .filter(|value| transport::decode_key(value).is_ok_and(|key| key.len() == 32))
        .unwrap_or_else(generate_token);
    let mut config = match existing {
        Some(install) => install.config,
        None => Config {
            server_id,
            label: hostname_label(),
            listen: listen.clone(),
            public_url: public_url.clone(),
            token_hash,
            transport_encryption: TransportEncryptionMode::Required,
            dev_unauthenticated: false,
            sessions: Vec::new(),
            autostart_backends: Vec::new(),
            agent_commands: BTreeMap::new(),
            rich_agent_pushes: false,
            opencode: agents::OpencodeConfig::default(),
            deepseek: agents::DeepseekConfig::default(),
            t3: agents::T3Config::default(),
        },
    };
    config.listen = listen;
    config.public_url = public_url.clone();
    if let Some(mode) = transport_encryption {
        config.transport_encryption = mode;
    }
    upsert_backend_session(&mut config, backend, None, None, socket_path)?;

    let path = config_dir.join(CONFIG_FILE);
    write_config(&path, &config)?;

    let payload = PairingPayload {
        kind: "muqun-gateway".into(),
        server_id: config.server_id.clone(),
        label: config.label.clone(),
        url: public_url,
        token,
        transport_key,
    };
    let pairing_path = config_dir.join(PAIRING_FILE);
    write_secret_file(
        &pairing_path,
        &serde_json::to_vec_pretty(&PairingFile {
            payload: payload.clone(),
        })?,
    )?;
    println!("wrote config: {}", path.display());
    println!("wrote pairing file: {}", pairing_path.display());
    println!("public URL: {} ({url_source})", payload.url);
    println!(
        "transport encryption: {}",
        config.transport_encryption.as_str()
    );
    if config.transport_encryption == TransportEncryptionMode::Disabled {
        println!(
            "warning: transport encryption is disabled; a leaked bearer token can call the API"
        );
    }
    if payload.url.contains("127.0.0.1") || payload.url.contains("localhost") {
        println!("warning: pairing URL is local-only; rerun setup after starting Tailscale or pass --public-url");
    } else if payload.url.starts_with("http://") {
        let protection = transport_protection(&config);
        if protection == "tailscale-wireguard" {
            println!("security: HTTP is carried inside Tailscale WireGuard; Tailscale Serve HTTPS is still preferred");
        } else {
            println!("warning: HTTP exposes bearer tokens on the network; configure HTTPS or bind only to Tailscale");
        }
    }
    println!("pairing identity is ready");
    println!("run `muqun-gateway manage` in any terminal to scan the QR code");
    Ok(())
}

pub(crate) struct ExistingInstall {
    pub(crate) config: Config,
    pub(crate) pairing: PairingFile,
}

pub(crate) fn load_existing_install(
    config_path: &std::path::Path,
    pairing_path: &std::path::Path,
) -> Option<ExistingInstall> {
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).ok()?).ok()?;
    let pairing: PairingFile = serde_json::from_slice(&std::fs::read(pairing_path).ok()?).ok()?;
    // A config whose pairing file points at a different server is inconsistent;
    // treat it as no identity so setup mints a clean one rather than stitching
    // two mismatched halves together.
    if pairing.payload.server_id != config.server_id {
        return None;
    }
    if hash_token(&pairing.payload.token) != config.token_hash {
        return None;
    }
    Some(ExistingInstall { config, pairing })
}

pub(crate) fn ensure_backend_available(backend: BackendKind) -> anyhow::Result<()> {
    backend_registry().ensure_available(backend)
}

pub(crate) fn validate_session_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 64,
        "backend id must be 1 to 64 bytes"
    );
    anyhow::ensure!(
        id.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "backend id may contain only letters, numbers, '.', '-', and '_'"
    );
    Ok(())
}

pub(crate) fn next_backend_id(config: &Config, backend: BackendKind) -> String {
    if config.sessions.is_empty() {
        return String::from("default");
    }
    let base = backend.as_str();
    if config.sessions.iter().all(|session| session.id != base) {
        return base.to_owned();
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| {
            config
                .sessions
                .iter()
                .all(|session| session.id != *candidate)
        })
        .expect("an unused backend id exists")
}

pub(crate) fn upsert_backend_session(
    config: &mut Config,
    backend: BackendKind,
    requested_id: Option<String>,
    label: Option<String>,
    socket_path: Option<String>,
) -> anyhow::Result<String> {
    ensure_backend_available(backend)?;
    if let Some(session) = config.sessions.iter_mut().find(|session| {
        session.backend == backend && requested_id.as_deref().is_none_or(|id| session.id == id)
    }) {
        if let Some(label) = label {
            validate_label(&label)?;
            session.label = label;
        }
        if let Some(socket_path) = socket_path {
            session.socket_path = socket_path;
        }
        return Ok(session.id.clone());
    }

    let id = requested_id.unwrap_or_else(|| next_backend_id(config, backend));
    validate_session_id(&id)?;
    anyhow::ensure!(
        config.sessions.iter().all(|session| session.id != id),
        "backend id {id} already exists"
    );
    let label = label.unwrap_or_else(|| default_backend_label(backend).to_owned());
    validate_label(&label)?;
    config.sessions.push(SessionConfig {
        id: id.clone(),
        label,
        socket_path: socket_path.unwrap_or_else(|| default_backend_socket(backend)),
        backend,
    });
    // Appended, and nothing already in the list is moved.
    //
    // This used to re-sort every entry tmux-first on each add, which was
    // harmless while the stored order decided nothing. It is not harmless any
    // more: `session_order_key` breaks a liveness tie by config position, so
    // position 0 is the session the app opens. A reader who ran
    // `muqun-gateway backend default herdr-1` and then added a tmux backend
    // had the tmux entry sorted in front of their stated default, and their
    // phone quietly changed which backend it opened.
    //
    // Adding a backend is not a statement about which one should be first.
    // `backend default` is, and it is the only thing that reorders now.
    Ok(id)
}

pub(crate) fn validate_label(label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!label.trim().is_empty(), "backend label cannot be empty");
    anyhow::ensure!(
        label.chars().count() <= MAX_WORKSPACE_LABEL_CHARS,
        "backend label is too long"
    );
    anyhow::ensure!(
        !label.chars().any(char::is_control),
        "backend label cannot contain control characters"
    );
    Ok(())
}

pub(crate) fn configure_backend(command: BackendCommand) -> anyhow::Result<()> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    match command {
        BackendCommand::Autostart { id, mode } => {
            let session = config
                .sessions
                .iter()
                .find(|session| session.id == id)
                .with_context(|| format!("backend {id} not found"))?;
            if matches!(mode, BackendAutostartMode::On) {
                backend_startup::validate(session)?;
                if !config.autostart_backends.contains(&id) {
                    config.autostart_backends.push(id.clone());
                }
            } else {
                config.autostart_backends.retain(|item| item != &id);
            }
            write_config(&path, &config)?;
            println!(
                "backend {id} autostart {}; applies on the next gateway start",
                if matches!(mode, BackendAutostartMode::On) {
                    "on"
                } else {
                    "off"
                }
            );
        }
        BackendCommand::List => {
            for session in &config.sessions {
                let endpoint = backend_endpoint(session);
                println!(
                    "{}\t{}\t{}\t{}",
                    session.id,
                    session.backend.as_str(),
                    session.label,
                    endpoint
                );
            }
            return Ok(());
        }
        BackendCommand::Add {
            backend,
            id,
            label,
            socket_path,
        } => {
            let id = upsert_backend_session(&mut config, backend.into(), id, label, socket_path)?;
            write_config(&path, &config)?;
            println!("backend {id} configured; restart the gateway to apply changes");
        }
        BackendCommand::Remove { id } => {
            validate_session_id(&id)?;
            anyhow::ensure!(config.sessions.len() > 1, "cannot remove the only backend");
            let previous_len = config.sessions.len();
            config.sessions.retain(|session| session.id != id);
            config.autostart_backends.retain(|item| item != &id);
            anyhow::ensure!(
                config.sessions.len() != previous_len,
                "backend {id} not found"
            );
            write_config(&path, &config)?;
            println!("backend {id} removed; restart the gateway to apply changes");
        }
        BackendCommand::Default { id } => {
            make_backend_default(&mut config, &id)?;
            write_config(&path, &config)?;
            println!("backend {id} is now the default; restart the gateway to apply changes");
        }
    }
    Ok(())
}

pub(crate) fn make_backend_default(config: &mut Config, id: &str) -> anyhow::Result<()> {
    validate_session_id(id)?;
    let position = config
        .sessions
        .iter()
        .position(|session| session.id == id)
        .with_context(|| format!("backend {id} not found"))?;
    if position > 0 {
        let session = config.sessions.remove(position);
        config.sessions.insert(0, session);
    }
    Ok(())
}

/// Why the installer must not adopt a plugin pairing over this standalone
/// install, if it must not.
///
/// An explicit import makes the plugin identity authoritative. The installer
/// runs the same import unattended on every update, though, and a standalone
/// install that has since paired devices under its own server id would have
/// every one of them re-pointed at an identity they never saw -- typically a
/// plugin install left over from before the standalone one existed. Only an
/// install nothing is paired to yet, or one that already carries the plugin
/// identity, is safe to adopt without being asked, and only while no gateway
/// is using it: stopping a service is not the installer's call to make here.
///
/// Never an error. This runs mid-install, where anything unreadable is a
/// reason to leave both identities alone, not to abort the update.
pub(crate) fn auto_import_skip_reason(
    source_config_dir: &std::path::Path,
    target_config_dir: &std::path::Path,
    target_state_dir: &std::path::Path,
) -> Option<String> {
    let target = load_existing_install(
        &target_config_dir.join(CONFIG_FILE),
        &target_config_dir.join(PAIRING_FILE),
    )?;
    let same_identity = load_existing_install(
        &source_config_dir.join(CONFIG_FILE),
        &source_config_dir.join(PAIRING_FILE),
    )
    .is_some_and(|source| source.config.server_id == target.config.server_id);
    if same_identity {
        return None;
    }
    match read_devices_at(&target_state_dir.join(DEVICES_FILE)) {
        Err(_) => {
            return Some(String::from(
                "the standalone device file could not be read, so devices may be paired to it",
            ))
        }
        Ok(devices) if !devices.is_empty() => {
            return Some(format!(
                "the standalone gateway (server id {}) already has {} paired device(s)",
                target.config.server_id,
                devices.len()
            ))
        }
        Ok(_) => {}
    }
    if state_lock::StateLock::acquire(target_state_dir).is_err() {
        return Some(String::from("the standalone gateway is running"));
    }
    None
}

pub(crate) fn import_herdr_plugin(
    source_config_dir: Option<PathBuf>,
    source_state_dir: Option<PathBuf>,
    target_config_dir: Option<PathBuf>,
    target_state_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let source_config_dir = source_config_dir.unwrap_or(default_herdr_plugin_config_dir()?);
    let source_state_dir = source_state_dir.unwrap_or(default_herdr_plugin_state_dir()?);
    let source = load_existing_install(
        &source_config_dir.join(CONFIG_FILE),
        &source_config_dir.join(PAIRING_FILE),
    )
    .context("Herdr plugin config and pairing files are missing or inconsistent")?;
    anyhow::ensure!(
        source
            .config
            .sessions
            .iter()
            .any(|session| session.backend == BackendKind::Herdr),
        "the source installation does not configure a Herdr backend"
    );
    let target_config_dir = target_config_dir.unwrap_or(standalone_config_dir()?);
    let target_state_dir = target_state_dir.unwrap_or(standalone_state_dir()?);
    std::fs::create_dir_all(&target_config_dir)?;
    // This merges records into the target's device file. A gateway running
    // against that directory holds the pre-merge list in memory and would
    // write it back over the merge at its next pairing, so the import has to
    // own the directory outright while it runs. Taken before the port check:
    // both installs default to the same port, so what answers there is often
    // the standalone gateway itself, and the lock names it exactly.
    let _target_lock = state_lock::StateLock::acquire(&target_state_dir).context(
        "cannot import into a state directory a gateway is using. If it runs as a service, \
         stop it with `muqun-gateway service uninstall` followed by `muqun-gateway stop`, \
         and run `muqun-gateway service install` again after importing",
    )?;
    anyhow::ensure!(
        gateway_listener_pids(source.config.port())
            .unwrap_or_default()
            .is_empty(),
        "stop the running Herdr plugin gateway before importing its pairing identity"
    );
    let target_config_path = target_config_dir.join(CONFIG_FILE);
    let target_pairing_path = target_config_dir.join(PAIRING_FILE);
    let target = if target_config_path.exists() || target_pairing_path.exists() {
        Some(
            load_existing_install(&target_config_path, &target_pairing_path).context(
                "standalone config and pairing files are inconsistent; refusing to overwrite them",
            )?,
        )
    } else {
        None
    };

    let mut merged = source.config;
    if let Some(target) = &target {
        for session in &target.config.sessions {
            if merged
                .sessions
                .iter()
                .any(|existing| existing.backend == session.backend)
            {
                continue;
            }
            upsert_backend_session(
                &mut merged,
                session.backend,
                None,
                Some(session.label.clone()),
                Some(session.socket_path.clone()),
            )?;
        }
        for (kind, command) in &target.config.agent_commands {
            merged
                .agent_commands
                .entry(kind.clone())
                .or_insert_with(|| command.clone());
        }
        merged.rich_agent_pushes |= target.config.rich_agent_pushes;
        for session in &target.config.sessions {
            if !target.config.autostart_backends.contains(&session.id) {
                continue;
            }
            if let Some(imported) = merged.sessions.iter().find(|item| {
                item.backend == session.backend && item.socket_path == session.socket_path
            }) {
                if !merged.autostart_backends.contains(&imported.id) {
                    merged.autostart_backends.push(imported.id.clone());
                }
            }
        }
    }

    backup_secret_file(&target_config_path)?;
    backup_secret_file(&target_pairing_path)?;
    write_config(&target_config_path, &merged)?;
    write_secret_file(
        &target_pairing_path,
        &serde_json::to_vec_pretty(&source.pairing)?,
    )?;

    merge_plugin_state::<DeviceRecord, _>(
        &source_state_dir.join(DEVICES_FILE),
        &target_state_dir.join(DEVICES_FILE),
        |left, right| left.id == right.id || left.token_hash == right.token_hash,
    )?;
    merge_plugin_state::<PushTokenRecord, _>(
        &source_state_dir.join(PUSH_TOKENS_FILE),
        &target_state_dir.join(PUSH_TOKENS_FILE),
        |left, right| left.token == right.token,
    )?;
    write_secret_file(
        &target_config_dir.join(HERDR_PLUGIN_IMPORT_MARKER),
        source_config_dir.to_string_lossy().as_bytes(),
    )?;

    println!(
        "imported Herdr plugin pairing into {}",
        target_config_dir.display()
    );
    println!("preserved server id: {}", merged.server_id);
    println!("configured backends:");
    for session in &merged.sessions {
        println!("  {} ({})", session.id, session.backend.as_str());
    }
    println!("source files were retained; run `muqun-gateway start` when ready");
    Ok(())
}

pub(crate) fn backup_secret_file(path: &std::path::Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("gateway-secret");
    let backup = path.with_file_name(format!("{file_name}.before-herdr-import"));
    if !backup.exists() {
        write_secret_file(&backup, &std::fs::read(path)?)?;
    }
    Ok(())
}

pub(crate) fn merge_plugin_state<T, F>(
    source_path: &std::path::Path,
    target_path: &std::path::Path,
    same: F,
) -> anyhow::Result<()>
where
    T: Serialize + serde::de::DeserializeOwned,
    F: Fn(&T, &T) -> bool,
{
    let mut source: Vec<T> = if source_path.exists() {
        serde_json::from_slice(&std::fs::read(source_path)?)?
    } else {
        Vec::new()
    };
    let target: Vec<T> = if target_path.exists() {
        serde_json::from_slice(&std::fs::read(target_path)?)?
    } else {
        Vec::new()
    };
    for value in target {
        if !source.iter().any(|existing| same(existing, &value)) {
            source.push(value);
        }
    }
    backup_secret_file(target_path)?;
    if !source.is_empty() || target_path.exists() || source_path.exists() {
        write_secret_file(target_path, &serde_json::to_vec_pretty(&source)?)?;
    }
    Ok(())
}

pub(crate) fn auto_public_url(port: u16) -> PublicUrlSelection {
    let Some(status) = tailscale_status_json() else {
        return PublicUrlSelection {
            url: format!("http://127.0.0.1:{port}"),
            source: String::from("localhost fallback"),
            listen_host: String::from("127.0.0.1"),
        };
    };

    if status.get("BackendState").and_then(Value::as_str) != Some("Running") {
        return PublicUrlSelection {
            url: format!("http://127.0.0.1:{port}"),
            source: String::from("tailscale not running"),
            listen_host: String::from("127.0.0.1"),
        };
    }

    if let Some(domain) = tailscale_serve_https_domain(port) {
        return PublicUrlSelection {
            url: format!("https://{domain}"),
            source: String::from("tailscale serve https"),
            listen_host: String::from("127.0.0.1"),
        };
    }

    if let Some(ip) = status
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .and_then(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .find(|ip| ip.contains('.'))
        })
    {
        // Prefer the MagicDNS name in the URL over the raw IP: it's stable across
        // IP changes and is the name the user gets HTTPS on the moment they point
        // Tailscale Serve at the gateway.
        //
        // The listener binds 0.0.0.0 and not that IP. Binding the tailnet
        // address makes the gateway's own start depend on Tailscale having come
        // up first -- before it has, the address does not exist on any
        // interface and bind fails outright -- and it is the same address the
        // machine loses whenever the tailnet reassigns it. Nothing is more
        // exposed by the wildcard than by the name already published in the QR:
        // every route in is token-checked either way.
        let magic_dns = status
            .pointer("/Self/DNSName")
            .and_then(Value::as_str)
            .map(|name| name.trim_end_matches('.'))
            .filter(|name| !name.is_empty());
        return match magic_dns {
            Some(name) => PublicUrlSelection {
                url: format!("http://{name}:{port}"),
                source: String::from("tailscale magicdns (http; set up Serve for https)"),
                listen_host: String::from("0.0.0.0"),
            },
            None => PublicUrlSelection {
                url: format!("http://{ip}:{port}"),
                source: String::from("tailscale ip"),
                listen_host: String::from("0.0.0.0"),
            },
        };
    }

    PublicUrlSelection {
        url: format!("http://127.0.0.1:{port}"),
        source: String::from("localhost fallback"),
        listen_host: String::from("127.0.0.1"),
    }
}

pub(crate) fn tailscale_status_json() -> Option<Value> {
    let output = ProcessCommand::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

pub(crate) fn tailscale_serve_https_domain(port: u16) -> Option<String> {
    let output = ProcessCommand::new("tailscale")
        .args(["serve", "status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let status: Value = serde_json::from_slice(&output.stdout).ok()?;
    let web = status.get("Web")?.as_object()?;
    for (host_port, config) in web {
        let handlers = config.get("Handlers").and_then(Value::as_object)?;
        let serves_gateway = handlers.values().any(|handler| {
            handler
                .get("Proxy")
                .and_then(Value::as_str)
                .is_some_and(|proxy| proxy_targets_port(proxy, port))
        });
        if serves_gateway {
            let domain = host_port
                .strip_suffix(":443")
                .unwrap_or(host_port)
                .trim_end_matches('.');
            if !domain.is_empty() {
                return Some(domain.to_string());
            }
        }
    }
    None
}

pub(crate) fn proxy_targets_port(proxy: &str, port: u16) -> bool {
    let without_path = proxy
        .strip_prefix("http://")
        .or_else(|| proxy.strip_prefix("https://"))
        .unwrap_or(proxy)
        .split('/')
        .next()
        .unwrap_or_default();
    without_path
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        == Some(port)
}

/// `service install|uninstall|status`.
///
/// Install deliberately stops the pid-file gateway first. The two ways of
/// running are not additive: leave the detached child up and the agent starts a
/// second gateway a moment later, which loses the port race, dies in the log,
/// and leaves a machine that looks installed and answers nothing.
pub(crate) fn run_service_command(command: ServiceCommand) -> anyhow::Result<()> {
    if !matches!(command, ServiceCommand::Status) {
        let _lock = state_lock::StateLock::acquire(&state_dir()?.join("lifecycle"))
            .context("another lifecycle command is in progress")?;
        return change_gateway_autostart(matches!(command, ServiceCommand::Install));
    }
    print_service_status()
}

pub(crate) fn change_gateway_autostart(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        let config_dir = config_dir()?;
        let install = load_existing_install(&config_dir.join(CONFIG_FILE), &config_dir.join(PAIRING_FILE))
            .context("configure a consistent gateway identity with `muqun-gateway setup` before enabling autostart")?;
        install
            .config
            .listen
            .parse::<std::net::SocketAddr>()
            .context("invalid gateway listen address")?;
    }
    let was_running = lifecycle::running_pid()?.is_some();
    let existing = service::state()?;
    if existing != service::ServiceState::NotInstalled {
        service::ensure_current_install(&service_paths()?)?;
    }
    if enabled {
        if let Some(pid) = lifecycle::running_pid()? {
            let external = crate::supervision::gateway_supervisor(pid)?;
            anyhow::ensure!(external.as_ref().is_none_or(|unit| unit.user_manager && unit.unit == format!("{}.service", service::SERVICE_LABEL)),
                "an external supervisor owns the gateway; remove that registration before installing the user service");
            #[cfg(target_os = "macos")]
            let supervised = service::owns_pid(pid);
            #[cfg(not(target_os = "macos"))]
            let supervised = external.is_some();
            if !supervised {
                stop_detached()?;
            }
        }
        service::install(&service_paths()?)?;
        lifecycle::wait_for(std::time::Duration::from_secs(15), || {
            Ok(lifecycle::running_pid()?.is_some() && crate::fetch_pending_pairing().is_ok())
        }).context("gateway autostart was installed, but readiness failed; inspect the user service before retrying")?;
        println!("The gateway now starts when you log in, and restarts if it stops.");
    } else {
        if existing == service::ServiceState::NotInstalled {
            println!("Gateway autostart is already off; the running gateway is unchanged.");
            return Ok(());
        }
        service::uninstall()?;
        if was_running {
            lifecycle::wait_for(std::time::Duration::from_secs(15), || {
                Ok(state_lock::running_owner(&state_dir()?)?.is_none())
            })?;
            match start_detached() {
                Ok(()) => println!("The gateway is still running; gateway autostart is off."),
                Err(error) => {
                    anyhow::bail!("autostart was removed, but the gateway did not restart: {error}")
                }
            }
        }
    }
    Ok(())
}

fn print_service_status() -> anyhow::Result<()> {
    match service::state()? {
        service::ServiceState::Installed => println!("service: installed ({})", service::unit_path()?.display()),
        service::ServiceState::Stopped => println!("service: installed, stopped ({}) -- starts at next login or with `muqun-gateway start`", service::unit_path()?.display()),
        service::ServiceState::FileOnly => println!("service: unit file only; run `muqun-gateway service install` to enable/repair autostart"),
        service::ServiceState::NotInstalled => println!("service: not installed -- gateway autostart is off"),
    }
    match load_config(None) {
        Ok(_) => println!("gateway: {}", lifecycle::summary()?),
        Err(_) => println!("gateway: not configured yet -- run `muqun-gateway setup`"),
    }
    Ok(())
}

pub(crate) fn service_paths() -> anyhow::Result<service::ServicePaths> {
    let (path, lc_ctype) = login_env::for_unit_file();
    Ok(service::ServicePaths {
        exe: executable_path()?,
        config: config_dir()?.join(CONFIG_FILE),
        state: state_dir()?,
        log: state_dir()?.join(LOG_FILE),
        home: dirs::home_dir().context("failed to locate the home directory")?,
        path,
        lc_ctype,
    })
}

/// A self-updating manager keeps running the old mapped image. Linux reports
/// that image's renamed/unlinked path; lifecycle must keep using the install path.
pub(crate) fn executable_path() -> anyhow::Result<std::path::PathBuf> {
    static EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    if let Some(exe) = EXE.get() {
        return Ok(exe.clone());
    }
    let exe = std::env::current_exe().context("failed to find current executable")?;
    // macOS reports the path the binary was started by -- a symlink, or
    // `./muqun-gateway` -- not the file. Resolve it, so the plist written and
    // the plist checked name the installed binary however the CLI was run.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(EXE.get_or_init(|| exe).clone())
}

pub(crate) fn start_background() -> anyhow::Result<()> {
    start_background_inner(true)
}

pub(crate) fn start_background_inner(verbose: bool) -> anyhow::Result<()> {
    lifecycle::control(lifecycle::Action::Start, verbose)
}

pub(crate) fn restart_gateway(verbose: bool) -> anyhow::Result<()> {
    lifecycle::control(lifecycle::Action::Restart, verbose)
}

pub(crate) fn start_detached() -> anyhow::Result<()> {
    start_detached_at(&executable_path()?)
}

pub(crate) fn start_detached_at(exe: &std::path::Path) -> anyhow::Result<()> {
    let state_dir = state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create state dir {}", state_dir.display()))?;
    if lifecycle::running_pid()?.is_some() {
        return Ok(());
    }
    // The pid file only knows about gateways this subcommand started. One
    // launched by systemd, or by hand, leaves no pid file at all -- and that
    // is precisely the pairing-losing case. Ask the state directory instead,
    // then hand the lock straight to the child. Without this the spawn below
    // would "succeed" and the child would die a moment later in the log,
    // where nobody is looking.
    drop(state_lock::StateLock::acquire(&state_dir)?);

    let config = config_dir()?.join(CONFIG_FILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(LOG_FILE))?;
    let err = log.try_clone()?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("run")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    detach_background_process(&mut command);
    let mut child = command.spawn().context("failed to start gateway")?;
    let pid = child.id();
    if let Err(error) = write_secret_file(&state_dir.join(PID_FILE), pid.to_string().as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error)
            .context("gateway child was stopped because its PID could not be recorded");
    }
    let outcome = lifecycle::wait_for(std::time::Duration::from_secs(15), || {
        if let Some(status) = child.try_wait()? {
            let cause = lifecycle::startup_failure_cause()
                .map(|cause| format!(": {cause}"))
                .unwrap_or_default();
            anyhow::bail!(
                "gateway exited during startup ({status}){cause}; inspect {}",
                state_dir.join(LOG_FILE).display()
            );
        }
        Ok(lifecycle::running_pid()? == Some(pid) && crate::fetch_pending_pairing().is_ok())
    });
    if outcome.is_err() {
        // Only the child we just spawned. Never signal a discovered listener or
        // its process group. A timed-out start must not leave an orphan retry.
        let _ = child.kill();
        let _ = child.wait();
        remove_pid_file()?;
    }
    outcome?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn detach_background_process(command: &mut ProcessCommand) {
    // Herdr may tear down the popup/action process group when a panel closes.
    // Start the gateway in a new session so it survives the manager UI.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn detach_background_process(_command: &mut ProcessCommand) {}

pub(crate) fn stop_background() -> anyhow::Result<()> {
    stop_background_inner(true)
}

/// The port this installation actually listens on, falling back to the default
/// when the config is missing or unreadable.
pub(crate) fn configured_port() -> u16 {
    load_config(None)
        .map(|config| config.port())
        .unwrap_or(DEFAULT_PORT)
}

pub(crate) fn stop_background_inner(verbose: bool) -> anyhow::Result<()> {
    lifecycle::control(lifecycle::Action::Stop, verbose)
}

pub(crate) fn stop_detached() -> anyhow::Result<()> {
    if let Some(pid) = lifecycle::running_pid()? {
        anyhow::ensure!(
            crate::supervision::gateway_supervisor(pid)?.is_none(),
            "a supervisor owns gateway pid {pid}; refusing a direct signal"
        );
        #[cfg(target_os = "macos")]
        {
            anyhow::ensure!(
                !service::owns_pid(pid),
                "launchd owns gateway pid {pid}; refusing a direct signal"
            );
            // macOS cannot tell a gateway this CLI detached from one another
            // LaunchAgent (`brew services`, a hand-written plist) runs. Say
            // what is about to happen, so a respawn loop has an explanation.
            eprintln!(
                "stopping gateway pid {pid} with SIGTERM: it is not run by the {} LaunchAgent. \
                 If another LaunchAgent runs it, launchd will start it again; stop it with \
                 `launchctl bootout gui/$(id -u)/<its label>` instead.",
                service::SERVICE_LABEL
            );
        }
        stop_pid(pid)?;
        lifecycle::wait_for(std::time::Duration::from_secs(15), || {
            Ok(state_lock::running_owner(&state_dir()?)?.is_none())
        })?;
    }
    remove_pid_file()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn backend_autostart_is_explicit_and_old_configs_stay_off() {
        let mut config = test_config("test");
        let old = serde_json::to_value(&config).unwrap();
        assert!(old.get("autostart_backends").is_none());
        assert!(serde_json::from_value::<Config>(old)
            .unwrap()
            .autostart_backends
            .is_empty());
        config.autostart_backends.push("default".into());
        let loaded: Config =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(loaded.autostart_backends, ["default"]);
    }

    #[test]
    fn a_tmux_session_is_explicit_while_old_sessions_stay_herdr() {
        let old: SessionConfig = serde_json::from_value(json!({
            "id": "default", "label": "Default", "socket_path": "/tmp/herdr.sock"
        }))
        .unwrap();
        assert_eq!(old.backend, BackendKind::Herdr);
        assert!(serde_json::to_value(&old).unwrap().get("backend").is_none());

        let tmux = SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        assert_eq!(serde_json::to_value(tmux).unwrap()["backend"], "tmux");
    }

    /// Config position is what breaks a liveness tie in `session_order_key`,
    /// so position 0 is the session the app opens. Adding a backend must
    /// therefore leave the existing order alone: it appends.
    #[test]
    fn adding_a_backend_appends_and_never_reorders_the_existing_ones() {
        let mut config = test_config("token");
        config.sessions.clear();
        upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
        upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
        assert_eq!(config.sessions[0].backend, BackendKind::Herdr);
        assert_eq!(config.sessions[1].backend, BackendKind::Tmux);
    }

    /// The bug this closes: `backend default` moves an entry to position 0,
    /// and the next `backend add` used to sort tmux back in front of it --
    /// silently changing which backend the reader's phone opens.
    #[test]
    fn a_chosen_default_survives_adding_another_backend() {
        let mut config = test_config("token");
        config.sessions.clear();
        let herdr =
            upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
        upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
        make_backend_default(&mut config, &herdr).unwrap();
        assert_eq!(config.sessions[0].id, herdr);

        upsert_backend_session(
            &mut config,
            BackendKind::Tmux,
            Some("tmux-late".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            config.sessions[0].id, herdr,
            "adding a backend must not overrule `backend default`"
        );
    }

    #[test]
    fn adding_a_backend_preserves_the_primary_session_and_is_idempotent() {
        let mut config = test_config("token");
        config.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };

        let id = upsert_backend_session(
            &mut config,
            BackendKind::Herdr,
            None,
            None,
            Some("/tmp/herdr.sock".into()),
        )
        .unwrap();
        assert_eq!(id, "herdr");
        assert_eq!(config.sessions[0].id, "default");
        assert_eq!(config.sessions[1].backend, BackendKind::Herdr);

        let id = upsert_backend_session(
            &mut config,
            BackendKind::Herdr,
            None,
            Some("Herdr local".into()),
            Some("/tmp/new.sock".into()),
        )
        .unwrap();
        assert_eq!(id, "herdr");
        assert_eq!(config.sessions.len(), 2);
        assert_eq!(config.sessions[1].label, "Herdr local");
        assert_eq!(config.sessions[1].socket_path, "/tmp/new.sock");
    }

    #[test]
    fn choosing_a_default_backend_only_reorders_sessions() {
        let mut config = test_config("token");
        config.sessions.push(SessionConfig {
            id: "tmux".into(),
            label: "Local tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        });
        make_backend_default(&mut config, "tmux").unwrap();
        assert_eq!(config.sessions[0].id, "tmux");
        assert_eq!(config.sessions[1].id, "default");
        assert!(make_backend_default(&mut config, "missing").is_err());
    }

    #[test]
    fn a_fresh_install_with_no_backend_named_defaults_to_tmux() {
        assert_eq!(
            resolve_setup_backend(None, None),
            BackendKind::Tmux,
            "nothing configured yet, and nothing asked for -- tmux is primary"
        );
    }

    #[test]
    fn an_explicit_backend_always_wins_over_whatever_already_exists() {
        let herdr_only = test_config("token"); // sessions[0] is Herdr, see test_config
        assert_eq!(
            resolve_setup_backend(Some(BackendKind::Tmux), Some(&herdr_only)),
            BackendKind::Tmux
        );
    }

    #[test]
    fn an_existing_herdr_install_stays_herdr_when_backend_is_left_off() {
        // The exact case the old `default_value_t = SetupBackend::Herdr` was
        // protecting: a bare `setup` (e.g. the Herdr-plugin action, which has
        // no way to pass --backend) on a machine already running Herdr must
        // not start asking for tmux, which might not even be installed.
        let herdr_only = test_config("token");
        assert_eq!(
            resolve_setup_backend(None, Some(&herdr_only)),
            BackendKind::Herdr
        );
    }

    #[test]
    fn an_existing_tmux_install_stays_tmux_when_backend_is_left_off() {
        let mut tmux_only = test_config("token");
        tmux_only.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        assert_eq!(
            resolve_setup_backend(None, Some(&tmux_only)),
            BackendKind::Tmux
        );
    }

    #[test]
    fn backend_ids_and_labels_reject_terminal_control_input() {
        assert!(validate_session_id("tmux-2").is_ok());
        assert!(validate_session_id("../tmux").is_err());
        assert!(validate_session_id("tmux\nforged").is_err());
        assert!(validate_label("Local tmux").is_ok());
        assert!(validate_label("tmux\x1b[2J").is_err());
    }

    #[test]
    fn an_existing_install_requires_one_consistent_pairing_identity() {
        let dir =
            std::env::temp_dir().join(format!("gateway-existing-install-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config("admin-token");
        let pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: config.server_id.clone(),
                label: config.label.clone(),
                url: config.public_url.clone(),
                token: "admin-token".into(),
                transport_key: "transport-key".into(),
            },
        };
        std::fs::write(dir.join(CONFIG_FILE), serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::write(
            dir.join(PAIRING_FILE),
            serde_json::to_vec(&pairing).unwrap(),
        )
        .unwrap();
        assert!(load_existing_install(&dir.join(CONFIG_FILE), &dir.join(PAIRING_FILE)).is_some());

        let mut stale = pairing;
        stale.payload.token = "different-token".into();
        std::fs::write(dir.join(PAIRING_FILE), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(load_existing_install(&dir.join(CONFIG_FILE), &dir.join(PAIRING_FILE)).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn imported_state_is_merged_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("gateway-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        let target = dir.join("target.json");
        std::fs::write(&source, br#"["one","two"]"#).unwrap();
        std::fs::write(&target, br#"["two","three"]"#).unwrap();
        merge_plugin_state::<String, _>(&source, &target, |left, right| left == right).unwrap();
        let merged: Vec<String> = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(merged, vec!["one", "two", "three"]);
        assert!(target
            .with_file_name("target.json.before-herdr-import")
            .exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plugin_import_keeps_the_paired_identity_and_merges_tmux() {
        let root = std::env::temp_dir().join(format!("gateway-import-{}", uuid::Uuid::new_v4()));
        let source_config_dir = root.join("plugin-config");
        let source_state_dir = root.join("plugin-state");
        let target_config_dir = root.join("standalone-config");
        let target_state_dir = root.join("standalone-state");
        for dir in [
            &source_config_dir,
            &source_state_dir,
            &target_config_dir,
            &target_state_dir,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut plugin = test_config("plugin-token");
        plugin.server_id = "paired-herdr".into();
        plugin.listen = "127.0.0.1:31987".into();
        plugin.public_url = "http://127.0.0.1:31987".into();
        let plugin_pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: plugin.server_id.clone(),
                label: plugin.label.clone(),
                url: plugin.public_url.clone(),
                token: "plugin-token".into(),
                transport_key: "plugin-transport-key".into(),
            },
        };
        write_config(&source_config_dir.join(CONFIG_FILE), &plugin).unwrap();
        write_secret_file(
            &source_config_dir.join(PAIRING_FILE),
            &serde_json::to_vec(&plugin_pairing).unwrap(),
        )
        .unwrap();

        let mut standalone = test_config("tmux-token");
        standalone.server_id = "discarded-tmux-identity".into();
        standalone.sessions[0] = SessionConfig {
            id: "default".into(),
            label: "tmux".into(),
            socket_path: String::new(),
            backend: BackendKind::Tmux,
        };
        let standalone_pairing = PairingFile {
            payload: PairingPayload {
                kind: "muqun-gateway".into(),
                server_id: standalone.server_id.clone(),
                label: standalone.label.clone(),
                url: standalone.public_url.clone(),
                token: "tmux-token".into(),
                transport_key: "tmux-transport-key".into(),
            },
        };
        write_config(&target_config_dir.join(CONFIG_FILE), &standalone).unwrap();
        write_secret_file(
            &target_config_dir.join(PAIRING_FILE),
            &serde_json::to_vec(&standalone_pairing).unwrap(),
        )
        .unwrap();

        import_herdr_plugin(
            Some(source_config_dir),
            Some(source_state_dir),
            Some(target_config_dir.clone()),
            Some(target_state_dir),
        )
        .unwrap();

        let merged: Config =
            serde_json::from_slice(&std::fs::read(target_config_dir.join(CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(merged.server_id, "paired-herdr");
        assert_eq!(merged.sessions.len(), 2);
        // The install being imported *into* keeps position 0, because that is
        // the session the reader's phone already opens (`session_order_key`
        // breaks a liveness tie by config position). Importing a plugin's
        // backend adds one; it does not re-point the app at it.
        assert_eq!(merged.sessions[0].backend, BackendKind::Herdr);
        assert_eq!(merged.sessions[1].backend, BackendKind::Tmux);
        assert!(target_config_dir.join(HERDR_PLUGIN_IMPORT_MARKER).exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_installer_never_swaps_the_identity_of_a_standalone_install_with_devices() {
        let root = std::env::temp_dir().join(format!("gateway-import-{}", uuid::Uuid::new_v4()));
        let plugin = root.join("plugin-config");
        let standalone = root.join("standalone-config");
        let state = root.join("standalone-state");
        std::fs::create_dir_all(&state).unwrap();
        write_test_install(&plugin, "stale-plugin", "plugin-token");

        // Nothing standalone yet: adopting the plugin is the migration.
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());

        // A standalone identity nothing is paired to loses nothing.
        write_test_install(&standalone, "standalone", "standalone-token");
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());

        // ...unless a gateway is using it: stopping that is not the installer's call.
        {
            // Waited on, not exact: the lock the check above took and dropped
            // can live on for a moment in another test thread's fork.
            let _running =
                state_lock::acquire_within(&state, state_lock::RELEASE_VISIBLE_WITHIN).unwrap();
            let reason = auto_import_skip_reason(&plugin, &standalone, &state)
                .expect("a running standalone gateway must be left alone");
            assert!(reason.contains("running"), "{reason}");
        }

        // Once a device is paired to it, the installer must leave it alone.
        write_devices_at(&state, &[test_device("phone", "phone-token")]).unwrap();
        let reason = auto_import_skip_reason(&plugin, &standalone, &state)
            .expect("paired standalone identity must be kept");
        assert!(reason.contains("1 paired device"), "{reason}");

        // An unreadable device file is a reason to skip, never to abort the install.
        std::fs::write(state.join(DEVICES_FILE), b"not json").unwrap();
        let reason = auto_import_skip_reason(&plugin, &standalone, &state)
            .expect("unreadable device file must be kept");
        assert!(reason.contains("could not be read"), "{reason}");

        // Unless it already is the plugin identity, where import changes nothing.
        write_test_install(&standalone, "stale-plugin", "plugin-token");
        assert!(auto_import_skip_reason(&plugin, &standalone, &state).is_none());
        std::fs::remove_dir_all(root).ok();
    }
    #[test]
    fn tailscale_serve_proxy_matching_uses_the_exact_port() {
        assert!(proxy_targets_port("http://127.0.0.1:23100", 23100));
        assert!(proxy_targets_port("http://localhost:23100/path", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:123100", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:23100.example", 23100));
    }
}
