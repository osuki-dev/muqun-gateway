#![recursion_limit = "256"]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::http::StatusCode;
use axum::Json;
use clap::Parser;
use serde_json::Value;

pub(crate) mod agents;
pub(crate) mod cli;
pub(crate) mod connectivity;
pub(crate) mod platform;
pub(crate) mod terminal;

// Backward-compatible re-exports at crate root
pub(crate) use agents::{agent_events, approvals, tasks};
pub(crate) use cli::*;
pub(crate) use connectivity::push::*;
pub(crate) use connectivity::{authority, gateway_listener, transport};
pub(crate) use platform::assets::*;
pub(crate) use platform::config::*;
pub(crate) use platform::http::*;
pub(crate) use platform::manage::*;
pub(crate) use platform::metadata::*;
pub(crate) use platform::server::*;
pub(crate) use platform::setup::*;
pub(crate) use platform::store::*;
pub(crate) use platform::uploads::*;
pub(crate) use platform::{discovery, git, i18n, parts, state_lock};
pub(crate) use terminal::factory::*;
pub(crate) use terminal::routes::*;
pub(crate) use terminal::{
    backend, backend_startup, command_catalog, composer, login_env, native, scrollback, shortcuts,
    supervision,
};

use authority::{hash_token, identify_device, DeviceRecord, PendingPairing};
use backend::{
    BackendError, CreateTab as BackendCreateTab, CreateWorkspace as BackendCreateWorkspace,
    OutputFormat as BackendOutputFormat, OutputSource as BackendOutputSource, Pane,
    PaneId as BackendPaneId, ReadPane as BackendReadPane, SendTextMode as BackendSendTextMode,
    SplitDirection as BackendSplitDirection, SplitPane as BackendSplitPane,
    StartAgent as BackendStartAgent, TabId as BackendTabId, WorkspaceId as BackendWorkspaceId,
    WorktreeRequest as BackendWorktreeRequest,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config: Config,
    pub(crate) pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
    pub(crate) pairing_requests: Arc<Mutex<VecDeque<u128>>>,
    pub(crate) push_tokens: Arc<Mutex<Vec<PushTokenRecord>>>,
    pub(crate) devices: Arc<Mutex<Vec<DeviceRecord>>>,
    pub(crate) assets: Arc<Mutex<AssetIndex>>,
    /// What panes with no scrollback of their own showed while the gateway was
    /// watching. Memory only, and only for those panes; see `scrollback`.
    pub(crate) scrollback: Arc<Mutex<scrollback::ScrollbackStore>>,
    /// Selected captured-history storage adapter. Current composition is memory
    /// only; future persistence belongs here, not in HTTP or live frame folding.
    pub(crate) history: Arc<dyn terminal::history::HistoryRepository>,
    /// The agent status transitions this gateway saw, so a phone coming back
    /// after a while can be told what happened. Memory only; see
    /// `agent_events`.
    pub(crate) agent_events: Arc<Mutex<agent_events::AgentEventLog>>,
    pub(crate) approval_events: tokio::sync::broadcast::Sender<ApprovalEvent>,
    /// One activity stream per session, shared by everyone who wants it. See
    /// [`subscribe_activity`].
    pub(crate) activity:
        Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<SessionActivity>>>>,
    /// The last backend liveness ordering, reused briefly so a burst of
    /// clients asking at once is answered once. See [`SESSION_LIVENESS_TTL`].
    pub(crate) session_liveness: Arc<Mutex<SessionLivenessCache>>,
    /// The OpenCode agent, which comes and goes: it is discovered, adopted or
    /// started, and re-attached whenever it moves. Routes ask it for the
    /// current manager rather than holding one.
    pub(crate) agent_runtime: Arc<agents::AgentRuntime>,
    /// The open `GET /api/ws` event sockets, for the per-device and total
    /// caps. See `agents::ws_routes`.
    pub(crate) ws_connections: Arc<agents::ws_routes::WsRegistry>,
    /// This process's instance generation: an opaque id minted once at start
    /// and never changed while the process lives. Everything the gateway
    /// keeps in memory -- the scrollback above all -- is only valid within
    /// one generation, so a client that sees it change drops what it holds
    /// and reads again. See [`new_generation`].
    pub(crate) generation: Arc<str>,
}

/// A fresh instance generation. Opaque to clients: they compare it for
/// equality and nothing else.
pub(crate) fn new_generation() -> Arc<str> {
    uuid::Uuid::new_v4().to_string().into()
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Before any subcommand, because every one of them either spawns a backend
    // program or writes down how to. An init system starts this process with an
    // environment that is not the user's -- and so does `ssh host muqun-gateway
    // setup`, and a `cron` line. See `login_env`.
    //
    // Before the runtime, deliberately. `adopt` writes to the process
    // environment, and setting an environment variable while another thread
    // may be reading one is the data race that made `set_var` unsafe in
    // edition 2024. Here nothing else exists yet: no worker threads, no tasks,
    // just this one thread and its arguments.
    //
    // On stderr rather than stdout: for `run` this is the gateway log, which is
    // where it is wanted, and for a command a human is watching it says nothing
    // at all, because a shell has already given the process everything.
    for note in login_env::adopt() {
        eprintln!("environment repaired from the login shell -- {note}");
    }
    init_tracing();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(dispatch(cli))
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::*;

pub(crate) type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;
