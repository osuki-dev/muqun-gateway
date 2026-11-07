//! Muqun Desktop composition root — Rune UI + tokio agents plane.

mod agent_fields;
mod branding;
mod config;
mod hub;
mod model;
mod platform;
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

fn main() -> anyhow::Result<()> {
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
    // `muqun-desktop <file>` opens the file in the viewer.
    let open = rune::extra_args()
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|a| std::path::absolute(a).unwrap_or_else(|_| PathBuf::from(a)));
    // A second launch focuses the first (and hands it the file), before
    // this one starts any agent. Rune's tooling flags (--scene, --snap, …)
    // always run on their own.
    let tooling = std::env::args().skip(1).any(|a| a.starts_with("--"));
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
    start_core(
        settings,
        state_dir.clone(),
        inbox.clone(),
        store.clone(),
        rx,
    )?;
    commands::send(&tx, Command::Refresh);
    let _ = theme::install(&config_dir, &state_dir, &config.appearance);

    ui::seed_config(config.clone());
    let geometry = config.window.clamped();

    rune::set_app_id(branding::APP_ID);
    let app = App::new(AppSpec::<UiState> {
        scenes: ui::scenes::scenes(),
        on_key: Some(ui::on_key),
        setup: Some(|s, waker| s.install_images(waker)),
        poll: Some(UiState::poll_splash),
        fonts: platform::fonts::embedded(),
        families: platform::fonts::families(),
        ..AppSpec::new(
            title,
            (geometry.w.max(640) as f32, geometry.h.max(480) as f32),
            || UiState::new(COMMANDS.get().expect("commands").clone()),
            ui::view,
        )
    })
    .compositor(ui::fx::Fx::default())
    .on_focus(|_, focused, _| platform::notify::set_window_focused(focused));

    let handle = app.handle();
    platform::notify::install({
        let handle = handle.clone();
        move |asid, token| {
            // A click on a notification: bring the window up (with the
            // compositor's token on Wayland) and open that session.
            match token {
                Some(token) => handle.activate(token),
                None => handle.open(),
            }
            handle.update(move |s, _| s.open_session(&asid));
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
    ui::spawn_inbox_pump(inbox, handle);

    tracing::info!(
        ms = started.elapsed().as_millis() as u64,
        "opening rune window"
    );
    app.run();
    instance::release();
    store.flush();
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
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
    let shutdown_store = store.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("muqun-core")
        .build()?;
    std::thread::Builder::new()
        .name("muqun-core-main".into())
        .spawn(move || {
            runtime.block_on(async move {
                let hub = select_hub(settings, &state_dir).await;
                tokio::spawn(stop_on_signal(shutdown_store));
                projection::spawn(hub, inbox, store, commands);
                std::future::pending::<()>().await;
            });
        })?;
    Ok(())
}

async fn stop_on_signal(store: Arc<ConfigStore>) {
    let signal = platform::shutdown::requested().await;
    tracing::info!(signal, "stop requested");
    instance::release();
    store.flush();
    std::process::exit(0);
}

async fn select_hub(
    settings: RuntimeSettings,
    state_dir: &std::path::Path,
) -> Arc<dyn hub::SessionHub> {
    if let Some(gateway) = hub::discover_local_gateway().await {
        match hub::RemoteHub::start(gateway.clone()) {
            Ok(remote) => {
                tracing::info!("hub: local gateway at {}", gateway.url);
                return remote;
            }
            Err(err) => tracing::warn!("local gateway found but unusable: {err:#}"),
        }
    }
    tracing::info!("hub: in-process");
    let agents = build_runtime(settings, true, Some(state_dir.to_path_buf()));
    if let Err(err) = attach_store(&agents, state_dir, StoreEncryption::Off).await {
        tracing::warn!("the session store is unavailable, routes are not kept: {err:#}");
    }
    agents.spawn_supervisor();
    Arc::new(hub::LocalHub::new(agents))
}
