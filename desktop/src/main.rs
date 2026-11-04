//! Muqun Desktop's composition root.
//!
//! One process, two threads (architecture §2): Blinc owns the main thread
//! for the window's whole life; the gateway's agents plane runs on a tokio
//! runtime started beside it before the window opens. They meet only at the
//! command channel (UI → core) and the inbox (core → UI).

mod agent_fields;
mod branding;
mod config;
mod hub;
mod model;
mod platform;
mod theme;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use blinc_app::prelude::*;
use blinc_app::windowed::WindowedApp;
use blinc_layout::widgets::text_area::text_area_state_with_placeholder;
use muqun_gateway::agents::compose::{
    attach_store, build_runtime, RuntimeSettings, StoreEncryption,
};

use crate::config::{ConfigEdit, DesktopConfig, WindowGeometry, CONFIG_FILE};
use crate::model::commands::{self, Command, CommandReceiver};
use crate::model::drain::Drain;
use crate::model::inbox::{Inbox, UiAction};
use crate::model::projection::{self, ConfigStore};
use crate::model::view::Bindings;
use crate::platform::coexist;

/// UI builder invocations (full-tree builds), for the idle measurement.
static ROOT_BUILDS: AtomicU64 = AtomicU64::new(0);

fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    // Before anything else from the gateway crate: every path it resolves
    // (message images, patches, the store) must land in our directories,
    // never in a gateway service's.
    let (config_dir, state_dir) = platform::directories()?;
    muqun_gateway::set_directories(config_dir.clone(), state_dir.clone())?;
    init_tracing();
    tracing::info!(
        config = %config_dir.display(),
        state = %state_dir.display(),
        "muqun-desktop starting (main entry)"
    );

    let config_path = config_dir.join(CONFIG_FILE);
    let config = load_config(&config_path);
    let store = ConfigStore::new(config_path, config.clone());

    let settings = runtime_settings(&config);

    let inbox = Arc::new(Inbox::new());
    let drain = Drain::new(inbox.clone());
    let (tx, rx) = commands::channel();
    start_core(settings, state_dir.clone(), inbox, store.clone(), rx)?;
    commands::send(&tx, Command::Refresh);

    ui::text::register_fonts();
    // Grammar set is tens of ms: warm it beside the first frames, not before.
    std::thread::spawn(ui::markdown::highlight::warm);
    ui::parts::scale::Density::from_config(config.appearance.density).install();
    ui::parts::scale::install_reduced_motion(config.appearance.reduced_motion);
    let scheme_watcher = theme::install(&config_dir, &state_dir, &config.appearance)?;

    let geometry = config.window.clamped();
    // Cap at 60 and skip Adaptive: the fps_adapter otherwise claws the cap
    // down to ~30 when a tick (or a heavy frame) keeps the loop hot. Set
    // MUQUN_ANIMATION_FPS=adaptive to restore the default heuristic.
    let animation_fps_cap = match std::env::var("MUQUN_ANIMATION_FPS").as_deref() {
        Ok("adaptive") | Ok("Adaptive") => None,
        Ok(raw) => Some(raw.parse::<u32>().unwrap_or(60).max(1)),
        _ => Some(60),
    };
    let window = WindowConfig {
        // Test builds from several worktrees run side by side, and a QA run
        // keeps its window from taking focus by name (`ui/timeline/qa.rs`); a
        // distinct title keeps input scripts and compositor rules aimed at
        // the right window.
        title: std::env::var("MUQUN_WINDOW_TITLE")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| branding::APP_NAME.to_string()),
        width: geometry.w,
        height: geometry.h,
        resizable: true,
        maximized: geometry.maximized,
        position: geometry.x.zip(geometry.y),
        min_size: Some(WindowGeometry::MIN),
        icon: branding::window_icon(),
        animation_fps_cap,
        ..Default::default()
    };

    ui::strings::Strings::set_language(config.general.language);
    let strings = ui::strings::Strings;
    let paths = Arc::new(ui::Paths {
        config_dir: config_dir.clone(),
        state_dir: state_dir.clone(),
    });
    let last_size = Arc::new(Mutex::new((geometry.w, geometry.h)));
    let exit_size = last_size.clone();
    let exit_store = store.clone();
    let mut app: Option<ui::App> = None;
    WindowedApp::run(window, move |ctx| {
        let builds = ROOT_BUILDS.fetch_add(1, Ordering::Relaxed) + 1;
        let app = app.get_or_insert_with(|| {
            tracing::info!(
                since_main_ms = started.elapsed().as_millis() as u64,
                "first frame"
            );
            ui::install_css(ctx);
            let composer = ctx
                .use_state_keyed("mq-composer-text", || {
                    text_area_state_with_placeholder(strings.composer_no_session())
                })
                .get();
            let app = ui::App {
                commands: tx.clone(),
                drain: drain.clone(),
                bindings: Bindings::new(ctx.use_scroll_ref_keyed("mq-timeline")),
                strings,
                config: store.clone(),
                composer,
                scheme_watcher: scheme_watcher.clone(),
                paths: paths.clone(),
            };
            let for_actions = app.clone();
            drain.attach(
                app.bindings.clone(),
                Arc::new(move |action| match action {
                    UiAction::SubmitComposer => ui::composer::submit_from_button(&for_actions),
                    UiAction::ComposerEdited => ui::composer::edited(&for_actions),
                    UiAction::ComposerKey { key, line, column } => {
                        ui::composer::on_key(&for_actions, key, line, column)
                    }
                    UiAction::Open(asid) => {
                        for_actions.open_session(&muqun_gateway::agents::AgentSessionId(asid))
                    }
                    UiAction::ToggleDiffPanel => {
                        ui::shell::toggle_right(&for_actions, crate::model::view::RightTab::Git);
                    }
                    UiAction::OpenDiffFile { path, .. } => {
                        ui::shell::open_diff_file(&for_actions, &path);
                    }
                    UiAction::ProjectPicked(cwd) => ui::composer::project_picked(cwd),
                    UiAction::FilesPicked(paths) => ui::composer::files_picked(&for_actions, paths),
                    UiAction::FoldersDropped(paths) => {
                        ui::composer::folders_dropped(&for_actions, paths)
                    }
                    UiAction::DropHover(on) => ui::composer::drop_hover(on),
                    UiAction::QaStep(step) => ui::composer::qa::run(&for_actions, &step),
                    UiAction::ImagePasted(png) => ui::composer::image_pasted(&for_actions, png),
                }),
            );
            ui::timeline::qa::start(&app);
            ui::composer::qa::start(&app);
            app
        });
        tracing::debug!(builds, width = ctx.width, height = ctx.height, "build_ui");
        let size = (ctx.width.round() as u32, ctx.height.round() as u32);
        let mut last = last_size.lock().unwrap();
        if *last != size && size.0 > 0 && size.1 > 0 {
            *last = size;
            app.send(Command::Edit(ConfigEdit::Window(WindowGeometry {
                w: size.0,
                h: size.1,
                ..geometry
            })));
        }
        drop(last);
        ui::sync_agent_colours(ctx);
        ui::root(ctx, app)
    })?;

    // The window is gone: write its last size now, not after a debounce
    // that will never fire.
    let (w, h) = *exit_size.lock().unwrap();
    exit_store.change(|config| {
        config.window.w = w;
        config.window.h = h;
    });
    exit_store.flush();
    tracing::info!(
        root_builds = ROOT_BUILDS.load(Ordering::Relaxed),
        "muqun-desktop exiting"
    );
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info,blinc_app::rebuild_source=warn")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// The configuration, or the defaults. A file that does not parse is moved
/// aside rather than silently overwritten by the next save.
fn load_config(path: &std::path::Path) -> DesktopConfig {
    match DesktopConfig::load(path) {
        Ok(config) => config,
        Err(err) => {
            let aside = path.with_extension("json.invalid");
            tracing::error!(
                "{err:#}; starting with defaults and keeping the file as {}",
                aside.display()
            );
            if let Err(err) = std::fs::rename(path, &aside) {
                tracing::warn!("could not move it aside: {err}");
            }
            DesktopConfig::default()
        }
    }
}

/// Every agent the gateway has, with the gateway's defaults (via the
/// config's `#[serde(default)]` blocks) and the two coexistence overrides of
/// architecture §4: OpenCode is only adopted while a gateway service runs
/// here, and a DeepSeek harness is only ever adopted, never spawned -- two
/// processes racing to spawn it on one port would fight.
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

/// SIGTERM / SIGINT: ask the window to close, which ends `WindowedApp::run`
/// and lets `main` return `Ok` (exit status 0, with the last size saved). If
/// there is no window to close yet, or it does not go in time, leave anyway,
/// still with status 0: being told to stop is not a failure.
async fn stop_on_signal(store: Arc<ConfigStore>) {
    let signal = platform::shutdown::requested().await;
    tracing::info!(signal, "stop requested; closing the window");
    blinc_layout::window_actions::close_window();
    tokio::time::sleep(platform::shutdown::GRACE).await;
    tracing::warn!("the window did not close in time; exiting");
    store.flush();
    std::process::exit(0);
}

/// The tokio runtime and the agents plane, on their own thread.
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
                // A gateway service on this computer owns the one agents
                // plane (and shares its sessions live with the phone); only
                // without one does this process run its own.
                let hub = select_hub(settings, &state_dir).await;
                tokio::spawn(stop_on_signal(shutdown_store));
                projection::spawn(hub, inbox, store, commands);
                // Live until the process ends with the window.
                std::future::pending::<()>().await;
            });
        })?;
    Ok(())
}

/// The gateway on this computer over loopback when it answers, else the
/// agents plane in this process. The in-process runtime is not even built
/// in the first case: no agent child process belongs to the desktop.
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
