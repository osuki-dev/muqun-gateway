//! Muqun Desktop composition root — Rune UI + tokio agents plane.

mod agent_fields;
mod branding;
mod config;
mod diagnostics;
mod hub;
mod model;
mod platform;
pub mod plugin;
mod term;
mod theme;
mod ui;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use muqun_gateway::agents::compose::{
    attach_store, build_runtime, RuntimeSettings, StoreEncryption,
};
use rune::{App, AppSpec};

use crate::config::{DesktopConfig, CONFIG_FILE};
use crate::model::commands::{self, Command, CommandReceiver, CommandSender};
use crate::model::inbox::Inbox;
use crate::model::projection::{self, ConfigStore};
use crate::platform::{coexist, instance};
use crate::ui::UiState;

static COMMANDS: OnceLock<CommandSender> = OnceLock::new();

/// glibc keeps a heap arena per thread and returns little of it: a long
/// session's window (tens of megabytes of JSON) parsed on a runtime worker
/// left its pages resident, and browsing four long sessions held ~1 GB.
/// Two arenas, prompt trimming and big blocks in their own mappings give
/// that memory back (measured: ~1 GB → ~330 MB on the same browse).
#[cfg(target_env = "gnu")]
fn tune_allocator() {
    // SAFETY: plain allocator tuning calls, made before any other thread.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 128 * 1024);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }
}

#[cfg(not(target_env = "gnu"))]
fn tune_allocator() {}

fn main() -> anyhow::Result<()> {
    tune_allocator();
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--plugin-pick-path")) {
        return platform::dialogs::plugin_pick_path(std::env::args_os().skip(2));
    }
    if let Some(pos) = std::env::args().position(|a| a == "--term-daemon") {
        let socket_path = std::env::args()
            .nth(pos + 1)
            .filter(|s| !s.starts_with("--"))
            .map(PathBuf::from);
        term::daemon::run_daemon(socket_path)?;
        return Ok(());
    }
    let started = Instant::now();
    let (config_dir, state_dir) = platform::directories()?;
    muqun_gateway::set_directories(config_dir.clone(), state_dir.clone())?;
    init_tracing();
    tracing::info!(
        config = %config_dir.display(),
        state = %state_dir.display(),
        "muqun-desktop starting (rune)"
    );

    let title = match std::env::var("MUQUN_WINDOW_TITLE") {
        Ok(t) if !t.is_empty() => Box::leak(t.into_boxed_str()) as &'static str,
        _ => branding::APP_NAME,
    };
    // A second launch focuses the first (and hands it the file), before
    // this one starts any agent. Rune's tooling flags (--scene, --snap, …)
    // always run on their own.
    let tooling = std::env::args().skip(1).any(|a| a.starts_with("--"));
    // `muqun-desktop <file>` opens the file in the viewer.
    let open = if tooling {
        None
    } else {
        rune::extra_args()
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(|a| std::path::absolute(a).unwrap_or_else(|_| PathBuf::from(a)))
    };
    let instance = if tooling {
        None
    } else {
        match instance::claim(title, &instance::Request::here(open.clone())) {
            instance::Claim::Forwarded => {
                tracing::info!("handed to the running Muqun");
                return Ok(());
            }
            instance::Claim::Primary(primary) => Some(primary),
        }
    };

    if instance.is_some() {
        diagnostics::initialize(state_dir.join("diagnostics"));
        diagnostics::Attempt::new(diagnostics::EventKind::DesktopStart, "Desktop", None)
            .finish(diagnostics::Outcome::Completed);
    }
    let config_path = config_dir.join(CONFIG_FILE);
    let config = load_config(&config_path);
    let store = ConfigStore::new(config_path, config.clone());
    let settings = runtime_settings(&config);

    let inbox = Arc::new(Inbox::new());
    platform::dialogs::install(inbox.clone());
    let (tx, rx) = commands::channel();
    COMMANDS
        .set(tx.clone())
        .map_err(|_| anyhow::anyhow!("commands already installed"))?;
    // A screenshot scene (`--scene`, `--snap`) shows demo data and ignores the core:
    // starting it would launch agents (and their daemons) for nothing.
    if std::env::args().any(|a| {
        a == "--scene" || a.starts_with("--scene=") || a == "--snap" || a.starts_with("--snap=")
    }) {
        std::thread::spawn(move || {
            let mut rx = rx;
            while rx.blocking_recv().is_some() {}
        });
    } else {
        theme::omarchy::watch(inbox.clone(), store.clone());
        if !tooling {
            platform::gateway_bootstrap::initialize(
                &config,
                config_dir.clone(),
                state_dir.clone(),
                inbox.clone(),
            );
        }
        start_core(
            settings,
            state_dir.clone(),
            inbox.clone(),
            store.clone(),
            rx,
        )?;
    }
    commands::send(&tx, Command::Refresh);
    let _ = theme::install(&config_dir, &state_dir, &config.appearance);

    let mut ui_config = config.clone();
    if config.appearance.theme_id == theme::omarchy::ID {
        if let Some((_, scheme)) = theme::apply::current_choice() {
            ui_config.appearance.scheme = scheme;
        }
    }
    ui::seed_config(ui_config);
    let geometry = config.window.clamped();

    rune::set_app_id(branding::APP_ID);
    let app = App::new(AppSpec::<UiState> {
        scenes: ui::scenes::scenes(),
        on_key: Some(ui::on_key),
        setup: Some(|s, waker| s.install_images(waker)),
        poll: Some(UiState::poll_frame),
        families: platform::fonts::families(),
        ..AppSpec::new(
            title,
            (geometry.w.max(640) as f32, geometry.h.max(480) as f32),
            || UiState::new(COMMANDS.get().expect("commands").clone()),
            ui::view,
        )
    })
    .window_options(rune::WindowOptions {
        transparent: platform::see_through_window(),
        ..rune::WindowOptions::default()
    })
    .compositor(ui::fx::Fx::default())
    .on_focus(|s, focused, tx| {
        platform::notify::set_window_focused(focused);
        if focused
            && s.mode == ui::Mode::Session
            && s.selected.is_some()
            && tx.focused().is_none()
            && !s.term.sink.focused
            && s.menu.is_none()
            && !s.timeline_search.active
            && s.zoom.is_none()
        {
            tx.focus("draft", false);
        }
    })
    // Closing keeps the app only when the owner asked for the tray and an
    // icon is up to come back through; otherwise it quits as before.
    .keep_running()
    .on_close(|s, tx| {
        if !(s.cfg.general.close_to_tray && platform::tray::active()) {
            tx.quit();
        }
    });

    let app = platform::configure_window(app);

    let handle = app.handle();
    // SIGTERM / SIGINT quit through the event loop, as the tray's Quit does,
    // so the window and its GPU device go away on this thread.
    platform::shutdown::on_request({
        let handle = handle.clone();
        move || handle.quit()
    });
    platform::notify::install({
        let handle = handle.clone();
        move |asid, token| {
            // A click on a notification: bring the window up (with the
            // compositor's token on Wayland) and open that session.
            match token {
                Some(token) => handle.activate(token),
                None => handle.open(),
            }
            handle.update(move |s, _| s.open_notification(&asid));
        }
    });
    if let Some(primary) = instance {
        let handle = handle.clone();
        primary.serve(move |request| {
            match request.token {
                Some(token) => handle.activate(token),
                None => handle.open(),
            }
            if let Some(path) = request.open {
                handle.update(move |s, _| s.open_file(path));
            }
        });
    }
    if let Some(path) = open {
        handle.update(move |s, _| s.open_file(path));
    }
    if !std::env::args().skip(1).any(|a| a.starts_with("--")) {
        let words = ui::tray_words(config.general.language, 0);
        let tray_handle = handle.clone();
        platform::tray::start(&words, move |request| match request {
            platform::tray::TrayRequest::Open(token) => match token {
                Some(token) => tray_handle.activate(token),
                None => tray_handle.open(),
            },
            platform::tray::TrayRequest::Session(asid) => {
                tray_handle.open();
                tray_handle.update(move |s, _| s.open_session(&asid));
            }
            platform::tray::TrayRequest::Quit => tray_handle.quit(),
        });
    }
    ui::spawn_inbox_pump(inbox, handle);

    tracing::info!(
        ms = started.elapsed().as_millis() as u64,
        "opening rune window"
    );
    // A failed start or a panic leaves through the same door: unwinding out
    // of `main` would run the libraries' exit handlers under the core's
    // threads (OpenSSL's cleanup under a SQLite open crashed that way).
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.try_run()));
    platform::shutdown::deadline();
    instance::release();
    store.flush();
    let code = match ran {
        Ok(Ok(())) => 0,
        Ok(Err(err)) => {
            tracing::error!("{err}");
            1
        }
        Err(_) => 101,
    };
    tracing::info!(code, "muqun-desktop stopped");
    diagnostics::flush();
    platform::shutdown::leave(code)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    use tracing_subscriber::prelude::*;
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(diagnostics::DesktopReports)
        .try_init();
}

fn load_config(path: &std::path::Path) -> DesktopConfig {
    match DesktopConfig::load(path) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!("config unreadable ({err:#}); using defaults");
            DesktopConfig::default()
        }
    }
}

fn runtime_settings(config: &DesktopConfig) -> RuntimeSettings {
    let mut settings = config.agents.runtime_settings();
    let presence = coexist::detect(&coexist::gateway_state_dirs());
    match coexist::apply(&mut settings.opencode, &presence) {
        Some(why) => tracing::info!("OpenCode autostart off for this run: {why}"),
        None => tracing::info!(
            ?presence,
            autostart = settings.opencode.autostart,
            "OpenCode ownership"
        ),
    }
    settings.deepseek.autostart = false;
    settings
}

fn start_core(
    settings: RuntimeSettings,
    state_dir: PathBuf,
    inbox: Arc<Inbox>,
    store: Arc<ConfigStore>,
    commands: CommandReceiver,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("muqun-core")
        .build()?;
    std::thread::Builder::new()
        .name("muqun-core-main".into())
        .spawn(move || {
            runtime.block_on(async move {
                let (hub, connected) = select_hub(settings, &state_dir, &store.get()).await;
                let dynamic_hub = hub::DynamicHub::new(hub);
                projection::spawn(dynamic_hub, inbox, store, commands, connected, state_dir);
                std::future::pending::<()>().await;
            });
        })?;
    Ok(())
}

async fn select_hub(
    settings: RuntimeSettings,
    state_dir: &std::path::Path,
    config: &DesktopConfig,
) -> (Arc<dyn hub::SessionHub>, bool) {
    // The sidebar restores this computer too. Restore its actual connection
    // before issuing any reads or prompts, including when it is offline.
    let connected = match configured_remote_hub(config) {
        Ok(Some(remote)) => return (remote, true),
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(%err, "saved computer connection is invalid; waiting for reconnection");
            false
        }
    };
    if let Some(gateway) = hub::discover_local_gateway().await {
        match hub::RemoteHub::start(gateway.clone()) {
            Ok(remote) => {
                tracing::info!("hub: local gateway at {}", gateway.url);
                return (remote, connected);
            }
            Err(err) => tracing::warn!("local gateway found but unusable: {err:#}"),
        }
    }
    (standalone_hub(settings, state_dir).await, connected)
}

async fn standalone_hub(
    settings: RuntimeSettings,
    state_dir: &std::path::Path,
) -> Arc<dyn hub::SessionHub> {
    tracing::info!("hub: in-process");
    let agents = build_runtime(settings, true, Some(state_dir.to_path_buf()));
    if let Err(err) = attach_store(&agents, state_dir, StoreEncryption::Off).await {
        tracing::warn!("the session store is unavailable, routes are not kept: {err:#}");
    }
    agents.spawn_supervisor();
    Arc::new(hub::LocalHub::new(agents))
}

fn configured_remote_hub(
    config: &DesktopConfig,
) -> anyhow::Result<Option<Arc<dyn hub::SessionHub>>> {
    if matches!(
        config.current_computer.as_str(),
        "" | "local" | "self" | "all"
    ) {
        return Ok(None);
    }
    let profile = config
        .computers
        .get(&config.current_computer)
        .ok_or_else(|| anyhow::anyhow!("saved computer has no profile"))?;
    let url = profile
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("saved computer has no URL"))?;
    let parsed = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "saved computer URL must use HTTP or HTTPS"
    );
    let token = profile
        .token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("saved computer is not paired"))?;
    let remote = hub::RemoteHub::start(hub::LocalGateway {
        url: url.to_owned(),
        token: token.to_owned(),
        device_id: profile.device_id.clone(),
        transport_key: profile.transport_key.clone(),
    });
    match remote {
        Ok(remote) => {
            tracing::info!(computer = %config.current_computer, "hub: restored remote computer");
            Ok(Some(remote))
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn invalid_saved_remote_is_an_error_not_a_local_selection() {
        let mut config = DesktopConfig {
            current_computer: "mini".into(),
            ..Default::default()
        };
        assert!(configured_remote_hub(&config).is_err());
        config.computers.insert(
            "mini".into(),
            config::ComputerProfile {
                url: Some("http://127.0.0.1:9".into()),
                token: Some(" ".into()),
                ..Default::default()
            },
        );
        assert!(configured_remote_hub(&config).is_err());
        let profile = config.computers.get_mut("mini").unwrap();
        profile.token = Some("fixture".into());
        profile.url = Some("file:///tmp/wrong".into());
        assert!(configured_remote_hub(&config).is_err());
        config.current_computer = "local".into();
        assert!(configured_remote_hub(&config).unwrap().is_none());
    }
}
