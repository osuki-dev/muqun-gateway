#![recursion_limit = "256"]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::process::{Command as ProcessCommand, Stdio};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use axum::body::{to_bytes, Body};
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse as _, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::{Stream, StreamExt as _};

mod agent_events;
mod approvals;
mod authority;
mod backend;
mod composer;
mod i18n;
mod login_env;
mod native;
mod parts;
mod scrollback;
mod service;
mod shortcuts;
mod state_lock;
mod supervision;
mod tasks;
mod transport;

use authority::{hash_token, identify_device, DeviceRecord, PairingCodeError, PendingPairing};

use crate::i18n::Locale;
use backend::{
    AgentStatus as BackendAgentStatus, BackendActivity, BackendError, BackendFuture, BackendKind,
    BackendRegistry, CreateTab as BackendCreateTab, CreateWorkspace as BackendCreateWorkspace,
    OutputFormat as BackendOutputFormat, OutputSource as BackendOutputSource, Pane,
    PaneId as BackendPaneId, ReadPane as BackendReadPane, SplitDirection as BackendSplitDirection,
    SplitPane as BackendSplitPane, StartAgent as BackendStartAgent, TabId as BackendTabId,
    TerminalBackend, TmuxWireIds, WorkspaceId as BackendWorkspaceId,
    WorktreeRequest as BackendWorktreeRequest, TMUX_PROGRAM,
};

const CONFIG_FILE: &str = "config.json";
const PAIRING_FILE: &str = "pairing.json";
const PUSH_TOKENS_FILE: &str = "push-tokens.json";
const DEVICES_FILE: &str = "devices.json";
/// The generation of `devices.json` that the last write replaced, kept so a
/// device file that goes bad is recoverable rather than terminal.
const DEVICES_BACKUP_FILE: &str = "devices.json.bak";
const PID_FILE: &str = "gateway.pid";
const LOG_FILE: &str = "gateway.log";
const HERDR_PLUGIN_IMPORT_MARKER: &str = ".herdr-plugin-imported";
// Deliberately outside the common-service range and outside the OS ephemeral
// ranges (Linux 32768-60999, macOS 49152-65535) so setup rarely collides.
const DEFAULT_PORT: u16 = 23847;
// Scrollback a phone can page back through. Herdr keeps far more; this is the
// ceiling on a single read, and 1000 ran out after a few screens of an agent
// transcript.
const MAX_OUTPUT_LINES: u32 = 5000;
/// Normalizing costs more than streaming text does, and a phone opens on the
/// last screen or two, so the parts endpoint asks for less than the raw one by
/// default. A client that wants the whole scrollback still says so.
const PARTS_DEFAULT_LINES: u32 = 400;
const MAX_SEND_TEXT_BYTES: usize = 64 * 1024;
const PAIRING_CODE_CHARACTER_COUNT: usize = 8;
const PAIRING_CODE_TTL_MS: u128 = 5 * 60 * 1000;
const MAX_PAIRING_CODE_ATTEMPTS: u8 = 8;
const PAIRING_RATE_LIMIT_WINDOW_MS: u128 = 10 * 60 * 1000;
const MAX_PAIRING_REQUESTS_PER_WINDOW: usize = 6;
const MAX_SEND_KEYS: usize = 32;
/// The argv a task may add to the agent's own command line. Generous enough for
/// any flag list a person types, bounded because everything a client sends is.
const MAX_AGENT_ARGS: usize = 32;
const MAX_AGENT_ARG_CHARS: usize = 512;
const MAX_WORKSPACE_LABEL_CHARS: usize = 120;
/// Long enough for an agent TUI to finish handling a pasted prompt, short
/// enough that the submit still feels like part of the same action.
const SUBMIT_KEYPRESS_DELAY: Duration = Duration::from_millis(150);
/// How often the pane is re-read while waiting for it to stop redrawing. A
/// prompt that names an image file makes Claude Code read and encode that file
/// and then rewrite its own input line into `[Image #N]`, and an Enter that
/// lands inside that window is swallowed outright. The window was measured at
/// under a second for one small image and at over three seconds for three large
/// ones off a cold page cache, so no fixed delay can cover it. A still screen is
/// only ever a hint about when to try: the staging arrives in bursts with quiet
/// gaps longer than this interval, so a pane can look still and not be.
const SUBMIT_SETTLE_INTERVAL: Duration = Duration::from_millis(150);
/// How long Herdr is watched for the agent to react to an Enter before that
/// Enter is written off. A real submission showed up in about half a second on
/// a live pane.
const SUBMIT_VERIFY_WINDOW: Duration = Duration::from_millis(700);
/// How often the agent's state is asked for inside that window.
const SUBMIT_VERIFY_INTERVAL: Duration = Duration::from_millis(150);
/// A beat between an Enter that went nowhere and the next one, so a burst of
/// staging work has room to finish instead of being sampled at the poll rate.
const SUBMIT_RETRY_INTERVAL: Duration = Duration::from_millis(450);
/// How many Enters one prompt is worth when Herdr can say whether the agent
/// took the last one. Every attempt after the first is fired at a pane that has
/// demonstrably not accepted its predecessor, so none of them can be the stray
/// keystroke that answers a permission menu, and the budget can afford to cover
/// a slow staging window.
const SUBMIT_MAX_ATTEMPTS: u32 = 6;
/// The same budget for a pane Herdr lists no agent for. There the only evidence
/// is that the screen moved, and the screen moves on its own, so a submit that
/// cannot be checked properly keeps pressing Enter as few times as possible.
const SUBMIT_BLIND_MAX_ATTEMPTS: u32 = 3;

/// How long a freshly spawned agent is given to become one Herdr can name, and
/// how often the first prompt is offered to it meanwhile. Twelve attempts at
/// three quarters of a second is nine seconds -- longer than any of the four
/// agents takes to draw its first prompt on this hardware, and short enough
/// that a spawn still answers inside a phone's patience.
const SPAWN_PROMPT_ATTEMPTS: u32 = 12;
const SPAWN_PROMPT_INTERVAL: Duration = Duration::from_millis(750);
/// How long such a pane is given to react before it is read back.
const SUBMIT_VERIFY_DELAY: Duration = Duration::from_millis(300);
/// Wall-clock ceiling on the whole settle-send-verify sequence, so a pane that
/// never stops redrawing cannot leave a background task polling Herdr forever.
const SUBMIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often each agent pane is checked for a permission menu. An approval
/// blocks the agent outright, so the phone should hear about one in about the
/// time it takes to glance at the screen; the cost is one `pane.read` per agent
/// pane per tick.
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// A permission menu is a screenful. Reading more only slows the poll down.
const APPROVAL_READ_LINES: u32 = 60;
/// Approval events are published from a background watcher and fanned out to
/// whatever event streams happen to be open. A slow client falls behind rather
/// than holding the watcher up.
const APPROVAL_EVENT_CAPACITY: usize = 64;

/// How many activity events one session's hub buffers for a slow subscriber.
///
/// Generous, because a subscriber that falls behind is dropped events, not
/// backpressure on the producer: a phone on a bad connection would otherwise
/// miss layout changes during a burst. Small enough that a session nobody is
/// reading holds nothing worth counting.
const ACTIVITY_EVENT_CAPACITY: usize = 128;
const MAX_PUSH_TOKENS: usize = 64;
const MAX_DEVICES: usize = 32;
const MAX_DEVICE_NAME_CHARS: usize = 80;
/// Device `last_seen` is kept in memory and only flushed to disk past this
/// interval so that routine polling does not rewrite the file on every request.
const DEVICE_LAST_SEEN_FLUSH_MS: u128 = 5 * 60 * 1000;
const MANAGE_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// Herdr's general `pane.updated` subscription is intentionally coalesced and
/// can arrive several seconds after terminal output is already readable. While
/// a phone is actively viewing one pane, sample that pane locally and publish a
/// frame only when its actual content changes. Herdr's revision is coalesced
/// along with `pane.updated`, so it can remain stale even after `pane.read`
/// already exposes new text. This keeps the mobile SSE live without polling
/// every pane or sending unchanged terminal frames over the network.
const STREAM_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(150);
const STREAM_OUTPUT_READ_TIMEOUT: Duration = Duration::from_millis(100);
const GATEWAY_API_VERSION: &str = "1.7.0";
const GATEWAY_API_MAJOR: u64 = 1;
/// The oldest Herdr socket protocol this gateway knows how to speak.
///
/// There is deliberately no ceiling. `PROTOCOL_VERSION` in Herdr versions its
/// *bincode TUI* client/server link; the JSON socket API this gateway actually
/// speaks merely echoes the same number back from `ping`. Its bumps therefore
/// track terminal input work the gateway never touches -- 17 to 19, across
/// Herdr 0.7.5 to 0.8.0, was repeat counts, a text-commit message and a Kitty
/// keyboard report -- while the JSON schema itself has only ever grown
/// additively: new event types, new optional params, no field removed or
/// retyped. Pinning a maximum turned every future Herdr release into a total
/// outage for changes this gateway does not consume, and `herdr update` puts
/// that one command away. So a newer Herdr is assumed compatible, and a real
/// break is caught where it would actually surface -- the request that fails --
/// rather than by refusing to serve anything at all.
///
/// The floor stays, because old and new are not symmetric. A genuinely ancient
/// Herdr predates JSON fields this gateway requires, so failing fast beats a
/// stream of unrelated errors from every workspace, pane, and event request.
const HERDR_PROTOCOL_MIN: u64 = 17;
const MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;
const UPLOADS_DIR: &str = "uploads";
/// Ceiling on one upload body, enforced by the framework's body limit so an
/// oversized request is cut off mid-stream instead of being buffered first.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// The client's own file name is only ever echoed back, never used to build a
/// path, so this is a display cap rather than a safety one.
const MAX_UPLOAD_NAME_CHARS: usize = 120;
/// Uploads are a handoff to a local agent, not storage: whatever the agent was
/// going to do with a file, it has done long before this.
const UPLOAD_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const UPLOAD_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Version of the unified content model this gateway speaks. Declared on every
/// content envelope so a client renders what it knows and falls back for the
/// rest; additive changes bump the minor. 1.1.0 added the parts endpoint, which
/// is why `capabilities.parts` is now true: nothing in 1.0.0 changed shape.
/// 1.2.0 adds the Codex and opencode marker dictionaries -- more panes answer
/// `parts: "dictionary"` where they used to answer `parts: "text"` -- and again
/// changes no payload's shape. 1.3.0 adds the composer capabilities: a pane's
/// descriptor may now carry `composer`, and the file search endpoint answers in
/// the same envelope. Both are additions -- a 1.2.0 client reads every 1.3.0
/// payload unchanged and simply does not see the new field. 1.4.0 is the v2
/// slice: native protocol adapters feed the same parts, so a pane may answer
/// `parts: "native"` -- a third value of an enum that already had two -- and the
/// closed part set gains `approval`, which an old client renders through
/// `fallback_text` like any type it does not know. Again nothing existing moved.
const CONTENT_SCHEMA_VERSION: &str = "1.4.0";
/// A phone previews artifacts, it does not download archives. Anything larger
/// is refused rather than streamed, so one request can never tie up the host.
const MAX_ASSET_CONTENT_BYTES: u64 = 10 * 1024 * 1024;
const ASSET_CONTENT_CHUNK_BYTES: usize = 64 * 1024;
/// Enough of a file's head to decide what it is. The same bytes settle both the
/// magic-number check and the "is this text" question.
const ASSET_SNIFF_BYTES: usize = 8 * 1024;
/// A workspace scan is a preview mechanism, not an indexer: it stays shallow,
/// stops at a fixed budget, and never descends into a dependency or build
/// directory. Depth 1 is the root's own children.
const ASSET_SCAN_MAX_DEPTH: usize = 4;
const ASSET_SCAN_MAX_ENTRIES: usize = 20_000;
const ASSET_SCAN_MAX_FILES: usize = 4_000;
const ASSET_LIST_DEFAULT_LIMIT: usize = 50;
const ASSET_LIST_MAX_LIMIT: usize = 200;
/// The index is a rolling window of what the workspaces produced recently;
/// the oldest entries are dropped so a long-lived gateway cannot grow without
/// end.
const MAX_INDEXED_ASSETS: usize = 4_000;
/// A worktree event re-scans that root, but only files written around the event
/// are announced: the first scan of an old checkout is not "just created".
const ASSET_EVENT_MAX_AGE_MS: u128 = 10 * 60 * 1000;
const MAX_ASSET_EVENTS_PER_WORKTREE: usize = 20;
/// Directories holding dependencies, build output, or vendored code. None of it
/// is something an agent just produced for the user to look at, and all of it
/// is big enough to swamp a scan. Names starting with a dot are skipped
/// separately, which covers `.git`, `.venv`, `.next`, and friends.
const ASSET_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "Pods",
    "DerivedData",
    "__pycache__",
    "venv",
    "coverage",
];
const API_CAPABILITIES: &[&str] = &[
    "agent_catalog",
    "agent_events",
    "agent_lifecycle_notifications",
    "agent_spawn",
    "assets",
    "device_revocation",
    "file_uploads",
    "one_time_pairing_codes",
    "pane_approvals",
    "pane_composer",
    "pane_file_search",
    "pane_interrupt",
    "pane_output_ansi",
    "pane_parts",
    "pane_parts_native",
    "pane_shortcuts",
    "configurable_agent_profiles",
    "per_device_tokens",
    "push_notifications",
    "push_token_revocation",
    "recent_cwds",
    "tasks",
    "terminal_backends",
    "multiple_terminal_backends",
    "terminal_input",
];

#[derive(Parser)]
#[command(name = "gateway", about = "Mobile gateway for terminal workspaces")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    /// Adopt an existing Herdr plugin pairing into the standalone gateway.
    ImportHerdrPlugin {
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
enum ServiceCommand {
    /// Register the gateway with this user's init system so it starts at login
    /// and comes back after a crash or a reboot.
    Install,
    /// Remove that registration. Pairings, devices and config are untouched.
    Uninstall,
    /// Whether an init system is currently managing the gateway.
    Status,
}

#[derive(Subcommand)]
enum BackendCommand {
    List,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupBackend {
    Herdr,
    Tmux,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum TransportEncryptionMode {
    #[default]
    Required,
    Disabled,
}

impl TransportEncryptionMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Disabled => "disabled",
        }
    }
}

impl From<SetupBackend> for BackendKind {
    fn from(value: SetupBackend) -> Self {
        match value {
            SetupBackend::Herdr => Self::Herdr,
            SetupBackend::Tmux => Self::Tmux,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    server_id: String,
    label: String,
    listen: String,
    public_url: String,
    /// Hash of the admin token. The admin token lives in `pairing.json` and is
    /// used by the local `manage` UI; paired devices get their own tokens.
    token_hash: String,
    /// Controls how newly paired devices authenticate their HTTP transport.
    /// Existing encrypted device records keep working after this changes.
    #[serde(default, skip_serializing_if = "is_required_transport")]
    transport_encryption: TransportEncryptionMode,
    sessions: Vec<SessionConfig>,
    /// Agent kind -> the executable `GET /api/agents/catalog` looks for on
    /// `PATH`. Only needed when a kind's binary is named something else on this
    /// machine; absent, every kind probes for its own name. Optional and
    /// omitted when empty, so an existing `config.json` keeps working untouched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agent_commands: BTreeMap<String, String>,
    /// Put the agent's own question, and the answers it is offering, into a
    /// blocked push.
    ///
    /// Off, and it has to stay off by default. Everything else this gateway
    /// notifies with is locale-free or a name the user typed; this is the one
    /// switch that puts terminal text on a lock screen, and it travels through
    /// Expo's servers and Apple's or Google's to get there. It is worth having
    /// because "Claude is asking something" is not worth unlocking a phone for
    /// and "Run `rm -rf build/`?" is -- but that is the owner's trade to make,
    /// on their own machine, deliberately.
    ///
    /// Omitted from a written config when false, so an existing `config.json`
    /// round-trips untouched.
    #[serde(default, skip_serializing_if = "is_false")]
    rich_agent_pushes: bool,
}

fn is_required_transport(mode: &TransportEncryptionMode) -> bool {
    *mode == TransportEncryptionMode::Required
}

/// `skip_serializing_if` for a flag whose absence is its default.
fn is_false(value: &bool) -> bool {
    !*value
}

impl Config {
    fn port(&self) -> u16 {
        self.listen
            .parse::<SocketAddr>()
            .map(|addr| addr.port())
            .unwrap_or(DEFAULT_PORT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionConfig {
    id: String,
    label: String,
    socket_path: String,
    /// Omitted for every existing config, where Herdr remains the default.
    #[serde(default, skip_serializing_if = "BackendKind::is_herdr")]
    backend: BackendKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingPayload {
    kind: String,
    server_id: String,
    label: String,
    url: String,
    token: String,
    /// QR-only bootstrap secret used to protect pairing before a device token
    /// exists. Older pairing files are upgraded the next time setup runs.
    #[serde(default)]
    transport_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingFile {
    payload: PairingPayload,
}

struct PublicUrlSelection {
    url: String,
    source: String,
    listen_host: String,
}

#[derive(Deserialize)]
struct PairRequestBody {
    request_id: String,
    device_name: Option<String>,
    /// A stable per-install identifier from the client. When present, a new
    /// pairing replaces any earlier device with the same value, so re-pairing a
    /// device does not leave a duplicate record behind.
    #[serde(default)]
    install_id: Option<String>,
}

#[derive(Deserialize)]
struct PairClaimBody {
    request_id: String,
    code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushTokenRecord {
    token: String,
    platform: String,
    device_name: Option<String>,
    /// The language this device asked to be notified in, as the exact code the
    /// app persists (`en` or `zh-TW`).
    ///
    /// A push is built in a watcher spawned at startup, where there is no
    /// request and therefore no header to read, so the language has to have
    /// been remembered from the last one there was. It is optional and
    /// `#[serde(default)]` because a `push-tokens.json` written before this
    /// field existed -- and a client too old to send it -- must keep working;
    /// both simply get English.
    #[serde(default)]
    locale: Option<String>,
    updated_unix_ms: u128,
}

impl PushTokenRecord {
    /// The language to write this device's pushes in. Anything unrecognized,
    /// including nothing at all, is English.
    fn locale(&self) -> Locale {
        self.locale
            .as_deref()
            .and_then(Locale::from_code)
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct RegisterPushTokenBody {
    token: String,
    platform: String,
    device_name: Option<String>,
    /// The language this device wants its notifications in. Additive and
    /// optional: an older client that does not send it registers exactly as it
    /// always did, and the request locale is used instead, which for the app is
    /// the same value it would have sent anyway.
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Deserialize)]
struct UnregisterPushTokenBody {
    token: String,
}

#[derive(Deserialize)]
struct SendPushNotificationBody {
    title: Option<String>,
    body: Option<String>,
    data: Option<serde_json::Map<String, Value>>,
}

/// A push after it has been put into words: exactly what one Expo message
/// carries.
#[derive(Debug, PartialEq, Eq)]
struct AgentPushNotification {
    title: String,
    body: String,
    data: serde_json::Map<String, Value>,
}

/// What happened, in the only three ways this gateway ever raises a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentNotice {
    ApprovalPending,
    AgentBlocked,
    AgentCompleted,
}

/// A push *before* it has been put into words.
///
/// The watchers that raise these are spawned at startup and run for the life of
/// the process; there is no request in scope and therefore no header to read a
/// language off. Choosing the words there would mean choosing one language for
/// every phone. So the ingredients travel instead, and [`AgentPushNotice::render`]
/// is called once per distinct locale among the registered devices -- each phone
/// is notified in the language it registered with.
///
/// Everything held here is either locale-free or a name a person chose: the ids
/// and urls in `data`, the server label the user typed, the agent's own name,
/// and the answers as [`approvals::PushChoice`], which is an index and a
/// decision and nothing the agent wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPushNotice {
    notice: AgentNotice,
    /// The label the user gave this server. Never translated -- it is theirs.
    server_label: String,
    /// The agent's own name. `None` renders as the reader's word for an agent.
    agent_name: Option<String>,
    data: serde_json::Map<String, Value>,
    /// Only an approval has these, and only ever as indices and decisions.
    choices: Vec<approvals::PushChoice>,
    /// The agent's own words, present only when the owner of the machine turned
    /// `rich_agent_pushes` on. The single exception to the rule above, and the
    /// reason the flag exists rather than the behaviour.
    detail: Option<PushDetail>,
}

/// What a blocked push says when the operator has opted into it: the question
/// the agent asked, and the answers it is offering, both verbatim and both cut
/// short.
///
/// Verbatim because a paraphrase of "Run `rm -rf build/`?" is not something to
/// approve from a lock screen, and cut short because a notification is a
/// glance: past a line or two the reader is opening the app anyway, which is
/// the outcome this is trying to make unnecessary rather than the one it is
/// trying to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushDetail {
    question: String,
    option_labels: Vec<String>,
}

/// A glance's worth of the agent's question.
const MAX_PUSH_QUESTION_CHARS: usize = 120;
/// Three answers is what a notification's action row shows; a menu with more
/// is one the user opens the app for.
const MAX_PUSH_OPTIONS: usize = 3;
const MAX_PUSH_OPTION_CHARS: usize = 40;

impl PushDetail {
    fn from_approval(approval: &approvals::Approval) -> Self {
        Self {
            question: truncate(approval.prompt.trim(), MAX_PUSH_QUESTION_CHARS),
            option_labels: approval
                .options
                .iter()
                .take(MAX_PUSH_OPTIONS)
                .map(|option| truncate(option.label.trim(), MAX_PUSH_OPTION_CHARS))
                .collect(),
        }
    }
}

impl AgentPushNotice {
    fn render(&self, locale: Locale) -> AgentPushNotification {
        let (heading, body) = match self.notice {
            AgentNotice::ApprovalPending => {
                ("Approval needed", "{name} is waiting for your approval.")
            }
            AgentNotice::AgentBlocked => ("Agent blocked", "{name} needs your input."),
            AgentNotice::AgentCompleted => ("Agent done", "{name} finished running."),
        };
        // Title carries which server so a multi-server user knows where to
        // look; body carries which agent and what happened. Only the server
        // label (which the user set) and the agent name -- never terminal
        // output or prompts.
        let heading = i18n::t(locale, heading);
        let title = if self.server_label.is_empty() {
            heading.to_owned()
        } else {
            format!("{heading} · {}", truncate(&self.server_label, 32))
        };
        let agent = match &self.agent_name {
            Some(name) => name.clone(),
            None => i18n::t(locale, "Agent").to_owned(),
        };
        let mut data = self.data.clone();
        if !self.choices.is_empty() {
            data.insert(
                "options".into(),
                json!(approvals::push_options(&self.choices, locale)),
            );
        }
        // The agent's own words are not translated and never will be: they are
        // a quotation. When they are here at all, they are also the body -- the
        // question is the whole reason the owner turned this on, and "{name}
        // needs your input." above it would be a line saying nothing.
        let body = match &self.detail {
            Some(detail) => {
                data.insert("question".into(), json!(detail.question));
                if !detail.option_labels.is_empty() {
                    data.insert("option_labels".into(), json!(detail.option_labels));
                }
                detail.question.clone()
            }
            None => i18n::t_slots(locale, body, &[("name", &agent)]),
        };
        AgentPushNotification { title, body, data }
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
    pairing_requests: Arc<Mutex<VecDeque<u128>>>,
    push_tokens: Arc<Mutex<Vec<PushTokenRecord>>>,
    devices: Arc<Mutex<Vec<DeviceRecord>>>,
    assets: Arc<Mutex<AssetIndex>>,
    /// What panes with no scrollback of their own showed while the gateway was
    /// watching. Memory only, and only for those panes; see `scrollback`.
    scrollback: Arc<Mutex<scrollback::ScrollbackStore>>,
    /// The agent status transitions this gateway saw, so a phone coming back
    /// after a while can be told what happened. Memory only; see
    /// `agent_events`.
    agent_events: Arc<Mutex<agent_events::AgentEventLog>>,
    approval_events: tokio::sync::broadcast::Sender<ApprovalEvent>,
    /// One activity stream per session, shared by everyone who wants it. See
    /// [`subscribe_activity`].
    activity: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<SessionActivity>>>>,
    /// The last backend liveness ordering, reused briefly so a burst of
    /// clients asking at once is answered once. See [`SESSION_LIVENESS_TTL`].
    session_liveness: Arc<Mutex<SessionLivenessCache>>,
}

/// The scrollback store, or nothing if a previous holder panicked while it was
/// locked.
///
/// A poisoned buffer is not worth failing a read over: the pane's own answer
/// from Herdr is still correct, only shorter. Every caller treats `None` as
/// "this gateway keeps no history", which is exactly what release/0.5.0 did.
fn lock_scrollback(
    state: &AppState,
) -> Option<std::sync::MutexGuard<'_, scrollback::ScrollbackStore>> {
    state.scrollback.lock().ok()
}

#[derive(Deserialize)]
struct OutputQuery {
    source: Option<String>,
    lines: Option<u32>,
    format: Option<String>,
    /// Absolute half-open range. Both or neither; the range wins over `lines`.
    start: Option<u32>,
    end: Option<u32>,
}

#[derive(Deserialize)]
struct SendTextBody {
    text: String,
}

#[derive(Deserialize)]
struct SendKeysBody {
    keys: Vec<String>,
}

/// How a client answers a pending approval: by option number, or by what the
/// answer means. `fingerprint` is optimistic concurrency -- send back the one
/// the approval was read with and a menu that changed underneath rejects the
/// answer instead of taking it.
#[derive(Deserialize)]
struct AnswerApprovalBody {
    #[serde(default)]
    option: Option<u32>,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
}

/// One published approval transition, fanned out to open event streams.
#[derive(Clone, Debug)]
struct ApprovalEvent {
    name: &'static str,
    payload: String,
}

#[derive(Deserialize)]
struct CreateWorkspaceBody {
    cwd: Option<String>,
    label: Option<String>,
    focus: Option<bool>,
}

#[derive(Deserialize)]
struct RenameWorkspaceBody {
    label: String,
}

#[derive(Deserialize)]
struct CreateTabBody {
    workspace_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    focus: Option<bool>,
}

#[derive(Deserialize)]
struct RenameTabBody {
    label: String,
}

#[derive(Deserialize)]
struct RenamePaneBody {
    label: String,
}

#[derive(Deserialize)]
struct SplitPaneBody {
    direction: String,
    ratio: Option<f64>,
    command: Option<Vec<String>>,
    cwd: Option<String>,
    env: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
struct ZoomPaneBody {
    mode: Option<String>,
}

#[derive(Deserialize)]
struct AgentSendBody {
    text: String,
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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(dispatch(cli))
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
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
        Command::ImportHerdrPlugin {
            config_dir,
            state_dir,
            target_config_dir,
            target_state_dir,
        } => import_herdr_plugin(config_dir, state_dir, target_config_dir, target_state_dir)?,
        Command::Devices => list_devices()?,
        Command::Revoke { device_id, all } => revoke_device(device_id, all)?,
    }
    Ok(())
}

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
fn resolve_setup_backend(explicit: Option<BackendKind>, existing: Option<&Config>) -> BackendKind {
    explicit.unwrap_or_else(|| {
        existing
            .and_then(|config| config.sessions.first())
            .map(|session| session.backend)
            .unwrap_or(BackendKind::Tmux)
    })
}

fn setup(
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
            sessions: Vec::new(),
            agent_commands: BTreeMap::new(),
            rich_agent_pushes: false,
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

struct ExistingInstall {
    config: Config,
    pairing: PairingFile,
}

fn load_existing_install(
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

fn ensure_backend_available(backend: BackendKind) -> anyhow::Result<()> {
    backend_registry().ensure_available(backend)
}

fn default_backend_socket(backend: BackendKind) -> String {
    backend_registry().default_socket(backend)
}

fn default_backend_label(backend: BackendKind) -> &'static str {
    backend_registry().default_label(backend)
}

fn backend_registry() -> BackendRegistry {
    BackendRegistry::new(
        std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .unwrap_or_else(default_socket_path),
    )
}

fn backend_endpoint(session: &SessionConfig) -> String {
    backend_registry()
        .endpoint(session.backend, &session.socket_path)
        .into_owned()
}

fn validate_session_id(id: &str) -> anyhow::Result<()> {
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

fn next_backend_id(config: &Config, backend: BackendKind) -> String {
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

fn upsert_backend_session(
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
    // Neither `GET /api/sessions` nor `gateway_metadata`'s "primary" session
    // for `/health` and `/api/meta` serve in this stored order any more --
    // both reorder every request by which backend actually has something
    // live in it (see `session_order_key` / `ordered_sessions`). What this
    // stored order still governs: the order `configure_backend list` and
    // `config.json` present to a human. Stable, so two sessions of the same
    // backend keep the order they were added in.
    config
        .sessions
        .sort_by_key(|session| match session.backend {
            BackendKind::Tmux => 0,
            BackendKind::Herdr => 1,
        });
    Ok(id)
}

fn validate_label(label: &str) -> anyhow::Result<()> {
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

fn write_config(path: &std::path::Path, config: &Config) -> anyhow::Result<()> {
    write_secret_file(path, &serde_json::to_vec_pretty(config)?)
        .with_context(|| format!("failed to write config {}", path.display()))
}

fn configure_backend(command: BackendCommand) -> anyhow::Result<()> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    match command {
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

fn make_backend_default(config: &mut Config, id: &str) -> anyhow::Result<()> {
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

fn import_herdr_plugin(
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
    let running = gateway_listener_pids(source.config.port()).unwrap_or_default();
    anyhow::ensure!(
        running.is_empty(),
        "stop the running gateway before importing its pairing identity"
    );

    let target_config_dir = target_config_dir.unwrap_or(standalone_config_dir()?);
    let target_state_dir = target_state_dir.unwrap_or(standalone_state_dir()?);
    std::fs::create_dir_all(&target_config_dir)?;
    std::fs::create_dir_all(&target_state_dir)?;
    // This merges records into the target's device file. A gateway running
    // against that directory holds the pre-merge list in memory and would
    // write it back over the merge at its next pairing, so the import has to
    // own the directory outright while it runs.
    let _target_lock = state_lock::StateLock::acquire(&target_state_dir)
        .context("cannot import into a state directory a gateway is using")?;
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

fn backup_secret_file(path: &std::path::Path) -> anyhow::Result<()> {
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

fn merge_plugin_state<T, F>(
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

fn auto_public_url(port: u16) -> PublicUrlSelection {
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

fn tailscale_status_json() -> Option<Value> {
    let output = ProcessCommand::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn tailscale_serve_https_domain(port: u16) -> Option<String> {
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

fn proxy_targets_port(proxy: &str, port: u16) -> bool {
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
fn run_service_command(command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install => {
            stop_background_inner(false)?;
            service::install(&service_paths()?)?;
            println!("The gateway now starts when you log in, and restarts if it stops.");
            println!("Undo it with: muqun-gateway service uninstall");
        }
        ServiceCommand::Uninstall => {
            service::uninstall()?;
            // Removing the agent stops the process it was supervising, and the
            // reader did not ask for their phone to lose the machine -- only
            // for it to stop coming back by itself. So hand it back to the
            // detached-child path it would have been on all along.
            //
            // The stop is not redundant. `launchctl bootout` returns before the
            // process it booted out has gone, and that process still owns the
            // state directory, so starting the replacement immediately loses
            // the lock race and dies -- observed: uninstall then reported the
            // lock error and left nothing running, which is the one outcome
            // this branch exists to prevent. Nothing can revive it now that the
            // unit is gone, so stopping first is safe as well as necessary.
            stop_background_inner(false)?;
            // Best effort by design. The registration is already gone, which is
            // what was asked for; failing the whole command because the
            // replacement did not come up would report the part that worked as
            // a failure, and leave the reader with no idea which half happened.
            match start_background_inner(false) {
                Ok(()) => println!(
                    "The gateway is still running, but it will not come back after a reboot."
                ),
                Err(error) => {
                    println!("Autostart is removed, but the gateway did not restart: {error}");
                    println!("Start it again with: muqun-gateway start");
                }
            }
        }
        ServiceCommand::Status => {
            // Only ever asked once there is a config to ask about. Without one
            // `configured_port` falls back to the default port, and the
            // listener check then reports whatever else is on it -- another
            // account's gateway, or one this install knows nothing about -- as
            // "running". A fresh install would be told it was already up.
            let running = match load_config(None) {
                Ok(config) => Some(
                    read_pid()?.is_some_and(process_running)
                        || !gateway_listener_pids(config.port())?.is_empty(),
                ),
                Err(_) => None,
            };
            match service::state()? {
                service::ServiceState::Installed => {
                    println!("service: installed ({})", service::unit_path()?.display());
                }
                service::ServiceState::FileOnly => {
                    println!(
                        "service: a unit file exists at {} but the init system has not loaded it.",
                        service::unit_path()?.display()
                    );
                    println!("Re-run `muqun-gateway service install` to repair it.");
                }
                service::ServiceState::NotInstalled => {
                    println!("service: not installed -- the gateway will not survive a reboot.");
                    println!("Install it with: muqun-gateway service install");
                }
            }
            match running {
                Some(true) => println!("gateway: running"),
                Some(false) => println!("gateway: not running"),
                None => println!("gateway: not configured yet -- run `muqun-gateway setup`"),
            }
        }
    }
    Ok(())
}

fn service_paths() -> anyhow::Result<service::ServicePaths> {
    let (path, lc_ctype) = login_env::for_unit_file();
    Ok(service::ServicePaths {
        exe: std::env::current_exe().context("failed to find current executable")?,
        config: config_dir()?.join(CONFIG_FILE),
        log: state_dir()?.join(LOG_FILE),
        home: dirs::home_dir().context("failed to locate the home directory")?,
        path,
        lc_ctype,
    })
}

fn start_background() -> anyhow::Result<()> {
    start_background_inner(true)
}

fn start_background_inner(verbose: bool) -> anyhow::Result<()> {
    let state_dir = state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create state dir {}", state_dir.display()))?;
    if let Some(pid) = read_pid()? {
        if process_running(pid) {
            if verbose {
                println!("gateway already running with pid {pid}");
            }
            return Ok(());
        }
    }
    // The pid file only knows about gateways this subcommand started. One
    // launched by systemd, or by hand, leaves no pid file at all -- and that
    // is precisely the pairing-losing case. Ask the state directory instead,
    // then hand the lock straight to the child. Without this the spawn below
    // would "succeed" and the child would die a moment later in the log,
    // where nobody is looking.
    drop(state_lock::StateLock::acquire(&state_dir)?);

    let exe = std::env::current_exe().context("failed to find current executable")?;
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
    let child = command.spawn().context("failed to start gateway")?;
    let pid = child.id();
    std::fs::write(state_dir.join(PID_FILE), pid.to_string())?;
    if verbose {
        println!("gateway started with pid {pid}");
    }
    Ok(())
}

#[cfg(unix)]
fn detach_background_process(command: &mut ProcessCommand) {
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
fn detach_background_process(_command: &mut ProcessCommand) {}

fn stop_background() -> anyhow::Result<()> {
    stop_background_inner(true)
}

/// The port this installation actually listens on, falling back to the default
/// when the config is missing or unreadable.
fn configured_port() -> u16 {
    load_config(None)
        .map(|config| config.port())
        .unwrap_or(DEFAULT_PORT)
}

fn stop_background_inner(verbose: bool) -> anyhow::Result<()> {
    let mut stopped = false;
    // Killing a supervised gateway by pid is never what anyone means: a
    // `Restart=always` unit immediately brings it back -- or, worse, loses
    // the state-directory lock race to whatever this stop was making room
    // for and then fails its restart every few seconds indefinitely. Skip
    // such pids and tell the operator the command that actually works.
    let mut supervised: Option<supervision::SystemdUnit> = None;
    let mut refuse_or_stop = |pid: u32, label: &str| -> anyhow::Result<()> {
        if let Some(unit) = supervision::managing_gateway_unit(pid) {
            if verbose {
                println!(
                    "gateway pid {pid} is managed by systemd as {}; leaving it to its supervisor",
                    unit.unit
                );
            }
            supervised.get_or_insert(unit);
            return Ok(());
        }
        stop_pid(pid)?;
        stopped = true;
        if verbose {
            println!("gateway stopped {label} {pid}");
        }
        Ok(())
    };

    if let Some(pid) = read_pid()? {
        if process_running(pid) {
            refuse_or_stop(pid, "pid")?;
        } else if verbose {
            println!("gateway pid file exists, but pid {pid} is not running");
        }
    }

    let port = configured_port();
    for pid in gateway_listener_pids(port)? {
        if process_running(pid) {
            refuse_or_stop(pid, "listener pid")?;
        }
    }

    remove_pid_file()?;
    if let Some(unit) = supervised {
        anyhow::bail!(
            "the running gateway is managed by systemd as {}; stop it with `{}`",
            unit.unit,
            unit.systemctl("stop")
        );
    }
    if verbose && !stopped {
        println!("gateway is not running");
    }
    // Killing the process is not stopping it once an init system is watching:
    // KeepAlive and Restart=always both put it straight back, so a reader who
    // ran `stop` and then found it running would have every reason to think the
    // command was broken. Say which one is holding it up instead.
    if verbose && stopped && service::is_installed() {
        println!(
            "note: the installed service will start it again. Run `muqun-gateway service uninstall`\n\
             to stop it for good."
        );
    }
    Ok(())
}

async fn run(config_path: Option<String>) -> anyhow::Result<()> {
    // Taken before the device list is read and held for the life of the
    // process. This gateway caches the whole list in memory and rewrites the
    // whole file on every change, so a second gateway against the same
    // directory would not interleave with it -- it would overwrite it, and
    // whichever devices the loser had paired would be silently unpaired.
    //
    // The binding has to be named: `let _ = ...` would drop the lock on the
    // spot and leave this gateway believing it owned a directory it had
    // already released.
    let _state_lock = state_lock::StateLock::acquire(&state_dir()?)?;
    ensure_pairing_transport_key()?;
    let config = load_config(config_path)?;
    warn_about_missing_backend_programs(&config);
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    // Read before `config` moves into the state, and printed after the routes
    // are built so it is the last thing on screen rather than the first.
    let listen_warning = unreachable_listen_warning(&config.listen, &config.public_url);

    let state = AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(load_push_tokens_for_service())),
        devices: Arc::new(Mutex::new(load_devices_for_service()?)),
        assets: Arc::new(Mutex::new(AssetIndex::default())),
        scrollback: Arc::new(Mutex::new(scrollback::ScrollbackStore::default())),
        agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
        approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
        activity: Arc::new(Mutex::new(HashMap::new())),
        session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
    };
    spawn_agent_notification_watchers(state.clone());
    spawn_approval_watchers(state.clone());
    spawn_upload_gc();

    let app = Router::new()
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi_json))
        .route("/api/pair/request", post(pair_request))
        .route("/api/pair/claim", post(pair_claim))
        .route("/api/pair/pending", get(pair_pending))
        .route("/api/meta", get(api_meta).patch(api_set_label))
        .route("/api/pairings", get(list_paired_devices))
        .route(
            "/api/pairings/{device_id}",
            axum::routing::delete(revoke_paired_device),
        )
        .route(
            "/api/devices/push-token",
            post(register_push_token).delete(unregister_push_token),
        )
        .route("/api/notifications/test", post(send_test_notification))
        .route("/health", get(health))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{session_id}/events", get(events))
        .route("/api/sessions/{session_id}/snapshot", get(snapshot))
        .route(
            "/api/sessions/{session_id}/workspaces",
            get(workspaces).post(create_workspace),
        )
        .route(
            "/api/sessions/{session_id}/workspaces/{workspace_id}/focus",
            post(focus_workspace),
        )
        .route(
            "/api/sessions/{session_id}/workspaces/{workspace_id}",
            patch(rename_workspace).delete(close_workspace),
        )
        .route(
            "/api/sessions/{session_id}/tabs",
            get(tabs).post(create_tab),
        )
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}/focus",
            post(focus_tab),
        )
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}",
            patch(rename_tab).delete(close_tab),
        )
        .route("/api/keymaps", get(keymaps))
        .route("/api/agents/catalog", get(agents_catalog))
        .route(
            "/api/sessions/{session_id}/agent-events",
            get(session_agent_events),
        )
        .route("/api/sessions/{session_id}/tasks", post(create_task))
        // Two spellings, one handler. The card said `agents/spawn` and the
        // route said `spawn`, so New Task 404'd against every real gateway
        // while both halves' tests passed against their own idea of the path.
        // Serving both is cheaper than a flag day between an app in a store
        // and a gateway a user updates when they feel like it.
        .route("/api/sessions/{session_id}/spawn", post(spawn_agent))
        .route("/api/sessions/{session_id}/agents/spawn", post(spawn_agent))
        .route("/api/sessions/{session_id}/recent-cwds", get(recent_cwds))
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/interrupt",
            post(interrupt_pane),
        )
        // The app names an agent by its pane; the route said pane and the
        // client said agent. Same handler, same target, both spellings.
        .route(
            "/api/sessions/{session_id}/agents/{pane_id}/interrupt",
            post(interrupt_pane),
        )
        .route("/api/sessions/{session_id}/panes", get(panes))
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}",
            get(pane).patch(rename_pane).delete(close_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/focus",
            post(focus_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/split",
            post(split_pane),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/zoom",
            post(zoom_pane),
        )
        .route("/api/sessions/{session_id}/agents", get(agents))
        .route("/api/sessions/{session_id}/agents/{target}", get(agent))
        .route(
            "/api/sessions/{session_id}/agents/{target}/focus",
            post(focus_agent),
        )
        .route(
            "/api/sessions/{session_id}/agents/{target}/send",
            post(send_agent),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/shortcuts",
            get(pane_shortcuts),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/output",
            get(pane_output),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/parts",
            get(pane_parts),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/files",
            get(pane_files),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/approval",
            get(pane_approval).post(answer_pane_approval),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/send-text",
            post(send_text),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/send-keys",
            post(send_keys),
        )
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}/assets",
            get(session_assets),
        )
        .route("/api/assets/{asset_id}/content", get(asset_content))
        // A route-level limit is applied inside the router-wide one, so uploads
        // get their own ceiling while every JSON route keeps the small one.
        .route(
            "/api/uploads",
            post(upload_file).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            encrypted_transport,
        ))
        .layer(middleware::from_fn_with_state(
            known_hosts(&state.config),
            known_host,
        ))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(request_locale))
        .with_state(state);

    if let Some(warning) = listen_warning {
        eprintln!("{warning}");
    }
    println!("terminal gateway listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Put the caller's language in scope for the whole request.
///
/// This is the outermost layer because every route under it can refuse, and a
/// refusal is the most common thing a person reads from this gateway. It reads
/// the headers once and hands the answer to [`i18n::scope`]; nothing below has
/// to remember to ask, and nothing below can ask for a locale the resolver
/// would not have given it.
async fn request_locale(request: Request<Body>, next: Next) -> Response {
    let locale = Locale::from_headers(request.headers());
    i18n::scope(locale, next.run(request)).await
}

const TRANSPORT_HEADER: &str = "x-muqun-transport";
const TRANSPORT_DEVICE_HEADER: &str = "x-muqun-device";
const TRANSPORT_ENVELOPE_HEADER: &str = "x-muqun-envelope";
const TRANSPORT_PROOF_HEADER: &str = "x-muqun-internal-device-proof";

#[derive(Serialize, Deserialize)]
struct EncryptedRequestPayload {
    token: String,
    #[serde(default)]
    content_type: Option<String>,
    body: String,
}

#[derive(Serialize)]
struct EncryptedResponsePayload {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

/// What an encrypted-transport request leaves behind for a handler that
/// answers with a stream instead of a finite body. A whole-response envelope
/// cannot authenticate a response that never ends, so the events handler
/// seals each event on its own -- and this is the key material and request
/// binding it seals under. Injected by `decrypt_transport_request`, so its
/// presence also proves the request itself authenticated.
#[derive(Clone)]
struct EncryptedStreamContext {
    material: Vec<u8>,
    request_aad: String,
    request_nonce: String,
}

/// Decrypt an authenticated request before Axum extractors see it, then seal
/// the complete response. Route names and byte counts remain HTTP metadata;
/// credentials and application payloads do not.
async fn encrypted_transport(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // The proof header is this middleware's own signal to the handlers below:
    // "this request really arrived sealed, and here is the key it was sealed
    // with". Only the decryption path may set it. A client could otherwise
    // send it itself on the cleartext path and be taken for the encrypted
    // device it is claiming to be -- and, worse, would have put the transport
    // key on the wire in the clear to do so, which is the one thing the
    // encrypted transport exists to prevent. Strip it on the way in, always,
    // before anything else looks at the request.
    let mut request = request;
    request.headers_mut().remove(TRANSPORT_PROOF_HEADER);

    if request
        .headers()
        .get(TRANSPORT_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return next.run(request).await;
    }

    match decrypt_transport_request(&state, request).await {
        Ok((request, material, aad, request_nonce)) => {
            let response = next.run(request).await;
            // An event stream never ends, so it cannot ride the one-envelope
            // response path -- buffering it here would simply hang the
            // connection. The events handler has already sealed every event
            // individually under the stream context injected above, so the
            // response passes through as standard SSE.
            if response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
            {
                let mut response = response;
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(TRANSPORT_HEADER),
                    HeaderValue::from_static("1"),
                );
                return response;
            }
            encrypt_transport_response(response, &material, &aad, &request_nonce).await
        }
        Err(error) => error.into_response(),
    }
}

async fn decrypt_transport_request(
    state: &AppState,
    request: Request<Body>,
) -> ApiResult<(Request<Body>, Vec<u8>, String, String)> {
    let (mut parts, body) = request.into_parts();
    let device_id = parts
        .headers
        .get(TRANSPORT_DEVICE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 80)
        .ok_or_else(|| {
            api_error(
                StatusCode::UNAUTHORIZED,
                "missing_transport_device",
                "missing encrypted transport device",
            )
        })?;
    let (token_hash, transport_key) = {
        let devices = lock_devices(state)?;
        let device = devices
            .iter()
            .find(|device| device.id == device_id)
            .ok_or_else(|| api_error(StatusCode::FORBIDDEN, "invalid_token", "invalid token"))?;
        let transport_key = device.transport_key.clone().ok_or_else(|| {
            api_error(
                StatusCode::UPGRADE_REQUIRED,
                "device_repair_required",
                "pair this device again to enable encrypted transport",
            )
        })?;
        (device.token_hash.clone(), transport_key)
    };
    let material = transport::decode_key(&transport_key).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "transport_key_unavailable",
            "encrypted transport is unavailable",
        )
    })?;
    let aad = format!(
        "{} {}",
        parts.method,
        parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), |value| value.as_str())
    );
    let body_bytes = to_bytes(body, MAX_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_envelope",
                "invalid encrypted request",
            )
        })?;
    let envelope_bytes = match parts.headers.get(TRANSPORT_ENVELOPE_HEADER) {
        Some(value) => transport::decode_key(value.to_str().unwrap_or_default()).map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_envelope",
                "invalid encrypted request",
            )
        })?,
        None => body_bytes.to_vec(),
    };
    let envelope: transport::Envelope = serde_json::from_slice(&envelope_bytes).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let plaintext = transport::open(
        &material,
        transport::Direction::Request,
        aad.as_bytes(),
        &envelope,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::FORBIDDEN,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let payload: EncryptedRequestPayload = serde_json::from_slice(&plaintext).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid request",
        )
    })?;
    if hash_token(&payload.token) != token_hash {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }
    // Only authenticated envelopes enter the replay cache. Otherwise anyone
    // who knows a device id could fill it with arbitrary nonces.
    remember_transport_nonce(device_id, &envelope)?;
    let body = transport::decode_key(&payload.body).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid request",
        )
    })?;
    parts.headers.remove(TRANSPORT_HEADER);
    parts.headers.remove(TRANSPORT_DEVICE_HEADER);
    parts.headers.remove(TRANSPORT_ENVELOPE_HEADER);
    parts.headers.insert(
        axum::http::HeaderName::from_static(TRANSPORT_PROOF_HEADER),
        HeaderValue::from_str(&transport_key).map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
        })?,
    );
    parts.headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", payload.token))
            .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid_token", "invalid token"))?,
    );
    if let Some(content_type) = payload.content_type {
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type).map_err(|_| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_content_type",
                    "invalid content type",
                )
            })?,
        );
    } else {
        parts.headers.remove(axum::http::header::CONTENT_TYPE);
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.extensions.insert(EncryptedStreamContext {
        material: material.clone(),
        request_aad: aad.clone(),
        request_nonce: envelope.nonce.clone(),
    });
    Ok((
        Request::from_parts(parts, Body::from(body)),
        material,
        aad,
        envelope.nonce,
    ))
}

fn remember_transport_nonce(device_id: &str, envelope: &transport::Envelope) -> ApiResult<()> {
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashMap<String, u128>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "replay_cache_failed",
                "request failed",
            )
        })?;
    let now = now_unix_ms();
    seen.retain(|_, timestamp| now.abs_diff(*timestamp) <= transport::MAX_CLOCK_SKEW_MS);
    let key = format!("{device_id}:{}", envelope.nonce);
    if seen.insert(key, envelope.timestamp_ms).is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "replayed_request",
            "request was already used",
        ));
    }
    Ok(())
}

async fn encrypt_transport_response(
    response: Response,
    material: &[u8],
    aad: &str,
    request_nonce: &str,
) -> Response {
    let (parts, body) = response.into_parts();
    let body = match to_bytes(body, MAX_UPLOAD_BYTES + MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_too_large",
                "failed to encrypt response",
            )
            .into_response()
        }
    };
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let payload = EncryptedResponsePayload {
        status: parts.status.as_u16(),
        headers,
        body: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body),
    };
    let plaintext = match serde_json::to_vec(&payload) {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let response_aad = format!("{aad}\n{request_nonce}");
    let envelope = match transport::seal(
        material,
        transport::Direction::Response,
        response_aad.as_bytes(),
        &plaintext,
        now_unix_ms(),
    ) {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut response = Json(envelope).into_response();
    response.headers_mut().insert(
        axum::http::HeaderName::from_static(TRANSPORT_HEADER),
        HeaderValue::from_static("1"),
    );
    response
}

/// Refuse a request that arrived under a name this gateway does not answer to.
///
/// The attack this closes is DNS rebinding, and it is the one thing a bearer
/// token does not stop. A page the user opens serves itself from a name the
/// attacker controls, that name is re-resolved to the gateway's address, and
/// from then on the browser considers the page *same-origin with the gateway*.
/// Same-origin means the same-origin policy is no longer in the way: no
/// preflight, and the script reads every response. It has no device token, so
/// the control routes still refuse it -- but the pairing routes are open by
/// necessity, and from there it can read the machine's label, occupy the single
/// pending-pairing slot for five minutes, burn the request budget for ten, and
/// put a name it chose in front of whoever is watching the manager panel.
///
/// The tell is the `Host` header: it carries the attacker's own name, because
/// that is the name the page was fetched from. The gateway knows which names
/// are its own, so it can simply not answer to any other.
///
/// Deliberately generous, because refusing a request the owner meant is worse
/// than the attack. An address literal always passes -- rebinding needs a name
/// whose resolution can be flipped, and a page served from a bare IP is already
/// same-origin with nothing but this gateway. `localhost` and any `.ts.net`
/// name pass, since the tailnet's names are Tailscale's to hand out and not an
/// attacker's. Everything else has to be the host of the configured public URL
/// or of the listen address. A refusal says so in the log, because a name the
/// owner reaches their own gateway by and this gateway has never been told
/// about should be a line to read, not a mystery.
async fn known_host(
    State(known): State<Vec<String>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().host().map(str::to_owned));
    if let Some(host) = host.as_deref() {
        if !host_is_known(host, &known) {
            eprintln!(
                "refused a request addressed to {host}: not a name this gateway answers to \
                 (known: {})",
                known.join(", ")
            );
            return api_error(
                StatusCode::FORBIDDEN,
                "unknown_host",
                "this gateway does not answer to that host name",
            )
            .into_response();
        }
    }
    next.run(request).await
}

/// The names configuration says this gateway is reached by.
fn known_hosts(config: &Config) -> Vec<String> {
    let mut hosts = vec![String::from("localhost")];
    if let Some(host) = reqwest::Url::parse(&config.public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    {
        hosts.push(host_name(&host));
    }
    hosts.push(host_name(&config.listen));
    hosts.retain(|host| !host.is_empty());
    hosts.sort();
    hosts.dedup();
    hosts
}

fn host_is_known(host: &str, known: &[String]) -> bool {
    let name = host_name(host);
    if name.is_empty() {
        return true;
    }
    // Rebinding needs a name. An address is not one, and a page served from an
    // address literal is same-origin with this gateway and nothing else.
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    // MagicDNS names live under a zone Tailscale hands out, not an attacker.
    if name.ends_with(".ts.net") {
        return true;
    }
    known.iter().any(|candidate| candidate == &name)
}

/// A `Host` header, or a `host:port` from configuration, reduced to the name.
fn host_name(host: &str) -> String {
    let host = host.trim();
    // A bracketed IPv6 literal fences the colons of the address itself.
    if let Some(rest) = host.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    // An unbracketed address is not a legal `Host`, but reading one as a name
    // with a port would cut `::1` down to `::`, so it is taken whole.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_ascii_lowercase();
    }
    host.rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(host, |(name, _)| name)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "cache-control",
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

async fn docs() -> Html<&'static str> {
    Html(DOCS_HTML)
}

async fn openapi_json() -> Json<Value> {
    Json(openapi_spec())
}

async fn pair_request(
    State(state): State<AppState>,
    Json(wire): Json<Value>,
) -> ApiResult<Response> {
    let (body, request_nonce) = decode_pairing_body::<PairRequestBody>(
        wire,
        b"POST /api/pair/request",
        transport::Direction::PairingRequest,
    )?;
    require_pairing_transport(state.config.transport_encryption, request_nonce.is_some())?;
    if !valid_request_id(&body.request_id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_id",
            "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        ));
    }
    let device_name = body
        .device_name
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Muqun app".into());
    // The manage UI renders this straight into a terminal, so an unfiltered
    // name could inject ANSI escapes and forge the pairing prompt.
    if !valid_device_name(&device_name) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_device_name",
            "device_name must be at most 80 characters and contain no control characters",
        ));
    }

    let now = now_unix_ms();
    let mut pending_pairing = state.pending_pairing.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_lock_failed",
            "failed to lock pending pairing state",
        )
    })?;
    if let Some(pending) = pending_pairing.as_ref() {
        if !authority::pairing_code_expired(pending, now, PAIRING_CODE_TTL_MS) {
            if pending.request_id == body.request_id {
                return pairing_response(
                    pair_request_response(&state.config, &body.request_id),
                    b"POST /api/pair/request",
                    request_nonce.as_deref(),
                );
            }
            return Err(api_error(
                StatusCode::CONFLICT,
                "pairing_in_progress",
                "another pairing request is awaiting confirmation",
            ));
        }
    }

    record_pairing_request(&state, now)?;

    let code = generate_pairing_code();
    let install_id = body
        .install_id
        .as_deref()
        .filter(|value| valid_install_id(value))
        .map(str::to_owned);
    let pending = PendingPairing {
        request_id: body.request_id.clone(),
        device_name,
        install_id,
        code: code.clone(),
        code_hash: hash_token(&code),
        created_unix_ms: now,
        failed_attempts: 0,
    };
    *pending_pairing = Some(pending);
    pairing_response(
        pair_request_response(&state.config, &body.request_id),
        b"POST /api/pair/request",
        request_nonce.as_deref(),
    )
}

fn record_pairing_request(state: &AppState, now_unix_ms: u128) -> ApiResult<()> {
    let mut requests = state.pairing_requests.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_rate_limit_failed",
            "failed to check pairing request limit",
        )
    })?;
    while requests
        .front()
        .is_some_and(|created| now_unix_ms.saturating_sub(*created) >= PAIRING_RATE_LIMIT_WINDOW_MS)
    {
        requests.pop_front();
    }
    if requests.len() >= MAX_PAIRING_REQUESTS_PER_WINDOW {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "pairing_rate_limited",
            "too many pairing requests; try again later",
        ));
    }
    requests.push_back(now_unix_ms);
    Ok(())
}

async fn pair_claim(State(state): State<AppState>, Json(wire): Json<Value>) -> ApiResult<Response> {
    let (body, request_nonce) = decode_pairing_body::<PairClaimBody>(
        wire,
        b"POST /api/pair/claim",
        transport::Direction::PairingRequest,
    )?;
    require_pairing_transport(state.config.transport_encryption, request_nonce.is_some())?;
    let (device_name, install_id, code) = {
        let mut pending = state.pending_pairing.lock().map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pairing_lock_failed",
                "failed to lock pending pairing state",
            )
        })?;
        let device_name = pending
            .as_ref()
            .map(|value| value.device_name.clone())
            .unwrap_or_else(|| "Muqun app".into());
        let install_id = pending.as_ref().and_then(|value| value.install_id.clone());
        let code = body.code.trim().to_ascii_uppercase();
        authority::consume_pairing_code(
            &mut pending,
            &body.request_id,
            &code,
            now_unix_ms(),
            PAIRING_CODE_TTL_MS,
            MAX_PAIRING_CODE_ATTEMPTS,
        )
        .map_err(|error| match error {
            PairingCodeError::Missing => api_error(
                StatusCode::FORBIDDEN,
                "pairing_not_requested",
                "no pending pairing request",
            ),
            PairingCodeError::Expired => api_error(
                StatusCode::GONE,
                "pairing_code_expired",
                "pairing code expired; request a new code",
            ),
            PairingCodeError::Invalid => api_error(
                StatusCode::FORBIDDEN,
                "invalid_pairing_code",
                "invalid pairing code",
            ),
        })?;
        (device_name, install_id, code)
    };

    // Each device gets its own token so it can be revoked without disturbing
    // the others. The admin token in pairing.json is never handed out.
    let token = generate_token();
    let device_transport_key = (state.config.transport_encryption
        == TransportEncryptionMode::Required)
        .then(generate_token);
    let record = DeviceRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: device_name,
        token_hash: hash_token(&token),
        transport_key: device_transport_key.clone(),
        paired_unix_ms: now_unix_ms(),
        last_seen_unix_ms: now_unix_ms(),
        install_id: install_id.clone(),
    };
    let device_id = record.id.clone();
    {
        let mut devices = lock_devices(&state)?;
        authority::enroll_device(&mut devices, record, MAX_DEVICES);
        write_devices(&devices).map_err(|err| {
            eprintln!("failed to write device tokens: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device_write_failed",
                "failed to save the new device token",
            )
        })?;
    }

    let mut response_payload = json!({
        "kind": "muqun-gateway",
        "server_id": state.config.server_id,
        "label": state.config.label,
        "url": state.config.public_url,
        "token": token,
        "device_id": device_id
    });
    if let Some(device_transport_key) = device_transport_key {
        response_payload["transport_key"] = Value::String(device_transport_key);
        response_payload["transport"] = Value::String("muqun-aes-256-gcm-v1".into());
    }
    let response = if request_nonce.is_some() {
        // Scanned the QR: request and response are both sealed with the
        // pre-shared key it carried. Unchanged from before this card.
        pairing_response(
            response_payload,
            b"POST /api/pair/claim",
            request_nonce.as_deref(),
        )?
    } else if state.config.transport_encryption == TransportEncryptionMode::Required {
        // Typed the address and code: there is no pre-shared key, so the code
        // just spent to authenticate this claim is also what protects the
        // response in transit. See `code_pairing_response` for why that is
        // sound with an eight-character code and what it still costs.
        code_pairing_response(
            response_payload,
            b"POST /api/pair/claim",
            &code,
            &body.request_id,
        )
        .await?
    } else {
        // Encryption is disabled -- the response was never going to be sealed
        // either way.
        pairing_response(response_payload, b"POST /api/pair/claim", None)?
    };
    if request_nonce.is_some() {
        if let Err(err) = rotate_pairing_transport_key() {
            // The device is already enrolled and the response is already sealed
            // with the scanned key. Do not strand it by discarding that response.
            // The manager will generate a fresh key on the next successful write.
            eprintln!("warning: failed to rotate pairing transport key: {err:#}");
        }
    }
    Ok(response)
}

fn pairing_transport_material() -> ApiResult<Vec<u8>> {
    let pairing = read_pairing_file().map_err(|err| {
        eprintln!("failed to read pairing transport key: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_transport_unavailable",
            "encrypted pairing is unavailable",
        )
    })?;
    transport::decode_key(&pairing.payload.transport_key).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_transport_unavailable",
            "encrypted pairing is unavailable",
        )
    })
}

/// Rejects only a request sealed with a key that can no longer be right: an
/// encrypted body arriving while this gateway's encryption is `Disabled`
/// means the caller is holding a QR (or a cached pairing key) from before the
/// owner turned it off.
///
/// This used to also reject the opposite pairing (`Required`, unencrypted)
/// with "scan the Gateway QR ...". That case is not an error anymore: card
/// #821 gave `pair_claim` a second way to authenticate an unencrypted
/// request -- the one-time code itself, checked in `pair_claim` and then
/// spent again as the key that seals the response (see
/// `code_pairing_response`). `pair_request` never carried anything secret, so
/// it never needed the QR's key to begin with.
fn require_pairing_transport(mode: TransportEncryptionMode, encrypted: bool) -> ApiResult<()> {
    if mode == TransportEncryptionMode::Disabled && encrypted {
        return Err(api_error(
            StatusCode::CONFLICT,
            "encrypted_pairing_disabled",
            "transport encryption is disabled on this gateway; scan its current QR code",
        ));
    }
    Ok(())
}

fn decode_pairing_body<T: serde::de::DeserializeOwned>(
    wire: Value,
    aad: &[u8],
    direction: transport::Direction,
) -> ApiResult<(T, Option<String>)> {
    if wire.get("version").is_none() {
        return serde_json::from_value(wire)
            .map(|body| (body, None))
            .map_err(|_| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "invalid request",
                )
            });
    }
    let envelope: transport::Envelope = serde_json::from_value(wire).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let plaintext = transport::open(
        &pairing_transport_material()?,
        direction,
        aad,
        &envelope,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::FORBIDDEN,
            "invalid_envelope",
            "invalid encrypted request",
        )
    })?;
    let nonce = envelope.nonce;
    serde_json::from_slice(&plaintext)
        .map(|body| (body, Some(nonce)))
        .map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid request",
            )
        })
}

fn pairing_response(value: Value, aad: &[u8], request_nonce: Option<&str>) -> ApiResult<Response> {
    let Some(request_nonce) = request_nonce else {
        return Ok(Json(value).into_response());
    };
    let plaintext = serde_json::to_vec(&value).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encoding_failed",
            "failed to encode response",
        )
    })?;
    let response_aad = [aad, b"\n", request_nonce.as_bytes()].concat();
    let envelope = transport::seal(
        &pairing_transport_material()?,
        transport::Direction::PairingResponse,
        &response_aad,
        &plaintext,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encryption_failed",
            "failed to encrypt response",
        )
    })?;
    Ok(Json(envelope).into_response())
}

/// Argon2id parameters for turning a claimed pairing code into transport key
/// material. OWASP's second recommended interactive Argon2id setting (m =
/// 19 MiB, t = 2, p = 1): materially more expensive per guess than a bare
/// hash without being slow enough to make the app visibly stall.
const CODE_KDF_MEMORY_KIB: u32 = 19_456;
const CODE_KDF_TIME_COST: u32 = 2;
const CODE_KDF_PARALLELISM: u32 = 1;
const CODE_KDF_OUTPUT_LEN: usize = 32;

/// The key material a claimed one-time code stands in for the QR's pre-shared
/// key, for exactly one response.
///
/// A scanned QR carries 256 bits of random key; a code a person can read off
/// a screen and type on a phone carries about 40 (eight glyphs from a
/// 32-symbol alphabet -- see `generate_pairing_code`). That gap is real: it
/// is the reason a PAKE is the "actually correctly shaped" answer to this
/// problem (card #821's own words). What closes enough of the gap to ship
/// tonight is that the code is not used as an AES key directly -- it is run
/// through Argon2id first, so an attacker who captures the sealed claim
/// response off the network cannot test candidate codes at hash speed. At
/// roughly 20-50 ms per guess on ordinary hardware, the full 32^8 space is
/// centuries of compute, not the minutes a bare HKDF would cost. Online
/// guessing is separately bounded the way it always was: eight wrong answers
/// burn the code (`MAX_PAIRING_CODE_ATTEMPTS`) and it is five minutes old at
/// most (`PAIRING_CODE_TTL_MS`).
///
/// The salt is derived from `request_id` rather than being random: both sides
/// need to land on the same key without a round trip to agree on a salt, and
/// `request_id` is already a CSPRNG value unique to this one pairing attempt
/// (see `createRequestId` in the app), so it is exactly as good a salt as one
/// generated fresh, minus the round trip. It is hashed to a fixed 32 bytes
/// first because Argon2's own salt floor is 8 bytes and `request_id` is only
/// guaranteed to be 1-80.
fn code_pairing_material(code: &str, request_id: &str) -> anyhow::Result<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use sha2::{Digest as _, Sha256};

    let mut salt = [0_u8; 32];
    salt.copy_from_slice(&Sha256::digest(
        [
            b"muqun-pairing-code-salt-v1".as_slice(),
            request_id.as_bytes(),
        ]
        .concat(),
    ));
    let params = Params::new(
        CODE_KDF_MEMORY_KIB,
        CODE_KDF_TIME_COST,
        CODE_KDF_PARALLELISM,
        Some(CODE_KDF_OUTPUT_LEN),
    )
    .map_err(|err| anyhow::anyhow!("invalid argon2 parameters: {err}"))?;
    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut material = [0_u8; 32];
    hasher
        .hash_password_into(code.as_bytes(), &salt, &mut material)
        .map_err(|err| anyhow::anyhow!("code-derived key material failed: {err}"))?;
    Ok(material)
}

/// Seals a claim response with material derived from the one-time code that
/// just authenticated it, for a device pairing by address and code rather
/// than by QR. See `code_pairing_material` for why an eight-character code
/// run through Argon2id is enough for this one message.
///
/// Argon2id is deliberately memory-hard and therefore not free to run: it is
/// spawned onto a blocking thread so a burst of pairing attempts cannot stall
/// the async runtime's worker threads.
async fn code_pairing_response(
    value: Value,
    aad: &[u8],
    code: &str,
    request_id: &str,
) -> ApiResult<Response> {
    let plaintext = serde_json::to_vec(&value).map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encoding_failed",
            "failed to encode response",
        )
    })?;
    let code = code.to_owned();
    let request_id = request_id.to_owned();
    let material = tokio::task::spawn_blocking(move || code_pairing_material(&code, &request_id))
        .await
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_encryption_failed",
                "failed to encrypt response",
            )
        })?
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_encryption_failed",
                "failed to encrypt response",
            )
        })?;
    let response_aad = [aad, b"\ncode-pairing\n"].concat();
    let envelope = transport::seal(
        &material,
        transport::Direction::PairingResponse,
        &response_aad,
        &plaintext,
        now_unix_ms(),
    )
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_encryption_failed",
            "failed to encrypt response",
        )
    })?;
    Ok(Json(envelope).into_response())
}

fn pair_request_response(config: &Config, request_id: &str) -> Value {
    json!({
        "request_id": request_id,
        "server_id": config.server_id,
        "server_label": config.label,
        "status": "pending",
        "expires_in_ms": PAIRING_CODE_TTL_MS
    })
}

async fn pair_pending(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    // Read by the local manage UI, which holds the admin token: a device has no
    // token of its own until it has read this code and claimed the pairing.
    require_admin(&state.config, &headers)?;
    let mut pending = state.pending_pairing.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_lock_failed",
            "failed to lock pending pairing state",
        )
    })?;
    if pending.as_ref().is_some_and(|value| {
        authority::pairing_code_expired(value, now_unix_ms(), PAIRING_CODE_TTL_MS)
    }) {
        *pending = None;
    }
    if let Some(pending) = pending.as_ref() {
        return Ok(Json(json!({
            "pending": true,
            "request_id": pending.request_id,
            "device_name": pending.device_name,
            "code": pending.code,
            "created_unix_ms": pending.created_unix_ms,
            "expires_unix_ms": pending.created_unix_ms + PAIRING_CODE_TTL_MS,
            "expires_in_ms": (pending.created_unix_ms + PAIRING_CODE_TTL_MS).saturating_sub(now_unix_ms())
        })));
    }
    Ok(Json(json!({ "pending": false })))
}

async fn register_push_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterPushTokenBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_push_token(&body.token)?;
    let platform = body.platform.trim().to_ascii_lowercase();
    if platform != "ios" && platform != "android" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_platform",
            "platform must be ios or android",
        ));
    }

    let mut tokens = state.push_tokens.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_lock_failed",
            "failed to lock push token state",
        )
    })?;
    // A body that names a language wins; a body that does not falls back to the
    // headers this very request arrived with, which is the same answer for the
    // app and a better one than English for anything else.
    let locale = body
        .locale
        .as_deref()
        .and_then(Locale::from_code)
        .unwrap_or_else(|| Locale::from_headers(&headers));
    let record = PushTokenRecord {
        token: body.token,
        platform,
        device_name: body.device_name.filter(|value| !value.trim().is_empty()),
        locale: Some(locale.as_str().to_owned()),
        updated_unix_ms: now_unix_ms(),
    };
    if let Some(existing) = tokens.iter_mut().find(|item| item.token == record.token) {
        *existing = record;
    } else {
        tokens.push(record);
    }
    tokens.sort_by_key(|item| item.updated_unix_ms);
    if tokens.len() > MAX_PUSH_TOKENS {
        let excess = tokens.len() - MAX_PUSH_TOKENS;
        tokens.drain(..excess);
    }
    write_push_tokens(&tokens).map_err(|err| {
        eprintln!("failed to write push tokens: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_write_failed",
            "failed to save push notification registration",
        )
    })?;
    Ok(Json(json!({ "ok": true, "device_count": tokens.len() })))
}

async fn unregister_push_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UnregisterPushTokenBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_push_token(&body.token)?;
    let mut tokens = state.push_tokens.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "push_token_lock_failed",
            "failed to lock push token state",
        )
    })?;
    let previous_len = tokens.len();
    tokens.retain(|record| record.token != body.token);
    if tokens.len() != previous_len {
        write_push_tokens(&tokens).map_err(|err| {
            eprintln!("failed to remove push token: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "push_token_write_failed",
                "failed to remove push notification registration",
            )
        })?;
    }
    Ok(Json(json!({
        "ok": true,
        "removed": tokens.len() != previous_len,
        "device_count": tokens.len()
    })))
}

async fn send_test_notification(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SendPushNotificationBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let tokens = state
        .push_tokens
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "push_token_lock_failed",
                "failed to lock push token state",
            )
        })?
        .clone();
    // The person waiting to see whether push works is the one holding the phone
    // that made this call, so the defaults are written in that request's
    // language. "Muqun Gateway" is the product's name and stays in Latin script
    // in every locale, the same way the app leaves "Gateway" alone.
    let locale = Locale::from_headers(&headers);
    let result = send_expo_push_notifications(
        &tokens,
        body.title.unwrap_or_else(|| "Muqun Gateway".into()),
        body.body.unwrap_or_else(|| {
            i18n::t(locale, "Muqun push notifications are connected.").to_owned()
        }),
        body.data.unwrap_or_else(|| {
            let mut data = serde_json::Map::new();
            data.insert("url".into(), json!("/"));
            data.insert("type".into(), json!("gateway.test"));
            data
        }),
    )
    .await
    .map_err(|err| {
        eprintln!("Expo push request failed: {err:#}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "expo_push_failed",
            "Expo push service request failed",
        )
    })?;
    Ok(Json(
        json!({ "ok": true, "device_count": tokens.len(), "expo": result }),
    ))
}

/// The program a session's backend has to spawn, and whether this process can
/// see it.
///
/// Reported because "unavailable" is not a diagnosis. A tmux backend has
/// exactly two ways to be unreachable -- no tmux server, or no tmux -- and only
/// one of them is visible to the person reading, since `tmux -V` in their own
/// shell answers happily while the daemon's `PATH` cannot find it at all. herdr
/// has nothing to look up: it is a socket, and the endpoint column already
/// names it.
fn backend_program_state(session: &SessionConfig) -> String {
    if session.backend != BackendKind::Tmux {
        return String::new();
    }
    let path = std::env::var("PATH").unwrap_or_default();
    match login_env::lookup(TMUX_PROGRAM, &path) {
        Some(found) => format!("tmux={}", found.display()),
        None => format!("tmux=NOT FOUND on PATH={path}"),
    }
}

/// Say so at startup when a configured backend's program cannot be found.
///
/// Without this the only symptom is one "terminal backend is unavailable" line
/// per poll, forever, naming a session id and nothing else -- which is what
/// this failure looked like for the whole time it went undiagnosed. It is a
/// warning and not a refusal: a gateway with a reachable herdr session and an
/// unreachable tmux one is still worth starting, and the app orders the
/// unreachable one last anyway.
fn warn_about_missing_backend_programs(config: &Config) {
    let path = std::env::var("PATH").unwrap_or_default();
    for session in &config.sessions {
        if session.backend != BackendKind::Tmux {
            continue;
        }
        if login_env::lookup(TMUX_PROGRAM, &path).is_none() {
            eprintln!(
                "session {}: backend=tmux, but no `tmux` on PATH={path}\n  \
                 This gateway cannot drive tmux until it can find it. If tmux works in your\n  \
                 shell but not here, the service is running with a different PATH: reinstall\n  \
                 it with `muqun-gateway service install`.",
                session.id
            );
        }
    }
}

fn status() -> anyhow::Result<()> {
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

fn manage() -> anyhow::Result<()> {
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

fn enable_managed_backend(backend: BackendKind) -> anyhow::Result<String> {
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

fn toggle_transport_encryption() -> anyhow::Result<String> {
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

fn remove_managed_backend(id: &str) -> anyhow::Result<String> {
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

fn set_managed_default_backend(id: &str) -> anyhow::Result<String> {
    let path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    make_backend_default(&mut config, id)?;
    write_config(&path, &config)?;
    Ok(format!("default is now {id}; restart gateway to apply"))
}

fn prompt_default_backend() -> anyhow::Result<Option<String>> {
    let config = load_config(None)?;
    prompt_backend_picker(&config.sessions, "Choose the default terminal backend")
}

fn prompt_remove_backend() -> anyhow::Result<Option<String>> {
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

fn prompt_backend_picker(
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

fn render_backend_picker(
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

fn confirm_remove_backend(session: &SessionConfig) -> anyhow::Result<bool> {
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

fn prompt_public_url() -> anyhow::Result<Option<String>> {
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

fn render_public_url_prompt(value: &str) -> anyhow::Result<()> {
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

fn prompt_revoke_device(devices: &[DeviceRecord]) -> anyhow::Result<Option<DeviceRecord>> {
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

fn render_revoke_device_picker(devices: &[DeviceRecord], selected: usize) -> anyhow::Result<()> {
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

fn confirm_revoke_device(device: &DeviceRecord) -> anyhow::Result<bool> {
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
fn update_public_url(public_url: &str, listen: &str) -> anyhow::Result<()> {
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

fn auto_upgrade_local_public_url() -> anyhow::Result<Option<String>> {
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
fn unreachable_listen_warning(listen: &str, public_url: &str) -> Option<String> {
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

fn is_local_public_url(url: &str) -> bool {
    url.contains("127.0.0.1") || url.contains("localhost")
}

struct TerminalModeGuard;

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

fn print_manage_screen(
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
fn relative_since(then_unix_ms: u128) -> String {
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

fn push_line(output: &mut String, line: impl AsRef<str>) {
    output.push_str(line.as_ref());
    output.push_str("\r\n");
}

fn write_centered_panel(lines: &[String]) -> anyhow::Result<()> {
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

fn write_two_column_panel(left: &[String], right: &[String]) -> anyhow::Result<()> {
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

fn display_width(value: &str) -> usize {
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

fn truncate(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

/// The first line of a multi-line error, for the manage screen's one-line
/// status field. Errors written for a terminal put the essential sentence
/// first and the remedy underneath; only the first fits here.
fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or(value).to_string()
}

fn push_wrapped_field(lines: &mut Vec<String>, label: &str, value: &str, width: usize) {
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

fn fetch_pending_pairing() -> anyhow::Result<Option<PendingPairing>> {
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

fn revoke_managed_device(device_id: &str) -> anyhow::Result<bool> {
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

fn local_management_addr(listen: SocketAddr) -> SocketAddr {
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

fn validate_public_url(value: &str) -> anyhow::Result<String> {
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

fn listen_for_explicit_public_url(public_url: &str, port: u16) -> String {
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

fn pairing_qr_offer(url: &str, server_id: &str, transport_key: Option<&str>) -> String {
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

fn url_component(value: &str) -> String {
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

fn render_qr(code: &QrCode) -> String {
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

async fn health(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(gateway_metadata(&state).await?))
}

async fn api_meta(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(gateway_metadata(&state).await?))
}

#[derive(Deserialize)]
struct SetLabelBody {
    label: String,
}

/// Set the server's display label from a paired device -- the app calls this
/// when the user renames the server, so push notifications carry that name
/// instead of the hostname default.
async fn api_set_label(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetLabelBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let label = update_server_label(&body.label)
        .map_err(|err| api_error(StatusCode::BAD_REQUEST, "invalid_label", &err.to_string()))?;
    Ok(Json(json!({ "label": label })))
}

async fn gateway_metadata(state: &AppState) -> ApiResult<Value> {
    // The primary session described below must be the one `GET /api/sessions`
    // leads with, not merely the first configured one: with tmux dead or
    // empty and herdr live, the app connects to whichever session
    // `sessions[0]` names, and `assertSupportedHerdr` has to validate *that*
    // session's metadata, not a stored-order tmux entry that always reports
    // `compatible: true`. See `ordered_sessions`.
    let ordered = ordered_sessions(state).await;
    let primary = ordered.first().copied().ok_or_else(|| {
        api_error(
            StatusCode::BAD_GATEWAY,
            "backend_unavailable",
            "no terminal backend is configured",
        )
    })?;
    let mut backends = Vec::with_capacity(state.config.sessions.len());
    let mut primary_metadata = None;
    let mut legacy_herdr = None;
    for session in &state.config.sessions {
        let (metadata, compatibility) = session_metadata(session).await;
        let metadata = json!({
            "sessionId": session.id,
            "label": session.label,
            "kind": session.backend,
            "connected": metadata.get("connected").cloned().unwrap_or(json!(false)),
            "version": metadata.get("version").cloned().unwrap_or(Value::Null),
            "protocol": metadata.get("protocol").cloned().unwrap_or(Value::Null),
        });
        if session.id == primary.id {
            primary_metadata = Some(metadata.clone());
            legacy_herdr = Some(compatibility);
        }
        backends.push(metadata);
    }
    Ok(json!({
        "ok": true,
        "gatewayVersion": env!("CARGO_PKG_VERSION"),
        "apiVersion": GATEWAY_API_VERSION,
        "apiMajor": GATEWAY_API_MAJOR,
        "minimumCompatibleApiVersion": "1.0.0",
        "legacyUnversionedApi": true,
        "capabilities": API_CAPABILITIES,
        "serverId": state.config.server_id,
        "label": state.config.label,
        "transportSecurity": {
            "protection": transport_protection(&state.config),
            "applicationLayerEncryption": false,
            "httpsRecommended": !state.config.public_url.starts_with("https://")
        },
        "backend": primary_metadata.unwrap_or(Value::Null),
        "backends": backends,
        "herdr": legacy_herdr.unwrap_or_else(|| json!({ "connected": false }))
    }))
}

fn transport_protection(config: &Config) -> &'static str {
    if config.public_url.starts_with("https://") {
        return "https";
    }
    let Ok(listen) = config.listen.parse::<SocketAddr>() else {
        return "unknown";
    };
    if listen.ip().is_loopback() {
        return "local-only";
    }
    match listen.ip() {
        std::net::IpAddr::V4(ip) if is_tailscale_ipv4(ip) => "tailscale-wireguard",
        _ => "unencrypted-http",
    }
}

fn is_tailscale_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// Whether a Herdr socket protocol is one this gateway will serve.
///
/// Open-ended above [`HERDR_PROTOCOL_MIN`]; see that constant for why.
fn herdr_protocol_supported(protocol: u64) -> bool {
    protocol >= HERDR_PROTOCOL_MIN
}

async fn session_metadata(session: &SessionConfig) -> (Value, Value) {
    match terminal_backend(session).metadata().await {
        Ok(metadata) => {
            // A backend that does not report a protocol at all is taken at the
            // floor rather than refused: absent is not the same as too old.
            let compatibility_protocol = metadata.protocol.unwrap_or(HERDR_PROTOCOL_MIN);
            let compatible = metadata.kind == BackendKind::Tmux
                || herdr_protocol_supported(compatibility_protocol);
            let mut compatibility = json!({
                "connected": true,
                "version": metadata.version,
                "protocol": compatibility_protocol,
                "compatible": compatible,
                "supportedProtocolMin": HERDR_PROTOCOL_MIN,
                // `null` is the ceiling: explicitly open-ended, which a client
                // can tell apart from a gateway too old to send the field.
                "supportedProtocolMax": Value::Null,
            });
            if let (Some(object), Some(response)) = (
                compatibility.as_object_mut(),
                metadata.compatibility_response,
            ) {
                object.insert("response".into(), response);
            }
            (
                json!({
                    "kind": metadata.kind,
                    "connected": true,
                    "version": metadata.version,
                    "protocol": metadata.protocol,
                }),
                compatibility,
            )
        }
        Err(err) => {
            eprintln!(
                "terminal metadata request failed for session {} (backend={}, endpoint={}): {err}",
                session.id,
                session.backend.as_str(),
                backend_endpoint(session),
            );
            (
                json!({ "kind": session.backend, "connected": false }),
                json!({ "connected": false, "error": "Terminal backend is unavailable" }),
            )
        }
    }
}

/// How much of "there's something to actually look at" a configured session
/// offers, most useful first. `GET /api/sessions` orders by this instead of a
/// fixed backend rank, because the app reads `sessions[0]` and shows no
/// picker -- whichever backend actually has something in it is the one that
/// needs to be first, and only the gateway is in a position to know which
/// that is on any given request.
///
/// Declared top-to-bottom in the order it should sort, so deriving `Ord`
/// gives exactly the "most significant first" comparison the endpoint wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SessionLiveness {
    HasPanes,
    Empty,
    Unreachable,
}

/// How long `GET /api/sessions` waits on a single backend before counting it
/// as unreachable. A dead socket that refuses the connection resolves this
/// fast on its own; this bound exists for the backend that accepts a
/// connection and then never answers, which would otherwise hang the whole
/// endpoint on one dead session.
const SESSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Turn a `list_panes` call into a liveness bucket, bounded by `timeout`.
///
/// Takes the future rather than a backend or a `SessionConfig` so it can be
/// exercised with a canned outcome -- immediate success with or without
/// panes, immediate failure, or a future that never resolves -- without
/// standing up a real Herdr or tmux backend.
async fn session_liveness(
    list_panes: BackendFuture<'_, Vec<Pane>>,
    timeout: Duration,
) -> SessionLiveness {
    match tokio::time::timeout(timeout, list_panes).await {
        Ok(Ok(panes)) if !panes.is_empty() => SessionLiveness::HasPanes,
        Ok(Ok(_)) => SessionLiveness::Empty,
        Ok(Err(_)) | Err(_) => SessionLiveness::Unreachable,
    }
}

/// `GET /api/sessions` ordering key: liveness first (has panes, then
/// empty-but-reachable, then unreachable), and the order they appear in the
/// config as the tiebreaker between otherwise-equal entries.
///
/// The tiebreaker used to be the backend kind, tmux always ahead of herdr, and
/// that made `muqun-gateway backend default` a lie: it moves an entry to the
/// front of `config.sessions`, which nothing then read. On a machine where
/// both backends are live -- which is what fixing tmux made ordinary -- the
/// reader had no way at all to say which one their phone should open, and the
/// answer moved on them whenever a probe was slow.
///
/// Config order is the reader's own statement of preference, so it is what
/// breaks the tie. Liveness still outranks it: a session that is down does not
/// get to be first just because somebody once preferred it.
///
/// Pure and independent of any backend, so every bucket -- including "every
/// configured session is unreachable" -- gets a fast, deterministic unit
/// test instead of depending on a live tmux or Herdr server.
fn session_order_key(index: usize, liveness: SessionLiveness) -> (SessionLiveness, usize) {
    (liveness, index)
}

/// Every configured session, ordered exactly as `GET /api/sessions` presents
/// them: liveness first, tmux ahead of herdr as the tiebreaker.
///
/// Shared with `gateway_metadata` so the "primary" session it describes for
/// `/health` and `/api/meta` is always the same one a client that reads
/// `sessions[0]` from `/api/sessions` actually connects to. Two copies of
/// this ordering agreeing by construction is the only way to keep them
/// agreeing at all -- a second, independently-written rank is a second place
/// for the two endpoints to drift apart.
///
/// Orders, never filters: every configured session stays in the result, even
/// when nothing is reachable, so a client that reads only the first entry
/// never sees `undefined` where it used to see a session.
/// How long a liveness verdict is reused before the backends are probed again.
///
/// `GET /api/sessions` and `/health` both order by liveness, every phone asks
/// on its own schedule, and every ask costs each configured backend two
/// commands -- which on tmux are two processes. Five devices polling twelve
/// seconds apart therefore probed roughly twice a second between them, for an
/// answer that is the same answer.
///
/// Short enough that a backend going down is reflected within a second, which
/// is far inside the window any client would notice; long enough that a burst
/// of clients asking at once is answered once.
const SESSION_LIVENESS_TTL: Duration = Duration::from_millis(1000);

async fn ordered_sessions(state: &AppState) -> Vec<&SessionConfig> {
    if let Some(order) = state
        .session_liveness
        .lock()
        .ok()
        .and_then(|cache| cache.fresh(Instant::now(), SESSION_LIVENESS_TTL))
    {
        // Recorded as indices rather than ids so a config reload cannot make a
        // stale entry name a session that is no longer there.
        if order.len() == state.config.sessions.len() {
            return order
                .into_iter()
                .filter_map(|index| state.config.sessions.get(index))
                .collect();
        }
    }
    let liveness =
        futures::future::join_all(state.config.sessions.iter().map(|session| async move {
            let backend = terminal_backend(session);
            // `probe_reachable` catches the one case `list_panes` cannot tell
            // apart on its own: tmux's `list_output` deliberately maps "no
            // server running" onto an empty pane list for every other
            // caller, which would otherwise make a dead tmux backend
            // indistinguishable from a live one that holds nothing. Both
            // calls run inside the one probe `session_liveness` bounds, so a
            // dead backend is still discovered by a single timeout, not two.
            let probe = async {
                match backend.probe_reachable().await {
                    Ok(false) => Err(BackendError::Unavailable),
                    _ => backend.list_panes().await,
                }
            };
            session_liveness(Box::pin(probe), SESSION_PROBE_TIMEOUT).await
        }))
        .await;
    let mut order: Vec<usize> = (0..state.config.sessions.len()).collect();
    order.sort_by_key(|&index| session_order_key(index, liveness[index]));
    if let Ok(mut cache) = state.session_liveness.lock() {
        cache.record(order.clone(), Instant::now());
    }
    order
        .into_iter()
        .map(|index| &state.config.sessions[index])
        .collect()
}

/// The last liveness ordering and when it was taken. See
/// [`SESSION_LIVENESS_TTL`].
#[derive(Default)]
struct SessionLivenessCache {
    taken: Option<(Instant, Vec<usize>)>,
}

impl SessionLivenessCache {
    fn fresh(&self, now: Instant, ttl: Duration) -> Option<Vec<usize>> {
        self.taken
            .as_ref()
            .filter(|(at, _)| now.duration_since(*at) < ttl)
            .map(|(_, order)| order.clone())
    }

    fn record(&mut self, order: Vec<usize>, now: Instant) {
        self.taken = Some((now, order));
    }
}

async fn sessions(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let sessions = ordered_sessions(&state).await;
    Ok(Json(json!({ "sessions": sessions })))
}

async fn snapshot(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    let workspaces = backend.list_workspaces().await.map_err(backend_api_error)?;
    let tabs = backend.list_tabs().await.map_err(backend_api_error)?;
    let panes = backend.list_panes().await.map_err(backend_api_error)?;
    let answer = backend::compat::snapshot(workspaces, tabs, panes);
    Ok(Json(note_and_amend_panes(&state, &session_id, answer)))
}

/// Let the scrollback store read a Herdr answer, and answer back for whatever
/// it holds.
///
/// Every pane entity the gateway hands out goes through here, because the
/// reader's pull-for-earlier is gated on the pane's `scroll`, not on its output.
/// Panes Herdr reports real scrollback for come out of this untouched.
fn note_and_amend_panes(state: &AppState, session_id: &str, mut value: Value) -> Value {
    if let Some(mut store) = lock_scrollback(state) {
        store.observe(session_id, &value);
        store.amend(session_id, &mut value);
    }
    value
}

async fn workspaces(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let workspaces = terminal_backend(session)
        .list_workspaces()
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::workspace_list(workspaces)))
}

async fn panes(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let answer = terminal_backend(session)
        .list_panes()
        .await
        .map(backend::compat::pane_list)
        .map_err(backend_api_error)?;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer)))
}

async fn agents(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    Ok(Json(
        backend_agent_list(session)
            .await
            .map_err(backend_api_error)?,
    ))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Comma-separated allow-list of event names, e.g. `pane_updated,pane_closed`.
    /// A client that only reacts to output changes should not be woken for the
    /// focus and layout churn that dominates the raw stream -- measured at ~20
    /// events per second on a busy session, almost none of it actionable on a
    /// phone. Absent means forward everything, so an old client is unaffected.
    #[serde(default)]
    types: Option<String>,
    /// When set, `pane.updated` events for THIS pane are enriched with the
    /// pane's current output inline, so the client paints immediately instead of
    /// firing a second read round-trip per update. Other panes' events, and all
    /// other event types, pass through unchanged. Absent means no enrichment, so
    /// an old client still works via its own reads.
    #[serde(default)]
    stream_pane: Option<String>,
    #[serde(default)]
    stream_lines: Option<u32>,
    #[serde(default)]
    stream_source: Option<String>,
    #[serde(default)]
    stream_format: Option<String>,
}

/// Resolved output-streaming settings for one events subscription.
struct StreamOutputOpts {
    pane: Option<String>,
    lines: u32,
    source: String,
    format: String,
}

struct StreamPaneFrame {
    revision: u64,
    output: String,
}

/// Pulls the terminal text out of a Herdr `pane.read` response, tolerating both
/// the bare-string and `{ text: ... }` result shapes across Herdr versions.
fn pane_read_text(value: &Value) -> Option<String> {
    // Herdr nests the text under `result.read.text`; tolerate `result.text` and a
    // bare-string `result` too across versions. Missing all three means no inline
    // output and the client falls back to its own read.
    for ptr in [
        "/result/read/output",
        "/result/read/text",
        "/result/output",
        "/result/text",
    ] {
        if let Some(text) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    value
        .pointer("/result")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Convert one sampled frame into the enriched `pane_updated` payload consumed
/// by Muqun. Output content, rather than revision, is used to decide whether to
/// emit because Herdr can expose new text before its coalesced revision advances.
fn stream_pane_update_payload(frame: &StreamPaneFrame, pane_id: &str) -> Option<String> {
    let payload = json!({
        "event": "pane_updated",
        "data": {
            "pane": {
                "pane_id": pane_id,
                "revision": frame.revision
            },
            "output": frame.output
        }
    });
    serde_json::to_string(&payload).ok()
}

async fn poll_stream_pane_update(
    backend: &dyn TerminalBackend,
    opts: &StreamOutputOpts,
) -> Option<StreamPaneFrame> {
    let pane = opts.pane.as_deref()?;
    let read = tokio::time::timeout(
        STREAM_OUTPUT_READ_TIMEOUT,
        backend.read_pane(&stream_read_request(pane, opts)),
    )
    .await
    .ok()?
    .ok()?;
    Some(StreamPaneFrame {
        revision: read.revision.unwrap_or_default(),
        output: read.text,
    })
}

/// If `line` is a `pane.updated` for the streamed pane, read that pane's output
/// and fold it into the event as `data.output`. Returns `None` to forward the
/// line untouched (wrong pane, wrong event, or a read failure -- the client
/// still has its revision and can fall back to a read).
async fn enrich_pane_update(
    line: &str,
    backend: &dyn TerminalBackend,
    opts: &StreamOutputOpts,
) -> Option<String> {
    let pane = opts.pane.as_deref()?;
    let mut value: Value = serde_json::from_str(line).ok()?;
    if normalize_event_name(value.get("event")?.as_str()?) != "pane_updated" {
        return None;
    }
    if value
        .pointer("/data/pane/pane_id")
        .and_then(Value::as_str)?
        != pane
    {
        return None;
    }
    // Bound the read so a slow or wedged Herdr can never stall the event loop:
    // a stalled enrich would starve the whole stream and drop the client back to
    // its slow safety poll. On timeout we forward the un-enriched line and the
    // client reads on its own.
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        backend.read_pane(&stream_read_request(pane, opts)),
    )
    .await
    .ok()?
    .ok()?;
    value
        .get_mut("data")
        .and_then(Value::as_object_mut)?
        .insert("output".into(), Value::String(read.text));
    serde_json::to_string(&value).ok()
}

fn stream_read_request(pane_id: &str, opts: &StreamOutputOpts) -> BackendReadPane {
    BackendReadPane {
        pane_id: BackendPaneId::new(pane_id),
        source: match opts.source.as_str() {
            "visible" => BackendOutputSource::Visible,
            "recent" => BackendOutputSource::Recent,
            "detection" => BackendOutputSource::Detection,
            _ => BackendOutputSource::RecentUnwrapped,
        },
        format: if opts.format == "text" {
            BackendOutputFormat::Text
        } else {
            BackendOutputFormat::Ansi
        },
        lines: opts.lines,
        start: None,
        end: None,
    }
}

/// Fold a streamed frame into what the gateway keeps for that pane.
///
/// Only for panes Herdr reports no scrollback for; everything else is left
/// exactly as it was, unrecorded. The key carries the source and format because
/// rows read as ANSI and rows read as plain text are different rows.
fn keep_stream_frame(
    store: &Arc<Mutex<scrollback::ScrollbackStore>>,
    session_id: &str,
    pane_id: &str,
    opts: &StreamOutputOpts,
    output: &str,
) {
    let Ok(mut store) = store.lock() else { return };
    store.record_frame(session_id, pane_id, &opts.source, &opts.format, output);
}

/// The output an enriched `pane.updated` carries, so the same frame that
/// reaches the reader also reaches the buffer.
fn enriched_pane_output(payload: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload)
        .ok()?
        .pointer("/data/output")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Normalises a filter token to the underscore form Herdr tags events with, so
/// a client may ask for either `pane.updated` or `pane_updated`.
fn normalize_event_name(value: &str) -> String {
    value.trim().replace('.', "_")
}

/// The SSE event name every encrypted stream record travels under. The real
/// event name is inside the sealed payload, where it is authenticated; the
/// outer name is the one piece of stream metadata deliberately left readable.
const ENCRYPTED_SSE_EVENT: &str = "muqun.encrypted";

/// Seals one connection's events, each under its own AES-256-GCM record.
///
/// The per-stream key binds the device key, a fresh stream id and the request
/// envelope's nonce (see `transport::derive_stream_key`); the nonce is the
/// event's sequence number, and the AAD carries request AAD, stream id and
/// seq. Together: a record that is modified, reordered, replayed -- within
/// this stream or from any other -- or dropped (the client checks seq
/// continuity) fails authentication on the phone.
struct EventStreamSealer {
    key: [u8; 32],
    stream_id: String,
    request_aad: String,
    seq: u64,
}

impl EventStreamSealer {
    fn new(context: &EncryptedStreamContext) -> anyhow::Result<Self> {
        let stream_id = generate_token();
        let key =
            transport::derive_stream_key(&context.material, &stream_id, &context.request_nonce)?;
        Ok(Self {
            key,
            stream_id,
            request_aad: context.request_aad.clone(),
            seq: 0,
        })
    }

    fn seal(&mut self, name: &str, data: &str) -> anyhow::Result<Event> {
        let record = self.seal_record(name, data)?;
        Ok(Event::default().event(ENCRYPTED_SSE_EVENT).data(record))
    }

    /// The `data:` line of one sealed record, split out so a test can open
    /// what left the sealer without reaching inside axum's `Event`.
    fn seal_record(&mut self, name: &str, data: &str) -> anyhow::Result<String> {
        let seq = self.seq;
        let plaintext = serde_json::to_vec(&json!({ "event": name, "data": data }))?;
        let aad = format!("{}\n{}\n{}", self.request_aad, self.stream_id, seq);
        let ciphertext = transport::seal_stream_event(&self.key, seq, aad.as_bytes(), &plaintext)?;
        // Only counted once sealing succeeded, so a failed record does not
        // burn a seq the client would then read as a gap.
        self.seq += 1;
        Ok(json!({
            "v": 1,
            "sid": self.stream_id,
            "seq": seq,
            "ciphertext": ciphertext,
        })
        .to_string())
    }
}

/// One event, sealed when this connection is encrypted and plain when it is
/// not. `None` means the record could not be sealed; the event is dropped
/// rather than ever leaving in the clear.
fn stream_event(sealer: &mut Option<EventStreamSealer>, name: &str, data: &str) -> Option<Event> {
    match sealer {
        Some(sealer) => match sealer.seal(name, data) {
            Ok(event) => Some(event),
            Err(error) => {
                eprintln!("failed to seal stream event: {error}");
                None
            }
        },
        None => Some(Event::default().event(name).data(data)),
    }
}

type GatewayEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;

async fn events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventsQuery>,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<Value>)> {
    require_device(&state, &headers)?;
    // Present exactly when the request arrived through the encrypted
    // transport. From here on every event this connection emits is sealed;
    // a device paired without a transport key keeps the plaintext stream.
    let mut sealer = match stream_crypto {
        Some(Extension(context)) => Some(EventStreamSealer::new(&context).map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
        })?),
        None => None,
    };
    let session = find_session(&state.config, &session_id)?.clone();
    let wanted: Option<std::collections::HashSet<String>> = query.types.as_ref().map(|value| {
        value
            .split(',')
            .map(normalize_event_name)
            .filter(|name| !name.is_empty())
            .collect()
    });
    let stream_opts = StreamOutputOpts {
        pane: query.stream_pane.clone().filter(|value| !value.is_empty()),
        lines: query.stream_lines.unwrap_or(240).min(MAX_OUTPUT_LINES),
        source: match query.stream_source.as_deref() {
            Some("recent-unwrapped") | Some("recent_unwrapped") | None => "recent_unwrapped".into(),
            Some(other) => other.to_string(),
        },
        format: match query.stream_format.as_deref() {
            Some("text") => "text".into(),
            _ => "ansi".into(),
        },
    };
    // `asset.created` is a gateway event on the same stream, so it obeys the
    // same allow-list as the Herdr ones: a client that filtered down to output
    // updates is not woken for artifacts it never asked about.
    let asset_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("asset_created"));
    // Approval transitions are published by the pane watcher, not by Herdr, and
    // obey the same allow-list. A client that asked for nothing else still gets
    // told when an agent is blocked, because that is the one thing it cannot
    // discover by watching output go by.
    let approval_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("approval_pending") || set.contains("approval_resolved"));
    let pane_events = wanted
        .as_ref()
        .is_none_or(|set| set.contains("pane_updated"));
    let mut approvals_rx = state.approval_events.subscribe();
    let assets = state.assets.clone();
    let scrollback_store = state.scrollback.clone();
    let backend = terminal_backend(&session);
    let mut activity = subscribe_activity(&state, &session);
    let stream = async_stream::stream! {
        let mut output_interval = tokio::time::interval(STREAM_OUTPUT_POLL_INTERVAL);
        output_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_stream_output: Option<String> = None;
        loop {
            tokio::select! {
                next = activity.recv() => match next {
                    Ok(SessionActivity::Event(activity)) => {
                        let data = activity.payload.to_string();
                        let keep = wanted.as_ref().is_none_or(|set| {
                            activity.name.is_empty() || set.contains(&activity.name)
                        });
                        if keep {
                            let payload = if stream_opts.pane.is_some() {
                                enrich_pane_update(&data, backend.as_ref(), &stream_opts)
                                    .await
                                    .unwrap_or_else(|| data.clone())
                            } else {
                                data.clone()
                            };
                            if let Some(pane_id) = stream_opts.pane.as_deref() {
                                if let Some(output) = enriched_pane_output(&payload) {
                                    keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &output);
                                }
                            }
                            if let Some(event) = stream_event(&mut sealer, "herdr", &payload) {
                                yield Ok(event);
                            }
                        }
                        if let Some(root) = worktree_event_root(&session_id, &data) {
                            let created = ingest_roots(assets.clone(), vec![root]).await;
                            if asset_events {
                                let now = now_unix_ms();
                                for entry in created
                                    .into_iter()
                                    .filter(|entry| now.saturating_sub(entry.modified_unix_ms) <= ASSET_EVENT_MAX_AGE_MS)
                                    .take(MAX_ASSET_EVENTS_PER_WORKTREE)
                                {
                                    let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
                                    let payload = asset_created_payload(&entry, asset_type);
                                    if let Some(event) = stream_event(&mut sealer, "asset.created", &payload) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        } else if let Some(removed) = worktree_event_removed_root(&data) {
                            if let Ok(mut index) = assets.lock() {
                                index.forget_under(&removed);
                            }
                        }
                    }
                    Ok(SessionActivity::Failed) => {
                        // The hub logs why and rebuilds the stream itself. This
                        // connection closes, which is the signal the app already
                        // knows how to act on.
                        if let Some(event) = stream_event(&mut sealer, "gateway.error", "Terminal activity stream unavailable") {
                            yield Ok(event);
                        }
                        break;
                    }
                    // Lagged: this subscriber fell behind a burst of layout
                    // changes. The next full refresh reconciles it, and closing
                    // a working connection over a missed redraw would be worse.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = output_interval.tick(), if stream_opts.pane.is_some() && pane_events => {
                    if let Some(frame) = poll_stream_pane_update(backend.as_ref(), &stream_opts).await {
                        if last_stream_output.as_deref() != Some(frame.output.as_str()) {
                            last_stream_output = Some(frame.output.clone());
                            if let Some(pane_id) = stream_opts.pane.as_deref() {
                                keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &frame.output);
                                if let Some(payload) = stream_pane_update_payload(&frame, pane_id) {
                                    if let Some(event) = stream_event(&mut sealer, "herdr", &payload) {
                                        yield Ok(event);
                                    }
                                }
                            }
                        }
                    }
                },
                approval = approvals_rx.recv(), if approval_events => {
                    match approval {
                        Ok(approval) => {
                            let wanted_name = normalize_event_name(approval.name);
                            if wanted.as_ref().is_none_or(|set| set.contains(&wanted_name)) {
                                if let Some(event) = stream_event(&mut sealer, approval.name, &approval.payload) {
                                    yield Ok(event);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                },
            }
        }
    };
    Ok(Sse::new(Box::pin(stream) as GatewayEventStream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn spawn_approval_watchers(state: AppState) {
    for session in state.config.sessions.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            watch_pane_approvals(state, session).await;
        });
    }
}

/// Watch every agent pane in a session for a permission menu appearing or
/// going away.
///
/// Herdr has no event for this -- a menu is drawn output, not a state change it
/// reports -- so the gateway polls. Only panes Herdr says are running an agent
/// are read, which is what keeps the poll to a handful of reads however many
/// shells the user has open.
async fn watch_pane_approvals(state: AppState, session: SessionConfig) {
    // pane id -> fingerprint of the menu that pane is blocked on.
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(APPROVAL_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Ok(panes) = terminal_backend(&session).list_panes().await else {
            continue;
        };
        // This listing is already being fetched, and it is the only place that
        // says which panes Herdr keeps no scrollback for. Reading it here means
        // the buffer knows what to keep before the reader ever opens the pane,
        // and costs Herdr nothing extra.
        if let Some(mut store) = lock_scrollback(&state) {
            store.observe(&session.id, &backend::compat::pane_list(panes.clone()));
        }

        // Every agent pane's screen in one request. On a socket backend this
        // is the same handful of reads it always was; on tmux it is one
        // process instead of one per pane, and this poll runs every 1.5
        // seconds for as long as the gateway is up.
        let agent_panes: Vec<(BackendPaneId, String)> = panes
            .iter()
            .filter_map(|pane| {
                let agent = pane.agent.as_deref().filter(|agent| !agent.is_empty())?;
                Some((pane.id.clone(), agent.to_owned()))
            })
            .collect();
        let ids: Vec<BackendPaneId> = agent_panes.iter().map(|(id, _)| id.clone()).collect();
        let screens: HashMap<String, String> = terminal_backend(&session)
            .read_visible_batch(&ids, APPROVAL_READ_LINES)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, text)| (id.as_str().to_owned(), text))
            .collect();

        let mut seen: Vec<String> = Vec::new();
        for (pane_id, agent) in &agent_panes {
            let pane_id = pane_id.as_str();
            let agent = agent.as_str();
            seen.push(pane_id.to_owned());

            let Some(text) = screens.get(pane_id) else {
                continue;
            };
            match approvals::detect(text) {
                Some(approval) => {
                    if pending.get(pane_id) == Some(&approval.fingerprint) {
                        continue;
                    }
                    // A different menu in the same pane is the old one resolved
                    // and a new one asked, in that order.
                    if let Some(previous) = pending.remove(pane_id) {
                        publish_approval(
                            &state,
                            "approval.resolved",
                            &session.id,
                            pane_id,
                            agent,
                            Some(&previous),
                            None,
                        );
                    }
                    pending.insert(pane_id.to_owned(), approval.fingerprint.clone());
                    publish_approval(
                        &state,
                        "approval.pending",
                        &session.id,
                        pane_id,
                        agent,
                        None,
                        Some(&approval),
                    );
                    deliver_agent_notification(
                        &state,
                        approval_notification(
                            &state.config.server_id,
                            &current_server_label(&state.config.label),
                            &session.id,
                            pane_id,
                            agent,
                            &approval,
                        ),
                    )
                    .await;
                }
                None => {
                    if let Some(previous) = pending.remove(pane_id) {
                        publish_approval(
                            &state,
                            "approval.resolved",
                            &session.id,
                            pane_id,
                            agent,
                            Some(&previous),
                            None,
                        );
                    }
                }
            }
        }

        // A pane that closed while blocked is a resolved approval too: whatever
        // the client was showing is no longer answerable.
        let vanished: Vec<String> = pending
            .keys()
            .filter(|pane_id| !seen.contains(pane_id))
            .cloned()
            .collect();
        for pane_id in vanished {
            let previous = pending.remove(&pane_id);
            publish_approval(
                &state,
                "approval.resolved",
                &session.id,
                &pane_id,
                "",
                previous.as_deref(),
                None,
            );
        }
    }
}

fn publish_approval(
    state: &AppState,
    name: &'static str,
    session_id: &str,
    pane_id: &str,
    agent: &str,
    fingerprint: Option<&str>,
    approval: Option<&approvals::Approval>,
) {
    let agent = (!agent.is_empty()).then_some(agent);
    let mut data = approval_data(session_id, pane_id, agent, approval, "menu");
    if let (Some(object), Some(fingerprint)) = (data.as_object_mut(), fingerprint) {
        object.insert("fingerprint".into(), json!(fingerprint));
    }
    // The same versioned envelope the endpoint answers with, so a client parses
    // an event and a response with one code path.
    let payload = content_envelope(data).to_string();
    let _ = state.approval_events.send(ApprovalEvent { name, payload });
}

/// The push a pending approval sends, before it is put into words.
///
/// Content-free by construction: the title carries the server the user named,
/// the body carries the agent's name, and the data carries ids and the option
/// *decisions* -- never the agent's own wording, which routinely quotes a
/// command or a path. Which language the words end up in is decided per device
/// at delivery; see [`AgentPushNotice`].
fn approval_notification(
    server_id: &str,
    server_label: &str,
    session_id: &str,
    pane_id: &str,
    agent: &str,
    approval: &approvals::Approval,
) -> AgentPushNotice {
    let mut data = serde_json::Map::new();
    data.insert("type".into(), json!("approval.pending"));
    data.insert("url".into(), json!(format!("/servers/{server_id}")));
    data.insert("server_id".into(), json!(server_id));
    data.insert("session_id".into(), json!(session_id));
    data.insert("pane_id".into(), json!(pane_id));
    // The notification category the client registered its approve/deny actions
    // under, plus which of those actions this particular menu offers.
    data.insert("categoryId".into(), json!("approval"));
    data.insert("fingerprint".into(), json!(approval.fingerprint));
    AgentPushNotice {
        notice: AgentNotice::ApprovalPending,
        server_label: server_label.trim().to_owned(),
        agent_name: (!agent.trim().is_empty()).then(|| agent.trim().to_owned()),
        data,
        // The answers travel as indices and decisions, and are worded at
        // delivery time. `options` is set on the payload there.
        choices: approval.push_choices(),
        // An approval push carries the fingerprint and the decisions, which is
        // enough to answer from the lock screen. The question itself is put on
        // the blocked push, and only when the owner asked for it.
        detail: None,
    }
}

/// What one session's activity hub broadcasts.
///
/// `Arc` because every subscriber gets a copy and the payload is a whole JSON
/// document; the point of the hub is that the work is done once.
#[derive(Clone, Debug)]
enum SessionActivity {
    Event(Arc<BackendActivity>),
    /// The underlying stream failed. Subscribers close their connection and
    /// the client reconnects; the hub rebuilds the stream on its own.
    Failed,
}

/// How long the hub waits before rebuilding a stream that failed.
const ACTIVITY_REBUILD_DELAY: Duration = Duration::from_secs(2);

/// Subscribe to a session's activity, starting the one stream that feeds it if
/// nobody had asked yet.
///
/// This exists because `activity_stream()` used to be called once per
/// subscriber: once by the notification watcher, and once more by every SSE
/// connection. For Herdr that is one socket subscription per phone. For tmux --
/// whose adapter polls, by a deliberate architectural choice recorded in
/// `docs/architecture.md` -- it was a whole independent poll per phone, and the
/// cost is processes: three `tmux` invocations every 500ms, times the number of
/// people looking. Five devices watching one session spawned thirty-six
/// processes a second, forever.
///
/// One stream per session, fanned out, is what "publish changes" was always
/// meant to be: the conversion happens once and the result is shared. The
/// gateway already does exactly this for approvals; activity was the odd one
/// out.
fn subscribe_activity(
    state: &AppState,
    session: &SessionConfig,
) -> tokio::sync::broadcast::Receiver<SessionActivity> {
    let mut hubs = match state.activity.lock() {
        Ok(hubs) => hubs,
        // A poisoned map must not take the stream down with it: fall back to
        // a private hub for this one subscriber, which is the old behaviour.
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(sender) = hubs.get(&session.id) {
        return sender.subscribe();
    }
    let (sender, receiver) = tokio::sync::broadcast::channel(ACTIVITY_EVENT_CAPACITY);
    hubs.insert(session.id.clone(), sender.clone());
    tokio::spawn(run_activity_hub(state.clone(), session.clone(), sender));
    receiver
}

/// Feed one session's hub for as long as anyone is listening.
///
/// Ends when the last subscriber goes away, and is started again by the next
/// `subscribe_activity`. That is what keeps a gateway nobody is talking to from
/// polling tmux forever.
async fn run_activity_hub(
    state: AppState,
    session: SessionConfig,
    sender: tokio::sync::broadcast::Sender<SessionActivity>,
) {
    loop {
        match terminal_backend(&session).activity_stream().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(activity) => {
                            // `send` fails only when nobody is listening, which
                            // is the condition this loop ends on anyway.
                            let _ = sender.send(SessionActivity::Event(Arc::new(activity)));
                        }
                        Err(err) => {
                            eprintln!(
                                "terminal activity failed for session {} (backend={}, endpoint={}): {err}",
                                session.id,
                                session.backend.as_str(),
                                backend_endpoint(&session),
                            );
                            let _ = sender.send(SessionActivity::Failed);
                            break;
                        }
                    }
                    if sender.receiver_count() == 0 {
                        break;
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "terminal activity stream could not be opened for session {} (backend={}): {err}",
                    session.id,
                    session.backend.as_str(),
                );
                let _ = sender.send(SessionActivity::Failed);
            }
        }
        if retire_activity_hub(&state, &session.id) {
            return;
        }
        tokio::time::sleep(ACTIVITY_REBUILD_DELAY).await;
    }
}

/// Drop a session's hub if nothing is listening any more, under the same lock
/// `subscribe_activity` takes.
///
/// The lock is the whole point: checking the count and removing the entry have
/// to be one step, or a subscriber arriving in between gets a receiver on a
/// hub whose producer has already decided to leave.
fn retire_activity_hub(state: &AppState, session_id: &str) -> bool {
    let mut hubs = match state.activity.lock() {
        Ok(hubs) => hubs,
        Err(poisoned) => poisoned.into_inner(),
    };
    match hubs.get(session_id) {
        Some(sender) if sender.receiver_count() == 0 => {
            hubs.remove(session_id);
            true
        }
        // Someone else replaced the entry; that hub owns itself now.
        None => true,
        _ => false,
    }
}

fn spawn_agent_notification_watchers(state: AppState) {
    for session in state.config.sessions.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            watch_agent_notifications(state, session).await;
        });
    }
}

async fn watch_agent_notifications(state: AppState, session: SessionConfig) {
    let mut statuses = seed_agent_statuses(&session).await;
    // This loop retries every two seconds forever, so a backend that is down
    // for good -- an uninstalled tmux, a herdr that is not running -- used to
    // write the same line to the log forty-three thousand times a day. That is
    // not a log, it is a denial of one: the lines that matter are the ones
    // around it, and they were unreadable. Only a *change* is news here.
    // One subscription to the session's shared hub, held for the life of the
    // process. It used to build a stream of its own, which on tmux meant this
    // watcher polled independently of every phone that was also polling.
    let mut activity = subscribe_activity(&state, &session);
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        {
            tokio::select! {
                _ = poll.tick() => {
                    for mut notification in poll_agent_notifications(&state, &session, &mut statuses).await {
                        enrich_blocked_notification(&state, &session, &mut notification).await;
                        deliver_agent_notification(&state, notification).await;
                    }
                }
                next = activity.recv() => match next {
                    Ok(SessionActivity::Event(event)) if event.name == "pane_agent_status_changed" => {
                        if let Some(mut notification) = absorb_agent_status_event(
                            &state,
                            &session.id,
                            &event.payload,
                            &mut statuses,
                        ) {
                            enrich_blocked_notification(&state, &session, &mut notification).await;
                            deliver_agent_notification(&state, notification).await;
                        }
                    }
                    // The hub logs the failure and rebuilds the stream. This
                    // watcher keeps its two-second poll going meanwhile, which
                    // is what actually drives notifications on a backend whose
                    // activity carries no agent status at all.
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // The hub retired while this watcher was the last holder.
                    // Take a new subscription, which starts it again.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        activity = subscribe_activity(&state, &session);
                    }
                }
            }
        }
    }
}

async fn poll_agent_notifications(
    state: &AppState,
    session: &SessionConfig,
    statuses: &mut HashMap<String, String>,
) -> Vec<AgentPushNotice> {
    let Ok(value) = backend_agent_list(session).await else {
        return Vec::new();
    };
    value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            absorb_agent_status_event(
                state,
                &session.id,
                &json!({
                    "event": "pane.agent_status_changed",
                    "data": agent
                }),
                statuses,
            )
        })
        .collect()
}

/// Send one notice to every registered device, each in its own language.
///
/// Devices are grouped by the locale they registered with and the notice is
/// worded once per group, so a household with an English phone and a Chinese
/// phone gets one Expo batch each rather than one batch in whichever language
/// happened to be asked for last. In the ordinary case every device shares a
/// locale and this is exactly the single request it always was.
async fn deliver_agent_notification(state: &AppState, notice: AgentPushNotice) {
    let tokens = match state.push_tokens.lock() {
        Ok(tokens) => tokens.clone(),
        Err(_) => {
            eprintln!("agent notification skipped: push token lock failed");
            return;
        }
    };
    let mut by_locale: BTreeMap<Locale, Vec<PushTokenRecord>> = BTreeMap::new();
    for token in tokens {
        by_locale.entry(token.locale()).or_default().push(token);
    }
    for (locale, tokens) in by_locale {
        let notification = notice.render(locale);
        if let Err(err) = send_expo_push_notifications(
            &tokens,
            notification.title,
            notification.body,
            notification.data,
        )
        .await
        {
            eprintln!("agent notification failed: {err:#}");
        }
    }
}

async fn seed_agent_statuses(session: &SessionConfig) -> HashMap<String, String> {
    let Ok(value) = backend_agent_list(session).await else {
        return HashMap::new();
    };
    value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            Some((
                agent.get("pane_id")?.as_str()?.to_owned(),
                agent.get("agent_status")?.as_str()?.to_ascii_lowercase(),
            ))
        })
        .collect()
}

async fn backend_agent_list(session: &SessionConfig) -> Result<Value, BackendError> {
    let terminal = terminal_backend(session);
    let mut agents = terminal.list_agents().await?;
    for agent in agents
        .iter_mut()
        .filter(|agent| agent.kind.is_some() && agent.status == BackendAgentStatus::Unknown)
    {
        let output = terminal
            .read_pane(&BackendReadPane {
                pane_id: agent.pane_id.clone(),
                source: BackendOutputSource::Visible,
                format: BackendOutputFormat::Text,
                lines: APPROVAL_READ_LINES,
                start: None,
                end: None,
            })
            .await;
        if let Ok(output) = output {
            agent.status = infer_tmux_agent_status(agent.kind.as_deref(), &output.text);
        }
    }
    Ok(backend::compat::agent_list(&agents))
}

fn infer_tmux_agent_status(agent: Option<&str>, visible: &str) -> backend::AgentStatus {
    if approvals::detect(visible).is_some() {
        return backend::AgentStatus::Blocked;
    }
    let Some(dictionary) = parts::dictionary_for(agent) else {
        return backend::AgentStatus::Unknown;
    };
    let normalized = parts::normalize_json(visible, Some(dictionary));
    match normalized
        .last()
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
    {
        Some("prompt") => backend::AgentStatus::Idle,
        Some("status") | Some("tool-block") => backend::AgentStatus::Working,
        _ => backend::AgentStatus::Unknown,
    }
}

/// One agent status change, after the bookkeeping and before anyone decides
/// what to do about it. Two things want it: the ring, which wants every one,
/// and the pushes, which want the two that are worth waking a phone for.
struct AgentTransition {
    pane_id: String,
    /// The agent's own name, when Herdr reported one.
    agent: Option<String>,
    from: Option<String>,
    to: String,
}

/// Read one status event and consume the transition it represents.
///
/// This is the only place `statuses` is written, which is what makes "exactly
/// once per event" a property of the code rather than a rule to remember: a
/// caller that wants both a ring entry and a push calls this once and hands the
/// answer to both.
fn agent_status_transition(
    event: &Value,
    statuses: &mut HashMap<String, String>,
) -> Option<AgentTransition> {
    let data = event.get("data").unwrap_or(event);
    let event_type = event
        .get("event")
        .or_else(|| data.get("type"))
        .and_then(Value::as_str)?;
    if event_type != "pane.agent_status_changed" {
        return None;
    }

    let pane_id = data.get("pane_id")?.as_str()?;
    let status = data.get("agent_status")?.as_str()?.to_ascii_lowercase();
    let previous = statuses.insert(pane_id.to_owned(), status.clone());
    if previous.as_deref() == Some(status.as_str()) {
        return None;
    }
    Some(AgentTransition {
        pane_id: pane_id.to_owned(),
        agent: ["display_agent", "agent", "title"]
            .into_iter()
            .find_map(|key| data.get(key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        from: previous,
        to: status,
    })
}

/// The push one transition raises, if it raises one at all.
///
/// Most transitions raise none -- an agent starting to work is not news to
/// someone who just asked it to. The two that do are the two a person is
/// waiting on: it needs them, or it is finished. It returns an unworded
/// [`AgentPushNotice`] rather than finished text because the same change may
/// have to be said in two languages.
fn notification_for_transition(
    transition: &AgentTransition,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Option<AgentPushNotice> {
    let pane_id = transition.pane_id.as_str();
    let (event_type, notice) = match (transition.to.as_str(), transition.from.as_deref()) {
        ("blocked", _) => ("agent.blocked", AgentNotice::AgentBlocked),
        ("idle" | "done" | "completed", Some("working")) => {
            ("agent.completed", AgentNotice::AgentCompleted)
        }
        _ => return None,
    };
    let agent_name = transition.agent.clone();
    let mut notification_data = serde_json::Map::new();
    notification_data.insert("type".into(), json!(event_type));
    notification_data.insert("url".into(), json!(format!("/servers/{server_id}")));
    notification_data.insert("server_id".into(), json!(server_id));
    notification_data.insert("session_id".into(), json!(session_id));
    notification_data.insert("pane_id".into(), json!(pane_id));

    Some(AgentPushNotice {
        notice,
        server_label: server_label.trim().to_owned(),
        agent_name,
        data: notification_data,
        choices: Vec::new(),
        // Filled in afterwards, and only for a blocked pane on a gateway whose
        // owner turned `rich_agent_pushes` on.
        detail: None,
    })
}

/// Everything one status event causes, in one call: it is remembered, and then
/// it may raise a push.
///
/// The ring records every transition and the push only ever names two of them,
/// which is the point -- a phone that missed the doorbell can still be told
/// that the agent worked for twenty minutes and then went idle.
fn absorb_agent_status_event(
    state: &AppState,
    session_id: &str,
    event: &Value,
    statuses: &mut HashMap<String, String>,
) -> Option<AgentPushNotice> {
    let transition = agent_status_transition(event, statuses)?;
    match state.agent_events.lock() {
        Ok(mut log) => {
            log.record(
                session_id,
                &transition.pane_id,
                transition.agent.as_deref(),
                transition.from.as_deref(),
                &transition.to,
                now_unix_ms(),
            );
        }
        // Losing one line of a digest must not cost the push that goes with it.
        Err(_) => eprintln!("agent event ring lock failed for session {session_id}"),
    }
    notification_for_transition(
        &transition,
        &state.config.server_id,
        &current_server_label(&state.config.label),
        session_id,
    )
}

/// Put the agent's own question on a blocked push, if this gateway's owner
/// asked for that.
///
/// The whole of what `rich_agent_pushes` does, in one place and behind one
/// check, so that "off" is a property of the code path and not a habit. Off, it
/// costs nothing: no pane is read, and the push is byte-for-byte the one this
/// gateway has always sent.
///
/// On, it reads the pane the way the approvals endpoint does and quotes what it
/// finds. A pane with no menu on it -- an agent blocked on something the
/// gateway cannot read -- is left as the content-free push it already was,
/// which is the right degradation: an empty question is worse than the generic
/// sentence, not better.
async fn enrich_blocked_notification(
    state: &AppState,
    session: &SessionConfig,
    notice: &mut AgentPushNotice,
) {
    if !state.config.rich_agent_pushes || notice.notice != AgentNotice::AgentBlocked {
        return;
    }
    let Some(pane_id) = notice
        .data
        .get("pane_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if let Ok((_, Some(approval))) = read_pane_approval(session, &pane_id).await {
        notice.detail = Some(PushDetail::from_approval(&approval));
    }
}

/// The two halves in one call, for tests that are about what a push says rather
/// than about what the ring holds.
#[cfg(test)]
fn notification_for_agent_status_event(
    event: &Value,
    statuses: &mut HashMap<String, String>,
    server_id: &str,
    server_label: &str,
    session_id: &str,
) -> Option<AgentPushNotice> {
    let transition = agent_status_transition(event, statuses)?;
    notification_for_transition(&transition, server_id, server_label, session_id)
}

async fn create_workspace(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    let workspace = backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: body.cwd.map(PathBuf::from),
            label: body.label,
            focus: body.focus.unwrap_or(false),
        })
        .await
        .map_err(backend_api_error)?;
    let tab = backend
        .list_tabs()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|tab| tab.workspace_id == workspace.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created tab")))?;
    let root_pane = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created pane")))?;
    Ok(Json(backend::compat::workspace_created(
        workspace, tab, root_pane,
    )))
}

async fn focus_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_workspace(&BackendWorkspaceId::new(workspace_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_focused")))
}

async fn rename_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_workspace(&BackendWorkspaceId::new(workspace_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_renamed")))
}

async fn close_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_workspace(&BackendWorkspaceId::new(workspace_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("workspace_closed")))
}

async fn tabs(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let tabs = terminal_backend(session)
        .list_tabs()
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::tab_list(tabs)))
}

async fn create_tab(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let backend = terminal_backend(session);
    let tab = backend
        .create_tab(&BackendCreateTab {
            workspace_id: body.workspace_id.map(BackendWorkspaceId::new),
            cwd: body.cwd.map(PathBuf::from),
            label: body.label,
            focus: body.focus.unwrap_or(false),
        })
        .await
        .map_err(backend_api_error)?;
    let root_pane = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .ok_or_else(|| backend_api_error(BackendError::InvalidResponse("created pane")))?;
    Ok(Json(backend::compat::tab_created(tab, root_pane)))
}

async fn focus_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_tab(&BackendTabId::new(tab_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_focused")))
}

async fn rename_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_tab(&BackendTabId::new(tab_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_renamed")))
}

async fn close_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_tab(&BackendTabId::new(tab_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("tab_closed")))
}

async fn pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let answer = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
        .map(backend::compat::pane_get)
        .map_err(backend_api_error)?;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer)))
}

async fn focus_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_focused")))
}

async fn rename_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenamePaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .rename_pane(&BackendPaneId::new(pane_id), &body.label)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_renamed")))
}

async fn close_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .close_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_closed")))
}

async fn split_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SplitPaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    if !matches!(body.direction.as_str(), "right" | "down") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_direction",
            "direction must be right or down",
        ));
    }
    if body
        .ratio
        .is_some_and(|ratio| !(0.05..=0.95).contains(&ratio))
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_ratio",
            "ratio must be between 0.05 and 0.95",
        ));
    }
    let session = find_session(&state.config, &session_id)?;
    let command = body
        .command
        .map(|parts| parts.join(" "))
        .filter(|text| !text.trim().is_empty());
    if let Some(text) = command.as_deref() {
        validate_text(text)?;
    }
    let backend = terminal_backend(session);
    let pane = backend
        .split_pane(&BackendSplitPane {
            pane_id: BackendPaneId::new(pane_id),
            direction: match body.direction.as_str() {
                "right" => BackendSplitDirection::Right,
                "down" => BackendSplitDirection::Down,
                _ => unreachable!("direction was validated above"),
            },
            ratio: body.ratio,
            cwd: body.cwd.map(PathBuf::from),
            env: body.env,
        })
        .await
        .map_err(backend_api_error)?;
    if let Some(text) = command {
        backend
            .send_text(&pane.id, &text)
            .await
            .map_err(backend_api_error)?;
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .map_err(backend_api_error)?;
    }
    Ok(Json(backend::compat::pane_created(pane)))
}

async fn zoom_pane(
    State(state): State<AppState>,
    Path((session_id, _pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ZoomPaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mode = body.mode.unwrap_or_else(|| "on".into());
    if !matches!(mode.as_str(), "on" | "off" | "toggle") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_zoom_mode",
            "mode must be on, off, or toggle",
        ));
    }
    find_session(&state.config, &session_id)?;
    // Released Muqun builds POST `zoom:on` whenever a terminal view mounts.
    // That request is view setup, not an explicit user command. Propagating it
    // changed the user's tmux/Herdr layout merely by opening the app and raced
    // stale snapshots into misleading target-not-found errors. Keep the route
    // and response for wire compatibility, but observation is side-effect free.
    Ok(Json(backend::compat::command_ok("pane_zoomed")))
}

async fn agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let agent = terminal_backend(session)
        .get_agent(&target)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::agent_get(agent)))
}

async fn focus_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_agent(&target)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("agent_focused")))
}

async fn send_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AgentSendBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    let session = find_session(&state.config, &session_id)?;
    let result = submit_agent_prompt(session, &target, &body.text)
        .await
        .map_err(|err| err.into_api_error("agent.prompt"))?;
    schedule_submit_keypress(session.clone(), target);
    Ok(Json(result))
}

/// Send a prompt to an agent and make sure it is actually submitted.
///
/// Shared by `POST .../agents/{target}/send` and by task dispatch, because the
/// second half of it -- the separate Enter -- is not an optional flourish, and a
/// second copy of it would drift.
async fn submit_agent_prompt(
    session: &SessionConfig,
    target: &str,
    text: &str,
) -> Result<Value, HerdrCallError> {
    terminal_backend(session)
        .prompt_agent(target, text)
        .await
        .map(|_| backend::compat::command_ok("agent_prompted"))
        .map_err(|err| HerdrCallError::Unavailable(err.to_string()))
}

async fn start_backend_agent(
    session: &SessionConfig,
    pane_id: &str,
    kind: &str,
    command: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<Value, HerdrCallError> {
    let started = terminal_backend(session)
        .start_agent(&BackendStartAgent {
            pane_id: BackendPaneId::new(pane_id),
            kind: kind.to_owned(),
            command: command.to_owned(),
            executable: tasks::find_on_path(command),
            args: args.to_vec(),
            timeout_ms,
        })
        .await
        .map_err(|err| HerdrCallError::Unavailable(err.to_string()))?;
    Ok(json!({ "result": { "argv": started.argv } }))
}

/// Belt and braces for a paste-vs-keypress race in agent TUIs: when the prompt
/// text and its newline arrive in one PTY write, Claude Code's input treats the
/// newline as pasted content and leaves the prompt sitting in its input box
/// unsubmitted (reproduced on camera, muqun card #571). A short beat later, a
/// separate Enter keystroke submits it. When the prompt DID submit, the input
/// box is empty and an Enter there is a no-op, so this is idempotent.
/// Best-effort by design: a pane that vanished between the two calls must not
/// turn a delivered prompt into an error. An agent target is its pane id, which
/// is what the key press needs.
///
/// The beat cannot be a fixed one, because a prompt that names an image file
/// keeps the agent busy staging it for as long as reading and encoding that
/// file takes, and every Enter sent before that finishes is discarded. So the
/// keystroke is sent at a pane that has stopped redrawing and is then checked
/// against Herdr's own reading of the agent, the same shape the approvals
/// confirm uses.
fn schedule_submit_keypress(session: SessionConfig, pane_id: String) {
    tokio::spawn(async move {
        submit_keypress(&session, &pane_id).await;
    });
}

/// Send the Enter that submits a prompt, and keep sending it until the agent
/// shows that one landed.
///
/// The screen cannot answer "did it submit". Staging an image repaints the pane
/// constantly without submitting anything, so "the text changed" says yes to a
/// prompt still sitting in the input box; measured against a live pane, that
/// false positive is what stopped the retry from ever running. What does
/// discriminate is Herdr's `state_change_seq`: it does not move at all for the
/// whole staging window, and a real submission advances it as the agent leaves
/// idle. So the settled screen only decides *when* to press Enter, and the
/// sequence decides whether it worked.
async fn submit_keypress(session: &SessionConfig, pane_id: &str) {
    tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
    let deadline = Instant::now() + SUBMIT_SETTLE_TIMEOUT;
    // A send is addressed to a pane, and a pane need not be running an agent
    // Herdr knows about, so the sequence is what this hopes for rather than what
    // it requires.
    let baseline = agent_state_change_seq(session, pane_id).await;
    if baseline.is_none() {
        eprintln!(
            "agent submit for pane {pane_id}: the terminal backend lists no agent state for it, \
             falling back to watching the screen"
        );
    }
    let attempts = if baseline.is_some() {
        SUBMIT_MAX_ATTEMPTS
    } else {
        SUBMIT_BLIND_MAX_ATTEMPTS
    };
    let mut previous: Option<String> = None;
    for attempt in 0..attempts {
        let Some(settled) = settled_pane_text(session, pane_id, &mut previous, deadline).await
        else {
            eprintln!("agent submit for pane {pane_id} gave up: the pane never settled");
            return;
        };
        let sent = terminal_backend(session)
            .send_keys(&BackendPaneId::new(pane_id), &["Enter".to_owned()])
            .await;
        if let Err(err) = sent {
            eprintln!(
                "agent submit for pane {pane_id} failed to send Enter: {}",
                err
            );
            return;
        }
        match baseline {
            Some(baseline) => {
                if agent_state_advanced(session, pane_id, baseline, deadline).await {
                    return;
                }
            }
            None => {
                tokio::time::sleep(SUBMIT_VERIFY_DELAY).await;
                let Ok(after) = read_pane_visible_text(session, pane_id).await else {
                    eprintln!(
                        "agent submit for pane {pane_id} gave up: the pane could not be verified"
                    );
                    return;
                };
                // All this says is that something moved. It is the weakest of
                // the two answers, which is why the blind budget is small.
                if after != settled {
                    return;
                }
                previous = Some(after);
            }
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(SUBMIT_RETRY_INTERVAL).await;
        }
    }
    eprintln!(
        "agent submit for pane {pane_id} gave up: the agent did not take the Enter \
         in {attempts} attempts"
    );
}

/// The backend's count of how many times this pane's agent has changed state.
///
/// `None` means the backend lists no agent for the pane, which a send to a plain
/// shell pane legitimately is, or that it could not be asked.
async fn agent_state_change_seq(session: &SessionConfig, pane_id: &str) -> Option<u64> {
    let value = match backend_agent_list(session).await {
        Ok(value) => value,
        Err(err) => {
            eprintln!("terminal backend agent list failed: {err}");
            return None;
        }
    };
    value
        .pointer("/result/agents")?
        .as_array()?
        .iter()
        .find(|agent| agent.get("pane_id").and_then(Value::as_str) == Some(pane_id))?
        .get("state_change_seq")?
        .as_u64()
}

/// Watch the agent for a beat and say whether it moved off the sequence it was
/// on before the Enter, which is the one reading that means the prompt went in.
async fn agent_state_advanced(
    session: &SessionConfig,
    pane_id: &str,
    baseline: u64,
    deadline: Instant,
) -> bool {
    let until = Instant::now() + SUBMIT_VERIFY_WINDOW;
    loop {
        tokio::time::sleep(SUBMIT_VERIFY_INTERVAL).await;
        if agent_state_change_seq(session, pane_id)
            .await
            .is_some_and(|seq| seq > baseline)
        {
            return true;
        }
        let now = Instant::now();
        if now >= until || now >= deadline {
            return false;
        }
    }
}

/// Read the pane until two consecutive reads come back identical, and hand back
/// that text. `previous` carries the last reading across attempts so a pane that
/// was already still is not waited on twice.
///
/// `None` means the pane was still moving at the deadline, or could not be read
/// at all -- either way there is nothing safe to press Enter against.
async fn settled_pane_text(
    session: &SessionConfig,
    pane_id: &str,
    previous: &mut Option<String>,
    deadline: Instant,
) -> Option<String> {
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        // The read failure is already logged one level down.
        let text = read_pane_visible_text(session, pane_id).await.ok()?;
        if previous.as_deref() == Some(text.as_str()) {
            return Some(text);
        }
        *previous = Some(text);
        tokio::time::sleep(SUBMIT_SETTLE_INTERVAL).await;
    }
}

#[derive(Debug, Deserialize)]
struct CreateTaskBody {
    /// Absolute path of the repo to work in. Has to be one this session already
    /// has open, or inside one.
    repo_path: String,
    /// Branch to work on. Present means "give this task its own checkout";
    /// absent means "work in the repo as it is".
    branch_name: Option<String>,
    /// Herdr agent kind, as listed by `GET /api/agents/catalog`.
    agent: String,
    /// First thing to say to the agent, once it is up and interactive.
    prompt: Option<String>,
    workspace_label: Option<String>,
    /// Extra arguments for the agent's own command line.
    agent_args: Option<Vec<String>>,
    /// How long to wait for the agent to become interactive.
    startup_timeout_ms: Option<u64>,
}

/// What the phone can start a task with: every kind the gateway knows, and
/// whether its executable is actually on this machine's `PATH`.
///
/// Not session-scoped: which binaries are installed is a property of the host,
/// not of a Herdr session, and the picker is drawn before a session is chosen.
async fn agents_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let agents = tasks::agent_catalog(&state.config.agent_commands);
    Ok(Json(json!({
        "agents": agents,
        "default_startup_timeout_ms": tasks::DEFAULT_AGENT_START_TIMEOUT_MS
    })))
}

/// Start a new piece of work: optionally a fresh checkout, a workspace to hold
/// it, an agent running in it, and the first prompt already typed.
///
/// # Why the answer can be a 207
///
/// This is four things happening in a row on someone else's machine, requested
/// from a phone. "It failed" tells the user nothing about whether they now have
/// a checkout, and the wrong guess makes them either abandon real work or
/// create a second copy of it. So every step is recorded, and a run that got
/// part of the way answers 207 with the same body a full success would have,
/// plus the failed step. Nothing was created at all is still a plain error.
///
/// # What gets rolled back, and what does not
///
/// Only a checkout this request created, and only while it is still useless --
/// that is, when the workspace to work in could not be made. Once there is a
/// pane sitting in the new checkout, the user has something they can use, and
/// deleting a fresh branch because `claude` happened not to be installed would
/// destroy more than it tidies. A checkout that was already there is never
/// touched.
async fn create_task(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTaskBody>,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    if !tasks::is_known_agent_kind(&body.agent, &state.config.agent_commands) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
    }
    if let Some(prompt) = body.prompt.as_deref() {
        validate_text(prompt)?;
    }
    if let Some(args) = body.agent_args.as_deref() {
        validate_agent_args(args)?;
    }
    if let Some(label) = body.workspace_label.as_deref() {
        if label.chars().count() > MAX_WORKSPACE_LABEL_CHARS || label.chars().any(char::is_control)
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_label",
                "workspace_label must be at most 120 printable characters",
            ));
        }
    }
    let timeout_ms = match body.startup_timeout_ms {
        None => tasks::DEFAULT_AGENT_START_TIMEOUT_MS,
        Some(value)
            if (tasks::MIN_AGENT_START_TIMEOUT_MS..=tasks::MAX_AGENT_START_TIMEOUT_MS)
                .contains(&value) =>
        {
            value
        }
        Some(_) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_timeout",
                "startup_timeout_ms must be between 3001 and 300000",
            ))
        }
    };
    if let Some(branch) = body.branch_name.as_deref() {
        tasks::validate_branch_name(branch).map_err(|err| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_branch_name",
                &err.message(i18n::current()),
            )
        })?;
    }

    // The fence. A path the session does not already have is not a path the
    // phone gets to run git in, whatever it claims about itself.
    let roots = task_repo_roots(&state, &session).await;
    let repo_path = tasks::resolve_repo_path(&body.repo_path, &roots).ok_or_else(|| {
        api_error(
            StatusCode::FORBIDDEN,
            "repo_not_allowed",
            "repo_path must be a directory inside a workspace this session has open",
        )
    })?;
    if body.branch_name.is_some() && !tasks::is_git_checkout(&repo_path) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "not_a_git_checkout",
            "repo_path is not a git checkout, so a branch cannot be made in it",
        ));
    }

    let mut steps = tasks::StepLog::new();
    let label = body
        .workspace_label
        .clone()
        .or_else(|| body.branch_name.clone());

    let place = match body.branch_name.as_deref() {
        Some(branch) => {
            match prepare_worktree(&session, &repo_path, branch, label.as_deref(), &mut steps).await
            {
                Ok(place) => place,
                Err(err) => return Ok(task_failure(err, &steps)),
            }
        }
        None => match prepare_workspace(&session, &repo_path, label.as_deref(), &mut steps).await {
            Ok(place) => place,
            Err(err) => return Ok(task_failure(err, &steps)),
        },
    };

    // From here on the user has somewhere to work, so nothing else is fatal and
    // nothing else is rolled back.
    let mut payload = json!({
        "workspace_id": place.workspace_id,
        "pane_id": place.pane_id,
        "worktree_path": place.worktree_path,
        "branch": body.branch_name,
        "agent": body.agent,
        "reused_worktree": place.reused,
        "agent_started": false,
        "prompt_submitted": false
    });

    let agent_command = tasks::agent_command(&body.agent, &state.config.agent_commands);
    match start_backend_agent(
        &session,
        &place.pane_id,
        &body.agent,
        &agent_command,
        body.agent_args.as_deref().unwrap_or_default(),
        timeout_ms,
    )
    .await
    {
        Ok(value) => {
            steps.ok(
                "agent",
                json!({
                    "kind": body.agent,
                    "pane_id": place.pane_id,
                    "argv": value.pointer("/result/argv").cloned()
                }),
            );
            payload["agent_started"] = json!(true);
        }
        Err(err) => {
            steps.failed("agent", err.code(), &err.message());
            return Ok(task_partial(payload, &steps));
        }
    }

    match body.prompt.as_deref() {
        None => steps.skipped("prompt", "no prompt was given"),
        Some(prompt) => {
            // An agent that has just been launched is not a named agent yet --
            // it is a program still deciding what it is, and Herdr only learns
            // its name once it has drawn its first prompt. Waiting for that is
            // the difference between a task that carries its instruction and
            // one that lands on an empty prompt: the first attempt used to
            // arrive before the agent existed and the whole spawn was reported
            // as a failure, though the pane and the agent were both up.
            let mut submitted = Err(HerdrCallError::Unavailable(
                "the agent never became ready".to_owned(),
            ));
            for attempt in 0..SPAWN_PROMPT_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(SPAWN_PROMPT_INTERVAL).await;
                }
                submitted = submit_agent_prompt(&session, &place.pane_id, prompt).await;
                if submitted.is_ok() {
                    break;
                }
            }
            match submitted {
                Ok(_) => {
                    schedule_submit_keypress(session.clone(), place.pane_id.clone());
                    steps.ok("prompt", json!({ "bytes": prompt.len() }));
                    payload["prompt_submitted"] = json!(true);
                }
                Err(err) => steps.failed("prompt", err.code(), &err.message()),
            }
        }
    }

    Ok(task_partial(payload, &steps))
}

/// Where the task will be worked on, however it got there.
struct TaskPlace {
    workspace_id: String,
    pane_id: String,
    worktree_path: Option<String>,
    reused: bool,
}

/// The no-branch case: a workspace on the repo as it stands.
async fn prepare_workspace(
    session: &SessionConfig,
    repo_path: &FsPath,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    steps.skipped("worktree", "no branch_name was given");
    let backend = terminal_backend(session);
    let workspace = match backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: Some(repo_path.to_owned()),
            label: label.map(str::to_owned),
            focus: false,
        })
        .await
    {
        Ok(workspace) => workspace,
        Err(err) => {
            let err = HerdrCallError::Unavailable(err.to_string());
            steps.failed("workspace", err.code(), &err.message());
            return Err(err);
        }
    };
    let pane = backend
        .list_panes()
        .await
        .map_err(|err| HerdrCallError::Unavailable(err.to_string()))?
        .into_iter()
        .find(|pane| pane.workspace_id == workspace.id)
        .ok_or_else(|| HerdrCallError::malformed("workspace.create"))?;
    let place = TaskPlace {
        workspace_id: workspace.id.as_str().to_owned(),
        pane_id: pane.id.as_str().to_owned(),
        worktree_path: None,
        reused: false,
    };
    steps.ok(
        "workspace",
        json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
    );
    Ok(place)
}

#[derive(Debug, Deserialize)]
struct SpawnBody {
    /// A Herdr agent kind, or a profile `agents.json` names.
    agent: String,
    /// Where the agent runs. Held to the same fence as a task's `repo_path`:
    /// a directory this session already works in, and nothing else. Absent
    /// means wherever Herdr puts a new tab.
    #[serde(default)]
    cwd: Option<String>,
    /// Put the agent beside what is already in this tab instead of in a tab of
    /// its own.
    #[serde(default)]
    tab_id: Option<String>,
    /// Typed and submitted once the agent is up.
    #[serde(default)]
    prompt: Option<String>,
}

/// Start an agent, from the phone, without describing a repository.
///
/// Task dispatch is the heavyweight door: it takes a repo, cuts a branch, makes
/// a checkout, and is the right thing when the work is new. This is the other
/// one -- "run codex here" -- which is what someone reaching for their phone in
/// a queue actually wants, and which used to take three calls and a knowledge
/// of which pane to split.
///
/// # What is checked before anything is created
///
/// The agent has to be one this gateway offers, and `cwd` has to be a directory
/// the session already works in -- the same fence `repo_path` is under, for the
/// same reason: a phone does not get to name a directory on the host and have
/// something run in it.
///
/// # Why the answer can be a 207
///
/// The same reason task dispatch's can. Once the pane exists the user has
/// somewhere to type, and "the agent did not come up" must not read as "nothing
/// happened" -- they would spawn a second one.
async fn spawn_agent(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SpawnBody>,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    if !tasks::is_known_agent_kind(&body.agent, &state.config.agent_commands)
        && !shortcuts::is_known_agent(&body.agent)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
    }
    if let Some(prompt) = body.prompt.as_deref() {
        validate_text(prompt)?;
    }

    let cwd = match body.cwd.as_deref() {
        None => None,
        Some(raw) => {
            let roots = task_repo_roots(&state, &session).await;
            let path = tasks::resolve_repo_path(raw, &roots).ok_or_else(|| {
                api_error(
                    StatusCode::FORBIDDEN,
                    "cwd_not_allowed",
                    "cwd must be a directory inside a workspace this session has open",
                )
            })?;
            Some(path.to_string_lossy().into_owned())
        }
    };

    let mut steps = tasks::StepLog::new();
    let place = spawn_place(&session, body.tab_id.as_deref(), cwd.as_deref(), &mut steps).await?;

    let mut payload = json!({
        "session_id": session_id,
        "pane_id": place.pane_id,
        "tab_id": place.tab_id,
        "agent": body.agent,
        "cwd": cwd,
        "agent_started": false,
        "prompt_submitted": false,
    });

    let agent_command = tasks::agent_command(&body.agent, &state.config.agent_commands);
    match start_backend_agent(
        &session,
        &place.pane_id,
        &body.agent,
        &agent_command,
        &[],
        tasks::DEFAULT_AGENT_START_TIMEOUT_MS,
    )
    .await
    {
        Ok(value) => {
            steps.ok(
                "agent",
                json!({
                    "kind": body.agent,
                    "pane_id": place.pane_id,
                    "argv": value.pointer("/result/argv").cloned()
                }),
            );
            payload["agent_started"] = json!(true);
        }
        Err(err) => {
            steps.failed("agent", err.code(), &err.message());
            // The pane is real and the user can type in it, so this is a 207
            // rather than an error that implies nothing was created.
            return Ok(task_partial(payload, &steps));
        }
    }

    match body.prompt.as_deref() {
        None => steps.skipped("prompt", "no prompt was given"),
        Some(prompt) => {
            // An agent that has just been launched is not a named agent yet --
            // it is a program still deciding what it is, and Herdr only learns
            // its name once it has drawn its first prompt. Waiting for that is
            // the difference between a task that carries its instruction and
            // one that lands on an empty prompt: the first attempt used to
            // arrive before the agent existed and the whole spawn was reported
            // as a failure, though the pane and the agent were both up.
            let mut submitted = Err(HerdrCallError::Unavailable(
                "the agent never became ready".to_owned(),
            ));
            for attempt in 0..SPAWN_PROMPT_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(SPAWN_PROMPT_INTERVAL).await;
                }
                submitted = submit_agent_prompt(&session, &place.pane_id, prompt).await;
                if submitted.is_ok() {
                    break;
                }
            }
            match submitted {
                Ok(_) => {
                    schedule_submit_keypress(session.clone(), place.pane_id.clone());
                    steps.ok("prompt", json!({ "bytes": prompt.len() }));
                    payload["prompt_submitted"] = json!(true);
                }
                Err(err) => steps.failed("prompt", err.code(), &err.message()),
            }
        }
    }

    Ok(task_partial(payload, &steps))
}

/// Where a spawned agent will run.
struct SpawnPlace {
    pane_id: String,
    tab_id: Option<String>,
}

/// Make somewhere for the agent to run: a split of the named tab, or a tab of
/// its own.
///
/// Splitting is what "run another agent on this" means -- the second agent
/// lands beside the first, in view, rather than in a tab the user has to go
/// find. A tab id naming nothing is refused before anything is created, because
/// the alternative is silently spawning somewhere the caller did not ask for.
async fn spawn_place(
    session: &SessionConfig,
    tab_id: Option<&str>,
    cwd: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let Some(tab_id) = tab_id else {
        return spawn_in_new_tab(session, cwd, steps).await;
    };
    spawn_beside(session, tab_id, cwd, steps).await
}

/// A task that asked for no particular tab, and the fallback for one whose tab
/// would not split.
async fn spawn_in_new_tab(
    session: &SessionConfig,
    cwd: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let backend = terminal_backend(session);
    let tab = backend
        .create_tab(&BackendCreateTab {
            workspace_id: None,
            cwd: cwd.map(PathBuf::from),
            label: None,
            focus: false,
        })
        .await
        .map_err(backend_api_error)?;
    let pane_id = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .map(|pane| pane.id.as_str().to_owned())
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "backend_malformed_response",
                "terminal backend did not return the created pane",
            )
        })?;
    let tab_id = Some(tab.id.as_str().to_owned());
    steps.ok("pane", json!({ "pane_id": pane_id, "tab_id": tab_id }));
    Ok(SpawnPlace { pane_id, tab_id })
}

/// A task placed beside what the reader was already looking at.
async fn spawn_beside(
    session: &SessionConfig,
    tab_id: &str,
    cwd: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let host = pane_in_tab(session, tab_id).await.ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "tab_not_found",
            "that tab has no pane to split",
        )
    })?;
    // A split that Ghostty refuses -- a tab already carrying as many panes as
    // its layout will hold, which is the ordinary state of a tab someone works
    // in -- must not lose the task. The tab was a preference, not the request:
    // the request was "start this agent". So a refusal falls back to a tab of
    // its own, recorded as such rather than passed off as the split that was
    // asked for.
    let split = terminal_backend(session)
        .split_pane(&BackendSplitPane {
            pane_id: BackendPaneId::new(&host),
            direction: BackendSplitDirection::Down,
            ratio: None,
            cwd: cwd.map(PathBuf::from),
            env: None,
        })
        .await;
    let pane = match split {
        Ok(pane) => pane,
        Err(err) => {
            steps.skipped(
                "split",
                &format!("{err} -- starting in a tab of its own instead"),
            );
            return spawn_in_new_tab(session, cwd, steps).await;
        }
    };
    let pane_id = pane.id.as_str().to_owned();
    steps.ok(
        "pane",
        json!({ "pane_id": pane_id, "tab_id": tab_id, "split_from": host }),
    );
    Ok(SpawnPlace {
        pane_id,
        tab_id: Some(tab_id.to_owned()),
    })
}

/// A pane to split in the named tab, preferring the one that has focus.
async fn pane_in_tab(session: &SessionConfig, tab_id: &str) -> Option<String> {
    terminal_backend(session)
        .list_panes()
        .await
        .ok()?
        .into_iter()
        .filter(|pane| pane.tab_id.as_str() == tab_id)
        .max_by_key(|pane| pane.focused)
        .map(|pane| pane.id.as_str().to_owned())
}

/// The directories this session is already working in, for a spawn picker.
///
/// Deliberately not a directory browser. It answers with the distinct working
/// directories of the panes Herdr reports right now and nothing else, so the
/// list a phone can pick from is exactly the list `cwd` will accept -- and a
/// phone cannot use it to walk the host's filesystem. `git` says which of them
/// is a checkout, because "start an agent here" usually means a repo.
async fn recent_cwds(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let roots = session_asset_roots(&state, &session, None).await;

    let cwds = tokio::task::spawn_blocking(move || {
        roots
            .into_iter()
            .map(|root| {
                json!({
                    "path": root.path.to_string_lossy(),
                    "name": root.path.file_name().unwrap_or_default().to_string_lossy(),
                    "pane_id": root.pane_id,
                    "workspace_id": root.workspace_id,
                    "git": tasks::is_git_checkout(&root.path),
                })
            })
            .collect::<Vec<Value>>()
    })
    .await
    .unwrap_or_default();

    Ok(Json(json!({ "session_id": session_id, "cwds": cwds })))
}

/// Stop whatever the agent in this pane is doing.
///
/// Sugar over send-keys, and the reason it is worth an endpoint is that the key
/// is not the same on every agent: `ctrl+c` at a shell, `esc` in every agent
/// this gateway has a profile for. A Stop button that guesses is wrong on most
/// panes, and the gateway is the piece that already knows which agent is in
/// this one.
///
/// It sends a keystroke and nothing else -- no signal, no kill. Whatever the
/// agent does with `esc` is the agent's business.
async fn interrupt_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let pane = pane_get(&session, &pane_id).await?;
    let agent = pane
        .get("agent")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let title = pane
        .get("terminal_title_stripped")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let key = shortcuts::interrupt_key(agent, title);

    send_pane_keys(&session, &pane_id, std::slice::from_ref(&key)).await?;

    Ok(Json(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "agent": agent,
        // Which key was actually sent, so a client can say what it did rather
        // than claim a stop it cannot see the effect of.
        "key": key,
        "sent": true,
    })))
}

/// The branch case.
///
/// Herdr's `worktree.create` makes the checkout *and* the workspace and pane in
/// one response, so there is no moment where a checkout exists with nothing
/// attached to it. Before creating anything it asks what checkouts the repo
/// already has: a phone that retried after losing its answer gets the existing
/// one back rather than a git error about the branch being checked out
/// elsewhere. A Herdr too old for these methods falls back to running git here.
async fn prepare_worktree(
    session: &SessionConfig,
    repo_path: &FsPath,
    branch: &str,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    let backend = terminal_backend(session);
    let request = BackendWorktreeRequest {
        cwd: repo_path.to_owned(),
        branch: branch.to_owned(),
        label: label.map(str::to_owned),
        focus: false,
    };
    let existing = backend
        .list_worktrees(&request.cwd)
        .await
        .ok()
        .and_then(|worktrees| {
            worktrees.into_iter().find(|worktree| {
                worktree
                    .branch
                    .as_deref()
                    .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name))
                    == Some(branch)
            })
        });

    if let Some(worktree) = existing {
        match backend.open_worktree(&request).await {
            Ok(placement) => {
                let path = worktree.path.to_string_lossy().into_owned();
                let place = TaskPlace {
                    workspace_id: placement.workspace_id.as_str().to_owned(),
                    pane_id: placement.pane_id.as_str().to_owned(),
                    worktree_path: Some(path.clone()),
                    reused: true,
                };
                steps.ok(
                    "worktree",
                    json!({ "path": path, "branch": branch, "reused": true }),
                );
                steps.ok(
                    "workspace",
                    json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
                );
                return Ok(place);
            }
            // Reuse is an optimisation, not a contract. If opening the existing
            // checkout fails, fall through and let the create path report a
            // real error rather than masking it with this one.
            Err(err) => eprintln!("task: worktree open for {branch} failed: {err}"),
        }
    }

    match backend.create_worktree(&request).await {
        Ok(placement) => {
            let path = placement
                .path
                .map(|path| path.to_string_lossy().into_owned());
            let place = TaskPlace {
                workspace_id: placement.workspace_id.as_str().to_owned(),
                pane_id: placement.pane_id.as_str().to_owned(),
                worktree_path: path.clone(),
                reused: false,
            };
            steps.ok(
                "worktree",
                json!({ "path": path, "branch": branch, "reused": false }),
            );
            steps.ok(
                "workspace",
                json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
            );
            Ok(place)
        }
        Err(BackendError::Unsupported(_)) => {
            prepare_worktree_with_git(session, repo_path, branch, label, steps).await
        }
        Err(err) => {
            let err = backend_call_error("worktree.create", err);
            steps.failed("worktree", err.code(), &err.message());
            Err(err)
        }
    }
}

/// The fallback for a Herdr without `worktree.*`: run git here, then ask for a
/// workspace on the result. This is the only path that can leave a checkout
/// with nothing attached, so it is the only one that rolls back.
async fn prepare_worktree_with_git(
    session: &SessionConfig,
    repo_path: &FsPath,
    branch: &str,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    let repo = repo_path.to_owned();
    let branch_name = branch.to_owned();
    let target = tasks::default_worktree_path(&repo, &branch_name).ok_or_else(|| {
        let err = HerdrCallError::Unavailable("repo_path has no parent directory".into());
        steps.failed("worktree", err.code(), &err.message());
        err
    })?;

    let outcome = {
        let repo = repo.clone();
        let branch_name = branch_name.clone();
        let target = target.clone();
        tokio::task::spawn_blocking(move || tasks::git_worktree_add(&repo, &target, &branch_name))
            .await
    };
    let added = match outcome {
        Ok(Ok(added)) => added,
        Ok(Err(err)) => {
            let err = HerdrCallError::Unavailable(err.to_string());
            steps.failed("worktree", "worktree_create_failed", &err.message());
            return Err(err);
        }
        Err(err) => {
            let err = HerdrCallError::Unavailable(format!("git task panicked: {err}"));
            steps.failed("worktree", "worktree_create_failed", &err.message());
            return Err(err);
        }
    };
    let path = added.path.to_string_lossy().into_owned();
    steps.ok(
        "worktree",
        json!({ "path": path, "branch": branch, "reused": !added.created }),
    );

    let backend = terminal_backend(session);
    let created = match backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: Some(PathBuf::from(&path)),
            label: label.map(str::to_owned),
            focus: false,
        })
        .await
    {
        Err(err) => Err(HerdrCallError::Unavailable(err.to_string())),
        Ok(workspace) => backend
            .list_panes()
            .await
            .map_err(|err| HerdrCallError::Unavailable(err.to_string()))?
            .into_iter()
            .find(|pane| pane.workspace_id == workspace.id)
            .map(|pane| TaskPlace {
                workspace_id: workspace.id.as_str().to_owned(),
                pane_id: pane.id.as_str().to_owned(),
                worktree_path: Some(path.clone()),
                reused: !added.created,
            })
            .ok_or_else(|| HerdrCallError::malformed("workspace.create")),
    };

    match created {
        Ok(place) => {
            steps.ok(
                "workspace",
                json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
            );
            Ok(place)
        }
        Err(err) => {
            steps.failed("workspace", err.code(), &err.message());
            // A checkout with no workspace is the one genuinely useless state,
            // and only ours to undo when this request is what made it.
            if added.created {
                let repo = repo.clone();
                let target = added.path.clone();
                let removed =
                    tokio::task::spawn_blocking(move || tasks::git_worktree_remove(&repo, &target))
                        .await;
                match removed {
                    Ok(Ok(())) => steps.rolled_back("worktree", json!({ "path": path })),
                    Ok(Err(remove_err)) => {
                        steps.failed("rollback", "rollback_failed", &remove_err.to_string())
                    }
                    Err(join_err) => {
                        steps.failed("rollback", "rollback_failed", &join_err.to_string())
                    }
                }
            } else {
                steps.skipped("rollback", "the checkout was already there");
            }
            Err(err)
        }
    }
}

/// Nothing usable was created: an ordinary error, with the steps attached so
/// the client can still see how far it got.
fn task_failure(err: HerdrCallError, steps: &tasks::StepLog) -> Response {
    // Herdr refusing is the user's request being wrong -- a path it does not
    // recognise, a branch already checked out somewhere it will not touch.
    // Anything else is the gateway or the socket failing, which is not.
    let status = match err {
        HerdrCallError::Herdr { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({
            "error": { "code": err.code(), "message": err.message() },
            "steps": steps.value()
        })),
    )
        .into_response()
}

/// Something usable was created. 207 when a later step failed, so a client can
/// tell "your agent is running" from "your checkout is waiting for you".
fn task_partial(mut payload: Value, steps: &tasks::StepLog) -> Response {
    payload["steps"] = steps.value();
    let status = if steps.has_failure() {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };
    (status, Json(payload)).into_response()
}

/// Directories a task is allowed to start in.
///
/// Herdr does not report "repos this session has" as such, so this is assembled
/// from what it does report: the repo root and checkout path of every workspace
/// that is a git checkout, plus the working directory of every pane. The repo
/// root matters because a pane is usually somewhere inside the repo rather than
/// at its top, and branching from the top is the normal request.
async fn task_repo_roots(state: &AppState, session: &SessionConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !is_scannable_root(&path) {
            return;
        }
        let Ok(canonical) = std::fs::canonicalize(&path) else {
            return;
        };
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    };

    match terminal_backend(session).list_workspaces().await {
        Ok(workspaces) => {
            for workspace in workspaces {
                if let Some(path) = workspace.repo_root {
                    push(path);
                }
                if let Some(path) = workspace.checkout_path {
                    push(path);
                }
            }
        }
        Err(err) => eprintln!("task roots: workspace list failed: {err}"),
    }

    for root in session_asset_roots(state, session, None).await {
        push(root.path);
    }
    roots
}

/// A Herdr call that separates "the socket did not answer" from "Herdr said no",
/// which the plain [`call_session_method`] deliberately does not: it hands the
/// whole envelope, error and all, straight to the client. Orchestrating several
/// calls needs to know which one refused and why.
#[derive(Debug)]
enum HerdrCallError {
    Unavailable(String),
    Herdr {
        method: String,
        error: Value,
    },
    /// Herdr answered successfully but not with the shape the schema promises,
    /// which is a bug somewhere rather than a user error.
    Malformed(String),
}

impl HerdrCallError {
    fn malformed(method: &str) -> Self {
        Self::Malformed(method.to_owned())
    }

    fn code(&self) -> &str {
        match self {
            Self::Unavailable(_) => "herdr_unavailable",
            Self::Herdr { error, .. } => error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("herdr_error"),
            Self::Malformed(_) => "invalid_herdr_response",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Unavailable(detail) => detail.clone(),
            Self::Herdr { method, error } => {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Herdr refused the request");
                format!("{method}: {message}")
            }
            Self::Malformed(method) => {
                format!("{method} did not answer with the expected fields")
            }
        }
    }

    fn into_api_error(self, method: &str) -> (StatusCode, Json<Value>) {
        let status = match self {
            Self::Herdr { .. } => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        let code = self.code().to_owned();
        let message = self.message();
        eprintln!("Herdr request {method} failed: {message}");
        api_error(status, &code, &message)
    }
}

fn backend_call_error(method: &str, error: BackendError) -> HerdrCallError {
    match error {
        BackendError::Refused { code, message } => HerdrCallError::Herdr {
            method: method.to_owned(),
            error: json!({ "code": code, "message": message }),
        },
        BackendError::InvalidResponse(_) => HerdrCallError::malformed(method),
        other @ (BackendError::Unavailable | BackendError::Unsupported(_)) => {
            HerdrCallError::Unavailable(other.to_string())
        }
        other @ BackendError::InvalidTarget(_) => HerdrCallError::Unavailable(other.to_string()),
    }
}

/// A requested range, clamped to what one response may carry.
///
/// Out-of-bounds clamps instead of failing: a reader paging toward the top will
/// always eventually ask for more than the pane holds, and that is how reaching
/// the top looks from outside, not a mistake worth an error for.
fn validate_output_range(start: Option<u32>, end: Option<u32>) -> ApiResult<Option<(u32, u32)>> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if start < end => Ok(Some((
            start,
            end.min(start.saturating_add(MAX_OUTPUT_LINES)),
        ))),
        (Some(_), Some(_)) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start must be less than end",
        )),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start and end must be given together",
        )),
    }
}

async fn pane_output(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<OutputQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let range = validate_output_range(query.start, query.end)?;
    let source = query.source.unwrap_or_else(|| "recent-unwrapped".into());
    if !matches!(
        source.as_str(),
        "visible" | "recent" | "recent-unwrapped" | "detection"
    ) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_source",
            "source must be visible, recent, recent-unwrapped, or detection",
        ));
    }
    let lines = query.lines.unwrap_or(200).min(MAX_OUTPUT_LINES);
    let format = query.format.unwrap_or_else(|| "text".into());
    if !matches!(format.as_str(), "text" | "ansi") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_format",
            "format must be text or ansi",
        ));
    }
    let herdr_source = if source == "recent-unwrapped" {
        "recent_unwrapped"
    } else {
        source.as_str()
    };
    let session = find_session(&state.config, &session_id)?;
    let request = BackendReadPane {
        pane_id: BackendPaneId::new(pane_id.clone()),
        source: match source.as_str() {
            "visible" => BackendOutputSource::Visible,
            "recent" => BackendOutputSource::Recent,
            "recent-unwrapped" => BackendOutputSource::RecentUnwrapped,
            "detection" => BackendOutputSource::Detection,
            _ => unreachable!("source was validated above"),
        },
        format: match format.as_str() {
            "text" => BackendOutputFormat::Text,
            "ansi" => BackendOutputFormat::Ansi,
            _ => unreachable!("format was validated above"),
        },
        lines,
        start: range.map(|(start, _)| start),
        end: range.map(|(_, end)| end),
    };
    let output = terminal_backend(session)
        .read_pane(&request)
        .await
        .map_err(backend_api_error)?;
    let mut answer = backend::compat::pane_read(output);

    // Herdr answered with everything it has. For a pane it keeps nothing above
    // the viewport for, everything it has is one screen -- and this is the read
    // that both feeds what the gateway kept and hands it back.
    //
    // Scoped to the tail path only (`range.is_none()`): stitching windows the
    // local buffer by `lines`, which has no relationship to a requested
    // `[start, end)`, and it only ever rewrites the text pointer, never
    // `range`. A range-addressed read already got the backend's own answer
    // for that exact slice; substituting a differently-windowed text under an
    // unchanged `range` would make the response lie about which lines it
    // holds, which is worse than the fabricated `start: 0` Task 3 already
    // ruled out.
    if range.is_none() {
        if let (Some(text), Some(mut store)) = (pane_read_text(&answer), lock_scrollback(&state)) {
            let served = store.serve_read(
                &session_id,
                &pane_id,
                herdr_source,
                &format,
                &text,
                lines as usize,
            );
            if served != text {
                scrollback::replace_read_text(&mut answer, &served);
            }
        }
    }
    Ok(Json(answer))
}

#[derive(Debug, Deserialize)]
struct PartsQuery {
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FileSearchQuery {
    /// What the user typed after the `@`. Absent or empty is not an error: it
    /// answers with the shallowest files in the workspace, which is what a
    /// picker should show before anything has been typed.
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The normalized transcript: the same text the raw output endpoint serves, read
/// through the marker dictionary of whichever agent is in the pane.
///
/// Additive on purpose. The raw ANSI endpoints stay forever, so a client that
/// dislikes what a dictionary made of a pane is one tap from the terminal view,
/// and a pane running no agent -- or one no dictionary covers yet -- is answered
/// with text parts rather than with an error.
async fn pane_parts(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<PartsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let lines = query
        .lines
        .unwrap_or(PARTS_DEFAULT_LINES)
        .clamp(1, MAX_OUTPUT_LINES);

    // Which agent is in the pane decides which source reads it, and the pane's
    // own working directory is the fence every workspace read is under.
    let (agent, root) = pane_agent_and_root(&session, &pane_id).await;

    // `recent_unwrapped` is the only source worth normalizing: the dictionaries
    // key off line starts, and it is the one source where a long line is one
    // line rather than however many the pane happens to be wide.
    let output = terminal_backend(&session)
        .read_pane(&BackendReadPane {
            pane_id: BackendPaneId::new(&pane_id),
            source: BackendOutputSource::RecentUnwrapped,
            format: BackendOutputFormat::Text,
            lines,
            start: None,
            end: None,
        })
        .await
        .map_err(backend_api_error)?;
    let (backend_text, revision) = (output.text, output.revision);
    // The transcript reads the same pane through a different endpoint, so it
    // has to be given the same rows: two views that disagree about where
    // history ends is the bug this whole thing is trying not to introduce.
    let text = match lock_scrollback(&state) {
        Some(mut store) => store.serve_read(
            &session_id,
            &pane_id,
            "recent_unwrapped",
            "text",
            &backend_text,
            lines as usize,
        ),
        None => backend_text,
    };

    let dictionary = parts::dictionary_for(agent.as_deref());

    // A native protocol, where the agent runs one and the operator pointed the
    // gateway at it, is a better source than the screen: it carries exit codes,
    // patches, checklists and pending permissions as data rather than as glyphs
    // a table has to guess at. It is also never required -- every failure below
    // falls through to the dictionary, so an adapter can add structure to a
    // pane and can never take one away.
    let native = match native::adapter_for(agent.as_deref()) {
        Some(adapter) => {
            native::read(
                adapter,
                root.as_deref(),
                native::DEFAULT_MESSAGE_LIMIT,
                i18n::current(),
            )
            .await
        }
        None => None,
    };
    let normalized = match &native {
        Some(read) => read.parts.clone(),
        None => parts::normalize_json(&text, dictionary),
    };

    // Reading a workspace's own skills and commands is blocking filesystem
    // work, so it runs off the async runtime like every other scan here.
    let composer = {
        let agent = agent.clone();
        tokio::task::spawn_blocking(move || composer::descriptor(agent.as_deref(), root.as_deref()))
            .await
            .unwrap_or_default()
    };

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        // Which source answered. `recent-unwrapped` is the pane's own text;
        // an adapter's id says the parts came from that agent's protocol and
        // that `range` spans the adapter's rendering rather than terminal rows.
        "source": match &native {
            Some(_) => "native",
            None => "recent-unwrapped",
        },
        "lines": lines,
        "revision": revision,
        "pane": pane_capabilities(
            &pane_id,
            agent.as_deref(),
            dictionary,
            native.as_ref(),
            composer,
        ),
        "parts": normalized,
    }))))
}

/// What Herdr says is running in a pane, and the workspace root that pane is
/// fenced to.
///
/// A `pane.get` that fails is not fatal to any caller: no agent means text
/// parts and no native source, which is exactly what an unreachable `pane.get`
/// should degrade to. The root is canonicalized and held to the same
/// `is_scannable_root` rule the assets and file-search APIs use, so a pane
/// sitting at `/` or in a home directory names no root at all.
async fn pane_agent_and_root(
    session: &SessionConfig,
    pane_id: &str,
) -> (Option<String>, Option<PathBuf>) {
    let Ok(pane) = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
    else {
        return (None, None);
    };
    let root = pane
        .cwd
        .filter(|path| is_scannable_root(path))
        .and_then(|path| std::fs::canonicalize(path).ok());
    (pane.agent, root)
}

/// The per-pane capability descriptor, because agent detection varies pane to
/// pane: `native` means the agent's own protocol answered, `dictionary` means
/// the parts were read off the screen, and `text` means this pane fell back to
/// prose and a client should not wait for tool blocks.
///
/// `native` is a third value of an existing enum, not a new concept: the parts
/// under it are the same closed set in the same envelope. What it does tell a
/// client is which coordinate system `range` is in -- terminal rows for a
/// dictionary read, rows of the adapter's own rendering for a native one, which
/// a client rebuilds by joining the parts' `fallback_text`.
///
/// `composer` is absent rather than null for an agent with no command table, so
/// a client can tell "this gateway knows nothing about this agent" from "this
/// agent understands no slash commands".
fn pane_capabilities(
    pane_id: &str,
    agent: Option<&str>,
    dictionary: Option<&'static parts::Dictionary>,
    native: Option<&native::NativeRead>,
    composer: Option<Value>,
) -> Value {
    let mut capabilities = json!({
        "pane_id": pane_id,
        "agent": agent,
        "parts": match (native.is_some(), dictionary.is_some()) {
            (true, _) => "native",
            (false, true) => "dictionary",
            (false, false) => "text",
        },
        "dictionary": dictionary.map(|dictionary| dictionary.id),
        // Absent unless a protocol actually answered, the same discipline
        // `composer` is under: a client must not have to tell "the adapter
        // could have read this pane" from "the adapter did".
        "native": native.map(|read| json!({
            "protocol": native::adapter_for(agent).map(|adapter| adapter.protocol),
            "version": read.version,
            "session": read.session,
        })),
        "image_input": "file-path",
    });
    if let (Some(object), Some(composer)) = (capabilities.as_object_mut(), composer) {
        object.insert("composer".to_owned(), composer);
    }
    capabilities
}

/// Fuzzy path search inside one pane's workspace, for the composer's `@` file
/// mentions.
///
/// Fenced exactly like the asset API: the only directory this can look in is
/// the pane's own working directory as Herdr reports it, canonicalized, and
/// every answer is a path relative to it. A pane whose cwd is not a workspace
/// -- the filesystem root, the home directory, a pane Herdr does not report --
/// is a miss rather than an error, the same way a fenced-out asset path is: it
/// must not be usable to probe the host.
///
/// Paths only. No contents, no sizes, no absolute paths. Reading a file is what
/// `GET /api/assets/{id}/content` is for, and it has its own fence.
async fn pane_files(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<FileSearchQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let limit = query
        .limit
        .unwrap_or(composer::FILE_SEARCH_DEFAULT_LIMIT)
        .clamp(1, composer::FILE_SEARCH_MAX_LIMIT);
    let needle = query.query.clone().unwrap_or_default();

    // The roots come from the same place the asset listing's do -- the pane
    // cwds Herdr reports -- so this endpoint cannot reach a directory the
    // assets API would refuse. Whole-session here (not workspace-scoped): the
    // narrowing below is to one exact pane, which is already narrower than one
    // workspace, so there is nothing for a workspace filter to add.
    let roots = session_asset_roots(&state, &session, None).await;
    let root = roots
        .iter()
        .find(|root| root.pane_id.as_deref() == Some(pane_id.as_str()))
        .and_then(|root| std::fs::canonicalize(&root.path).ok());

    let files = match root.clone() {
        Some(root) => {
            let needle = needle.clone();
            tokio::task::spawn_blocking(move || composer::search_files(&root, &needle, limit))
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "query": needle,
        "limit": limit,
        // The directory every path below is relative to, or null when this pane
        // has no workspace the gateway will look in.
        "root": root.map(|root| root.to_string_lossy().to_string()),
        "files": files
            .into_iter()
            .map(|hit| json!({ "path": hit.path, "name": hit.name, "kind": hit.kind }))
            .collect::<Vec<Value>>(),
    }))))
}

/// Which agents have a key row and command list, and where to add one. Lets a
/// client tell "this agent has no profile yet" from "the gateway is old".
async fn keymaps(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(shortcuts::catalog()))
}

#[derive(Debug, Deserialize)]
struct AgentEventsQuery {
    /// The highest `seq` the client already has. Absent means "everything you
    /// still hold", which is what a phone opening a server cold asks for.
    #[serde(default)]
    since: Option<u64>,
}

/// What the agents in this session did recently, oldest first.
///
/// For the app's "while you were away" digest: a phone that was off the network
/// for an hour has, at best, a stack of notifications it cannot order and, at
/// worst, none at all. This answers with the transitions themselves, so the
/// digest is built from what happened rather than from what was delivered.
///
/// Memory only and bounded, so `missed` is a real answer rather than an
/// embarrassment: it says the ring rolled past the caller's `since`, and a
/// client that knows its digest is partial can say so instead of implying a
/// complete account.
async fn session_agent_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session_id)?;
    let log = state.agent_events.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent_events_lock_failed",
            "failed to read recent agent activity",
        )
    })?;
    let events = log.since(&session_id, query.since);
    Ok(Json(json!({
        "session_id": session_id,
        "since": query.since,
        "events": events.iter().map(agent_events::AgentEvent::to_json).collect::<Vec<_>>(),
        // What to send as `since` next time, whether or not this answer was
        // empty, so a client polling an idle session does not walk backwards.
        "next_since": log.latest_seq(&session_id),
        "missed": log.missed(&session_id, query.since),
        "capacity": agent_events::RING_CAPACITY,
    })))
}

/// One pane as Herdr describes it, already unwrapped from the response
/// envelope. A pane the socket cannot answer for is a 502 rather than a guess:
/// every caller here is about to act on what this pane is running.
async fn pane_get(session: &SessionConfig, pane_id: &str) -> ApiResult<Value> {
    let pane = terminal_backend(session)
        .get_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    Ok(backend::compat::pane_get(pane)
        .pointer("/result/pane")
        .cloned()
        .unwrap_or_default())
}

/// The key row and slash commands for whatever this pane is running.
///
/// Resolving this here rather than in the client means a client picks up a new
/// agent when the developer updates the gateway, without shipping a new build.
async fn pane_shortcuts(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let pane = pane_get(&session, &pane_id).await?;
    let pane = &pane;

    // Herdr reports the agent on the pane itself when one is attached; the
    // stripped title is what is left of the terminal title, which is how a
    // full-screen program like an editor announces itself.
    let agent = pane
        .get("agent")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let title = pane
        .get("terminal_title_stripped")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    // The working directory scopes project-local commands, e.g. a repo's own
    // `.claude/commands`.
    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    Ok(Json(shortcuts::resolve(agent, title, cwd)))
}

/// Whether the pane is blocked on a permission menu, and what it is asking.
///
/// Deliberately its own endpoint rather than a part. `docs/content-model.md`
/// keeps the part set closed and gives approvals a part type only in v2; until
/// then this carries the same information without spending the closed set on
/// it, and without a client having to poll the transcript to learn that the
/// agent is waiting.
async fn pane_approval(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    // An agent that reports its approvals is believed over the screen: a menu
    // read off a terminal can only ever be the last frame drawn, and a protocol
    // says outright whether it is still waiting and on which request.
    let (agent, root) = pane_agent_and_root(&session, &pane_id).await;
    if let Some(adapter) = native::adapter_for(agent.as_deref()) {
        // Only when an endpoint is configured: an adapter with nothing behind
        // it must leave the pane on the drawn-menu path rather than answering
        // "idle" for a pane that is in fact blocked.
        if native::endpoint(adapter).is_some() {
            let pending = native::pending(adapter, root.as_deref(), i18n::current()).await;
            return Ok(Json(content_envelope(native_approval_data(
                &session_id,
                &pane_id,
                agent.as_deref(),
                pending.as_ref(),
            ))));
        }
    }

    let (agent, approval) = read_pane_approval(&session, &pane_id).await?;
    Ok(Json(content_envelope(approval_data(
        &session_id,
        &pane_id,
        agent.as_deref(),
        approval.as_ref(),
        "menu",
    ))))
}

/// The same payload for a pane whose agent reports its approvals.
///
/// One shape, two sources: `approval` carries the request the client answers,
/// and only the fields a protocol can honestly fill in. There is no cursor, so
/// no option is `selected`; the identity is the agent's own request id rather
/// than a fingerprint of the drawn text, because the agent guarantees it.
fn native_approval_data(
    session_id: &str,
    pane_id: &str,
    agent: Option<&str>,
    pending: Option<&native::NativeApproval>,
) -> Value {
    json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "state": if pending.is_some() { "pending" } else { "idle" },
        "approval": pending.map(|pending| {
            let request = &pending.request;
            json!({
                "approval_id": request.id,
                "prompt": request.prompt,
                "tool": request.tool,
                "context": request.context,
                "options": request
                    .options
                    .iter()
                    .map(|option| json!({
                        "index": option.index,
                        "label": option.label,
                        "decision": option.decision,
                    }))
                    .collect::<Vec<Value>>(),
            })
        }),
        "pane": {
            "pane_id": pane_id,
            "agent": agent,
            "approvals": "protocol",
        },
    })
}

/// Answer the menu the pane is blocked on.
///
/// The answer is named by option number or by decision (`allow`,
/// `allow_always`, `deny`), never by keystroke: which keys move an agent's
/// cursor is exactly the detail a client should not have to know, and having to
/// know it is what makes raw `send-keys` the fallback path rather than the one
/// a client reaches for.
async fn answer_pane_approval(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AnswerApprovalBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    // An agent that reports its approvals is answered through its protocol
    // rather than through its keyboard. Naming the request by id is what makes
    // that strictly safer: there is no cursor to walk, and a request that was
    // resolved between the read and the answer is refused by the agent instead
    // of being answered blind by whatever the menu was redrawn into.
    if let Some(answered) = answer_native_approval(&session, &session_id, &pane_id, &body).await? {
        return Ok(Json(answered));
    }

    let (agent, pending) = read_pane_approval(&session, &pane_id).await?;
    let Some(pending) = pending else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_not_pending",
            "the pane is not waiting on an approval",
        ));
    };
    // The phone may be acting on a notification from a minute ago. A
    // fingerprint mismatch means the agent has moved on to a different
    // question, and answering that one blind is exactly what must not happen.
    if let Some(expected) = body.fingerprint.as_deref() {
        if expected != pending.fingerprint {
            return Err(api_error(
                StatusCode::CONFLICT,
                "approval_changed",
                "the pane is waiting on a different approval",
            ));
        }
    }

    let index = match (body.option, body.decision.as_deref()) {
        (Some(index), _) => index,
        (None, Some(name)) => {
            let decision = match name {
                "allow" => approvals::Decision::Allow,
                "allow_always" => approvals::Decision::AllowAlways,
                "deny" => approvals::Decision::Deny,
                _ => {
                    return Err(api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_decision",
                        "decision must be allow, allow_always, or deny",
                    ))
                }
            };
            pending
                .option_for(decision)
                .map(|option| option.index)
                .ok_or_else(|| {
                    api_error(
                        StatusCode::CONFLICT,
                        "decision_unavailable",
                        "this approval offers no option with that meaning",
                    )
                })?
        }
        (None, None) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_answer",
                "answer with an option number or a decision",
            ))
        }
    };
    let answered = pending
        .option(index)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_option",
                "this approval has no option with that number",
            )
        })?
        .clone();
    let keys = pending.keys_for(index).ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_option",
            "this approval has no option with that number",
        )
    })?;

    send_pane_keys(&session, &pane_id, &keys).await?;

    // Some menus act on the digit; some want it confirmed afterwards. From out
    // here the two look identical, so the confirm is deferred and only sent
    // when the same menu is demonstrably still standing -- the lesson from
    // following a pasted prompt with Enter, where the two raced.
    let mut sent = keys;
    tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
    let mut after = read_pane_approval(&session, &pane_id)
        .await
        .map(|(_, approval)| approval)
        .unwrap_or_default();
    if after
        .as_ref()
        .is_some_and(|approval| approval.fingerprint == pending.fingerprint)
    {
        send_pane_keys(&session, &pane_id, &["Enter".to_string()]).await?;
        sent.push("Enter".into());
        tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
        after = read_pane_approval(&session, &pane_id)
            .await
            .map(|(_, approval)| approval)
            .unwrap_or_default();
    }
    let resolved = after
        .as_ref()
        .is_none_or(|approval| approval.fingerprint != pending.fingerprint);

    let mut data = approval_data(
        &session_id,
        &pane_id,
        agent.as_deref(),
        after.as_ref(),
        "menu",
    );
    if let Some(object) = data.as_object_mut() {
        object.insert("resolved".into(), json!(resolved));
        object.insert("sent_keys".into(), json!(sent));
        object.insert(
            "answered".into(),
            json!({
                "fingerprint": pending.fingerprint,
                "index": answered.index,
                "decision": answered.decision.as_str(),
            }),
        );
    }
    Ok(Json(content_envelope(data)))
}

/// Answer a pane's approval through the agent's own protocol, when it has one.
///
/// `Ok(None)` means this pane has no native source -- no adapter, no configured
/// endpoint, or nothing pending there -- and the caller falls through to the
/// drawn-menu path. Nothing here can make a pane unanswerable.
async fn answer_native_approval(
    session: &SessionConfig,
    session_id: &str,
    pane_id: &str,
    body: &AnswerApprovalBody,
) -> ApiResult<Option<Value>> {
    let (agent, root) = pane_agent_and_root(session, pane_id).await;
    let Some(adapter) = native::adapter_for(agent.as_deref()) else {
        return Ok(None);
    };
    if native::endpoint(adapter).is_none() {
        return Ok(None);
    }
    // Past this point the protocol is the authority, and a pane it says is not
    // waiting is not answered by keystroke either: the menu still on the screen
    // is the last frame of one that has already been resolved.
    let Some(pending) = native::pending(adapter, root.as_deref(), i18n::current()).await else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_not_pending",
            "the pane is not waiting on an approval",
        ));
    };

    // A protocol names its answers, so an option number is read off the part
    // the client was shown rather than off a cursor the agent moved.
    let decision = match (body.decision.as_deref(), body.option) {
        (Some(name), _) => decision_named(name)?,
        (None, Some(index)) => pending
            .request
            .options
            .iter()
            .find(|option| option.index == index)
            .map(|option| decision_named(option.decision))
            .transpose()?
            .ok_or_else(|| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_option",
                    "this approval has no option with that number",
                )
            })?,
        (None, None) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_answer",
                "answer with an option number or a decision",
            ))
        }
    };
    let accepted = native::answer(&pending, decision).await.unwrap_or(false);
    if !accepted {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_changed",
            "the agent no longer has that request pending",
        ));
    }

    let mut data = approval_data(session_id, pane_id, agent.as_deref(), None, "protocol");
    if let Some(object) = data.as_object_mut() {
        object.insert("resolved".into(), json!(true));
        // No keystrokes were sent, and saying so is how a client can tell the
        // two paths apart without the wire naming either agent's protocol.
        object.insert("sent_keys".into(), json!([] as [String; 0]));
        object.insert(
            "answered".into(),
            json!({
                "approval_id": pending.request.id,
                "decision": decision.as_str(),
            }),
        );
    }
    Ok(Some(content_envelope(data)))
}

/// The decision a client named, or a 400.
fn decision_named(name: &str) -> ApiResult<approvals::Decision> {
    match name {
        "allow" => Ok(approvals::Decision::Allow),
        "allow_always" => Ok(approvals::Decision::AllowAlways),
        "deny" => Ok(approvals::Decision::Deny),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_decision",
            "decision must be allow, allow_always, or deny",
        )),
    }
}

async fn send_pane_keys(
    session: &SessionConfig,
    pane_id: &str,
    keys: &[String],
) -> ApiResult<Value> {
    terminal_backend(session)
        .send_keys(&BackendPaneId::new(pane_id), keys)
        .await
        .map_err(backend_api_error)?;
    Ok(backend::compat::command_ok("pane_keys_sent"))
}

/// What a pane is drawing right now, as plain text.
///
/// `visible` rather than the transcript source the parts endpoint reads: both
/// callers care about the current screen, and the scrollback of an answered menu
/// or an already-submitted prompt would only mislead them.
async fn read_pane_visible_text(session: &SessionConfig, pane_id: &str) -> ApiResult<String> {
    terminal_backend(session)
        .read_pane(&BackendReadPane {
            pane_id: BackendPaneId::new(pane_id),
            source: BackendOutputSource::Visible,
            format: BackendOutputFormat::Text,
            lines: APPROVAL_READ_LINES,
            start: None,
            end: None,
        })
        .await
        .map(|output| output.text)
        .map_err(backend_api_error)
}

/// Read a pane's agent and whatever menu it is drawing.
async fn read_pane_approval(
    session: &SessionConfig,
    pane_id: &str,
) -> ApiResult<(Option<String>, Option<approvals::Approval>)> {
    let agent = pane_get(session, pane_id)
        .await
        .ok()
        .and_then(|pane| pane.get("agent").and_then(Value::as_str).map(str::to_owned))
        .filter(|agent| !agent.is_empty());
    let text = read_pane_visible_text(session, pane_id).await?;
    Ok((agent, approvals::detect(&text)))
}

/// The payload both the endpoint and the SSE events carry.
fn approval_data(
    session_id: &str,
    pane_id: &str,
    agent: Option<&str>,
    approval: Option<&approvals::Approval>,
    source: &str,
) -> Value {
    json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "state": if approval.is_some() { "pending" } else { "idle" },
        "approval": approval.map(approvals::Approval::to_json),
        // Per-pane capability, in the same shape the parts endpoint answers
        // with. "menu" means the approval was read off what the agent drew, and
        // a client that dislikes the reading still has raw send-keys.
        // "protocol" means the agent reported it and was answered by name, so
        // there is no cursor to race and no keystroke was sent.
        "pane": {
            "pane_id": pane_id,
            "agent": agent,
            "approvals": source,
        },
    })
}

async fn send_text(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendTextBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .send_text(&BackendPaneId::new(pane_id), &body.text)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_text_sent")))
}

async fn send_keys(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendKeysBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    if body.keys.is_empty() || body.keys.len() > MAX_SEND_KEYS {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_keys",
            "keys must contain 1 to 32 entries",
        ));
    }
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .send_keys(&BackendPaneId::new(pane_id), &body.keys)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("pane_keys_sent")))
}

/// A file type the gateway is willing to store. Binary formats get both fields
/// from their magic bytes; UTF-8 text earns only a small extension allow-list
/// after its content has passed the text probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadKind {
    extension: &'static str,
    mime: &'static str,
}

/// Accept a file from the phone, park it in the gateway's own upload
/// directory and hand back a local path. The app then sends that path to an
/// agent as ordinary text, so the agent reads the file straight off this
/// machine.
///
/// Images and PDFs are identified by magic bytes. Source and document text has
/// to be valid, low-control UTF-8 before its display-name extension is allowed
/// to influence the generated stored name. Nothing from the client is ever
/// used as a path.
async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    // Taking the rejection by hand keeps authorization ahead of body parsing,
    // and keeps every failure in the same JSON error shape as the other routes.
    require_device(&state, &headers)?;
    let mut multipart = multipart.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "expected a multipart/form-data body with a file field",
        )
    })?;

    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(upload_body_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let client_name = field.file_name().map(str::to_owned);
        let bytes = field.bytes().await.map_err(upload_body_error)?;
        upload = Some((client_name, bytes));
        break;
    }

    let Some((client_name, bytes)) = upload else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_file",
            "expected a multipart/form-data body with a file field",
        ));
    };
    let Some(client_name) = client_name else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_filename",
            "the file field must carry a filename",
        ));
    };
    if bytes.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_file",
            "the file field is empty",
        ));
    }

    // Executables are checked separately from the content allow-list so the
    // refusal says what it means, and so adding a document format cannot
    // accidentally make an executable acceptable.
    if looks_executable(&bytes) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "executable_rejected",
            "executables and scripts are not accepted",
        ));
    }
    let Some(kind) = sniff_upload_kind(&bytes)
        .or_else(|| sniff_office_upload_kind(&bytes))
        .or_else(|| sniff_document_upload_kind(&bytes, &client_name))
    else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_file_type",
            "only supported images, office documents, PDF, and UTF-8 text are accepted",
        ));
    };

    let dir = ensure_uploads_dir().map_err(|err| {
        eprintln!("failed to prepare the upload directory: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;
    let path = dir.join(stored_upload_name(kind));
    write_upload_file(&path, &bytes).map_err(|err| {
        eprintln!("failed to write upload {}: {err:#}", path.display());
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "name": sanitize_upload_name(&client_name),
        "size": bytes.len(),
        "mime": kind.mime
    })))
}

/// The body limit is enforced by the framework while the body streams, so an
/// oversized upload surfaces here as a length-limit error rather than as a
/// fully buffered file the gateway then has to measure.
fn upload_body_error(err: MultipartError) -> (StatusCode, Json<Value>) {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload_too_large",
            "the upload must be at most 25 MiB",
        );
    }
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_multipart",
        "expected a multipart/form-data body with a file field",
    )
}

/// Reject anything the host could be talked into running, whatever the file is
/// called. Extensions are not consulted: only the leading bytes are.
fn looks_executable(bytes: &[u8]) -> bool {
    /// Mach-O thin and fat binaries, in both byte orders. `cafebabe` also
    /// covers Java class files, which is no loss here.
    const MACH_O_MAGICS: [[u8; 4]; 6] = [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];

    bytes.starts_with(b"#!")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || MACH_O_MAGICS
            .iter()
            .any(|magic| bytes.starts_with(magic.as_slice()))
}

/// Decide what a file is from its content. Only images are accepted, and the
/// client's extension is never consulted, so a `.png` holding anything else is
/// refused however it was named.
fn sniff_upload_kind(bytes: &[u8]) -> Option<UploadKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(UploadKind {
            extension: "png",
            mime: "image/png",
        });
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(UploadKind {
            extension: "jpg",
            mime: "image/jpeg",
        });
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(UploadKind {
            extension: "gif",
            mime: "image/gif",
        });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(UploadKind {
            extension: "webp",
            mime: "image/webp",
        });
    }
    if is_heic(bytes) {
        return Some(UploadKind {
            extension: "heic",
            mime: "image/heic",
        });
    }
    None
}

/// Recognise modern Office/OpenDocument containers without extracting them.
/// The central directory carries entry names in plain bytes even when the
/// entries themselves are compressed, which is enough to distinguish a Word,
/// Excel or PowerPoint package from an arbitrary zip. ODF additionally puts an
/// uncompressed `mimetype` entry first by specification.
fn sniff_office_upload_kind(bytes: &[u8]) -> Option<UploadKind> {
    let names = zip_entry_names(bytes)?;
    let has = |needle: &str| names.contains(&needle);
    let ooxml_root = has("[Content_Types].xml") && has("_rels/.rels");
    let mut ooxml = Vec::new();
    if ooxml_root && has("word/document.xml") {
        ooxml.push(UploadKind {
            extension: "docx",
            mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        });
    }
    if ooxml_root && has("xl/workbook.xml") {
        ooxml.push(UploadKind {
            extension: "xlsx",
            mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        });
    }
    if ooxml_root && has("ppt/presentation.xml") {
        ooxml.push(UploadKind {
            extension: "pptx",
            mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        });
    }
    if ooxml.len() == 1 {
        return ooxml.pop();
    }
    if !ooxml.is_empty() {
        // A package claiming to be multiple Office document kinds is not one
        // the app or an agent should be asked to interpret.
        return None;
    }

    if !has("content.xml") || !has("META-INF/manifest.xml") {
        return None;
    }
    let (name, mime) = stored_first_zip_entry(bytes)?;
    if name != "mimetype" {
        return None;
    }
    match mime {
        b"application/vnd.oasis.opendocument.text" => Some(UploadKind {
            extension: "odt",
            mime: "application/vnd.oasis.opendocument.text",
        }),
        b"application/vnd.oasis.opendocument.spreadsheet" => Some(UploadKind {
            extension: "ods",
            mime: "application/vnd.oasis.opendocument.spreadsheet",
        }),
        b"application/vnd.oasis.opendocument.presentation" => Some(UploadKind {
            extension: "odp",
            mime: "application/vnd.oasis.opendocument.presentation",
        }),
        _ => None,
    }
}

const ZIP_LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
const ZIP_CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
const ZIP_END_HEADER: &[u8; 4] = b"PK\x05\x06";
const MAX_OFFICE_ZIP_ENTRIES: usize = 4_096;

fn zip_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn zip_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Read only bounded metadata from an ordinary (non-Zip64) central directory.
/// Encrypted entries, split archives and malformed offsets all fail closed.
fn zip_entry_names(bytes: &[u8]) -> Option<Vec<&str>> {
    if bytes.len() < 22 || !bytes.starts_with(ZIP_LOCAL_HEADER) {
        return None;
    }
    let search_start = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let eocd = (search_start..=bytes.len() - 22)
        .rev()
        .find(|offset| bytes.get(*offset..*offset + 4) == Some(ZIP_END_HEADER.as_slice()))?;
    if zip_u16(bytes, eocd + 4)? != 0 || zip_u16(bytes, eocd + 6)? != 0 {
        return None;
    }
    let disk_entries = zip_u16(bytes, eocd + 8)? as usize;
    let entry_count = zip_u16(bytes, eocd + 10)? as usize;
    if entry_count != disk_entries || entry_count == 0 || entry_count > MAX_OFFICE_ZIP_ENTRIES {
        return None;
    }
    let central_size = zip_u32(bytes, eocd + 12)? as usize;
    let central_offset = zip_u32(bytes, eocd + 16)? as usize;
    let central_end = central_offset.checked_add(central_size)?;
    if central_end > eocd || central_end > bytes.len() {
        return None;
    }

    let mut cursor = central_offset;
    let mut names = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if bytes.get(cursor..cursor + 4)? != ZIP_CENTRAL_HEADER {
            return None;
        }
        // Bit zero is traditional ZIP encryption. An agent could not inspect
        // it anyway, and accepting opaque encrypted containers would defeat
        // the entry-name validation this function exists to provide.
        if zip_u16(bytes, cursor + 8)? & 1 != 0 {
            return None;
        }
        let name_len = zip_u16(bytes, cursor + 28)? as usize;
        let extra_len = zip_u16(bytes, cursor + 30)? as usize;
        let comment_len = zip_u16(bytes, cursor + 32)? as usize;
        let name_start = cursor.checked_add(46)?;
        let name_end = name_start.checked_add(name_len)?;
        let next = name_end.checked_add(extra_len)?.checked_add(comment_len)?;
        if next > central_end {
            return None;
        }
        let name = std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?;
        if name.contains('\0') || name.starts_with('/') || name.split('/').any(|part| part == "..")
        {
            return None;
        }
        names.push(name);
        cursor = next;
    }
    (cursor == central_end).then_some(names)
}

/// ODF's first entry is mandated to be uncompressed `mimetype`. Reading that
/// tiny stored value does not inflate attacker-controlled data.
fn stored_first_zip_entry(bytes: &[u8]) -> Option<(&str, &[u8])> {
    if bytes.get(0..4)? != ZIP_LOCAL_HEADER {
        return None;
    }
    let flags = zip_u16(bytes, 6)?;
    let method = zip_u16(bytes, 8)?;
    if flags & 1 != 0 || method != 0 {
        return None;
    }
    let compressed = zip_u32(bytes, 18)? as usize;
    let uncompressed = zip_u32(bytes, 22)? as usize;
    if compressed != uncompressed || compressed > 256 {
        return None;
    }
    let name_len = zip_u16(bytes, 26)? as usize;
    let extra_len = zip_u16(bytes, 28)? as usize;
    let name_start = 30usize;
    let name_end = name_start.checked_add(name_len)?;
    let data_start = name_end.checked_add(extra_len)?;
    let data_end = data_start.checked_add(compressed)?;
    Some((
        std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?,
        bytes.get(data_start..data_end)?,
    ))
}

/// Documents stay deliberately narrow: PDF has an unambiguous signature and
/// everything else must first pass the same UTF-8 text probe used by the asset
/// browser. The filename may then preserve a useful source/document extension,
/// but can never turn binary bytes into an accepted upload.
fn sniff_document_upload_kind(bytes: &[u8], client_name: &str) -> Option<UploadKind> {
    if bytes.starts_with(b"%PDF-") {
        return Some(UploadKind {
            extension: "pdf",
            mime: "application/pdf",
        });
    }
    if !looks_textual(bytes) {
        return None;
    }

    let extension = client_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    Some(match extension.as_deref() {
        Some("md" | "markdown" | "mdx") => UploadKind {
            extension: "md",
            mime: "text/markdown; charset=utf-8",
        },
        Some("json") => UploadKind {
            extension: "json",
            mime: "application/json",
        },
        Some("jsonl" | "ndjson") => UploadKind {
            extension: "jsonl",
            mime: "application/x-ndjson",
        },
        Some("csv") => UploadKind {
            extension: "csv",
            mime: "text/csv; charset=utf-8",
        },
        Some("yaml" | "yml") => UploadKind {
            extension: "yaml",
            mime: "application/yaml",
        },
        Some("toml") => UploadKind {
            extension: "toml",
            mime: "application/toml",
        },
        Some("xml") => UploadKind {
            extension: "xml",
            mime: "application/xml",
        },
        Some("rtf") => UploadKind {
            extension: "rtf",
            mime: "application/rtf",
        },
        Some("ts") => text_upload_kind("ts"),
        Some("tsx") => text_upload_kind("tsx"),
        Some("js") => text_upload_kind("js"),
        Some("jsx") => text_upload_kind("jsx"),
        Some("css") => text_upload_kind("css"),
        Some("html") => text_upload_kind("html"),
        Some("py") => text_upload_kind("py"),
        Some("rs") => text_upload_kind("rs"),
        Some("go") => text_upload_kind("go"),
        Some("java") => text_upload_kind("java"),
        Some("kt") => text_upload_kind("kt"),
        Some("swift") => text_upload_kind("swift"),
        Some("c") => text_upload_kind("c"),
        Some("h") => text_upload_kind("h"),
        Some("cpp") => text_upload_kind("cpp"),
        Some("hpp") => text_upload_kind("hpp"),
        Some("sql") => text_upload_kind("sql"),
        Some("log") => text_upload_kind("log"),
        _ => text_upload_kind("txt"),
    })
}

fn text_upload_kind(extension: &'static str) -> UploadKind {
    UploadKind {
        extension,
        mime: "text/plain; charset=utf-8",
    }
}

/// HEIC is an ISO base media file: a `ftyp` box whose brand names the flavour.
fn is_heic(bytes: &[u8]) -> bool {
    const HEIC_BRANDS: [&[u8; 4]; 10] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    ];

    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && HEIC_BRANDS.iter().any(|brand| &bytes[8..12] == *brand)
}

/// The name on disk is generated here in full: a random stem plus the
/// extension the content earned. A client name can therefore never traverse,
/// collide, or smuggle in a second extension.
fn stored_upload_name(kind: UploadKind) -> String {
    format!("{}.{}", uuid::Uuid::new_v4(), kind.extension)
}

/// Reduce the client's file name to something safe to show. It is echoed back
/// so the app can label the attachment; it never touches the filesystem.
fn sanitize_upload_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .replace(|c: char| c.is_control(), "");
    let base = base.trim();
    if base.is_empty() || base.chars().all(|c| c == '.') {
        return String::from("upload");
    }
    base.chars().take(MAX_UPLOAD_NAME_CHARS).collect()
}

fn uploads_dir() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join(UPLOADS_DIR))
}

/// Uploads live beside the gateway's other state, in their own directory so a
/// sweep can never reach a token file.
fn ensure_uploads_dir() -> anyhow::Result<PathBuf> {
    let dir = uploads_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create upload dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to lock down upload dir {}", dir.display()))?;
    }
    Ok(dir)
}

fn write_upload_file(path: &FsPath, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, bytes)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Sweep old uploads at startup and every hour after that. A phone that
/// uploads a screenshot and loses interest should not leave it on the host.
fn spawn_upload_gc() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPLOAD_GC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick completes immediately, which is the startup sweep.
            ticker.tick().await;
            let Ok(dir) = uploads_dir() else {
                continue;
            };
            if let Err(err) = purge_expired_uploads(&dir, SystemTime::now()) {
                eprintln!("failed to sweep old uploads: {err:#}");
            }
        }
    });
}

/// Delete every stored upload older than the retention window. Returns how many
/// files were removed.
fn purge_expired_uploads(dir: &FsPath, now: SystemTime) -> anyhow::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read upload dir {}", dir.display()))?
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if !upload_expired(modified, now) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) => eprintln!("failed to remove {}: {err}", entry.path().display()),
        }
    }
    Ok(removed)
}

/// A file whose timestamp sits in the future is left alone rather than deleted:
/// a clock change should not destroy an upload the user just made.
fn upload_expired(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age >= UPLOAD_RETENTION)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Assets: the file half of the unified content model.
//
// An asset is a file a session's workspaces produced that the user may want to
// look at on the phone. Two feeds keep the index current, in this order:
//
// 1. Herdr's `worktree.*` events, which the gateway already subscribes to.
//    They carry the checkout's root path (and its workspace), never the files
//    written inside it -- protocol 17 has no per-file event -- so what they
//    give is a precise "this root just changed" trigger, and the files come
//    from scanning exactly that root at that moment.
// 2. An mtime scan of the session's workspace roots, which is what a cold start
//    or a plain listing uses, and what makes the endpoint work on a session
//    that never touched a worktree.
//
// Reading a file is gated on provenance. The first rule is the one that has
// always applied: the path canonicalizes to a regular file inside a root the
// session currently has. The second rule exists because a workspace closes
// while the file it produced is still the thing the user wants to look at --
// a removed worktree used to take a twelve-hour-old asset with it. So when the
// roots no longer contain the path, an entry that *was* indexed while its root
// was live is still served, and only ever by replaying its stored canonical
// path: it is canonicalized again at read time and must come back byte-for-byte
// equal, so a symlink swapped into the old location resolves elsewhere and
// misses. Nothing that was never indexed becomes reachable by either rule, and
// every failure of either is the same 404 as an unknown id.
// ---------------------------------------------------------------------------

/// What a file is, decided from its bytes. The client picks a viewer from this,
/// so it is derived again on every read rather than trusted from a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Image,
    Markdown,
    Text,
    Pdf,
    Binary,
}

impl AssetKind {
    fn as_str(self) -> &'static str {
        match self {
            AssetKind::Image => "image",
            AssetKind::Markdown => "markdown",
            AssetKind::Text => "text",
            AssetKind::Pdf => "pdf",
            AssetKind::Binary => "binary",
        }
    }

    /// Binary is the one kind with nothing to show, so it is the one kind the
    /// content endpoint refuses instead of streaming.
    fn previewable(self) -> bool {
        !matches!(self, AssetKind::Binary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AssetType {
    kind: AssetKind,
    mime: &'static str,
}

/// A directory a session works in, and the Herdr identifiers that explain where
/// it came from.
#[derive(Debug, Clone)]
struct AssetRoot {
    path: PathBuf,
    session_id: String,
    workspace_id: Option<String>,
    tab_id: Option<String>,
    pane_id: Option<String>,
}

#[derive(Debug, Clone)]
struct AssetEntry {
    id: String,
    path: PathBuf,
    name: String,
    size: u64,
    modified_unix_ms: u128,
    root: PathBuf,
    session_id: String,
    workspace_id: Option<String>,
    tab_id: Option<String>,
    pane_id: Option<String>,
}

/// Which unit an assets request is scoped to.
///
/// tmux's tab is the tmux window, and is the granularity this exists to scope
/// to: a tmux *session* -- what this gateway calls a workspace -- spans every
/// project the developer happens to have a window open on, so scoping by
/// workspace narrows nothing on a machine with one tmux session (see
/// `tmux.rs:840-841`: `fields[0]` / the session is the workspace, `fields[1]`
/// / the window is the tab).
///
/// Herdr's tabs sit *inside* one of its own workspaces -- a herdr workspace's
/// `tab_count` need not be one -- and herdr's own workspace is already the
/// granularity card #802 scoped to. Narrowing a herdr session down to one tab
/// could hide a sibling tab's files that belong to the very same piece of
/// work, which would be a behavior change on a path herdr already had right
/// ("herdr must not change"). So a herdr session's tab id is resolved to the
/// workspace that owns it (`resolve_asset_scope`), and everything downstream
/// -- roots, the index, the cache -- scopes on that workspace instead of the
/// tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AssetScope {
    Tab(String),
    Workspace(String),
}

impl AssetScope {
    fn matches_root(&self, root: &AssetRoot) -> bool {
        match self {
            AssetScope::Tab(id) => root.tab_id.as_deref() == Some(id.as_str()),
            AssetScope::Workspace(id) => root.workspace_id.as_deref() == Some(id.as_str()),
        }
    }

    fn matches_entry(&self, entry: &AssetEntry) -> bool {
        match self {
            AssetScope::Tab(id) => entry.tab_id.as_deref() == Some(id.as_str()),
            AssetScope::Workspace(id) => entry.workspace_id.as_deref() == Some(id.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
struct ScannedFile {
    path: PathBuf,
    name: String,
    size: u64,
    modified_unix_ms: u128,
}

/// Everything the gateway knows about produced files, keyed by asset id. The
/// id is derived from the path, so the same file rediscovered by a later scan
/// -- or by a different root -- lands on the same entry instead of a duplicate.
#[derive(Debug, Default)]
struct AssetIndex {
    entries: HashMap<String, AssetEntry>,
    /// Keyed on `(session_id, scope)`. `scope` is `None` for the whole-session
    /// callers and `Some` for a scoped one, so the two never share a slot --
    /// see `session_asset_roots`.
    roots: HashMap<(String, Option<AssetScope>), Vec<AssetRoot>>,
}

impl AssetIndex {
    /// Fold one scanned file in. Answers whether this path had not been seen
    /// before, which is what makes an `asset.created` event honest.
    fn upsert(&mut self, entry: AssetEntry) -> bool {
        match self.entries.get_mut(&entry.id) {
            Some(existing) => {
                existing.size = entry.size;
                existing.modified_unix_ms = entry.modified_unix_ms;
                existing.name = entry.name;
                // Workspace roots nest: `~/.ws` and `~/.ws/api` both see the
                // same file. The deeper root is the one that actually produced
                // it, so it wins the attribution.
                if entry.root.as_os_str().len() > existing.root.as_os_str().len() {
                    existing.root = entry.root;
                    existing.session_id = entry.session_id;
                    existing.workspace_id = entry.workspace_id;
                    existing.tab_id = entry.tab_id;
                    existing.pane_id = entry.pane_id;
                }
                false
            }
            None => {
                self.entries.insert(entry.id.clone(), entry);
                true
            }
        }
    }

    fn get(&self, id: &str) -> Option<AssetEntry> {
        self.entries.get(id).cloned()
    }

    fn remember_roots(
        &mut self,
        session_id: &str,
        scope: Option<&AssetScope>,
        roots: Vec<AssetRoot>,
    ) {
        self.roots
            .insert((session_id.to_owned(), scope.cloned()), roots);
    }

    fn known_roots(&self, session_id: &str, scope: Option<&AssetScope>) -> Vec<AssetRoot> {
        self.roots
            .get(&(session_id.to_owned(), scope.cloned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Newest first, cut to one page.
    fn session_assets(
        &self,
        session_id: &str,
        scope: &AssetScope,
        since_unix_ms: Option<u128>,
        limit: usize,
    ) -> Vec<AssetEntry> {
        let mut entries = self.session_assets_ordered(session_id, scope, since_unix_ms);
        entries.truncate(limit);
        entries
    }

    /// The same ordering with nothing dropped -- newest first, with the path as
    /// the tie-break so two files written in the same millisecond still order
    /// the same way on every call.
    ///
    /// The caller that wants this rather than a page is the one that has to
    /// look past the page to fill it: a `kind` filter cannot be answered from
    /// the index, because what a file is comes from its bytes.
    ///
    /// Filtered by `scope` as well as `session_id`: an entry ingested under
    /// this session from an unrelated tab or workspace -- whether from before
    /// this scoping existed, or from the cold-start reindex in `asset_content`,
    /// which still rebuilds a whole session's worth of roots -- must not leak
    /// into a listing scoped to one tab (or, for herdr, one workspace). An
    /// entry with neither a workspace_id nor a tab_id (the exact-path lookup
    /// can index one without ever calling `pane_list_roots`) never matches a
    /// specific scope either, for the same reason: an unattributed file is not
    /// known to belong here.
    fn session_assets_ordered(
        &self,
        session_id: &str,
        scope: &AssetScope,
        since_unix_ms: Option<u128>,
    ) -> Vec<AssetEntry> {
        let mut entries: Vec<AssetEntry> = self
            .entries
            .values()
            .filter(|entry| entry.session_id == session_id)
            .filter(|entry| scope.matches_entry(entry))
            .filter(|entry| match since_unix_ms {
                Some(since) => entry.modified_unix_ms > since,
                None => true,
            })
            .cloned()
            .collect();
        entries.sort_by(|left, right| {
            right
                .modified_unix_ms
                .cmp(&left.modified_unix_ms)
                .then_with(|| left.path.cmp(&right.path))
        });
        entries
    }

    /// A removed worktree takes its files with it: keeping them would hand out
    /// ids that can only ever 404.
    fn forget_under(&mut self, root: &FsPath) {
        self.entries
            .retain(|_, entry| !entry.path.starts_with(root));
    }

    /// Keep the index a rolling window over what was produced recently.
    fn prune(&mut self) {
        if self.entries.len() <= MAX_INDEXED_ASSETS {
            return;
        }
        let mut ordered: Vec<(String, u128)> = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.modified_unix_ms))
            .collect();
        ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for (id, _) in ordered.into_iter().skip(MAX_INDEXED_ASSETS) {
            self.entries.remove(&id);
        }
    }
}

/// Opaque to the client, stable for the gateway: the same file keeps its id
/// across scans and across restarts, and the id itself carries no path a caller
/// could bend into something else.
fn asset_id(path: &FsPath) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let mut id = String::from("as_");
    for byte in digest.iter().take(12) {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

fn system_time_unix_ms(time: Option<SystemTime>) -> u128 {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// Decide what a file is from its first bytes. Images and PDFs are recognised
/// by magic number; everything else has to look like text before it can be one
/// of the text kinds, and only then does the extension get a say -- it decides
/// markdown from plain text and nothing else, so a `.md` full of binary is
/// still binary.
fn sniff_asset_type(bytes: &[u8], name: &str) -> AssetType {
    if let Some(image) = sniff_upload_kind(bytes) {
        return AssetType {
            kind: AssetKind::Image,
            mime: image.mime,
        };
    }
    if bytes.starts_with(b"%PDF-") {
        return AssetType {
            kind: AssetKind::Pdf,
            mime: "application/pdf",
        };
    }
    if !looks_textual(bytes) {
        return AssetType {
            kind: AssetKind::Binary,
            mime: "application/octet-stream",
        };
    }
    if has_markdown_extension(name) {
        return AssetType {
            kind: AssetKind::Markdown,
            mime: "text/markdown; charset=utf-8",
        };
    }
    AssetType {
        kind: AssetKind::Text,
        mime: "text/plain; charset=utf-8",
    }
}

fn has_markdown_extension(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .is_some_and(|extension| matches!(extension.as_str(), "md" | "markdown" | "mdx"))
}

/// Text is UTF-8 without NUL bytes and without a crowd of control characters.
/// The probe is a prefix of the file, so a multi-byte character cut in half at
/// the end is not held against it; an invalid sequence anywhere else is.
fn looks_textual(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }
    let valid_up_to = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(_) => return false,
    };
    let text = &bytes[..valid_up_to];
    if text.is_empty() {
        return false;
    }
    let control = text
        .iter()
        .filter(|byte| **byte < 0x20 && !matches!(**byte, b'\t' | b'\n' | b'\r' | 0x0c))
        .count();
    control * 20 <= text.len()
}

/// A pane sitting at the filesystem root, or straight in the home directory, is
/// not a workspace: scanning from there would sweep the whole machine.
fn is_scannable_root(path: &FsPath) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path.components().count() < 3 {
        return false;
    }
    if dirs::home_dir().is_some_and(|home| path == home) {
        return false;
    }
    true
}

/// Herdr does not report a "workspace root" as such. What it does report, for
/// every pane, is the directory that pane runs in -- which is where an agent
/// working in that pane writes its output.
fn pane_list_roots(session_id: &str, response: &Value) -> Vec<AssetRoot> {
    let mut roots: Vec<AssetRoot> = Vec::new();
    let Some(panes) = response.pointer("/result/panes").and_then(Value::as_array) else {
        return roots;
    };
    for pane in panes {
        let Some(cwd) = pane
            .get("cwd")
            .and_then(Value::as_str)
            .or_else(|| pane.get("foreground_cwd").and_then(Value::as_str))
        else {
            continue;
        };
        let path = PathBuf::from(cwd);
        if !is_scannable_root(&path) {
            continue;
        }
        if roots.iter().any(|root| root.path == path) {
            continue;
        }
        roots.push(AssetRoot {
            path,
            session_id: session_id.to_owned(),
            workspace_id: pane
                .get("workspace_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tab_id: pane
                .get("tab_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pane_id: pane
                .get("pane_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    roots
}

/// Herdr's `worktree.*` events carry the checkout root and its workspace, not
/// the files written inside it. Treated as a trigger rather than as content,
/// they are still the precise signal the asset index wants: scan this root, now.
fn worktree_event_root(session_id: &str, line: &str) -> Option<AssetRoot> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let event = value.get("event").and_then(Value::as_str)?;
    if !matches!(event, "worktree_created" | "worktree_opened") {
        return None;
    }
    let path = PathBuf::from(
        value
            .pointer("/data/worktree/path")
            .and_then(Value::as_str)?,
    );
    if !is_scannable_root(&path) {
        return None;
    }
    Some(AssetRoot {
        path,
        session_id: session_id.to_owned(),
        workspace_id: value
            .pointer("/data/workspace/workspace_id")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/data/workspace_id").and_then(Value::as_str))
            .map(str::to_owned),
        // The event carries the checkout's workspace, never a tab -- herdr's
        // own worktree.* payloads have no tab in them. Scoping never needs it:
        // a herdr session always resolves a request's tab to its workspace
        // (see `AssetScope`), and a tmux session never produces this event in
        // the first place (tmux worktrees are made through the git fallback,
        // with no protocol event of their own).
        tab_id: None,
        pane_id: None,
    })
}

fn worktree_event_removed_root(line: &str) -> Option<PathBuf> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("event").and_then(Value::as_str)? != "worktree_removed" {
        return None;
    }
    Some(PathBuf::from(
        value
            .pointer("/data/worktree/path")
            .and_then(Value::as_str)?,
    ))
}

/// Walk one root for files worth showing. Shallow, budgeted, and blind to
/// dependency directories, dot directories, and symlinks -- a link is how a
/// scan would leave the root, so it is never followed.
fn scan_workspace_root(root: &FsPath, max_depth: usize, max_files: usize) -> Vec<ScannedFile> {
    let mut files: Vec<ScannedFile> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            visited += 1;
            if visited > ASSET_SCAN_MAX_ENTRIES || files.len() >= max_files {
                return files;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth + 1 >= max_depth || ASSET_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files.push(ScannedFile {
                path: entry.path(),
                name,
                size: metadata.len(),
                modified_unix_ms: system_time_unix_ms(metadata.modified().ok()),
            });
        }
    }
    files
}

/// Scan one root and fold the result into the index. Answers with the entries
/// that were not there before, newest first.
fn ingest_root(index: &Mutex<AssetIndex>, root: &AssetRoot) -> Vec<AssetEntry> {
    let Ok(canonical) = std::fs::canonicalize(&root.path) else {
        return Vec::new();
    };
    let files = scan_workspace_root(&canonical, ASSET_SCAN_MAX_DEPTH, ASSET_SCAN_MAX_FILES);
    let Ok(mut index) = index.lock() else {
        eprintln!(
            "asset index lock failed while scanning {}",
            canonical.display()
        );
        return Vec::new();
    };
    let mut created: Vec<AssetEntry> = Vec::new();
    for file in files {
        let entry = AssetEntry {
            id: asset_id(&file.path),
            path: file.path,
            name: file.name,
            size: file.size,
            modified_unix_ms: file.modified_unix_ms,
            root: canonical.clone(),
            session_id: root.session_id.clone(),
            workspace_id: root.workspace_id.clone(),
            tab_id: root.tab_id.clone(),
            pane_id: root.pane_id.clone(),
        };
        if index.upsert(entry.clone()) {
            created.push(entry);
        }
    }
    index.prune();
    created.sort_by_key(|entry| std::cmp::Reverse(entry.modified_unix_ms));
    created
}

/// Scanning is blocking filesystem work, so it runs off the async runtime.
async fn ingest_roots(index: Arc<Mutex<AssetIndex>>, roots: Vec<AssetRoot>) -> Vec<AssetEntry> {
    tokio::task::spawn_blocking(move || {
        let mut created = Vec::new();
        for root in &roots {
            created.extend(ingest_root(&index, root));
        }
        created
    })
    .await
    .unwrap_or_default()
}

/// Herdr is the source of truth for where a session works, but a listing still
/// has to answer when the socket is down, so the last known roots are kept.
///
/// `scope` narrows the pane list tmux hands back -- every pane on the whole
/// server -- down to the one tab a caller asked about (or, for a herdr
/// session, the workspace that tab lives in; see `AssetScope`). `None` keeps
/// the old, whole-session answer for the callers that still want it (the
/// recent-directories picker, a task's candidate repo roots, and the
/// cold-start reindex), so nothing about them changes.
///
/// The cache is keyed on `(session_id, scope)`, not on `session_id` alone: a
/// scoped call and a whole-session call against the same session must not
/// read back each other's roots when `list_panes` fails and the last known
/// set is served instead. Collapsing the key back to `session_id` alone would
/// quietly widen every scoped listing back out to the whole machine the
/// moment the socket hiccuped -- the exact bug this function exists to close,
/// just deferred to the fallback path.
async fn session_asset_roots(
    state: &AppState,
    session: &SessionConfig,
    scope: Option<&AssetScope>,
) -> Vec<AssetRoot> {
    let response = terminal_backend(session)
        .list_panes()
        .await
        .map(backend::compat::pane_list)
        .map_err(|err| err.to_string());
    let mut fresh = match response {
        Ok(value) => pane_list_roots(&session.id, &value),
        Err(err) => {
            eprintln!("asset roots: pane.list failed: {err}");
            Vec::new()
        }
    };
    if let Some(scope) = scope {
        fresh.retain(|root| scope.matches_root(root));
    }
    if fresh.is_empty() {
        return match state.assets.lock() {
            Ok(index) => index.known_roots(&session.id, scope),
            Err(_) => Vec::new(),
        };
    }
    if let Ok(mut index) = state.assets.lock() {
        index.remember_roots(&session.id, scope, fresh.clone());
    }
    fresh
}

/// What a request's tab id actually scopes to, for this session's backend.
///
/// tmux: the tab id names a tmux window directly, which is exactly the unit
/// this card narrows to -- no lookup needed.
///
/// Herdr: herdr's own tabs sit inside one of its workspaces, and a herdr
/// workspace is the granularity that must not narrow (see `AssetScope`). The
/// tab id is translated to the workspace that owns it by asking the live pane
/// list which workspace that tab's panes belong to. A tab the pane list no
/// longer has -- a closed tab, or a socket hiccup -- still needs *some* scope
/// to key the cache and filter on, so the tab id itself is kept as the
/// fallback: this keeps the answer scoped (and therefore empty rather than
/// silently widened back to every workspace) even when the live lookup can't
/// resolve it.
async fn resolve_asset_scope(session: &SessionConfig, tab_id: &str) -> AssetScope {
    if !session.backend.is_herdr() {
        return AssetScope::Tab(tab_id.to_owned());
    }
    let workspace_id = terminal_backend(session)
        .list_panes()
        .await
        .ok()
        .and_then(|panes| {
            panes
                .into_iter()
                .find(|pane| pane.tab_id.as_str() == tab_id)
        })
        .map(|pane| pane.workspace_id.as_str().to_owned())
        .unwrap_or_else(|| tab_id.to_owned());
    AssetScope::Workspace(workspace_id)
}

fn canonical_roots(roots: &[AssetRoot]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(&root.path).ok())
        .collect()
}

/// The one gate on reading a file. Whatever an id claimed, the path has to
/// canonicalize to a regular file inside a root the session currently has.
/// Canonicalizing first is what closes symlink escapes: a link inside a root
/// that points outside it resolves to the outside path, and fails here.
fn resolve_asset_path(path: &FsPath, roots: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    roots
        .iter()
        .any(|root| canonical != *root && canonical.starts_with(root))
        .then_some(canonical)
}

/// What an id that is already in the index is allowed to read.
///
/// Root containment first, exactly as before: while the workspace that made the
/// file is still open, nothing about this changed. What is new is the second
/// answer, for the asset whose workspace has since closed -- a worktree removed
/// after the agent finished, which used to 404 the file it produced.
///
/// That fallback replays the entry's own stored canonical path and nothing
/// else. The path is canonicalized again and has to come back equal to what was
/// stored: a symlink dropped where the file used to be canonicalizes to its
/// target, which is a different path, and misses. A directory left in its place
/// is not a regular file, and misses. The file being gone at all misses. Since
/// the caller is an index lookup, a path that was never indexed has no entry to
/// replay and never reaches here -- provenance is what is being served, not a
/// filesystem.
fn resolve_indexed_asset_path(stored: &FsPath, roots: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = resolve_asset_path(stored, roots) {
        return Some(path);
    }
    let canonical = std::fs::canonicalize(stored).ok()?;
    (canonical == stored && canonical.is_file()).then_some(canonical)
}

/// Resolve one exact path into an asset. The app needs this because a file
/// path printed in a terminal has to map to the file it names -- matching by
/// name against the listing would land on the wrong one. The path is held to
/// the same fence as every other read, and anything that fails it answers "no
/// match" rather than an error, so this cannot be used to probe the host.
///
/// A scan is not involved, so a file deeper or more obscure than the scan
/// bothers with still resolves: the user pointed at it.
/// A session's roots with the gateway's uploads directory appended, so a
/// lookup can answer for a file the gateway itself stored on the phone's
/// behalf. `None` -- a state dir that cannot be resolved -- leaves the roots
/// exactly as they were.
fn with_uploads_root(
    mut roots: Vec<AssetRoot>,
    session_id: &str,
    uploads: Option<PathBuf>,
) -> Vec<AssetRoot> {
    if let Some(path) = uploads {
        roots.push(AssetRoot {
            path,
            session_id: session_id.to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        });
    }
    roots
}

fn asset_entry_for_path(raw: &str, roots: &[AssetRoot]) -> Option<AssetEntry> {
    let path = std::fs::canonicalize(raw).ok()?;
    if !path.is_file() {
        return None;
    }
    // Workspace roots nest, so the deepest containing root owns the file.
    let (owner, root) = roots
        .iter()
        .filter_map(|root| {
            std::fs::canonicalize(&root.path)
                .ok()
                .map(|canonical| (root, canonical))
        })
        .filter(|(_, canonical)| path != *canonical && path.starts_with(canonical))
        .max_by_key(|(_, canonical)| canonical.as_os_str().len())?;
    let metadata = std::fs::metadata(&path).ok()?;
    Some(AssetEntry {
        id: asset_id(&path),
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        path,
        size: metadata.len(),
        modified_unix_ms: system_time_unix_ms(metadata.modified().ok()),
        root,
        session_id: owner.session_id.clone(),
        workspace_id: owner.workspace_id.clone(),
        tab_id: owner.tab_id.clone(),
        pane_id: owner.pane_id.clone(),
    })
}

/// Read only as much of a file as it takes to type it.
fn read_asset_head(path: &FsPath) -> Vec<u8> {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut head = Vec::new();
    if file
        .take(ASSET_SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return Vec::new();
    }
    head
}

fn asset_json(entry: &AssetEntry, asset_type: AssetType) -> Value {
    json!({
        "id": entry.id,
        "path": entry.path.to_string_lossy(),
        "name": entry.name,
        "kind": asset_type.kind.as_str(),
        "mime": asset_type.mime,
        "size": entry.size,
        "modified_unix_ms": entry.modified_unix_ms,
        "origin": {
            "session_id": entry.session_id,
            "workspace_id": entry.workspace_id,
            "pane_id": entry.pane_id,
            "root": entry.root.to_string_lossy(),
        },
        "previewable": asset_type.kind.previewable(),
    })
}

/// The versioned envelope every content-model response carries.
fn content_envelope(data: Value) -> Value {
    json!({
        "schema_version": CONTENT_SCHEMA_VERSION,
        "capabilities": {
            "parts": true,
            "assets": true,
            "image_upload": true,
            // Slash-command catalogue and `@` file search. A pane's own
            // descriptor still says whether this particular agent has a table.
            "composer": true,
        },
        "data": data,
    })
}

#[derive(Debug, Deserialize)]
struct AssetsQuery {
    /// Unix milliseconds, matching every `modified_unix_ms` on the wire; only
    /// files modified strictly after this are returned, so a client can poll
    /// for what is new without re-reading the list.
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
    /// Comma-separated allow-list of asset kinds, e.g. `markdown,pdf`. Absent
    /// or empty means every kind, so an old client is unaffected.
    #[serde(default)]
    kind: Option<String>,
    /// One absolute path to resolve exactly, for a file path the user tapped in
    /// terminal output. Takes precedence over `since` and `limit`, and answers
    /// with either the one asset or none.
    #[serde(default)]
    path: Option<String>,
}

/// The `kind=` allow-list, normalized.
///
/// Comma-separated rather than one kind per request because a client's filters
/// do not map one to one onto the taxonomy -- a "documents" filter is markdown
/// and pdf -- and asking for both at once keeps "newest first" one ordering
/// instead of two lists the client has to merge and re-cut.
///
/// The values are the same strings an asset carries as its `kind`, so what can
/// be asked for is exactly what can be read back. A value that is not one of
/// them matches nothing, the way an unknown name in the events `types=`
/// allow-list matches nothing: the request is answered rather than refused, and
/// the applied list is echoed back so a client can see what it asked for.
fn asset_kind_filter(kind: Option<&str>) -> Vec<String> {
    kind.unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Build one page of assets, newest first, sniffing each candidate as it goes.
///
/// The kinds allow-list is applied while walking rather than to a page already
/// cut, which is the whole point of it: `kind=image&limit=50` answers with the
/// 50 newest images, not with the images among the 50 newest files. A session
/// whose agent is editing source code writes source files faster than it writes
/// artifacts, so the second reading returns an empty list on a workspace that
/// is full of images.
///
/// Sniffing is the cost here, so a candidate is read once and the walk stops
/// the moment the page is full. Without a filter the candidates are already the
/// page, which is the old work and the old cost exactly.
fn asset_page(entries: Vec<AssetEntry>, kinds: &[String], limit: usize) -> Vec<Value> {
    let mut page: Vec<Value> = Vec::new();
    for entry in entries {
        if page.len() >= limit {
            break;
        }
        let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
        if !kinds.is_empty() && !kinds.iter().any(|kind| kind == asset_type.kind.as_str()) {
            continue;
        }
        page.push(asset_json(&entry, asset_type));
    }
    page
}

/// List what one tab produced recently, newest first.
///
/// Scoped to a tab rather than to a session or a workspace: with the tmux
/// backend a session is the whole tmux server and a workspace is a tmux
/// session -- one `tmux new-session`, which is commonly one long-running
/// `Work` session with a window per project -- so either one still pools
/// every project anyone happens to have open in that session. An agent's
/// Files sheet asks "what did this piece of work touch", which is the tab
/// (the tmux window) it is showing, not every tab the workspace happens to
/// contain. `tab_id` is a wire id exactly like a pane id on the other
/// handlers: the client already holds it from whatever pane or agent view
/// opened this sheet, and it is compared as-is against the wire-form `tab_id`
/// `list_panes()` already hands back, with no separate decode step needed
/// because both sides went through the same `TmuxWireIds` seam.
///
/// A herdr session's tab id is translated to its owning workspace before any
/// of this scoping happens (`resolve_asset_scope`), because herdr's own tabs
/// sit inside a workspace and that workspace is the granularity herdr must
/// keep -- see `AssetScope`.
async fn session_assets(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    Query(query): Query<AssetsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let scope = resolve_asset_scope(&session, &tab_id).await;
    let roots = session_asset_roots(&state, &session, Some(&scope)).await;

    // An exact path lookup asks about one file, so it neither waits for a scan
    // nor pages: it answers with that file or with nothing.
    if let Some(wanted) = query.path.clone() {
        // The uploads directory rides along as one more root, the same fold-in
        // `asset_content`'s cold start does. An upload's path is printed into
        // the pane and handed back by the upload response, but the file lives
        // in the gateway's own directory, never under a pane's cwd -- so
        // without this, the one path the client is guaranteed to hold is the
        // one path the lookup could never answer. Serving it widens nothing:
        // the directory is the gateway's own.
        let lookup_roots = with_uploads_root(roots.clone(), &session_id, uploads_dir().ok());
        let entry = tokio::task::spawn_blocking(move || {
            let entry = asset_entry_for_path(&wanted, &lookup_roots)?;
            let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
            Some((entry, asset_type))
        })
        .await
        .unwrap_or_default();
        let assets = match entry {
            Some((entry, asset_type)) => {
                // Remember it, so the id the client just received resolves at
                // the content endpoint even if no scan would have found it.
                lock_assets(&state)?.upsert(entry.clone());
                vec![asset_json(&entry, asset_type)]
            }
            None => Vec::new(),
        };
        return Ok(Json(content_envelope(json!({
            "session_id": session_id,
            "tab_id": tab_id,
            "assets": assets,
            "path": query.path,
        }))));
    }

    ingest_roots(state.assets.clone(), roots.clone()).await;

    let limit = query
        .limit
        .unwrap_or(ASSET_LIST_DEFAULT_LIMIT)
        .clamp(1, ASSET_LIST_MAX_LIMIT);
    let since = query.since.map(u128::from);
    let kinds = asset_kind_filter(query.kind.as_deref());
    // Unfiltered, the index cuts the page and only that page is sniffed:
    // metadata came from the scan, and reading a handful of file heads is cheap
    // where reading every file in the workspace would not be. A `kind` filter
    // has to be given the whole workspace in order instead, because the index
    // does not know what a file is -- its bytes do -- and the page has to be
    // filled from the newest matches rather than from whatever the newest
    // handful of files happened to be.
    let entries = {
        let index = lock_assets(&state)?;
        if kinds.is_empty() {
            index.session_assets(&session_id, &scope, since, limit)
        } else {
            index.session_assets_ordered(&session_id, &scope, since)
        }
    };
    let filter = kinds.clone();
    let assets = tokio::task::spawn_blocking(move || asset_page(entries, &filter, limit))
        .await
        .unwrap_or_default();

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "tab_id": tab_id,
        "assets": assets,
        "limit": limit,
        "since": since.map(|since| since as u64),
        // The allow-list that was actually applied, empty when there was none.
        // Always present, so a client can tell a gateway that understands
        // `kind=` from an older one that ignored it.
        "kind": kinds,
        "roots": roots
            .iter()
            .map(|root| root.path.to_string_lossy())
            .collect::<Vec<_>>(),
    }))))
}

/// Stream one asset back, read-only.
async fn asset_content(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;

    let mut entry = lock_assets(&state)?.get(&asset_id);
    if entry.is_none() {
        // Cold start: the app may hold an id from before a restart, so rebuild
        // the index from the live sessions once before answering. Uploads
        // are not under any pane's cwd -- they are the gateway's own
        // directory, not a workspace one -- so a rebuild from pane roots
        // alone would never see an upload made before the restart that just
        // emptied the index, even though the file is still on disk and
        // `resolve_indexed_asset_path` would happily serve it once the entry
        // exists. It is folded in here as one more root per session, which
        // widens nothing the gateway was not already willing to serve: the
        // uploads directory is its own, not a workspace's.
        let uploads = uploads_dir().ok();
        for session in state.config.sessions.clone() {
            let mut roots = session_asset_roots(&state, &session, None).await;
            if let Some(uploads) = &uploads {
                roots.push(AssetRoot {
                    path: uploads.clone(),
                    session_id: session.id.clone(),
                    workspace_id: None,
                    tab_id: None,
                    pane_id: None,
                });
            }
            ingest_roots(state.assets.clone(), roots).await;
        }
        entry = lock_assets(&state)?.get(&asset_id);
    }
    let Some(entry) = entry else {
        return Err(asset_not_found());
    };

    // Roots are resolved again rather than trusted from the index: what is
    // inside a workspace now is what decides. When it decides nothing -- the
    // workspace closed -- the entry's own stored path answers instead, under
    // the equality guard in `resolve_indexed_asset_path`.
    let session = find_session(&state.config, &entry.session_id)
        .map_err(|_| asset_not_found())?
        .clone();
    let roots = canonical_roots(&session_asset_roots(&state, &session, None).await);
    let entry_path = entry.path.clone();
    let Some(path) =
        tokio::task::spawn_blocking(move || resolve_indexed_asset_path(&entry_path, &roots))
            .await
            .unwrap_or_default()
    else {
        return Err(asset_not_found());
    };

    let metadata = std::fs::metadata(&path).map_err(|_| asset_not_found())?;
    if metadata.len() > MAX_ASSET_CONTENT_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "asset_too_large",
            "the asset is larger than 10 MiB",
        ));
    }

    let sniff_path = path.clone();
    let name = entry.name.clone();
    let asset_type =
        tokio::task::spawn_blocking(move || sniff_asset_type(&read_asset_head(&sniff_path), &name))
            .await
            .map_err(|_| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "asset_read_failed",
                    "failed to read the asset",
                )
            })?;

    let mut entry = entry;
    entry.size = metadata.len();
    entry.modified_unix_ms = system_time_unix_ms(metadata.modified().ok());
    if !asset_type.kind.previewable() {
        return Ok((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": {
                    "code": "asset_not_previewable",
                    "message": "this asset has no preview; only its metadata is returned",
                },
                "asset": asset_json(&entry, asset_type),
            })),
        )
            .into_response());
    }

    let file = tokio::fs::File::open(&path).await.map_err(|err| {
        eprintln!("failed to open asset {}: {err}", path.display());
        asset_not_found()
    })?;
    let stream = async_stream::stream! {
        let mut file = file;
        let mut buffer = vec![0u8; ASSET_CONTENT_CHUNK_BYTES];
        loop {
            match tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await {
                Ok(0) => break,
                Ok(read) => yield Ok::<_, std::io::Error>(
                    axum::body::Bytes::copy_from_slice(&buffer[..read]),
                ),
                Err(err) => {
                    yield Err(err);
                    break;
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", asset_type.mime)
        .header("content-length", metadata.len())
        .header(
            "content-disposition",
            format!("inline; filename=\"{}\"", header_safe_name(&entry.name)),
        )
        .header("x-asset-kind", asset_type.kind.as_str())
        .header("x-content-schema-version", CONTENT_SCHEMA_VERSION)
        .body(Body::from_stream(stream))
        .map_err(|err| {
            eprintln!("failed to build asset response: {err}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "asset_read_failed",
                "failed to read the asset",
            )
        })
}

/// One answer for "no such asset" and for "that path is not inside a workspace
/// root": a caller must not be able to tell the two apart and map the host.
fn asset_not_found() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "asset_not_found",
        "asset not found in a session workspace",
    )
}

fn lock_assets(state: &AppState) -> ApiResult<std::sync::MutexGuard<'_, AssetIndex>> {
    state.assets.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "asset_lock_failed",
            "failed to lock the asset index",
        )
    })
}

/// A file name only ever reaches a header after being reduced to characters
/// that cannot end a quoted string or start a new header line.
fn header_safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .take(MAX_UPLOAD_NAME_CHARS)
        .collect();
    let safe = safe.trim().to_string();
    if safe.is_empty() {
        return String::from("asset");
    }
    safe
}

/// The SSE payload for a newly produced file, in the same versioned envelope
/// the asset endpoints answer with.
fn asset_created_payload(entry: &AssetEntry, asset_type: AssetType) -> String {
    content_envelope(json!({ "asset": asset_json(entry, asset_type) })).to_string()
}

/// The only call site that turns a session into a live backend. tmux ids
/// collide with URL percent-encoding past pane 9 (see `backend::tmux_wire`),
/// so a tmux session is wrapped here to translate ids to and from wire form
/// before any handler sees them -- a future handler that reaches its backend
/// through this function gets the translation automatically, with nothing to
/// remember. herdr sessions are returned unwrapped: herdr's own ids already
/// work today and must not change.
fn terminal_backend(session: &SessionConfig) -> Box<dyn TerminalBackend> {
    let backend = backend_registry().connect(session.backend, &session.socket_path);
    match session.backend {
        BackendKind::Tmux => Box::new(TmuxWireIds::new(backend)),
        BackendKind::Herdr => backend,
    }
}

fn backend_api_error(error: BackendError) -> (StatusCode, Json<Value>) {
    let (status, code, message) = match &error {
        BackendError::InvalidTarget(_) => (
            StatusCode::NOT_FOUND,
            "backend_target_not_found",
            "terminal target not found",
        ),
        BackendError::Unavailable => (
            StatusCode::BAD_GATEWAY,
            "backend_unavailable",
            "terminal backend is unavailable",
        ),
        BackendError::InvalidResponse(_)
        | BackendError::Refused { .. }
        | BackendError::Unsupported(_) => (
            StatusCode::BAD_GATEWAY,
            "backend_error",
            "terminal backend request failed",
        ),
    };
    // Adapter diagnostics may name a local socket or tmux target. Keep them in
    // the host log and return only a stable, non-sensitive API error.
    eprintln!("terminal backend request failed: {error}");
    api_error(status, code, message)
}

type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;

fn bearer_token(headers: &HeaderMap) -> ApiResult<&str> {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "missing_authorization",
            "missing Authorization header",
        ));
    };
    let Ok(value) = value.to_str() else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_authorization",
            "invalid Authorization header",
        ));
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_authorization",
            "expected Bearer token",
        ));
    };
    Ok(token)
}

/// Control routes are for paired devices only. The admin token deliberately
/// does not authorise these: it sits in plaintext on disk for the manage UI,
/// and these routes can run commands on the host.
fn require_device(state: &AppState, headers: &HeaderMap) -> ApiResult<String> {
    let token = bearer_token(headers)?;
    let mut devices = lock_devices(state)?;
    let Some(device_id) = identify_device(&devices, token) else {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    };
    if let Some(transport_key) = devices
        .iter()
        .find(|device| device.id == device_id)
        .and_then(|device| device.transport_key.as_deref())
    {
        let proof = headers
            .get(TRANSPORT_PROOF_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !authority::authenticates_admin(&hash_token(transport_key), proof) {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                "device_proof_required",
                "encrypted device proof is required",
            ));
        }
    }
    if authority::touch_device(
        &mut devices,
        &device_id,
        now_unix_ms(),
        DEVICE_LAST_SEEN_FLUSH_MS,
    ) {
        if let Err(err) = write_devices(&devices) {
            // Losing a last-seen timestamp must not fail the request.
            eprintln!("failed to persist device last-seen: {err:#}");
        }
    }
    Ok(device_id)
}

/// The local manage UI's credential, which authorises nothing but reading the
/// pending pairing code.
fn require_admin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    let token = bearer_token(headers)?;
    if !authority::authenticates_admin(&config.token_hash, token) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "invalid_token",
            "invalid token",
        ));
    }
    Ok(())
}

fn require_pairing_manager(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if require_admin(&state.config, headers).is_ok() {
        return Ok(());
    }
    require_device(state, headers).map(|_| ())
}

fn lock_devices(state: &AppState) -> ApiResult<std::sync::MutexGuard<'_, Vec<DeviceRecord>>> {
    state.devices.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_lock_failed",
            "failed to lock device state",
        )
    })
}

async fn list_paired_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let current_id = require_device(&state, &headers)?;
    let devices = lock_devices(&state)?;
    let items = devices
        .iter()
        .map(|device| {
            json!({
                "id": device.id,
                "name": device.name,
                "paired_unix_ms": device.paired_unix_ms,
                "last_seen_unix_ms": device.last_seen_unix_ms,
                "current": device.id == current_id
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "devices": items })))
}

async fn revoke_paired_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    // Paired devices can manage pairings through the app; the local Manager
    // pane uses its admin token for the same narrow operation. The admin token
    // still cannot access terminal/workspace control routes.
    require_pairing_manager(&state, &headers)?;
    let mut devices = lock_devices(&state)?;
    let previous_len = devices.len();
    devices.retain(|device| device.id != device_id);
    let removed = devices.len() != previous_len;
    if !removed {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            "device not found",
        ));
    }
    write_devices(&devices).map_err(|err| {
        eprintln!("failed to write device tokens: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_write_failed",
            "failed to revoke the device token",
        )
    })?;
    Ok(Json(
        json!({ "ok": true, "revoked": device_id, "device_count": devices.len() }),
    ))
}

fn validate_text(text: &str) -> ApiResult<()> {
    if text.len() > MAX_SEND_TEXT_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text_too_large",
            "text must be at most 65536 bytes",
        ));
    }
    Ok(())
}

/// The one client-supplied field that becomes the argv of a process on the
/// host.
///
/// It is not an escalation and the bound is not pretending otherwise: a caller
/// who can reach this endpoint holds a device token, and a device token can
/// already type into the pane the agent is about to run in. Nor is it a shell
/// -- Herdr starts an agent by argv and never through one, so nothing in here
/// is parsed by anything but the agent's own option parser.
///
/// It is bounded because every other string a client sends this gateway is,
/// and "the argument list of a program on your machine" is the wrong field to
/// be the exception. Control characters go with it, for the same reason a
/// device name cannot carry them: an argument list is echoed back in the task's
/// step log, and a newline in one forges a line.
fn validate_agent_args(args: &[String]) -> ApiResult<()> {
    if args.len() > MAX_AGENT_ARGS {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "too_many_agent_args",
            "agent_args must contain at most 32 entries",
        ));
    }
    if args
        .iter()
        .any(|arg| arg.chars().count() > MAX_AGENT_ARG_CHARS)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "agent_arg_too_long",
            "each agent_args entry must be at most 512 characters",
        ));
    }
    if args.iter().any(|arg| arg.chars().any(char::is_control)) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_agent_args",
            "agent_args entries must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_push_token(token: &str) -> ApiResult<()> {
    let valid_prefix =
        token.starts_with("ExponentPushToken[") || token.starts_with("ExpoPushToken[");
    if token.len() > 256 || !valid_prefix || !token.ends_with(']') {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_push_token",
            "token must be an Expo push token",
        ));
    }
    Ok(())
}

async fn send_expo_push_notifications(
    tokens: &[PushTokenRecord],
    title: String,
    body: String,
    data: serde_json::Map<String, Value>,
) -> anyhow::Result<Value> {
    if tokens.is_empty() {
        return Ok(json!({ "data": [] }));
    }
    let messages = tokens
        .iter()
        .map(|record| {
            json!({
                "to": record.token,
                "title": title,
                "body": body,
                "data": data,
                "sound": "default",
                "channelId": "gateway"
            })
        })
        .collect::<Vec<_>>();
    let response = reqwest::Client::new()
        .post("https://exp.host/--/api/v2/push/send")
        .json(&messages)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

fn find_session<'a>(config: &'a Config, session_id: &str) -> ApiResult<&'a SessionConfig> {
    config
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "session not found",
            )
        })
}

/// Every refusal this gateway makes, in the language the request asked for.
///
/// `code` is the wire's vocabulary: a client dispatches on it, so it is the
/// same bytes in every locale and is never looked up in the catalog. `message`
/// is prose for a person, and is passed in as its own English text -- which is
/// also the key it is translated by, so a message nobody has translated yet
/// still reads correctly, just in English.
///
/// The locale is ambient rather than an argument because this constructor is
/// reached from seventy-nine places, many of them helpers several calls below a
/// handler that have no business knowing a request exists. See
/// [`request_locale`] for the scope it is set in and [`i18n::current`] for what
/// happens outside one.
fn api_error(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<Value>) {
    api_error_in(i18n::current(), status, code, message)
}

/// The same constructor with the language named outright, for the tests and for
/// any caller that knows its reader better than the ambient scope does.
fn api_error_in(
    locale: Locale,
    status: StatusCode,
    code: &str,
    message: &str,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "code": code, "message": i18n::t(locale, message) } })),
    )
}

fn load_config(config_path: Option<String>) -> anyhow::Result<Config> {
    // Resolved lazily on purpose: `config_dir` consults the rename migration,
    // which loads the pre-rename config by explicit path. An eagerly evaluated
    // default would recurse between the two until the stack ran out.
    let path = match config_path {
        Some(path) => PathBuf::from(path),
        None => config_dir()?.join(CONFIG_FILE),
    };
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    let standalone = standalone_config_dir()?;
    if !standalone.join(HERDR_PLUGIN_IMPORT_MARKER).exists() {
        if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            return Ok(path.into());
        }
    }
    Ok(standalone)
}

/// Where a test run keeps the state a handler persists.
///
/// The handlers under test call the same `write_devices` a running gateway
/// does, and that resolved to the state directory of whatever gateway is
/// installed for the current user -- so running the checks on a machine that
/// also runs this gateway overwrote its real `devices.json` with a test
/// fixture, unpairing every device its owner had. A test binary gets its own
/// directory instead, once per process, and never learns the real one.
#[cfg(test)]
fn test_state_dir() -> std::path::PathBuf {
    static TEST_STATE_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TEST_STATE_DIR
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "muqun-gateway-test-state-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).expect("failed to create the test state directory");
            dir
        })
        .clone()
}

#[cfg(test)]
fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(test_state_dir())
}

#[cfg(not(test))]
fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    let standalone_config = standalone_config_dir()?;
    if !standalone_config.join(HERDR_PLUGIN_IMPORT_MARKER).exists() {
        if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
            return Ok(path.into());
        }
    }
    standalone_state_dir()
}

/// The gateway's standalone directory name before the muqun-gateway rename.
/// Kept only so an existing install's config and state can be found once and
/// carried forward; nothing else should reference it.
const PRE_RENAME_STANDALONE_DIR_NAME: &str = "herdr-gateway";

/// Move an install's directory from the pre-rename name to the current one,
/// the first time it is asked for after upgrading.
///
/// A same-filesystem `rename` is atomic at the directory-entry level -- there
/// is no window in which both names exist with half the files in each -- so
/// this step itself cannot leave an install half-migrated. What can still
/// split state across both names is a gateway built under the pre-rename
/// name that is *still running* against this directory: it does not know the
/// directory was just renamed out from under it, and the next thing it
/// writes -- a push token, an upload, its own pid file -- recreates the old
/// directory fresh, right next to the one this just moved. So this refuses
/// to migrate at all while such a process is found listening on the old
/// config's port, rather than migrate and let the two diverge; the caller
/// gets a plain error naming the pid to stop instead of a silently split
/// install. Once the new name exists, the old one is never inspected again:
/// a directory an owner later recreates under the old name is left alone
/// rather than merged in.
fn migrate_renamed_standalone_dir(parent: &std::path::Path) -> anyhow::Result<PathBuf> {
    let new_dir = parent.join("muqun-gateway");
    let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
    if new_dir.exists() || !old_dir.exists() {
        return Ok(new_dir);
    }
    let old_config_path = old_dir.join(CONFIG_FILE).to_string_lossy().into_owned();
    if let Ok(old_config) = load_config(Some(old_config_path)) {
        if let Ok(old_pids) = listener_pids_named(old_config.port(), PRE_RENAME_STANDALONE_DIR_NAME)
        {
            if let Some(&pid) = old_pids.first() {
                anyhow::bail!(
                    "a gateway is still running under the pre-rename name {} (pid {pid}); stop \
                     it before this directory can be migrated to muqun-gateway -- migrating \
                     underneath a still-running process would split its data across both names",
                    PRE_RENAME_STANDALONE_DIR_NAME
                );
            }
        }
    }
    std::fs::rename(&old_dir, &new_dir).with_context(|| {
        format!(
            "failed to migrate {} to {}",
            old_dir.display(),
            new_dir.display()
        )
    })?;
    eprintln!("migrated {} to {}", old_dir.display(), new_dir.display());
    Ok(new_dir)
}

fn standalone_config_dir() -> anyhow::Result<PathBuf> {
    let parent = dirs::config_dir().context("failed to locate config directory")?;
    migrate_renamed_standalone_dir(&parent)
}

fn standalone_state_dir() -> anyhow::Result<PathBuf> {
    let parent = dirs::data_dir()
        .or_else(dirs::config_dir)
        .context("failed to locate state directory")?;
    migrate_renamed_standalone_dir(&parent)
}

fn default_herdr_plugin_config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::config_dir()
        .context("failed to locate config directory")?
        .join("herdr/plugins/config/herdr.gateway"))
}

fn default_herdr_plugin_state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::state_dir()
        .or_else(dirs::data_dir)
        .or_else(dirs::config_dir)
        .context("failed to locate state directory")?
        .join("herdr/plugins/herdr.gateway"))
}

fn read_push_tokens() -> anyhow::Result<Vec<PushTokenRecord>> {
    let path = state_dir()?.join(PUSH_TOKENS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read push token file {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_push_tokens(tokens: &[PushTokenRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    let path = dir.join(PUSH_TOKENS_FILE);
    write_secret_file(&path, &serde_json::to_vec_pretty(tokens)?)
}

fn read_devices() -> anyhow::Result<Vec<DeviceRecord>> {
    read_devices_at(&state_dir()?.join(DEVICES_FILE))
}

/// Load the push-token list for a process that will later persist it.
///
/// Same shape as the device file -- held in memory, rewritten whole -- but not
/// the same stakes. A device re-registers its push token on the next app
/// launch, so a list lost here refills itself, while a lost pairing has to be
/// re-established by hand on the device. So this starts anyway rather than
/// refusing the way `load_devices_for_service` does, and says on stderr what
/// it dropped instead of starting silently empty.
fn load_push_tokens_for_service() -> Vec<PushTokenRecord> {
    match read_push_tokens() {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!(
                "could not read the push token file ({error:#}); starting with none -- devices \
                 will re-register on their next app launch"
            );
            Vec::new()
        }
    }
}

/// Read the paired-device list, keeping "nothing is paired yet" and "the
/// pairings are there but I cannot read them" as two different answers.
///
/// Collapsing them is what makes this file dangerous. The list is held in
/// memory for the life of the process and every change writes the whole list
/// back, so a caller that turns an unreadable file into an empty list has
/// armed the next pairing to overwrite every record that file still held. An
/// absent file is genuinely empty; a present one that will not parse is an
/// error the caller has to handle.
fn read_devices_at(path: &std::path::Path) -> anyhow::Result<Vec<DeviceRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read device file {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse device file {}", path.display()))
}

/// Load the device list for a process that will later persist it.
///
/// Starting a gateway with an empty list because its device file could not be
/// read is unrecoverable: the first pairing writes that empty list back over
/// the file. Refusing to start leaves the file, and the backup beside it,
/// exactly as they are so an operator can restore them.
fn load_devices_for_service() -> anyhow::Result<Vec<DeviceRecord>> {
    let dir = state_dir()?;
    read_devices_at(&dir.join(DEVICES_FILE)).with_context(|| {
        format!(
            "refusing to start with an unreadable device file -- starting would overwrite it \
             with an empty list and permanently lose every pairing. Restore it from {} and \
             start again.",
            dir.join(DEVICES_BACKUP_FILE).display()
        )
    })
}

fn write_devices(devices: &[DeviceRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    write_devices_at(&dir, devices)
}

/// Persist the device list, keeping the generation it replaces beside it.
///
/// The backup is what turns a bad list into an inconvenience instead of a
/// loss. Writing it costs one more small file per change, which is nothing
/// next to re-pairing every device the owner has.
fn write_devices_at(dir: &std::path::Path, devices: &[DeviceRecord]) -> anyhow::Result<()> {
    let path = dir.join(DEVICES_FILE);
    if let Ok(previous) = std::fs::read(&path) {
        if !previous.is_empty() {
            write_secret_file(&dir.join(DEVICES_BACKUP_FILE), &previous)?;
        }
    }
    write_secret_file(&path, &serde_json::to_vec_pretty(devices)?)
}

fn list_devices() -> anyhow::Result<()> {
    let devices = read_devices()?;
    if devices.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for device in devices {
        println!(
            "{}  {}  paired {}  last seen {}",
            device.id,
            truncate(&device.name, 32),
            format_unix_ms(device.paired_unix_ms),
            format_unix_ms(device.last_seen_unix_ms)
        );
    }
    Ok(())
}

fn revoke_device(device_id: Option<String>, all: bool) -> anyhow::Result<()> {
    if all {
        let devices = read_devices()?;
        let mut count = 0_usize;
        for device in devices {
            count += usize::from(revoke_managed_device(&device.id)?);
        }
        println!("revoked {count} device token(s)");
        return Ok(());
    }
    let Some(device_id) = device_id else {
        anyhow::bail!("pass a device id from `gateway devices`, or --all");
    };
    // A running gateway owns the in-memory device list. Revoke through its
    // narrow manager API so a later pairing write cannot resurrect a token
    // removed only from disk. The helper falls back to disk when it is stopped.
    if !revoke_managed_device(&device_id)? {
        anyhow::bail!("device {device_id} not found");
    }
    println!("revoked device {device_id}");
    Ok(())
}

/// Drop one device straight from the file, with no gateway running.
///
/// The whole read-modify-write happens under the state-directory lock, not
/// just the write. Locking the write alone would still lose records: another
/// writer's change can land between the read below and the write that follows
/// it, and the write puts back a list that never contained it. Holding the
/// lock across both also means that when a gateway *is* running -- it owns the
/// directory for its lifetime -- this refuses instead of editing the list
/// behind its back, which is right, because the running gateway would rewrite
/// its own in-memory list over this one at the next pairing anyway.
fn revoke_device_by_id(device_id: &str) -> anyhow::Result<bool> {
    revoke_device_at(&state_dir()?, device_id)
}

fn revoke_device_at(dir: &std::path::Path, device_id: &str) -> anyhow::Result<bool> {
    let removed = update_devices_at(dir, |devices| {
        let previous_len = devices.len();
        devices.retain(|device| device.id != device_id);
        (devices.len() != previous_len).then_some(())
    })?;
    Ok(removed.is_some())
}

/// Load the device list, change it, and store it back, owning the state
/// directory from the first byte read to the last byte written.
///
/// The lock has to span the read as well as the write. A change that locked
/// only its write would read the list, let another writer's pairing land in
/// the gap, and then store a list that never contained it -- the whole-file
/// overwrite this module exists to prevent, just with a smaller window. So
/// `change` runs inside the critical section, and must not block: everything
/// else that wants to touch this directory is refused while it runs.
///
/// `change` returns `None` when it decided not to change anything, which
/// leaves the file -- and the backup generation beside it -- untouched.
fn update_devices_at<T>(
    dir: &std::path::Path,
    change: impl FnOnce(&mut Vec<DeviceRecord>) -> Option<T>,
) -> anyhow::Result<Option<T>> {
    let _lock = state_lock::StateLock::acquire(dir)
        .context("cannot edit the device list on disk while a gateway owns it")?;
    let mut devices = read_devices_at(&dir.join(DEVICES_FILE))?;
    let Some(outcome) = change(&mut devices) else {
        return Ok(None);
    };
    write_devices_at(dir, &devices)?;
    Ok(Some(outcome))
}

fn format_unix_ms(value: u128) -> String {
    if value == 0 {
        return String::from("never");
    }
    let seconds_ago = now_unix_ms().saturating_sub(value) / 1000;
    match seconds_ago {
        0..=59 => String::from("just now"),
        60..=3599 => format!("{}m ago", seconds_ago / 60),
        3600..=86399 => format!("{}h ago", seconds_ago / 3600),
        _ => format!("{}d ago", seconds_ago / 86400),
    }
}

fn pid_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(state_dir()?.join(PID_FILE))
}

fn read_pid() -> anyhow::Result<Option<u32>> {
    let path = pid_path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    Ok(text.trim().parse::<u32>().ok())
}

/// Whether `config.json` has been rewritten more recently than the running
/// gateway process started.
///
/// `run` reads `config.json` exactly once, at startup, into a plain `Config`
/// value that lives for the process's lifetime (see `AppState`); nothing
/// re-reads the file afterward. So once the process is up, the file on disk
/// and the settings it is actually enforcing (transport encryption, the
/// backend list, the default backend, the public URL) can only drift apart,
/// never resync, until the next restart.
///
/// There is no live channel to ask the running process what it loaded, so
/// this compares two mtimes as a proxy: the pid file, written immediately
/// after the process is spawned (see `start_background_inner`), stands in
/// for "when the running process started". If `config.json` was modified
/// after that, the running process cannot have seen the change.
fn config_changed_since_start() -> bool {
    let Ok(config_path) = config_dir().map(|dir| dir.join(CONFIG_FILE)) else {
        return false;
    };
    let Ok(pid_file) = pid_path() else {
        return false;
    };
    let (Ok(config_meta), Ok(pid_meta)) = (
        std::fs::metadata(&config_path),
        std::fs::metadata(&pid_file),
    ) else {
        return false;
    };
    let (Ok(config_mtime), Ok(pid_mtime)) = (config_meta.modified(), pid_meta.modified()) else {
        return false;
    };
    config_mtime > pid_mtime
}

fn read_pairing_file() -> anyhow::Result<PairingFile> {
    let bytes = std::fs::read(config_dir()?.join(PAIRING_FILE))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn ensure_pairing_transport_key() -> anyhow::Result<()> {
    let path = config_dir()?.join(PAIRING_FILE);
    let mut pairing: PairingFile = serde_json::from_slice(&std::fs::read(&path)?)?;
    if transport::decode_key(&pairing.payload.transport_key).is_ok_and(|key| key.len() == 32) {
        return Ok(());
    }
    pairing.payload.transport_key = generate_token();
    write_secret_file(&path, &serde_json::to_vec_pretty(&pairing)?)
}

fn rotate_pairing_transport_key() -> anyhow::Result<()> {
    let path = config_dir()?.join(PAIRING_FILE);
    let mut pairing: PairingFile = serde_json::from_slice(&std::fs::read(&path)?)?;
    pairing.payload.transport_key = generate_token();
    write_secret_file(&path, &serde_json::to_vec_pretty(&pairing)?)
}

fn remove_pid_file() -> anyhow::Result<()> {
    let path = pid_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn process_running(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn stop_pid(pid: u32) -> anyhow::Result<()> {
    let status = ProcessCommand::new("kill")
        .arg(pid.to_string())
        .status()
        .context("failed to run kill")?;
    if !status.success() {
        anyhow::bail!("failed to stop pid {pid}");
    }
    Ok(())
}

const GATEWAY_PROCESS_NAME: &str = "muqun-gateway";

/// Find gateway processes listening on `port`. Anything that is not the gateway
/// is filtered out by process name: `stop` must never kill an unrelated service
/// that happens to hold the port.
fn gateway_listener_pids(port: u16) -> anyhow::Result<Vec<u32>> {
    listener_pids_named(port, GATEWAY_PROCESS_NAME)
}

/// Same search, but for an arbitrary process name rather than this build's own
/// [`GATEWAY_PROCESS_NAME`]. Used to detect a gateway still running under the
/// pre-rename binary name, which this build's own name-filtered search would
/// never find.
fn listener_pids_named(port: u16, process_name: &str) -> anyhow::Result<Vec<u32>> {
    #[cfg(target_os = "macos")]
    let mut pids = {
        let output = ProcessCommand::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
            .context("failed to inspect listening sockets")?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!("failed to inspect listening sockets");
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .filter(|pid| process_matches_name(*pid, process_name))
            .collect::<Vec<_>>()
    };

    #[cfg(not(target_os = "macos"))]
    let mut pids = {
        let output = ProcessCommand::new("ss")
            .args(["-ltnp"])
            .output()
            .context("failed to inspect listening sockets")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let port_suffix = format!(":{port}");
        let mut pids = Vec::new();
        for line in text.lines() {
            if !line.contains(&port_suffix) || !line.contains(process_name) {
                continue;
            }
            let Some(pid_start) = line.find("pid=") else {
                continue;
            };
            let pid_text = line[pid_start + 4..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if let Ok(pid) = pid_text.parse::<u32>() {
                pids.push(pid);
            }
        }
        pids
    };

    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Confirm a pid really belongs to a process named `process_name` before
/// trusting it (e.g. before signalling it, or before refusing a directory
/// migration on its account).
#[cfg(target_os = "macos")]
fn process_matches_name(pid: u32, process_name: &str) -> bool {
    ProcessCommand::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(process_name))
        })
}

/// Marks a directory as never-to-be-committed.
///
/// Config directories are routinely symlinked into a dotfiles repository, and a
/// habitual `git add -A` there would publish the admin token and the paired
/// device list. File mode 0600 does not survive a commit; a `.gitignore` does.
/// Written next to the secrets themselves so it travels with them wherever
/// `HERDR_PLUGIN_CONFIG_DIR` points.
fn write_secret_dir_gitignore(dir: &std::path::Path) {
    let path = dir.join(".gitignore");
    if path.exists() {
        return;
    }
    let body = "# The gateway keeps tokens and paired-device records here.\n\
                # Never let them reach a repository.\n\
                *\n";
    if let Err(err) = std::fs::write(&path, body) {
        eprintln!("could not write {}: {err}", path.display());
    }
}

/// Narrow the directory the secrets sit in, not only the secrets.
///
/// The files have been 0600 for a while, so nothing in here was readable by
/// another account. The directory was left at whatever the umask gave it,
/// which on a normal machine is world-listable -- and a listing of it is the
/// names of the paired devices' record file, the pairing file, and whether a
/// gateway is configured on this account at all. Free to fix, and it removes
/// the last thing a second account on a shared machine could learn.
///
/// A failure is reported and not fatal: a directory the owner has deliberately
/// re-permissioned, or one on a filesystem with no modes at all, must not stop
/// the gateway writing its own config.
#[cfg(unix)]
fn lock_down_secret_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::metadata(dir) else {
        return;
    };
    if metadata.permissions().mode() & 0o077 == 0 {
        return;
    }
    if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        eprintln!("could not restrict {}: {err}", dir.display());
    }
}

#[cfg(not(unix))]
fn lock_down_secret_dir(_dir: &std::path::Path) {}

/// Replace a secret file's contents in one step, never leaving it short.
///
/// Truncating the real file and writing into it is what loses pairings. It
/// puts the file through a window -- open, chmod, write -- in which it is
/// zero bytes on disk, so a process killed mid-write leaves nothing behind,
/// and a reader that lands in that window sees an empty file rather than the
/// records that are still supposed to be there. Writing a complete temporary
/// file first and renaming it over the target closes that window: `rename` is
/// atomic within a directory, so every reader sees either the whole previous
/// generation or the whole new one, and a crash at any point leaves one of
/// the two rather than a stump.
fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        write_secret_dir_gitignore(dir);
        lock_down_secret_dir(dir);
    }
    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("secret file path has no file name")?;
    // The sequence number matters as much as the pid: two threads of one
    // process writing the same file would otherwise pick the same temporary
    // and rename each other's half-written copy into place.
    static TEMPORARY_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let write_temporary = || -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            // `mode` only applies when the file is created, so a temporary left
            // by an older build keeps its old permissions until they are set
            // explicitly.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            std::io::Write::write_all(&mut file, bytes)?;
            // The rename is only worth anything if the bytes reached the disk
            // before the name started pointing at them.
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&temporary, bytes)?;
        }
        std::fs::rename(&temporary, path)?;
        Ok(())
    };

    match write_temporary() {
        Ok(()) => Ok(()),
        Err(error) => {
            // A half-written temporary must not be left to be mistaken for
            // state, and must not block the next attempt.
            std::fs::remove_file(&temporary).ok();
            Err(error.context(format!("failed to write {}", path.display())))
        }
    }
}

fn default_socket_path() -> String {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("herdr")
        .join("herdr.sock")
        .to_string_lossy()
        .into_owned()
}

fn hostname_label() -> String {
    // HOSTNAME is often unset on macOS, which left every server named the
    // generic "Server". Fall back to the real machine name so a fresh
    // install is at least recognisable before the user renames it.
    if let Ok(value) = std::env::var("HOSTNAME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if let Ok(name) = String::from_utf8(output.stdout) {
            let name = name.trim().trim_end_matches(".local").trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "Server".into()
}

/// Persist a new display label into the config (and the pairing payload), so a
/// name set from the app shows up in push notifications. Mirrors
/// `update_public_url`.
fn update_server_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    anyhow::ensure!(
        !label.is_empty()
            && label.chars().count() <= MAX_DEVICE_NAME_CHARS
            && !label.chars().any(char::is_control),
        "label must be non-empty, at most {MAX_DEVICE_NAME_CHARS} characters, and free of control characters"
    );
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.label = label.to_string();
    write_secret_file(&config_path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    if let Ok(mut pairing) = read_pairing_file() {
        pairing.payload.label = label.to_string();
        let _ = write_secret_file(
            &config_dir()?.join(PAIRING_FILE),
            &serde_json::to_vec_pretty(&pairing)?,
        );
    }
    Ok(label.to_string())
}

/// The label as it stands on disk right now, so a rename from the app takes
/// effect for notifications without restarting the gateway.
fn current_server_label(fallback: &str) -> String {
    load_config(None)
        .map(|config| config.label)
        .unwrap_or_else(|_| fallback.to_string())
}

fn generate_token() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The glyphs a pairing code is drawn from: no `0`/`O` and no `1`/`I`/`L`, so a
/// person reading one off a screen cannot mistype it into a different code.
const PAIRING_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Eight glyphs of this alphabet is a shade under forty bits, and that number
/// is only true if every glyph is equally likely at every position. Two things
/// were making it false.
///
/// Folding a byte with `%` biases the fold: 256 is not a multiple of 31, so the
/// first eight glyphs came up a ninth more often than the other twenty-three.
/// And a v4 UUID is not sixteen random bytes -- four bits of byte 6 say
/// "version 4" and two of byte 8 say "RFC variant" -- so the position that
/// happened to land on byte 6 could only ever produce sixteen of the
/// thirty-one glyphs, costing that character most of its entropy.
///
/// Neither was reachable: eight wrong answers burn the code and six requests
/// burn ten minutes, so nothing gets near enough guesses for a fraction of a
/// bit to matter. It is fixed because the number the code is worth should be
/// the number it says it is.
fn generate_pairing_code() -> String {
    // The last whole multiple of the alphabet below 256. A byte at or above it
    // would fold unevenly, so it is redrawn rather than folded.
    let ceiling = (256 / PAIRING_CODE_ALPHABET.len()) * PAIRING_CODE_ALPHABET.len();
    let mut source = RandomBytes::default();
    let mut characters = String::with_capacity(PAIRING_CODE_CHARACTER_COUNT);
    while characters.len() < PAIRING_CODE_CHARACTER_COUNT {
        let byte = usize::from(source.next_byte());
        if byte >= ceiling {
            continue;
        }
        characters.push(PAIRING_CODE_ALPHABET[byte % PAIRING_CODE_ALPHABET.len()] as char);
    }
    format!("{}-{}", &characters[..4], &characters[4..])
}

/// Random bytes from the CSPRNG behind `Uuid::new_v4`, minus the bytes of a v4
/// UUID that are not random. Keeps the gateway's entropy on one source instead
/// of adding a second dependency for eight characters.
#[derive(Default)]
struct RandomBytes {
    buffer: Vec<u8>,
}

impl RandomBytes {
    fn next_byte(&mut self) -> u8 {
        if self.buffer.is_empty() {
            let bytes = *uuid::Uuid::new_v4().as_bytes();
            // Byte 6 carries the version nibble and byte 8 the variant bits.
            // Neither is random, so neither is spent.
            self.buffer = bytes
                .into_iter()
                .enumerate()
                .filter(|(index, _)| !matches!(index, 6 | 8))
                .map(|(_, byte)| byte)
                .collect();
        }
        // The refill puts fourteen bytes here, so this is never the default.
        self.buffer.pop().unwrap_or_default()
    }
}

/// Device names are echoed into the manage UI's terminal box, so control
/// characters (ANSI escapes in particular) are rejected outright.
/// A client install identifier: an opaque token the app generates once and
/// keeps. Bounded and control-character-free like every other client string.
fn valid_install_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_device_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_DEVICE_NAME_CHARS
        && !value.chars().any(char::is_control)
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Terminal Gateway API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Token-protected mobile API for controlling local terminal workspaces through a configured tmux or Herdr backend. Human-readable text is localized: send X-Muqun-Locale (or Accept-Language) with `en` or `zh-TW`. Error `code` values, decision names and other wire vocabulary are the same bytes in every locale."
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            }
        },
        "security": [{ "bearerAuth": [] }],
        "paths": {
            "/health": { "get": simple_endpoint("Gateway health") },
            "/api/meta": { "get": simple_endpoint("Gateway API, backend, and legacy compatibility metadata") },
            "/api/pair/request": {
                "post": {
                    "summary": "Request pairing from Muqun app",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("device_name", "string")], &["request_id"])),
                    "responses": ok_response()
                }
            },
            "/api/pair/claim": {
                "post": {
                    "summary": "Claim pairing token with request id and confirmation code",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("code", "string")], &["request_id", "code"])),
                    "responses": ok_response()
                }
            },
            "/api/pairings": { "get": simple_endpoint("List devices holding a gateway token") },
            "/api/pairings/{deviceId}": {
                "delete": {
                    "summary": "Revoke one device's gateway token",
                    "parameters": [path_param("deviceId")],
                    "responses": ok_response()
                }
            },
            "/api/devices/push-token": {
                "post": {
                    "summary": "Register this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string"), ("platform", "string"), ("device_name", "string"), ("locale", "string")], &["token", "platform"])),
                    "responses": ok_response()
                },
                "delete": {
                    "summary": "Remove this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string")], &["token"])),
                    "responses": ok_response()
                }
            },
            "/api/notifications/test": {
                "post": {
                    "summary": "Send a test push notification to registered Muqun devices",
                    "requestBody": json_body(object_schema(&[("title", "string"), ("body", "string"), ("data", "object")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/uploads": {
                "post": {
                    "summary": "Upload one image and get back a local path for an agent to read",
                    "description": "Images only: png, jpeg, gif, webp, and heic. The type is decided by sniffing the content, not by the filename, and everything else, including executables and scripts, is refused. The stored name is generated by the gateway; the returned name is only the sanitised client name. Uploads are deleted after 48 hours.",
                    "requestBody": multipart_file_body(),
                    "responses": upload_responses()
                }
            },
            "/api/sessions/{sessionId}/tabs/{tabId}/assets": {
                "get": {
                    "summary": "List files this tab produced recently, newest first",
                    "description": "Unified content model, schema version 1.0.0. The response is the versioned envelope: schema_version, capabilities, and data, with the assets under data.assets. Assets are fed by the Herdr worktree events the gateway subscribes to, and by an mtime scan of the tab's roots, which is what a cold start uses. The scan is shallow, budgeted, and skips dot directories, dependency directories, and build output. Scoped to tabId, not to the whole session or workspace: a tmux-backed session spans every project the developer has a window open on, and a tmux-backed workspace (a whole tmux session, commonly one long-running session with a window per project) spans every one of those projects too, so this answers only for the one tab the caller is looking at. A herdr-backed session's tabId is resolved to the workspace it belongs to instead, because herdr's own tabs sit inside one of its workspaces and that workspace must not narrow further.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("tabId"),
                        query_param("since", "Unix milliseconds, the same unit as modified_unix_ms; only files modified strictly after this are returned"),
                        query_param("limit", "How many assets to return, 1 to 200, default 50"),
                        query_param("kind", "Comma-separated allow-list of kinds -- image, markdown, text, pdf, binary -- filtered during the scan, so kind=image&limit=50 answers with the 50 newest images rather than the images among the 50 newest files. Absent or empty means every kind; a value outside the taxonomy matches nothing rather than erroring. The applied list is echoed back as data.kind"),
                        query_param("path", "Resolve one absolute path exactly, for a file path tapped in terminal output. Takes precedence over since and limit. Answers with one asset, or with none when the path does not canonicalize to a file inside this tab's roots -- a fenced-out path is a miss, not an error")
                    ],
                    "responses": assets_responses()
                }
            },
            "/api/assets/{assetId}/content": {
                "get": {
                    "summary": "Stream one asset's bytes, read-only",
                    "description": "The path must canonicalize to a regular file inside a workspace root the session currently has, so a symlink out of the root, a traversal, and an unknown id are all a 404. An asset indexed while its root was a live workspace outlives that workspace: when the roots no longer contain it, the entry's stored canonical path is replayed and served only if it canonicalizes back to itself byte-for-byte, so a symlink swapped into the old location is the same 404. A path that was never indexed has no entry to replay. The kind is sniffed again from the bytes on every read; a binary asset answers 415 with its metadata and no body. Assets larger than 10 MiB are refused with 413.",
                    "parameters": [path_param("assetId")],
                    "responses": asset_content_responses()
                }
            },
            "/api/sessions": { "get": simple_endpoint("List configured terminal backend sessions") },
            "/api/sessions/{sessionId}/events": {
                "get": {
                    "summary": "Stream Herdr lifecycle events as Server-Sent Events",
                    "description": "Herdr events arrive as SSE `herdr` events. The gateway adds its own `asset.created` event, carrying one asset in the content-model envelope, when a Herdr worktree event reveals newly produced files. It obeys the same `types=` allow-list, under the name `asset.created`.",
                    "parameters": [path_param("sessionId")],
                    "responses": {
                        "200": { "description": "SSE stream of Herdr event JSON lines, plus gateway asset.created events" },
                        "401": { "description": "Missing or invalid authorization" },
                        "403": { "description": "Invalid token" }
                    }
                }
            },
            "/api/sessions/{sessionId}/agent-events": {
                "get": {
                    "summary": "Recent agent status transitions in this session, oldest first",
                    "description": "An in-memory ring of the last 200 status transitions per session, for a client building a digest of what happened while it was away. Every transition is recorded, not only the two that raise a push, so a pane that worked and then went idle is visible even though only one notification was sent. Each event carries seq, pane_id, agent, from, to and unix_ms -- ids and statuses, never terminal output or an agent's own wording. Poll with since=<the highest seq already seen>; the answer's next_since is what to send next time, and missed is true when the ring has already dropped something after that point. Nothing is persisted: a restarted gateway answers with an empty list, because it was not watching.",
                    "parameters": [
                        path_param("sessionId"),
                        query_param("since", "Only transitions with a higher seq are returned. Absent means everything still held")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/snapshot": { "get": session_endpoint("Return the session snapshot") },
            "/api/sessions/{sessionId}/workspaces": {
                "get": session_endpoint("List workspaces"),
                "post": {
                    "summary": "Create a workspace",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("cwd", "string"), ("label", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}/focus": { "post": resource_endpoint("Focus a workspace", "workspaceId") },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}": {
                "patch": {
                    "summary": "Rename a workspace",
                    "parameters": [path_param("sessionId"), path_param("workspaceId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a workspace", "workspaceId")
            },
            "/api/sessions/{sessionId}/tabs": {
                "get": session_endpoint("List tabs"),
                "post": {
                    "summary": "Create a tab",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("workspace_id", "string"), ("label", "string"), ("cwd", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/tabs/{tabId}/focus": { "post": resource_endpoint("Focus a tab", "tabId") },
            "/api/sessions/{sessionId}/tabs/{tabId}": {
                "patch": {
                    "summary": "Rename a tab",
                    "parameters": [path_param("sessionId"), path_param("tabId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a tab", "tabId")
            },
            "/api/sessions/{sessionId}/panes": { "get": session_endpoint("List panes") },
            "/api/sessions/{sessionId}/panes/{paneId}": {
                "get": resource_endpoint("Get a pane", "paneId"),
                "patch": {
                    "summary": "Rename a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a pane", "paneId")
            },
            "/api/sessions/{sessionId}/panes/{paneId}/focus": { "post": resource_endpoint("Focus a pane", "paneId") },
            "/api/sessions/{sessionId}/panes/{paneId}/split": {
                "post": {
                    "summary": "Split a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["direction"],
                        "properties": {
                            "direction": { "type": "string", "enum": ["right", "down"] },
                            "ratio": { "type": "number" },
                            "command": { "type": "array", "items": { "type": "string" } },
                            "cwd": { "type": "string" },
                            "env": { "type": "object", "additionalProperties": true }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/zoom": {
                "post": {
                    "summary": "Acknowledge the legacy app viewport request without changing backend zoom",
                    "description": "Compatibility no-op. Released app builds call this when mounting a terminal; observing a pane must not mutate tmux or Herdr layout.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "enum": ["on", "off", "toggle"], "default": "on" }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/keymaps": { "get": session_endpoint("Agent keymap coverage") },
            "/api/agents/catalog": {
                "get": {
                    "summary": "Agent kinds a task can be started with, and whether each one is installed",
                    "description": "Herdr resolves an agent kind to its canonical executable itself, so this is a picker feed rather than a launch table: `available` is a PATH probe for that executable on this host, and false is a hint, not a veto. Not session-scoped, because which binaries are installed is a property of the machine. `command` is remapped by `agent_commands` in the gateway's config.json for a host whose binary is named something else.",
                    "responses": agents_catalog_responses()
                }
            },
            "/api/sessions/{sessionId}/tasks": {
                "post": {
                    "summary": "Start a new task: a checkout, a workspace, an agent, and the first prompt",
                    "description": "With `branch_name`, the task gets its own git worktree; without it, the task runs in the repo as it stands. `repo_path` must be, or be inside, a workspace this session already has open -- anything else is 403, and a symlink out of one resolves to the outside path and fails there too. `branch_name` is held to letters, digits, dot, underscore, dash and slash, with `..`, a leading dash, and dot-leading segments refused, so it can only ever be a ref and never an argument. The agent is started, then the gateway waits for it to become interactive before the prompt is sent. Asking twice for the same branch reuses the existing checkout rather than making a second one.",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["repo_path", "agent"],
                        "properties": {
                            "repo_path": { "type": "string", "description": "Absolute path, inside a workspace this session has open" },
                            "branch_name": { "type": "string", "description": "Branch for a dedicated worktree; omit to work in the repo as it stands" },
                            "agent": { "type": "string", "description": "Agent kind from GET /api/agents/catalog" },
                            "prompt": { "type": "string", "description": "Sent once the agent is interactive" },
                            "workspace_label": { "type": "string" },
                            "agent_args": { "type": "array", "items": { "type": "string" } },
                            "startup_timeout_ms": { "type": "integer", "description": "How long to wait for the agent to become interactive, 3001 to 300000, default 30000" }
                        }
                    })),
                    "responses": task_responses()
                }
            },
            "/api/sessions/{sessionId}/spawn": {
                "post": {
                    "summary": "Start an agent in a new pane, without describing a repository",
                    "description": "The light half of task dispatch: run this agent, here. The agent must be one this gateway offers -- a Herdr kind or a profile in agents.json -- and cwd, when given, must be a directory this session already works in, exactly the fence repo_path is under; anything else is 403. With tab_id the pane is split off whatever that tab has focused, so a second agent lands beside the first; without it the agent gets a tab of its own. GET recent-cwds answers with the directories cwd will accept. The reply names the pane and says whether the agent came up and whether the prompt landed; a 207 means the pane exists and something after it did not, which is not the same as nothing having happened.",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["agent"],
                        "properties": {
                            "agent": { "type": "string", "description": "Agent kind from GET /api/agents/catalog, or a profile named in agents.json" },
                            "cwd": { "type": "string", "description": "Absolute path, inside a workspace this session has open; omit to take the backend's default" },
                            "tab_id": { "type": "string", "description": "Split this tab's focused pane instead of opening a new tab" },
                            "prompt": { "type": "string", "description": "Sent once the agent is interactive" }
                        }
                    })),
                    "responses": task_responses()
                }
            },
            "/api/sessions/{sessionId}/recent-cwds": {
                "get": {
                    "summary": "The distinct working directories of this session's panes",
                    "description": "A picker for spawn, and deliberately not a directory browser: it answers with the cwds the session reports for the panes that exist right now, deduplicated and held to the same rule the asset scan uses, so the filesystem root and a bare home directory are not on it. Each entry carries path, name, the pane and workspace it came from, and git, which says whether the directory is a checkout. Nothing here can be used to walk the host.",
                    "parameters": [path_param("sessionId")],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/interrupt": {
                "post": {
                    "summary": "Stop whatever the agent in this pane is doing",
                    "description": "Sugar over send-keys, and worth an endpoint because the key is not the same on every agent: ctrl+c at a shell, esc in every agent this gateway has a profile for, and whatever agents.json says when it names one. A keystroke and nothing else -- no signal and no kill. The reply names the key that was sent, so a client can say what it did. The same key is on the pane's shortcuts response as `interrupt`.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/shortcuts": {
                "get": resource_endpoint("Key row and slash commands for a pane", "paneId")
            },
            "/api/sessions/{sessionId}/agents": { "get": session_endpoint("List agents") },
            "/api/sessions/{sessionId}/agents/{target}": { "get": resource_endpoint("Get an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/focus": { "post": resource_endpoint("Focus an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/send": {
                "post": {
                    "summary": "Send and submit text to an agent",
                    "parameters": [path_param("sessionId"), path_param("target")],
                    "requestBody": json_body(object_schema(&[("text", "string")], &["text"])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/output": {
                "get": {
                    "summary": "Read pane output",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("source", "Pane read source, for example recent-unwrapped, recent, visible, or detection"),
                        query_param("lines", "Maximum line count"),
                        query_param("start", "First absolute line to read, 0 being the oldest the pane holds. Requires end."),
                        query_param("end", "One past the last absolute line to read. Requires start. A range wins over lines, and is clamped to 5000 lines and to what the pane holds rather than refused."),
                        query_param("format", "Output format: text or ansi")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/parts": {
                "get": {
                    "summary": "Read the pane's transcript normalized into content-model parts",
                    "description": "Unified content model, schema version 1.4.0. Same envelope as the asset endpoints: schema_version, capabilities, and data, with the ordered parts under data.parts. Two sources can answer, and data.pane.parts says which did. native: the agent runs a protocol the gateway was pointed at (opencode's server API today), so a tool's exit code, the patch an edit produced, the checklist a todo write submitted and any pending permission arrive as data; range then spans the adapter's own rendering, which is the parts' fallback_text joined by newlines. dictionary: the pane's recent-unwrapped text read through the marker table of whichever agent the session reports -- Claude Code, Qoder, Codex and opencode. text: no table covers this pane, so everything degraded to prose, which is an answer and not an error. Whichever source answered, every part carries fallback_text verbatim, so an unknown type still renders and a source that drifts loses structure and never loses content. data.pane.composer carries the slash commands this agent understands and whether @ file mentions make sense, and is absent entirely for an agent the gateway has no table for. The raw output endpoint is unchanged and remains the fallback path.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("lines", "How many lines of scrollback to normalize, 1 to 5000, default 400")
                    ],
                    "responses": parts_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/files": {
                "get": {
                    "summary": "Fuzzy path search inside the pane's workspace, for @ file mentions",
                    "description": "Answers paths only -- no contents, no sizes, no absolute paths. The only directory searched is the pane's own working directory as the session reports it, canonicalized, which is the same fence the asset API is gated on; a pane sitting at the filesystem root or straight in the home directory has no workspace and answers with an empty list rather than an error, so this cannot be used to probe the host. Symlinks are never followed, and dot, dependency and build directories are skipped, so nothing outside the root can be named. The query is a fuzzy subsequence match over the relative path, ranked so that the file name beats the directories above it; an empty query answers with the shallowest files, which is what a picker shows before anything is typed. kind is decided from the name alone because nothing is read -- the asset content endpoint sniffs the bytes again when a file is actually opened.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("query", "What the user typed after the @; empty or absent lists the shallowest files"),
                        query_param("limit", "How many matches to return, 1 to 50, default 20")
                    ],
                    "responses": file_search_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/approval": {
                "get": {
                    "summary": "Read whether the pane is blocked on an approval, and what it asks",
                    "description": "Agents ask for permission by drawing a numbered menu and blocking. This reads that menu off the pane's visible screen: the question, the answers, which one the cursor is on, what each answer means (allow, allow_always, deny), and the lines the agent drew around the request. data.state is pending or idle, and data.approval is null when idle. The fingerprint identifies this question with these answers; send it back on POST and an approval that changed underneath is rejected rather than answered blind. Approvals are not parts: docs/content-model.md keeps the part set closed and gives approvals a part type only in v2, so until then they ride their own endpoint and their own SSE events (approval.pending, approval.resolved).",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": approval_responses()
                },
                "post": {
                    "summary": "Answer the approval the pane is blocked on",
                    "description": "Answer by option number or by decision (allow, allow_always, deny); the gateway turns it into the keystrokes that agent's menu wants, and confirms with Enter only when the same menu is still standing afterwards. 409 when the pane is not waiting, or when the pending approval is not the one the fingerprint names. Raw send-keys remains the fallback for a menu no client understands.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "description": "Give option or decision; fingerprint is optional optimistic concurrency.",
                        "properties": {
                            "option": { "type": "integer", "description": "The option number the agent printed" },
                            "decision": { "type": "string", "enum": ["allow", "allow_always", "deny"] },
                            "fingerprint": { "type": "string" }
                        }
                    })),
                    "responses": approval_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-text": {
                "post": {
                    "summary": "Send text to a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": { "text": { "type": "string" } }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-keys": {
                "post": {
                    "summary": "Send key names to a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["keys"],
                        "properties": { "keys": { "type": "array", "items": { "type": "string" } } }
                    })),
                    "responses": ok_response()
                }
            }
        }
    })
}

fn task_steps_schema() -> Value {
    json!({
        "type": "array",
        "description": "What happened, in order: worktree, workspace, agent, prompt, and rollback if one was needed. Present on success and on a partial run alike, so a client never has to guess how far a request got.",
        "items": {
            "type": "object",
            "required": ["step", "status"],
            "properties": {
                "step": { "type": "string", "enum": ["worktree", "workspace", "agent", "prompt", "rollback"] },
                "status": { "type": "string", "enum": ["ok", "skipped", "failed", "rolled_back"] },
                "detail": { "type": "object" },
                "reason": { "type": "string" },
                "error": object_schema(&[("code", "string"), ("message", "string")], &["code", "message"])
            }
        }
    })
}

fn task_result_schema() -> Value {
    json!({
        "type": "object",
        "required": ["workspace_id", "pane_id", "agent", "agent_started", "prompt_submitted", "steps"],
        "properties": {
            "workspace_id": { "type": "string" },
            "pane_id": { "type": "string", "description": "Where the agent runs; also the target for the agent endpoints" },
            "worktree_path": { "type": ["string", "null"], "description": "Absent when no branch_name was given" },
            "branch": { "type": ["string", "null"] },
            "agent": { "type": "string" },
            "reused_worktree": { "type": "boolean", "description": "True when the branch already had a checkout, which is what makes a retry safe" },
            "agent_started": { "type": "boolean" },
            "prompt_submitted": { "type": "boolean" },
            "steps": task_steps_schema()
        }
    })
}

fn task_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Every step succeeded",
        "content": { "application/json": { "schema": task_result_schema() } }
    });
    responses["207"] = json!({
        "description": "Somewhere to work was created, but a later step failed -- the agent did not come up, or the prompt did not land. The body is the same shape as a 200; the failed step names what went wrong. Nothing is rolled back here: the checkout and pane are usable.",
        "content": { "application/json": { "schema": task_result_schema() } }
    });
    responses["400"] = json!({
        "description": "Unknown agent kind, malformed branch name, repo_path that is not a git checkout, or Herdr refusing the request. Nothing was created; a worktree this request made and could not attach a workspace to is removed again."
    });
    responses["403"] =
        json!({ "description": "repo_path is not inside a workspace this session has open" });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["502"] = json!({ "description": "Herdr is unavailable, or answered without the fields its schema promises" });
    responses
}

fn agents_catalog_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Agent kinds, sorted, with PATH availability",
        "content": { "application/json": { "schema": json!({
            "type": "object",
            "required": ["agents", "default_startup_timeout_ms"],
            "properties": {
                "agents": { "type": "array", "items": json!({
                    "type": "object",
                    "required": ["kind", "command", "available", "source"],
                    "properties": {
                        "kind": { "type": "string" },
                        "command": { "type": "string" },
                        "available": { "type": "boolean" },
                        "path": { "type": ["string", "null"] },
                        "source": { "type": "string", "enum": ["builtin", "config"] }
                    }
                }) },
                "default_startup_timeout_ms": { "type": "integer" }
            }
        }) } }
    });
    responses
}

fn simple_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "responses": ok_response()
    })
}

fn session_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId")],
        "responses": ok_response()
    })
}

fn resource_endpoint(summary: &str, resource_param: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId"), path_param(resource_param)],
        "responses": ok_response()
    })
}

fn object_schema(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), json!({ "type": ty })))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "required": required,
        "properties": properties
    })
}

fn path_param(name: &str) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "schema": { "type": "string" }
    })
}

fn query_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": description,
        "schema": { "type": "string" }
    })
}

fn multipart_file_body() -> Value {
    json!({
        "required": true,
        "content": {
            "multipart/form-data": {
                "schema": {
                    "type": "object",
                    "required": ["file"],
                    "properties": { "file": { "type": "string", "format": "binary" } }
                }
            }
        }
    })
}

fn upload_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Stored upload",
        "content": { "application/json": { "schema": object_schema(
            &[("path", "string"), ("name", "string"), ("size", "integer"), ("mime", "string")],
            &["path", "name", "size", "mime"],
        ) } }
    });
    responses["400"] =
        json!({ "description": "Malformed multipart body, or no usable file field" });
    responses["413"] = json!({ "description": "Upload is larger than 25 MiB" });
    responses["415"] = json!({ "description": "Content is an executable or script, or not an accepted image type" });
    responses
}

fn asset_schema() -> Value {
    json!({
        "type": "object",
        "required": ["id", "path", "name", "kind", "mime", "size", "modified_unix_ms", "origin", "previewable"],
        "properties": {
            "id": { "type": "string" },
            "path": { "type": "string" },
            "name": { "type": "string" },
            "kind": { "type": "string", "enum": ["image", "markdown", "text", "pdf", "binary"] },
            "mime": { "type": "string" },
            "size": { "type": "integer" },
            "modified_unix_ms": { "type": "integer" },
            "origin": object_schema(
                &[("session_id", "string"), ("workspace_id", "string"), ("pane_id", "string"), ("root", "string")],
                &["session_id"],
            ),
            "previewable": { "type": "boolean" }
        }
    })
}

fn content_envelope_schema(data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["schema_version", "capabilities", "data"],
        "properties": {
            "schema_version": { "type": "string", "const": CONTENT_SCHEMA_VERSION },
            "capabilities": object_schema(
                &[("parts", "boolean"), ("assets", "boolean"), ("image_upload", "boolean"), ("composer", "boolean")],
                &["parts", "assets", "image_upload", "composer"],
            ),
            "data": data
        }
    })
}

fn assets_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Recent assets, newest first",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "assets"],
            "properties": {
                "session_id": { "type": "string" },
                "assets": { "type": "array", "items": asset_schema() },
                "limit": { "type": "integer" },
                "since": { "type": ["integer", "null"], "description": "Unix milliseconds, echoed back" },
                "path": { "type": ["string", "null"], "description": "The requested exact path, echoed back on a path lookup" },
                "roots": { "type": "array", "items": { "type": "string" } }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses
}

/// One part on the wire. Deliberately loose about the payload and strict about
/// the two fields the contract rests on: `type`, so a client can dispatch, and
/// `fallback_text`, so it can render whatever it could not dispatch on.
fn part_schema() -> Value {
    json!({
        "type": "object",
        "required": ["type", "fallback_text"],
        "properties": {
            "type": {
                "type": "string",
                "enum": ["text", "tool-block", "diff", "todo", "table", "status", "prompt", "asset-ref", "approval"],
                "description": "Closed set. A client that does not know a value renders fallback_text; new types arrive on a minor bump"
            },
            "fallback_text": { "type": "string", "description": "The source lines verbatim" },
            "range": object_schema(&[("start", "integer"), ("end", "integer")], &["start", "end"]),
            "markdown": { "type": "string", "description": "text: prose, rendered best-effort" },
            "tool": { "type": "string", "description": "tool-block: the tool the agent named" },
            "input": { "type": "string", "description": "tool-block: what it was called with" },
            "result": { "type": "array", "items": { "type": "string" }, "description": "tool-block: the result lines" },
            "status": { "type": "string", "enum": ["ok", "error", "running"], "description": "tool-block: read off the first result line; running means no result yet" },
            "truncated": { "type": "boolean", "description": "tool-block: the agent printed an ellipsis, so this is not all of it" },
            "file": { "type": ["string", "null"], "description": "diff: the file the block edited, when the tool named one" },
            "hunks": { "type": "array", "items": { "type": "string" }, "description": "diff: the numbered source lines" },
            "items": {
                "type": "array",
                "description": "todo: the checklist",
                "items": object_schema(&[("text", "string"), ("done", "boolean")], &["text", "done"])
            },
            "text": { "type": "string", "description": "status and prompt: the line's content" },
            "spinner": { "type": "boolean", "description": "status: the line is one of the agent's animated frames" },
            "approval_id": { "type": "string", "description": "approval: what POST .../approval answers. Only a source that reports approval state can raise one; a pane read through a marker dictionary carries its menu on the approvals endpoint instead" },
            "prompt": { "type": "string", "description": "approval: the question, written by the gateway from the protocol's own action name and never quoted out of a terminal" },
            "context": { "type": "array", "items": { "type": "string" }, "description": "approval: what is being asked for -- the command, the path, the host -- verbatim from the protocol" },
            "options": {
                "type": "array",
                "description": "approval: the answers, labelled by the gateway so an agent's own wording (which routinely embeds the command) never travels",
                "items": object_schema(&[("index", "integer"), ("label", "string"), ("decision", "string")], &["index", "label", "decision"])
            }
        }
    })
}

/// What a pane's composer can offer. Absent from `data.pane` entirely when the
/// gateway has no command table for the agent, which is how a client tells "no
/// table" from "no commands".
fn composer_schema() -> Value {
    json!({
        "type": "object",
        "description": "Absent for an agent the gateway has no table for",
        "required": ["version", "table", "slash_commands", "file_mentions"],
        "properties": {
            "version": { "type": "integer", "description": "Bumped whenever a builtin table changes, so a client can cache this" },
            "table": { "type": "string", "description": "Which table answered, the same id the part dictionaries use" },
            "captured_from": { "type": "string", "description": "The agent release the builtin table was read off" },
            "file_mentions": { "type": "boolean", "description": "Whether @ in the composer means 'mention a file' to this agent" },
            "slash_commands": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "description", "source"],
                    "properties": {
                        "name": { "type": "string", "description": "The literal text to send, leading slash included" },
                        "description": { "type": "string" },
                        "args_hint": { "type": ["string", "null"], "description": "What may follow the command; null means it runs exactly as typed, so a client may send it on one tap" },
                        "source": { "type": "string", "enum": ["builtin", "workspace"], "description": "builtin: the gateway's table for this agent. workspace: a skill or command file found in the pane's workspace, which wins over a builtin of the same name" }
                    }
                }
            }
        }
    })
}

fn file_search_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Fuzzy path matches inside the pane's workspace, best first",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "files", "root"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "query": { "type": "string", "description": "The query, echoed back" },
                "limit": { "type": "integer", "description": "The clamped limit, echoed back" },
                "root": { "type": ["string", "null"], "description": "The workspace directory every path is relative to, or null when this pane has none the gateway will look in" },
                "files": {
                    "type": "array",
                    "items": object_schema(&[("path", "string"), ("name", "string"), ("kind", "string")], &["path", "name", "kind"])
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses
}

fn parts_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The pane's transcript as ordered parts",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "parts", "pane"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "source": { "type": "string", "enum": ["recent-unwrapped", "native"], "description": "recent-unwrapped: the pane's own text, which is what the dictionaries key off. native: the agent's own protocol answered, and range spans the adapter's rendering -- which is the parts' fallback_text joined by newlines -- rather than terminal rows" },
                "lines": { "type": "integer", "description": "How many lines were read, echoed back" },
                "revision": { "type": ["integer", "null"], "description": "Herdr's pane revision for this read, when it reported one" },
                "pane": {
                    "type": "object",
                    "required": ["pane_id", "parts", "image_input"],
                    "properties": {
                        "pane_id": { "type": "string" },
                        "agent": { "type": ["string", "null"], "description": "What Herdr reports is running in the pane" },
                        "parts": { "type": "string", "enum": ["native", "dictionary", "text"], "description": "native: the agent's own protocol answered. dictionary: typed parts read off the screen. text: no dictionary covers this pane, everything degraded to prose" },
                        "dictionary": { "type": ["string", "null"], "description": "Which dictionary normalized it, for cache keys and bug reports" },
                        "native": {
                            "type": ["object", "null"],
                            "description": "Null unless a protocol actually answered, so a client cannot mistake 'an adapter could have read this pane' for 'an adapter did'",
                            "properties": {
                                "protocol": { "type": ["string", "null"], "description": "What the agent's own release calls it, for bug reports" },
                                "version": { "type": ["string", "null"], "description": "The version the agent's server reported" },
                                "session": { "type": ["string", "null"], "description": "The agent's own session identity, so a client can tell one native read from the next" }
                            }
                        },
                        "image_input": { "type": "string", "description": "How an image reaches this agent; file-path means upload first, then send the path" },
                        "composer": composer_schema()
                    }
                },
                "parts": { "type": "array", "items": part_schema() }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["502"] =
        json!({ "description": "Herdr is unavailable, or the pane could not be read" });
    responses
}

fn approval_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The pane's approval state, in the content-model envelope",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "state", "approval", "pane"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "state": { "type": "string", "enum": ["pending", "idle"] },
                "pane": {
                    "type": "object",
                    "required": ["pane_id", "approvals"],
                    "properties": {
                        "pane_id": { "type": "string" },
                        "agent": { "type": ["string", "null"] },
                        "approvals": { "type": "string", "enum": ["menu"], "description": "How the approval was obtained: menu means it was read off what the agent drew" }
                    }
                },
                "approval": {
                    "type": ["object", "null"],
                    "required": ["fingerprint", "prompt", "options"],
                    "properties": {
                        "fingerprint": { "type": "string", "description": "Stable identity of this question with these answers" },
                        "prompt": { "type": "string", "description": "The question, verbatim" },
                        "tool": { "type": ["string", "null"], "description": "The tool the request is about, when the agent named one" },
                        "context": { "type": "array", "items": { "type": "string" }, "description": "The lines the agent drew around the question, verbatim and capped" },
                        "hint": { "type": ["string", "null"], "description": "The agent's own key-hint footer" },
                        "range": object_schema(&[("start", "integer"), ("end", "integer")], &["start", "end"]),
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["index", "label", "selected", "decision"],
                                "properties": {
                                    "index": { "type": "integer", "description": "The number the agent printed, which is what POST takes" },
                                    "label": { "type": "string", "description": "The answer verbatim" },
                                    "selected": { "type": "boolean", "description": "Where the agent's cursor is" },
                                    "decision": { "type": "string", "enum": ["allow", "allow_always", "deny", "other"] }
                                }
                            }
                        }
                    }
                },
                "resolved": { "type": "boolean", "description": "POST only: whether the menu was gone after answering" },
                "sent_keys": { "type": "array", "items": { "type": "string" }, "description": "POST only: the keys the gateway sent, including a deferred Enter when one was needed" },
                "answered": {
                    "type": "object",
                    "description": "POST only: which option was taken",
                    "properties": {
                        "fingerprint": { "type": "string" },
                        "index": { "type": "integer" },
                        "decision": { "type": "string" }
                    }
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["409"] = json!({ "description": "The pane is not waiting on an approval, or it is waiting on a different one" });
    responses["502"] =
        json!({ "description": "Herdr is unavailable, or the pane could not be read" });
    responses
}

fn asset_content_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The asset's bytes, with the sniffed type in content-type and x-asset-kind",
        "content": { "*/*": { "schema": { "type": "string", "format": "binary" } } }
    });
    responses["404"] = json!({ "description": "Unknown asset, or a path that does not resolve inside a session workspace root" });
    responses["413"] = json!({ "description": "The asset is larger than 10 MiB" });
    responses["415"] = json!({
        "description": "Binary asset: metadata only, no preview",
        "content": { "application/json": { "schema": {
            "type": "object",
            "properties": { "error": { "type": "object" }, "asset": asset_schema() }
        } } }
    });
    responses
}

fn json_body(schema: Value) -> Value {
    json!({
        "required": true,
        "content": { "application/json": { "schema": schema } }
    })
}

fn ok_response() -> Value {
    json!({
        "200": {
            "description": "Successful response",
            "content": { "application/json": { "schema": {} } }
        },
        "401": { "description": "Missing or invalid authorization" },
        "403": { "description": "Invalid token" },
        "502": { "description": "Terminal backend unavailable or returned an error" }
    })
}

const DOCS_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Terminal Gateway API Docs</title>
  </head>
  <body>
    <script id="api-reference" data-url="/openapi.json"></script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
  </body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_config(token: &str) -> Config {
        Config {
            server_id: "server-1".into(),
            label: "test".into(),
            listen: "127.0.0.1:23100".into(),
            public_url: "http://127.0.0.1:23100".into(),
            token_hash: hash_token(token),
            transport_encryption: TransportEncryptionMode::Required,
            sessions: vec![SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: "/tmp/herdr.sock".into(),
                backend: BackendKind::Herdr,
            }],
            agent_commands: BTreeMap::new(),
            rich_agent_pushes: false,
        }
    }

    #[test]
    fn a_liveness_verdict_is_reused_only_inside_its_window() {
        // Five phones polling on their own schedules asked every backend the
        // same question about twice a second between them, and on tmux each
        // ask is two processes.
        let mut cache = SessionLivenessCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.fresh(t0, SESSION_LIVENESS_TTL), None);

        cache.record(vec![1, 0], t0);
        assert_eq!(
            cache.fresh(t0 + Duration::from_millis(500), SESSION_LIVENESS_TTL),
            Some(vec![1, 0])
        );
        // Past the window the backends are asked again, so a session that
        // actually went down is reflected rather than remembered.
        assert_eq!(
            cache.fresh(t0 + SESSION_LIVENESS_TTL, SESSION_LIVENESS_TTL),
            None
        );
    }

    /// The defect this hub exists for: `activity_stream()` used to be built
    /// once per subscriber, so N phones watching one tmux session meant N
    /// independent polls -- and a tmux poll costs processes, not just sockets.
    #[tokio::test]
    async fn many_subscribers_share_one_activity_stream() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();

        let a = subscribe_activity(&state, &session);
        let b = subscribe_activity(&state, &session);
        let c = subscribe_activity(&state, &session);

        let hubs = state.activity.lock().unwrap();
        assert_eq!(hubs.len(), 1, "three subscribers must not make three hubs");
        assert_eq!(hubs.get(&session.id).unwrap().receiver_count(), 3);
        drop(hubs);
        drop((a, b, c));
    }

    /// The proof header is the encrypted-transport middleware talking to the
    /// handlers below it, and nothing else may put words in its mouth.
    ///
    /// Before this was stripped on the way in, a client could send
    /// `x-muqun-internal-device-proof` itself over cleartext and be taken for
    /// the encrypted device whose transport key it named -- having just put
    /// that key on the wire in the clear to do it, which is precisely what the
    /// encrypted transport exists to prevent.
    #[tokio::test]
    async fn a_client_cannot_forge_the_transport_proof_header() {
        use axum::routing::get;
        use tower::ServiceExt;

        let state = test_state("admin", Vec::new());
        let app = Router::new()
            .route(
                "/probe",
                get(|headers: axum::http::HeaderMap| async move {
                    // What the handlers below the middleware would see.
                    headers
                        .get(TRANSPORT_PROOF_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("absent")
                        .to_string()
                }),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                encrypted_transport,
            ))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(TRANSPORT_PROOF_HEADER, "a-transport-key-i-do-not-own")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "absent",
            "a client-supplied device proof must never reach a handler"
        );
    }

    /// Two sessions are two streams; the hub is per session, not global.
    #[tokio::test]
    async fn each_session_gets_its_own_hub() {
        let mut state = test_state("admin", Vec::new());
        let mut second = state.config.sessions[0].clone();
        second.id = "second".into();
        state.config.sessions.push(second.clone());
        let first = state.config.sessions[0].clone();

        let a = subscribe_activity(&state, &first);
        let b = subscribe_activity(&state, &second);
        assert_eq!(state.activity.lock().unwrap().len(), 2);
        drop((a, b));
    }

    /// And the other half of sharing: when the last subscriber goes, the hub
    /// is retired, so a gateway nobody is talking to stops polling.
    #[tokio::test]
    async fn the_hub_retires_when_the_last_subscriber_leaves() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();

        let subscriber = subscribe_activity(&state, &session);
        assert_eq!(state.activity.lock().unwrap().len(), 1);
        drop(subscriber);

        // `retire_activity_hub` is what the producer calls between streams;
        // with nothing listening it removes the entry and reports that it
        // should stop.
        assert!(retire_activity_hub(&state, &session.id));
        assert!(state.activity.lock().unwrap().is_empty());
    }

    /// A subscriber arriving while the producer is deciding to leave must not
    /// be handed a receiver on a hub that then exits.
    #[tokio::test]
    async fn a_hub_with_a_live_subscriber_is_not_retired() {
        let state = test_state("admin", Vec::new());
        let session = state.config.sessions[0].clone();
        let subscriber = subscribe_activity(&state, &session);

        assert!(!retire_activity_hub(&state, &session.id));
        assert_eq!(state.activity.lock().unwrap().len(), 1);
        drop(subscriber);
    }

    fn test_device(id: &str, token: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: format!("device {id}"),
            token_hash: hash_token(token),
            transport_key: None,
            paired_unix_ms: 1_000,
            // Fresh enough that require_device will not flush to disk.
            last_seen_unix_ms: now_unix_ms(),
            install_id: None,
        }
    }

    fn test_state(admin_token: &str, devices: Vec<DeviceRecord>) -> AppState {
        AppState {
            config: test_config(admin_token),
            pending_pairing: Arc::new(Mutex::new(None)),
            pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
            push_tokens: Arc::new(Mutex::new(Vec::new())),
            devices: Arc::new(Mutex::new(devices)),
            assets: Arc::new(Mutex::new(AssetIndex::default())),
            scrollback: Arc::new(Mutex::new(scrollback::ScrollbackStore::default())),
            agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
            approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
            activity: Arc::new(Mutex::new(HashMap::new())),
            session_liveness: Arc::new(Mutex::new(SessionLivenessCache::default())),
        }
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    fn encrypted_test_request(device_id: &str, material: &[u8], token: &str) -> Request<Body> {
        let payload = EncryptedRequestPayload {
            token: token.into(),
            content_type: Some("application/json".into()),
            body: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"ok":true}"#),
        };
        let envelope = transport::seal(
            material,
            transport::Direction::Request,
            b"POST /api/test",
            &serde_json::to_vec(&payload).unwrap(),
            now_unix_ms(),
        )
        .unwrap();
        Request::builder()
            .method("POST")
            .uri("/api/test")
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, device_id)
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn a_stolen_bearer_token_is_not_a_device_transport_credential() {
        let token = "device-token";
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);
        assert!(require_device(&state, &bearer_headers(token)).is_err());

        let stolen_token_material = base64::engine::general_purpose::STANDARD
            .decode(hash_token(token))
            .unwrap();
        let rejected = decrypt_transport_request(
            &state,
            encrypted_test_request("phone-1", &stolen_token_material, token),
        )
        .await;
        assert!(rejected.is_err());

        let (request, _, _, _) =
            decrypt_transport_request(&state, encrypted_test_request("phone-1", &material, token))
                .await
                .unwrap();
        assert_eq!(bearer_token(request.headers()).unwrap(), token);
        assert_eq!(
            require_device(&state, request.headers()).unwrap(),
            "phone-1"
        );
    }

    /// The decrypted request carries the stream context a sealing handler
    /// needs, and records sealed under it open exactly the way the app's
    /// decryptor is specified to: derive from (device key, sid, request
    /// nonce), AAD of request AAD + sid + seq, nonce = seq.
    #[tokio::test]
    async fn an_encrypted_request_leaves_a_stream_context_the_sealer_honours() {
        let token = "device-token";
        let transport_key = generate_token();
        let material = transport::decode_key(&transport_key).unwrap();
        let mut device = test_device("phone-1", token);
        device.transport_key = Some(transport_key);
        let state = test_state("admin-token", vec![device]);

        let (request, _, aad, nonce) =
            decrypt_transport_request(&state, encrypted_test_request("phone-1", &material, token))
                .await
                .unwrap();
        let context = request
            .extensions()
            .get::<EncryptedStreamContext>()
            .expect("stream context is injected for every encrypted request")
            .clone();
        assert_eq!(context.request_aad, aad);
        assert_eq!(context.request_nonce, nonce);
        assert_eq!(context.material, material);

        let mut sealer = EventStreamSealer::new(&context).unwrap();
        let first: Value =
            serde_json::from_str(&sealer.seal_record("herdr", "{\"n\":1}").unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&sealer.seal_record("approval.pending", "{}").unwrap()).unwrap();
        assert_eq!(first["v"], 1);
        assert_eq!(first["seq"], 0);
        assert_eq!(second["seq"], 1);
        let sid = first["sid"].as_str().unwrap();
        assert_eq!(second["sid"].as_str().unwrap(), sid);

        let key = transport::derive_stream_key(&material, sid, &nonce).unwrap();
        let open = |record: &Value| {
            let seq = record["seq"].as_u64().unwrap();
            let aad = format!("{}\n{}\n{}", aad, sid, seq);
            transport::open_stream_event(
                &key,
                seq,
                aad.as_bytes(),
                record["ciphertext"].as_str().unwrap(),
            )
        };
        let inner: Value = serde_json::from_slice(&open(&first).unwrap()).unwrap();
        assert_eq!(inner["event"], "herdr");
        assert_eq!(inner["data"], "{\"n\":1}");
        let inner: Value = serde_json::from_slice(&open(&second).unwrap()).unwrap();
        assert_eq!(inner["event"], "approval.pending");

        // A record moved to another slot in the stream never opens: the seq is
        // in both the nonce and the AAD, so reorder and replay both fail.
        let replayed = json!({
            "v": 1, "sid": sid, "seq": 1,
            "ciphertext": first["ciphertext"].as_str().unwrap(),
        });
        assert!(open(&replayed).is_err());
    }

    /// A paired device's headers with the app's locale header on them, which is
    /// what every real request from the app carries.
    fn locale_headers(token: &str, locale: &str) -> HeaderMap {
        let mut headers = bearer_headers(token);
        headers.insert(
            axum::http::HeaderName::from_static(i18n::LOCALE_HEADER),
            HeaderValue::from_str(locale).unwrap(),
        );
        headers
    }

    fn error_body(refusal: &(StatusCode, Json<Value>)) -> Value {
        refusal.1 .0.clone()
    }

    #[test]
    fn an_output_range_needs_both_ends_or_neither() {
        assert!(validate_output_range(None, None).unwrap().is_none());
        assert_eq!(
            validate_output_range(Some(10), Some(20)).unwrap(),
            Some((10, 20))
        );
        assert!(validate_output_range(Some(10), None).is_err());
        assert!(validate_output_range(None, Some(20)).is_err());
    }

    #[test]
    fn an_output_range_must_run_forwards() {
        assert!(validate_output_range(Some(20), Some(20)).is_err());
        assert!(validate_output_range(Some(21), Some(20)).is_err());
    }

    #[test]
    fn an_oversized_output_range_is_trimmed_from_its_start_rather_than_refused() {
        // A reader scrolling toward the top always eventually overreaches. That is
        // an arrival at the top, not a client mistake, so it clamps.
        let (start, end) = validate_output_range(Some(0), Some(50_000))
            .unwrap()
            .unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, MAX_OUTPUT_LINES);
    }

    /// The refusal a request reads is in the language it asked for, and the
    /// `code` beside it is not.
    ///
    /// This goes through `require_device` rather than through `api_error`
    /// directly because the interesting part is the *ambient* locale: no handler
    /// and no helper on this path takes a `Locale` argument, and the answer
    /// still changes language. That is the whole mechanism, asserted end to end.
    #[tokio::test]
    async fn a_refusal_is_written_in_the_language_the_request_asked_for() {
        let state = test_state("admin-token", vec![test_device("device-1", "token-1")]);

        let english = i18n::scope(Locale::En, async {
            require_device(&state, &locale_headers("wrong", "en")).unwrap_err()
        })
        .await;
        let chinese = i18n::scope(Locale::ZhTw, async {
            require_device(&state, &locale_headers("wrong", "zh-TW")).unwrap_err()
        })
        .await;

        assert_eq!(english.0, chinese.0, "the status is not prose");
        let english = error_body(&english);
        let chinese = error_body(&chinese);
        assert_eq!(english["error"]["code"], "invalid_token");
        assert_eq!(
            english["error"]["code"], chinese["error"]["code"],
            "a client dispatches on the code, so it has no language"
        );
        assert_eq!(english["error"]["message"], "invalid token");
        assert_eq!(chinese["error"]["message"], "token 無效");
    }

    /// Outside a request there is no locale to read, and English is the answer
    /// -- never a panic and never a missing message.
    #[test]
    fn a_refusal_built_outside_a_request_is_english() {
        let state = test_state("admin-token", vec![test_device("device-1", "token-1")]);
        let refusal = require_device(&state, &bearer_headers("wrong")).unwrap_err();
        assert_eq!(error_body(&refusal)["error"]["message"], "invalid token");
    }

    /// The wire vocabulary is the contract; the prose is not.
    #[test]
    fn error_codes_are_byte_identical_across_locales() {
        // One of each shape: a validation refusal, an auth refusal, a
        // not-found, and one whose message quotes API vocabulary that must
        // survive translation intact.
        let cases = [
            ("session_not_found", "session not found"),
            ("invalid_platform", "platform must be ios or android"),
            (
                "invalid_decision",
                "decision must be allow, allow_always, or deny",
            ),
            (
                "invalid_source",
                "source must be visible, recent, recent-unwrapped, or detection",
            ),
            (
                "unknown_agent",
                "agent is not one this gateway offers; see GET /api/agents/catalog",
            ),
        ];
        for (code, message) in cases {
            let english = api_error_in(Locale::En, StatusCode::BAD_REQUEST, code, message);
            let chinese = api_error_in(Locale::ZhTw, StatusCode::BAD_REQUEST, code, message);
            let english = error_body(&english);
            let chinese = error_body(&chinese);
            assert_eq!(english["error"]["code"], code);
            assert_eq!(chinese["error"]["code"], code);
            assert_eq!(english["error"]["message"], message);
            assert_ne!(
                chinese["error"]["message"], message,
                "{code} has no translation"
            );
        }

        // The literals inside a message are API vocabulary a client sends back,
        // so only the sentence around them moves.
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "invalid_decision",
            "decision must be allow, allow_always, or deny",
        ));
        let message = chinese["error"]["message"].as_str().unwrap();
        for literal in ["decision", "allow", "allow_always", "deny"] {
            assert!(message.contains(literal), "{literal} was translated away");
        }
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "invalid_source",
            "source must be visible, recent, recent-unwrapped, or detection",
        ));
        let message = chinese["error"]["message"].as_str().unwrap();
        for literal in ["visible", "recent", "recent-unwrapped", "detection"] {
            assert!(message.contains(literal), "{literal} was translated away");
        }
        let chinese = error_body(&api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
        assert!(chinese["error"]["message"]
            .as_str()
            .unwrap()
            .contains("GET /api/agents/catalog"));
    }

    /// A message nobody has translated is still a message.
    #[test]
    fn an_untranslated_refusal_falls_back_to_its_english() {
        let refusal = api_error_in(
            Locale::ZhTw,
            StatusCode::BAD_GATEWAY,
            "herdr_error",
            "pane.read: Herdr refused the request",
        );
        assert_eq!(
            error_body(&refusal)["error"]["message"],
            "pane.read: Herdr refused the request"
        );
        assert_eq!(error_body(&refusal)["error"]["code"], "herdr_error");
    }

    /// A `push-tokens.json` written before the locale field existed still
    /// loads, and the device it describes is notified in English.
    #[test]
    fn a_push_token_registered_before_locales_existed_still_loads() {
        let old: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "device_name": "Phone", "updated_unix_ms": 1 }]"#,
        )
        .expect("the field is optional, so an older file is still a valid one");
        assert_eq!(old[0].locale, None);
        assert_eq!(old[0].locale(), Locale::En);

        let current: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "device_name": "Phone", "locale": "zh-TW", "updated_unix_ms": 1 }]"#,
        )
        .unwrap();
        assert_eq!(current[0].locale(), Locale::ZhTw);

        // And a value that is not a locale this gateway serves is not an error
        // either -- the device simply gets English.
        let odd: Vec<PushTokenRecord> = serde_json::from_str(
            r#"[{ "token": "ExponentPushToken[abc]", "platform": "ios",
                  "locale": "zh-Hans", "updated_unix_ms": 1 }]"#,
        )
        .unwrap();
        assert_eq!(odd[0].locale(), Locale::En);
    }

    #[test]
    fn a_secret_directory_is_marked_never_to_be_committed() {
        let dir = std::env::temp_dir().join(format!("herdr-gitignore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("config.json");
        write_secret_file(&secret, b"{}").unwrap();

        let ignore = dir.join(".gitignore");
        assert!(
            ignore.exists(),
            "a secret directory must carry a .gitignore"
        );
        assert!(std::fs::read_to_string(&ignore).unwrap().contains('*'));

        // An existing file is left alone: the developer may have written it.
        std::fs::write(&ignore, "mine\n").unwrap();
        write_secret_file(&secret, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "mine\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The files have been 0600 for a while. The directory around them was
    /// left at the umask, and a world-listable directory still says which
    /// devices' record file is there and that this account runs a gateway.
    #[cfg(unix)]
    #[test]
    fn a_secret_directory_is_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("herdr-secret-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let secret = dir.join("devices.json");
        write_secret_file(&secret, b"[]").unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the directory is still readable");
        let file = std::fs::metadata(&secret).unwrap().permissions().mode();
        assert_eq!(file & 0o777, 0o600, "the secret is still readable");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn device_fixture(id: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: id.into(),
            token_hash: authority::hash_token(id),
            transport_key: None,
            paired_unix_ms: 1,
            last_seen_unix_ms: 1,
            install_id: None,
        }
    }

    /// The shape of the loss this file was written for: a device file that a
    /// process could not read became an empty list, and the next pairing
    /// wrote that empty list back over the records that were still there.
    #[test]
    fn an_unreadable_device_file_is_an_error_not_an_empty_list() {
        let dir = std::env::temp_dir().join(format!("gateway-devices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEVICES_FILE);

        // A file that was never written is genuinely empty.
        assert!(read_devices_at(&path).unwrap().is_empty());

        // A write cut short leaves nothing to parse. That is not "no devices".
        std::fs::write(&path, b"").unwrap();
        assert!(
            read_devices_at(&path).is_err(),
            "a zero-byte device file read as an empty device list"
        );

        // Nor is a half-written one.
        std::fs::write(&path, b"[{\"id\":\"phone\",\"na").unwrap();
        assert!(
            read_devices_at(&path).is_err(),
            "a truncated device file read as an empty device list"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Replacing the list must never put the file through a state in which it
    /// holds less than a whole generation, because every reader of it treats
    /// what it finds as the complete set of pairings.
    #[cfg(unix)]
    #[test]
    fn replacing_a_secret_file_never_leaves_it_short() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("gateway-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEVICES_FILE);

        write_secret_file(&path, b"[\"first\"]").unwrap();
        let first_inode = std::fs::metadata(&path).unwrap().ino();

        write_secret_file(&path, b"[\"second\"]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[\"second\"]");
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            first_inode,
            "the file was rewritten in place, so it was empty for part of the write"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the replacement lost the owner-only mode"
        );

        // The temporary the rename came from must not be left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "left a temporary behind: {leftovers:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Editing the device list on disk while a gateway owns the directory is
    /// the same loss from the other side: the gateway is holding the whole
    /// pre-edit list in memory and writes it back at the next pairing, so an
    /// edit made underneath it is undone without anyone being told. Refusing
    /// is the only honest answer -- the caller's own fallback is to ask the
    /// running gateway to do it instead.
    #[test]
    fn revoking_on_disk_refuses_while_a_gateway_owns_the_directory() {
        let dir = std::env::temp_dir().join(format!("gateway-revoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();

        let owner = state_lock::acquire_within(&dir, state_lock::RELEASE_VISIBLE_WITHIN).unwrap();
        // Exact, not waited on: while the owner holds it this must be refused
        // every time.
        let refused = revoke_device_at(&dir, "phone");
        assert!(
            refused.is_err(),
            "a device was revoked on disk behind a running gateway's back"
        );
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"],
            "the refused revoke still rewrote the device list"
        );

        // With no gateway running there is no in-memory list to contradict,
        // and the same call goes through. Waited on rather than asserted on
        // the next instruction -- see `state_lock::acquire_within`.
        drop(owner);
        assert!(
            state_lock::retry_while_directory_is_busy(|| revoke_device_at(&dir, "phone")).unwrap()
        );
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tablet"]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lock has to span the read *and* the write, not just the write.
    ///
    /// A change that locked only its write would still lose records, just
    /// through a smaller window: it reads the whole list, another writer's
    /// change lands in the gap, and then it stores a list that never
    /// contained it. This forces exactly that interleaving -- one change is
    /// held open between its read and its write while a second one tries to
    /// go -- and the second must be refused rather than allowed to slip in
    /// and be overwritten a moment later.
    #[test]
    fn a_device_list_change_owns_the_directory_from_its_read_to_its_write() {
        let dir =
            std::env::temp_dir().join(format!("gateway-devices-rmw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();

        let slow_dir = dir.clone();
        let slow = std::thread::spawn(move || {
            state_lock::retry_while_directory_is_busy(|| {
                update_devices_at(&slow_dir, |devices| {
                    devices.retain(|device| device.id != "phone");
                    // Sitting between the read and the write is the entire
                    // point: this is the window a write-only lock would leave
                    // open, and it is far longer than the pre-`exec` window
                    // that makes an unrelated refusal possible.
                    std::thread::sleep(Duration::from_millis(400));
                    Some(())
                })
            })
        });

        std::thread::sleep(Duration::from_millis(100));
        // Exact, not waited on: the slow change is provably mid-flight.
        let competing = update_devices_at(&dir, |devices| {
            devices.retain(|device| device.id != "tablet");
            Some(())
        });
        assert!(
            competing.is_err(),
            "a second change ran while another was between its read and its write, so the \
             slower one was about to store a list that never had this change in it"
        );

        assert!(slow.join().unwrap().unwrap().is_some());
        assert_eq!(
            read_devices_at(&dir.join(DEVICES_FILE))
                .unwrap()
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tablet"],
            "the in-flight change did not land intact"
        );

        // Once the directory is free again the same change goes through.
        assert!(
            state_lock::retry_while_directory_is_busy(|| revoke_device_at(&dir, "tablet")).unwrap()
        );
        assert!(read_devices_at(&dir.join(DEVICES_FILE)).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bad list is survivable as long as the one it replaced is still there.
    #[test]
    fn writing_the_device_list_keeps_the_generation_it_replaced() {
        let dir =
            std::env::temp_dir().join(format!("gateway-devices-bak-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        write_devices_at(&dir, &[device_fixture("phone"), device_fixture("tablet")]).unwrap();
        // Nothing was replaced by the first write, so there is nothing to keep.
        assert!(!dir.join(DEVICES_BACKUP_FILE).exists());

        write_devices_at(&dir, &[]).unwrap();
        assert!(read_devices_at(&dir.join(DEVICES_FILE)).unwrap().is_empty());

        let kept = read_devices_at(&dir.join(DEVICES_BACKUP_FILE)).unwrap();
        assert_eq!(
            kept.iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"],
            "the replaced pairings were not recoverable"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn event_filter_matches_dot_and_underscore() {
        assert_eq!(normalize_event_name("pane.updated"), "pane_updated");
        assert_eq!(normalize_event_name(" pane_updated "), "pane_updated");
    }

    /// The rebinding case, written as the header it arrives in. A page served
    /// from a name the attacker owns keeps sending that name in `Host` even
    /// after the name has been re-pointed at this machine, which is exactly
    /// what makes the header worth reading.
    #[test]
    fn a_name_this_gateway_was_never_told_about_is_refused() {
        let mut config = test_config("secret");
        config.listen = "100.99.165.54:23847".into();
        config.public_url = "http://mac-mini.example-tailnet.ts.net:23847".into();
        let known = known_hosts(&config);

        for good in [
            "100.99.165.54:23847",
            "100.99.165.54",
            "mac-mini.example-tailnet.ts.net:23847",
            "MAC-MINI.example-tailnet.TS.NET",
            "localhost:23847",
            "127.0.0.1:23847",
            "[::1]:23847",
            // A trailing dot is the same name spelled absolutely.
            "mac-mini.example-tailnet.ts.net.",
        ] {
            assert!(host_is_known(good, &known), "{good} should be answered");
        }

        for bad in [
            "rebind.attacker.example:23847",
            "attacker.example",
            "gateway.attacker.example",
            // The suffix rules are suffixes of a label, not of a string.
            "evil-ts.net",
            "notlocalhost",
            "ts.net.attacker.example",
        ] {
            assert!(!host_is_known(bad, &known), "{bad} should be refused");
        }
    }

    /// Ellen's gateway answers on a bare Tailscale address, and a great many
    /// installs will. An address literal has to pass, because rebinding needs a
    /// name whose resolution can be flipped and an address has none.
    #[test]
    fn an_address_is_always_a_host_this_gateway_answers_to() {
        let known = known_hosts(&test_config("secret"));
        for address in [
            "100.99.165.54:23847",
            "192.168.1.20:23847",
            "10.0.0.1",
            "[fd7a:115c:a1e0::1]:23847",
            "[::1]",
        ] {
            assert!(
                host_is_known(address, &known),
                "{address} should be answered"
            );
        }
    }

    #[test]
    fn the_known_hosts_are_the_public_url_and_the_listen_address() {
        let mut config = test_config("secret");
        config.listen = "0.0.0.0:23847".into();
        config.public_url = "https://desk.example-tailnet.ts.net".into();
        assert_eq!(
            known_hosts(&config),
            vec![
                String::from("0.0.0.0"),
                String::from("desk.example-tailnet.ts.net"),
                String::from("localhost"),
            ]
        );
    }

    #[test]
    fn a_host_header_is_read_down_to_its_name() {
        assert_eq!(host_name("Example.COM:23847"), "example.com");
        assert_eq!(host_name("example.com"), "example.com");
        assert_eq!(host_name("[::1]:23847"), "::1");
        assert_eq!(host_name("::1"), "::1");
        assert_eq!(host_name("example.com."), "example.com");
        // Not a port, so not cut off.
        assert_eq!(host_name("example.com:notaport"), "example.com:notaport");
    }

    #[test]
    fn token_hash_is_stable_and_not_plaintext() {
        let hash = hash_token("secret");
        assert_eq!(hash, hash_token("secret"));
        assert_ne!(hash, "secret");
    }

    #[test]
    fn control_routes_accept_a_device_token_and_report_which_device() {
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        assert_eq!(
            require_device(&state, &bearer_headers("device-token")).unwrap(),
            "device-1"
        );
    }

    #[test]
    fn control_routes_reject_the_admin_token() {
        // The admin token sits in plaintext on disk for the manage UI. Control
        // routes can run commands on the host, so it must not reach them.
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        let err = require_device(&state, &bearer_headers("secret")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn app_mount_zoom_is_side_effect_free_for_both_backends() {
        for backend in [BackendKind::Herdr, BackendKind::Tmux] {
            let mut state = test_state("secret", vec![test_device("device-1", "device-token")]);
            state.config.sessions[0].backend = backend;
            state.config.sessions[0].socket_path = String::from("/definitely/not/a/socket");
            let response = zoom_pane(
                State(state),
                Path((String::from("default"), String::from("missing-pane"))),
                bearer_headers("device-token"),
                Json(ZoomPaneBody {
                    mode: Some(String::from("on")),
                }),
            )
            .await
            .unwrap();
            assert_eq!(response.0["result"]["type"], "pane_zoomed");
        }
    }

    #[test]
    fn pending_pairing_accepts_only_the_admin_token() {
        let config = test_config("secret");
        assert!(require_admin(&config, &bearer_headers("secret")).is_ok());
        let err = require_admin(&config, &bearer_headers("device-token")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn pairing_revocation_accepts_manager_or_device_but_control_stays_device_only() {
        let state = test_state("admin-token", vec![test_device("device-1", "device-token")]);
        assert!(require_pairing_manager(&state, &bearer_headers("admin-token")).is_ok());
        assert!(require_pairing_manager(&state, &bearer_headers("device-token")).is_ok());
        assert!(require_pairing_manager(&state, &bearer_headers("wrong")).is_err());
        assert!(require_device(&state, &bearer_headers("admin-token")).is_err());
    }

    #[test]
    fn auth_rejects_invalid_bearer_token() {
        let state = test_state("secret", vec![test_device("device-1", "device-token")]);
        let err = require_device(&state, &bearer_headers("wrong")).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn auth_rejects_overlong_token() {
        let state = test_state("secret", Vec::new());
        let err = require_device(&state, &bearer_headers(&"x".repeat(257))).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        let config = test_config("secret");
        let err = require_admin(&config, &bearer_headers(&"x".repeat(257))).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn revoking_a_device_token_stops_it_authenticating() {
        let devices = vec![
            test_device("device-1", "token-1"),
            test_device("device-2", "token-2"),
        ];
        assert_eq!(
            identify_device(&devices, "token-1"),
            Some("device-1".into())
        );

        let remaining = devices
            .into_iter()
            .filter(|device| device.id != "device-1")
            .collect::<Vec<_>>();
        assert_eq!(identify_device(&remaining, "token-1"), None);
        // Revoking one device must leave the others working.
        assert_eq!(
            identify_device(&remaining, "token-2"),
            Some("device-2".into())
        );
    }

    #[test]
    fn device_names_reject_terminal_escape_injection() {
        assert!(valid_device_name("Ellen's iPhone"));
        assert!(valid_device_name("Pixel 9 Pro"));
        assert!(!valid_device_name(""));
        // The manage UI draws this into a terminal box.
        assert!(!valid_device_name("evil\x1b[2J\x1b[Hcode: AAAA-BBBB"));
        assert!(!valid_device_name("two\nlines"));
        assert!(!valid_device_name("tab\there"));
        assert!(!valid_device_name(&"x".repeat(MAX_DEVICE_NAME_CHARS + 1)));
    }

    #[test]
    fn configured_port_is_read_from_the_listen_address() {
        let mut config = test_config("secret");
        config.listen = "127.0.0.1:23847".into();
        assert_eq!(config.port(), 23847);
        config.listen = "0.0.0.0:9000".into();
        assert_eq!(config.port(), 9000);
        // A malformed listen address must not silently target another service.
        config.listen = "not-an-address".into();
        assert_eq!(config.port(), DEFAULT_PORT);
    }

    #[test]
    fn validate_text_enforces_size_limit() {
        assert!(validate_text("ok").is_ok());
        let too_large = "x".repeat(MAX_SEND_TEXT_BYTES + 1);
        let err = validate_text(&too_large).unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The only field on this API that becomes argv for a process on the host.
    /// A device token can already type into the pane, so this is a bound and
    /// not a fence -- but it is the same bound every other client string is
    /// under, and an argument list is not the field to leave unbounded.
    #[test]
    fn agent_args_are_bounded_like_every_other_client_string() {
        assert!(validate_agent_args(&[]).is_ok());
        assert!(validate_agent_args(&["--model".into(), "opus".into()]).is_ok());

        let too_many = vec![String::from("-v"); MAX_AGENT_ARGS + 1];
        assert_eq!(
            validate_agent_args(&too_many).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        let too_long = vec!["x".repeat(MAX_AGENT_ARG_CHARS + 1)];
        assert_eq!(
            validate_agent_args(&too_long).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        // A newline in an argument forges a line of the step log it is echoed
        // into, which is the same reason a device name cannot carry one.
        assert_eq!(
            validate_agent_args(&["--flag\nvalue".into()])
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert!(validate_agent_args(&["--flag\u{0}".into()]).is_err());
    }

    #[test]
    fn pairing_code_uses_unambiguous_characters() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), authority::PAIRING_CODE_LENGTH);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(authority::valid_pairing_code(&code));
        assert!(!authority::valid_pairing_code("ABCD2345"));
        assert!(!authority::valid_pairing_code("abcD-2345"));
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
        assert!(!code.contains('L'));
    }

    /// What the code is worth is what every position can hold and how evenly it
    /// holds it. Both halves are asserted, because both were wrong: one
    /// position could only reach sixteen of the thirty-one glyphs because it
    /// was reading a UUID's version nibble, and every position leaned on the
    /// first eight because a byte was folded with `%`.
    ///
    /// The bands are wide on purpose. Twenty thousand draws puts about 645 of
    /// each glyph in each position and about 5161 overall; a tenth of that is
    /// seven standard deviations, so the old bias (a ninth over, five of them)
    /// fails and a fair generator does not flake.
    #[test]
    fn every_glyph_can_land_in_every_position_and_none_is_favoured() {
        const DRAWS: usize = 20_000;
        let mut counts =
            vec![vec![0_usize; PAIRING_CODE_ALPHABET.len()]; PAIRING_CODE_CHARACTER_COUNT];
        for _ in 0..DRAWS {
            let code = generate_pairing_code();
            assert!(
                authority::valid_pairing_code(&code),
                "{code} is not a pairing code"
            );
            let glyphs: Vec<u8> = code.bytes().filter(|byte| *byte != b'-').collect();
            for (position, glyph) in glyphs.iter().enumerate() {
                let index = PAIRING_CODE_ALPHABET
                    .iter()
                    .position(|candidate| candidate == glyph)
                    .expect("a code is drawn from the alphabet");
                counts[position][index] += 1;
            }
        }

        for (position, row) in counts.iter().enumerate() {
            for (index, count) in row.iter().enumerate() {
                assert!(
                    *count > 0,
                    "position {position} never produced {}",
                    PAIRING_CODE_ALPHABET[index] as char
                );
            }
        }

        let total = DRAWS * PAIRING_CODE_CHARACTER_COUNT;
        let expected = total / PAIRING_CODE_ALPHABET.len();
        for index in 0..PAIRING_CODE_ALPHABET.len() {
            let seen: usize = counts.iter().map(|row| row[index]).sum();
            assert!(
                seen * 10 > expected * 9 && seen * 10 < expected * 11,
                "{} came up {seen} times against {expected} expected",
                PAIRING_CODE_ALPHABET[index] as char
            );
        }
    }

    #[test]
    fn request_id_validation_is_restrictive() {
        assert!(valid_request_id("iphone-15.req_1"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("has space"));
        assert!(!valid_request_id(&"x".repeat(81)));
    }

    #[test]
    fn pairing_requests_are_rate_limited_per_window() {
        let state = test_state("secret", Vec::new());
        for _ in 0..MAX_PAIRING_REQUESTS_PER_WINDOW {
            assert!(record_pairing_request(&state, 1_000).is_ok());
        }
        let error = record_pairing_request(&state, 1_001).unwrap_err();
        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(record_pairing_request(&state, 1_000 + PAIRING_RATE_LIMIT_WINDOW_MS).is_ok());
    }

    #[test]
    fn tailscale_serve_proxy_matching_uses_the_exact_port() {
        assert!(proxy_targets_port("http://127.0.0.1:23100", 23100));
        assert!(proxy_targets_port("http://localhost:23100/path", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:123100", 23100));
        assert!(!proxy_targets_port("http://127.0.0.1:23100.example", 23100));
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
    fn public_url_validation_allows_http_without_allowing_url_injection() {
        assert_eq!(
            validate_public_url("http://100.100.100.100:23100/").unwrap(),
            "http://100.100.100.100:23100"
        );
        assert!(validate_public_url("ftp://100.100.100.100/file").is_err());
        assert!(validate_public_url("http://user:secret@100.100.100.100:23100").is_err());
        assert!(validate_public_url("http://100.100.100.100:23100?token=secret").is_err());
    }

    fn test_pending_pairing(created_unix_ms: u128) -> Option<PendingPairing> {
        let code = "2345-6789".to_owned();
        Some(PendingPairing {
            request_id: "request-1".into(),
            device_name: "Muqun test".into(),
            install_id: None,
            code_hash: hash_token(&code),
            code,
            created_unix_ms,
            failed_attempts: 0,
        })
    }

    fn consume_test_pairing_code(
        pending: &mut Option<PendingPairing>,
        request_id: &str,
        code: &str,
        now_unix_ms: u128,
    ) -> Result<(), PairingCodeError> {
        authority::consume_pairing_code(
            pending,
            request_id,
            code,
            now_unix_ms,
            PAIRING_CODE_TTL_MS,
            MAX_PAIRING_CODE_ATTEMPTS,
        )
    }

    #[test]
    fn pairing_code_is_consumed_after_one_successful_claim() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_001),
            Ok(())
        );
        assert!(pending.is_none());
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_002),
            Err(PairingCodeError::Missing)
        );
    }

    #[test]
    fn expired_pairing_code_is_rejected_and_cleared() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(
                &mut pending,
                "request-1",
                "2345-6789",
                1_000 + PAIRING_CODE_TTL_MS
            ),
            Err(PairingCodeError::Expired)
        );
        assert!(pending.is_none());
    }

    #[test]
    fn repeated_invalid_pairing_attempts_invalidate_code() {
        let mut pending = test_pending_pairing(1_000);
        for _ in 0..MAX_PAIRING_CODE_ATTEMPTS {
            assert_eq!(
                consume_test_pairing_code(&mut pending, "request-1", "AAAA-AAAA", 1_001),
                Err(PairingCodeError::Invalid)
            );
        }
        assert!(pending.is_none());
    }

    #[test]
    fn stream_pane_read_becomes_inline_update() {
        let frame = StreamPaneFrame {
            revision: 42,
            output: "hello\n".into(),
        };
        let encoded = stream_pane_update_payload(&frame, "w1:p2").unwrap();
        let payload: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(frame.revision, 42);
        assert_eq!(payload["event"], "pane_updated");
        assert_eq!(payload["data"]["pane"]["pane_id"], "w1:p2");
        assert_eq!(payload["data"]["pane"]["revision"], 42);
        assert!(payload["data"]["pane"].get("source_revision").is_none());
        assert_eq!(payload["data"]["output"], "hello\n");
    }

    #[test]
    fn herdr_compatibility_has_a_floor_and_no_ceiling() {
        // The floor is the actual contract: below it the JSON API is a
        // different shape, so it stays frozen here on purpose.
        assert_eq!(HERDR_PROTOCOL_MIN, 17);
        assert!(!herdr_protocol_supported(1));
        assert!(!herdr_protocol_supported(16));

        // Both Herdr releases in play today.
        assert!(herdr_protocol_supported(17)); // 0.7.5
        assert!(herdr_protocol_supported(19)); // 0.8.0

        // And everything above them. A Herdr newer than this build has ever
        // seen is still served -- the protocol number tracks TUI wire changes
        // the gateway never speaks, so a bump is not evidence of a break.
        assert!(herdr_protocol_supported(20));
        assert!(herdr_protocol_supported(99));
        assert!(herdr_protocol_supported(u64::MAX));
    }

    #[test]
    fn pairing_payload_contains_mobile_connection_fields() {
        let payload = PairingPayload {
            kind: "muqun-gateway".into(),
            server_id: "server-1".into(),
            label: "machine".into(),
            url: "http://100.1.2.3:23100".into(),
            token: "secret".into(),
            transport_key: "transport-secret".into(),
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["kind"], "muqun-gateway");
        assert_eq!(value["url"], "http://100.1.2.3:23100");
        assert_eq!(value["token"], "secret");
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
    fn pairing_transport_policy_rejects_only_a_stale_encrypted_request() {
        // `Required` no longer forces an encrypted request through this gate:
        // `pair_claim` authenticates an unencrypted one with the one-time
        // code instead (see `code_pairing_response`), and `pair_request`
        // never carried anything worth protecting.
        assert!(require_pairing_transport(TransportEncryptionMode::Required, true).is_ok());
        assert!(require_pairing_transport(TransportEncryptionMode::Required, false).is_ok());
        assert!(require_pairing_transport(TransportEncryptionMode::Disabled, false).is_ok());
        // The one case still worth rejecting: a request sealed with a key
        // from before encryption was turned off.
        assert!(require_pairing_transport(TransportEncryptionMode::Disabled, true).is_err());
    }

    /// Reads the code `manage` would show a reader on the machine's screen,
    /// after a device with no QR key has called `pair_request`.
    fn pending_code(state: &AppState) -> String {
        state
            .pending_pairing
            .lock()
            .unwrap()
            .as_ref()
            .expect("pair_request left a pending code")
            .code
            .clone()
    }

    #[tokio::test]
    async fn manual_pairing_ends_up_with_the_same_transport_key_a_qr_pairing_would() {
        let state = test_state("admin-token", vec![]);
        assert_eq!(
            state.config.transport_encryption,
            TransportEncryptionMode::Required
        );
        // No `k`: exactly what a device that typed the address, rather than
        // scanning a QR, sends.
        let request_response = pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-1", "device_name": "Readers phone" })),
        )
        .await
        .unwrap();
        assert_eq!(request_response.status(), StatusCode::OK);

        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-1", "code": code })),
        )
        .await
        .unwrap();
        assert_eq!(claim_response.status(), StatusCode::OK);

        // The body is a sealed envelope, not a plain pairing payload: it was
        // never sent in the clear even though the request that earned it
        // carried no pre-shared key.
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: transport::Envelope = serde_json::from_slice(&body).unwrap();
        let material = code_pairing_material(&code, "manual-1").unwrap();
        let plaintext = transport::open(
            &material,
            transport::Direction::PairingResponse,
            b"POST /api/pair/claim\ncode-pairing\n",
            &envelope,
            now_unix_ms(),
        )
        .expect("a reader who typed the correct code can open the response");
        let payload: Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(payload["kind"], "muqun-gateway");
        // Same shape a QR pairing gets from this gateway: a device transport
        // key, not just a bearer token. No silent downgrade.
        assert_eq!(payload["transport"], "muqun-aes-256-gcm-v1");
        assert!(payload["device_id"].as_str().is_some());
        let transport_key = payload["transport_key"].as_str().unwrap();
        assert!(transport::decode_key(transport_key).is_ok_and(|key| key.len() == 32));
    }

    #[tokio::test]
    async fn manual_pairing_response_cannot_be_opened_without_the_code() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-eaves", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-eaves", "code": code })),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: transport::Envelope = serde_json::from_slice(&body).unwrap();

        // A passive observer who saw the wire traffic but not the code typed
        // into the phone has to brute force the code before this opens --
        // guessing wrong does not open it.
        let wrong_material = code_pairing_material("AAAA-AAAA", "manual-eaves").unwrap();
        assert!(transport::open(
            &wrong_material,
            transport::Direction::PairingResponse,
            b"POST /api/pair/claim\ncode-pairing\n",
            &envelope,
            now_unix_ms(),
        )
        .is_err());
    }

    #[tokio::test]
    async fn manual_pairing_omits_the_transport_key_when_encryption_is_disabled() {
        let mut state = test_state("admin-token", vec![]);
        state.config.transport_encryption = TransportEncryptionMode::Disabled;
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-2", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        let claim_response = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-2", "code": code })),
        )
        .await
        .unwrap();
        assert_eq!(claim_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(claim_response.into_body(), usize::MAX)
            .await
            .unwrap();
        // Plain JSON, not a sealed envelope: `Disabled` never sealed a
        // response before this card and still does not.
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["kind"], "muqun-gateway");
        assert!(payload.get("transport_key").is_none());
        assert!(payload.get("transport").is_none());
    }

    #[tokio::test]
    async fn manual_pairing_code_is_single_use() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "device_name": "Phone" })),
        )
        .await
        .unwrap();
        let code = pending_code(&state);
        pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "code": code.clone() })),
        )
        .await
        .unwrap();

        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-3", "code": code })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "pairing_not_requested");
    }

    #[tokio::test]
    async fn manual_pairing_code_expires_server_side() {
        let state = test_state("admin-token", vec![]);
        let code = "2345-6789".to_owned();
        *state.pending_pairing.lock().unwrap() = Some(PendingPairing {
            request_id: "manual-4".into(),
            device_name: "Phone".into(),
            install_id: None,
            code_hash: hash_token(&code),
            code,
            created_unix_ms: 0,
            failed_attempts: 0,
        });

        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-4", "code": "2345-6789" })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body["error"]["code"], "pairing_code_expired");
    }

    #[tokio::test]
    async fn manual_pairing_wrong_codes_are_rate_limited_and_burn_the_pairing() {
        let state = test_state("admin-token", vec![]);
        pair_request(
            State(state.clone()),
            Json(json!({ "request_id": "manual-5", "device_name": "Phone" })),
        )
        .await
        .unwrap();

        for _ in 0..MAX_PAIRING_CODE_ATTEMPTS {
            let (status, Json(body)) = pair_claim(
                State(state.clone()),
                Json(json!({ "request_id": "manual-5", "code": "AAAA-AAAA" })),
            )
            .await
            .unwrap_err();
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"]["code"], "invalid_pairing_code");
        }

        // The correct code no longer works either: the pairing burned after
        // `MAX_PAIRING_CODE_ATTEMPTS` wrong guesses, same as it always did.
        let real_code = "2345-6789";
        let (status, Json(body)) = pair_claim(
            State(state.clone()),
            Json(json!({ "request_id": "manual-5", "code": real_code })),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "pairing_not_requested");
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
    fn blocked_agent_event_creates_one_notification() {
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "type": "pane.agent_status_changed",
                "pane_id": "w1:p2",
                "workspace_id": "w1",
                "display_agent": "Codex",
                "agent_status": "blocked"
            }
        });
        let mut statuses = HashMap::new();
        let notice = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Agent blocked · Studio");
        assert_eq!(notification.body, "Codex needs your input.");
        assert_eq!(notification.data["type"], "agent.blocked");
        assert_eq!(notification.data["url"], "/servers/server-1");
        // The same transition, said to a phone that reads Chinese. The name the
        // agent goes by is not translated -- it is a name.
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "代理程式等待中 · Studio");
        assert_eq!(chinese.body, "Codex 需要你的輸入。");
        assert_eq!(chinese.data["type"], "agent.blocked");
        assert_eq!(chinese.data["url"], "/servers/server-1");
        assert!(notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .is_none());
    }

    #[test]
    fn working_to_idle_creates_completion_notification() {
        let mut statuses = HashMap::from([("w1:p2".into(), "working".into())]);
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": "w1:p2",
                "agent": "codex",
                "agent_status": "idle"
            }
        });
        let notice = notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Agent done · Studio");
        assert_eq!(notification.body, "codex finished running.");
        assert_eq!(notification.data["type"], "agent.completed");
        assert_eq!(notification.data["pane_id"], "w1:p2");
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "代理程式已完成 · Studio");
        assert_eq!(chinese.body, "codex 已執行完畢。");
    }

    /// The agent's name goes where the sentence wants it, not where the English
    /// happened to put it.
    ///
    /// The old body was `format!("{name} {tail}")` over fragments like "needs
    /// your input.", which fixes the name to the front of the sentence in every
    /// language there will ever be. A whole format string per locale is what
    /// makes the slot movable, and an agent that reports no name at all gets the
    /// reader's own word for one rather than the English "Agent".
    #[test]
    fn a_push_names_the_agent_from_a_slot_and_not_from_a_concatenation() {
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p9", "agent": "   ", "agent_status": "blocked" }
        });
        let mut statuses = HashMap::new();
        let notice =
            notification_for_agent_status_event(&event, &mut statuses, "s", "", "default").unwrap();
        assert_eq!(notice.agent_name, None);
        // No server label, so the title is the heading on its own.
        assert_eq!(notice.render(Locale::En).title, "Agent blocked");
        assert_eq!(notice.render(Locale::En).body, "Agent needs your input.");
        assert_eq!(notice.render(Locale::ZhTw).title, "代理程式等待中");
        assert!(notice.render(Locale::ZhTw).body.ends_with("需要你的輸入。"));
        assert!(notice.render(Locale::ZhTw).body.starts_with("代理程式"));
    }

    #[test]
    fn an_approval_push_says_that_something_needs_answering_and_never_what() {
        // The whole privacy rule for notifications, asserted end to end on a
        // real menu: the agent quoted the command in its own option label, and
        // none of it may reach Expo.
        let approval =
            approvals::detect(include_str!("../tests/fixtures/approval-claude-bash.txt"))
                .expect("the fixture is a pending approval");
        let notice = approval_notification(
            "server-1", "Studio", "default", "wM:p1", "claude", &approval,
        );
        let notification = notice.render(Locale::En);
        assert_eq!(notification.title, "Approval needed · Studio");
        assert_eq!(notification.body, "claude is waiting for your approval.");
        assert_eq!(notification.data["type"], "approval.pending");
        assert_eq!(notification.data["pane_id"], "wM:p1");
        // The category the client registered its approve/deny actions under,
        // and which of them this menu offers.
        assert_eq!(notification.data["categoryId"], "approval");
        assert_eq!(notification.data["options"][0]["decision"], "allow");
        assert_eq!(notification.data["options"][2]["decision"], "deny");
        assert_eq!(notification.data["fingerprint"], approval.fingerprint);
        let rendered = Value::Object(notification.data).to_string();
        assert!(!rendered.contains("npm"), "the command must not travel");
        assert!(!rendered.contains("Do you want"), "nor the question");

        // Translating the four labels the gateway wrote for itself cannot
        // weaken any of that: the words changed, whose words they are did not.
        let chinese = notice.render(Locale::ZhTw);
        assert_eq!(chinese.title, "需要核准 · Studio");
        assert_eq!(chinese.body, "claude 正在等待你的核准。");
        assert_eq!(chinese.data["options"][0]["label"], "核准");
        assert_eq!(chinese.data["options"][2]["label"], "拒絕");
        assert_eq!(
            chinese.data["options"][0]["decision"], "allow",
            "the decision is wire vocabulary and has no language"
        );
        let rendered = Value::Object(chinese.data).to_string();
        assert!(!rendered.contains("npm"), "the command must not travel");
        assert!(!rendered.contains("Do you want"), "nor the question");
    }

    #[test]
    fn the_approval_payload_carries_the_pane_and_answers_in_the_content_envelope() {
        let approval =
            approvals::detect(include_str!("../tests/fixtures/approval-claude-bash.txt")).unwrap();
        let pending = content_envelope(approval_data_menu(
            "default",
            "wM:p1",
            Some("claude"),
            Some(&approval),
        ));
        assert_eq!(pending["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(pending["data"]["state"], "pending");
        assert_eq!(pending["data"]["pane"]["approvals"], "menu");
        assert_eq!(
            pending["data"]["approval"]["options"][2]["decision"],
            "deny"
        );

        // An idle pane is answered with the same shape and a null approval, so
        // a client has one code path rather than two.
        let idle = content_envelope(approval_data_menu("default", "wM:p1", Some("claude"), None));
        assert_eq!(idle["data"]["state"], "idle");
        assert!(idle["data"]["approval"].is_null());
        assert_eq!(idle["data"]["pane"]["approvals"], "menu");
    }

    #[test]
    fn approvals_are_announced_as_a_capability_and_documented() {
        // Additive: the routes and events are new, so a client gates on the
        // capability rather than probing for a 404.
        assert!(API_CAPABILITIES.contains(&"pane_approvals"));
        let spec = openapi_spec();
        let approval = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/approval"];
        assert!(approval["get"].is_object());
        assert!(approval["post"]["requestBody"].is_object());
        assert!(approval["post"]["responses"]["409"].is_object());
    }

    fn status_event(pane_id: &str, agent: &str, status: &str) -> Value {
        json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": pane_id, "agent": agent, "agent_status": status }
        })
    }

    #[test]
    fn the_ring_records_every_transition_and_not_only_the_ones_worth_a_push() {
        // The digest is the reason: "it worked for twenty minutes and then went
        // idle" is the sentence a returning user wants, and only the last half
        // of it ever rang a doorbell.
        let state = test_state("admin", vec![test_device("d1", "token")]);
        let mut statuses = HashMap::new();

        let started = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "working"),
            &mut statuses,
        );
        let finished = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "idle"),
            &mut statuses,
        );
        // A repeat of the status the pane is already in is not a transition and
        // must not appear twice in a digest.
        let repeated = absorb_agent_status_event(
            &state,
            "default",
            &status_event("w1:p1", "claude", "idle"),
            &mut statuses,
        );

        assert!(started.is_none(), "starting work wakes nobody");
        assert!(finished.is_some(), "finishing does");
        assert!(repeated.is_none());

        let log = state.agent_events.lock().unwrap();
        let events = log.since("default", None);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from, None);
        assert_eq!(events[0].to, "working");
        assert_eq!(events[1].from.as_deref(), Some("working"));
        assert_eq!(events[1].to, "idle");
        assert_eq!(events[1].agent.as_deref(), Some("claude"));
        assert_eq!(log.latest_seq("default"), 2);
    }

    #[tokio::test]
    async fn the_digest_endpoint_answers_what_is_new_and_where_to_resume_from() {
        let state = test_state("admin", vec![test_device("d1", "token")]);
        let mut statuses = HashMap::new();
        for status in ["working", "blocked", "idle"] {
            absorb_agent_status_event(
                &state,
                "default",
                &status_event("w1:p1", "claude", status),
                &mut statuses,
            );
        }

        let answer = session_agent_events(
            State(state.clone()),
            Path("default".into()),
            Query(AgentEventsQuery { since: None }),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(answer["events"].as_array().unwrap().len(), 3);
        assert_eq!(answer["events"][0]["to"], "working");
        assert_eq!(answer["events"][0]["from"], Value::Null);
        assert_eq!(answer["next_since"], 3);
        assert_eq!(answer["missed"], false);
        assert_eq!(answer["capacity"], agent_events::RING_CAPACITY);

        // Polling from where the last answer left off returns nothing and still
        // says where to resume from, so an idle session does not walk backwards.
        let resumed = session_agent_events(
            State(state.clone()),
            Path("default".into()),
            Query(AgentEventsQuery { since: Some(3) }),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(resumed["events"].as_array().unwrap().len(), 0);
        assert_eq!(resumed["next_since"], 3);
        assert_eq!(resumed["missed"], false);

        // Nothing here is readable without a paired device, and a session this
        // gateway does not have is a 404 rather than an empty digest.
        assert_eq!(
            session_agent_events(
                State(state.clone()),
                Path("default".into()),
                Query(AgentEventsQuery { since: None }),
                bearer_headers("not-a-token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            session_agent_events(
                State(state),
                Path("other".into()),
                Query(AgentEventsQuery { since: None }),
                bearer_headers("token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn first_idle_event_does_not_create_false_completion() {
        let mut statuses = HashMap::new();
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p2", "agent_status": "idle" }
        });
        assert!(notification_for_agent_status_event(
            &event,
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .is_none());
        assert_eq!(statuses.get("w1:p2").map(String::as_str), Some("idle"));
    }

    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"IHDR and the rest of a real file");
        bytes
    }

    fn test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let local_offset = bytes.len() as u32;
            bytes.extend_from_slice(ZIP_LOCAL_HEADER);
            bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
            bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
            bytes.extend_from_slice(&0u16.to_le_bytes()); // stored
            bytes.extend_from_slice(&0u16.to_le_bytes()); // time
            bytes.extend_from_slice(&0u16.to_le_bytes()); // date
            bytes.extend_from_slice(&0u32.to_le_bytes()); // crc (not read by the probe)
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes()); // extra
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(body);

            central.extend_from_slice(ZIP_CENTRAL_HEADER);
            central.extend_from_slice(&20u16.to_le_bytes()); // made by
            central.extend_from_slice(&20u16.to_le_bytes()); // needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // stored
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&0u32.to_le_bytes()); // crc
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(ZIP_END_HEADER);
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk
        bytes.extend_from_slice(&0u16.to_le_bytes()); // central disk
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&central_offset.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment
        bytes
    }

    #[test]
    fn uploads_are_typed_by_content_not_by_name() {
        assert_eq!(sniff_upload_kind(&png_bytes()).unwrap().mime, "image/png");
        assert_eq!(
            sniff_upload_kind(b"\xff\xd8\xff\xe0\x00\x10JFIF")
                .unwrap()
                .mime,
            "image/jpeg"
        );
        assert_eq!(sniff_upload_kind(b"GIF89a....").unwrap().extension, "gif");
        assert_eq!(sniff_upload_kind(b"GIF87a....").unwrap().extension, "gif");
        assert_eq!(
            sniff_upload_kind(b"RIFF\x24\x00\x00\x00WEBPVP8 ")
                .unwrap()
                .mime,
            "image/webp"
        );
        assert_eq!(
            sniff_upload_kind(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00")
                .unwrap()
                .mime,
            "image/heic"
        );
        assert_eq!(
            sniff_upload_kind(b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00")
                .unwrap()
                .extension,
            "heic"
        );
    }

    #[test]
    fn safe_documents_are_typed_by_content_before_name() {
        let pdf = sniff_document_upload_kind(b"%PDF-1.7\n1 0 obj", "notes.txt").unwrap();
        assert_eq!(pdf.extension, "pdf");
        assert_eq!(pdf.mime, "application/pdf");

        let markdown = sniff_document_upload_kind(b"# notes\n\nplain\n", "notes.md").unwrap();
        assert_eq!(markdown.extension, "md");
        assert_eq!(markdown.mime, "text/markdown; charset=utf-8");

        let source = sniff_document_upload_kind(b"const answer = 42;\n", "answer.ts").unwrap();
        assert_eq!(source.extension, "ts");
        assert_eq!(source.mime, "text/plain; charset=utf-8");

        let unknown = sniff_document_upload_kind(b"plain UTF-8\n", "README.weird").unwrap();
        assert_eq!(unknown.extension, "txt");
    }

    #[test]
    fn modern_office_packages_are_recognised_from_their_zip_structure() {
        let docx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("word/document.xml", b"document"),
        ]);
        assert_eq!(sniff_office_upload_kind(&docx).unwrap().extension, "docx");

        let xlsx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("xl/workbook.xml", b"workbook"),
        ]);
        assert_eq!(sniff_office_upload_kind(&xlsx).unwrap().extension, "xlsx");

        let pptx = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("ppt/presentation.xml", b"presentation"),
        ]);
        assert_eq!(sniff_office_upload_kind(&pptx).unwrap().extension, "pptx");
    }

    #[test]
    fn open_document_packages_require_the_stored_mimetype_first() {
        let odt = test_zip(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("content.xml", b"content"),
            ("META-INF/manifest.xml", b"manifest"),
        ]);
        assert_eq!(sniff_office_upload_kind(&odt).unwrap().extension, "odt");

        let mimetype_late = test_zip(&[
            ("content.xml", b"content"),
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("META-INF/manifest.xml", b"manifest"),
        ]);
        assert!(sniff_office_upload_kind(&mimetype_late).is_none());
    }

    #[test]
    fn binary_documents_and_archives_are_refused() {
        let ordinary_zip = test_zip(&[("hello.txt", b"hello")]);
        assert!(sniff_office_upload_kind(&ordinary_zip).is_none());
        assert!(sniff_document_upload_kind(&ordinary_zip, "archive.zip").is_none());
        let ambiguous = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("word/document.xml", b"document"),
            ("xl/workbook.xml", b"workbook"),
        ]);
        assert!(sniff_office_upload_kind(&ambiguous).is_none());
        let traversal = test_zip(&[
            ("[Content_Types].xml", b"types"),
            ("_rels/.rels", b"rels"),
            ("../word/document.xml", b"document"),
        ]);
        assert!(sniff_office_upload_kind(&traversal).is_none());
        assert!(sniff_document_upload_kind(b"hello\0world", "notes.txt").is_none());
        assert!(sniff_document_upload_kind(b"\xff\xfe\x00x", "notes.txt").is_none());
        // A filename can preserve a useful extension only after the bytes have
        // passed the text probe; it cannot disguise an archive as source.
        assert!(sniff_document_upload_kind(b"PK\x03\x04", "archive.ts").is_none());
    }

    #[test]
    fn a_truncated_or_binary_upload_is_not_mistaken_for_a_known_type() {
        // Half a signature is not a match.
        assert!(sniff_upload_kind(b"\x89PN").is_none());
        assert!(sniff_upload_kind(b"\x89PNG\r\n\x1a").is_none());
        assert!(sniff_upload_kind(b"RIFF\x24\x00\x00\x00WEB").is_none());
        assert!(sniff_upload_kind(b"\x00\x00\x00\x18ftyp").is_none());
        // An ISO base media file that is not a HEIC flavour.
        assert!(sniff_upload_kind(b"\x00\x00\x00\x18ftypqt  \x00\x00\x00\x00").is_none());
        assert!(sniff_upload_kind(b"\x1f\x8b\x08\x00\x00\x00\x00\x00").is_none());
        assert!(sniff_upload_kind(b"").is_none());
    }

    #[test]
    fn executables_and_scripts_are_refused_whatever_they_are_called() {
        assert!(looks_executable(b"MZ\x90\x00\x03"));
        assert!(looks_executable(b"\x7fELF\x02\x01\x01"));
        assert!(looks_executable(b"\xfe\xed\xfa\xce\x00"));
        assert!(looks_executable(b"\xfe\xed\xfa\xcf\x00"));
        assert!(looks_executable(b"\xce\xfa\xed\xfe\x00"));
        assert!(looks_executable(b"\xcf\xfa\xed\xfe\x00"));
        assert!(looks_executable(b"\xca\xfe\xba\xbe\x00"));
        assert!(looks_executable(b"\xbe\xba\xfe\xca\x00"));
        assert!(looks_executable(b"#!/bin/sh\nrm -rf /\n"));
        assert!(looks_executable(b"#!"));

        // A script is refused twice over: it is executable, and it carries no
        // image magic number either.
        assert!(sniff_upload_kind(b"#!/bin/sh\nrm -rf /\n").is_none());

        assert!(!looks_executable(&png_bytes()));
        assert!(!looks_executable(b"%PDF-1.7"));
        assert!(!looks_executable(b"# a markdown file\n"));
        assert!(!looks_executable(b"M"));
    }

    #[test]
    fn a_client_file_name_is_only_ever_echoed_back_after_scrubbing() {
        assert_eq!(sanitize_upload_name("../../evil.png"), "evil.png");
        assert_eq!(sanitize_upload_name("..\\..\\evil.png"), "evil.png");
        assert_eq!(sanitize_upload_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_upload_name("shot\r\n.png"), "shot.png");
        assert_eq!(sanitize_upload_name("bell\x07.txt"), "bell.txt");
        assert_eq!(sanitize_upload_name("  spaced.png  "), "spaced.png");
        assert_eq!(sanitize_upload_name(""), "upload");
        assert_eq!(sanitize_upload_name("   "), "upload");
        assert_eq!(sanitize_upload_name(".."), "upload");
        assert_eq!(sanitize_upload_name("../.."), "upload");
        assert_eq!(sanitize_upload_name("photo.png"), "photo.png");

        let long = format!("{}.png", "n".repeat(400));
        assert_eq!(
            sanitize_upload_name(&long).chars().count(),
            MAX_UPLOAD_NAME_CHARS
        );

        // A multi-byte name must not be cut mid-character.
        let wide = "截图".repeat(200);
        assert!(sanitize_upload_name(&wide).chars().count() <= MAX_UPLOAD_NAME_CHARS);
    }

    #[test]
    fn the_stored_name_comes_from_the_sniffed_type_and_nothing_else() {
        let kind = sniff_upload_kind(&png_bytes()).unwrap();
        let first = stored_upload_name(kind);
        let second = stored_upload_name(kind);
        assert!(first.ends_with(".png"));
        assert_ne!(first, second, "each upload gets its own name");
        assert!(!first.contains('/') && !first.contains('\\') && !first.contains(".."));
        assert_eq!(
            first.len(),
            "00000000-0000-0000-0000-000000000000.png".len()
        );
    }

    #[test]
    fn uploads_expire_after_the_retention_window_but_survive_a_clock_jump() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!upload_expired(now - Duration::from_secs(47 * 3600), now));
        assert!(upload_expired(now - UPLOAD_RETENTION, now));
        assert!(upload_expired(now - Duration::from_secs(49 * 3600), now));
        // A timestamp in the future means the clock moved, not that the file is
        // old; deleting it would lose an upload the user just made.
        assert!(!upload_expired(now + Duration::from_secs(3600), now));
    }

    #[test]
    fn the_sweep_removes_only_files_past_the_retention_window() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-uploads-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();

        let fresh = dir.join("fresh.png");
        std::fs::write(&fresh, b"fresh").unwrap();
        let stale = dir.join("stale.png");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(now - UPLOAD_RETENTION - Duration::from_secs(60))
            .unwrap();

        assert_eq!(purge_expired_uploads(&dir, now).unwrap(), 1);
        assert!(fresh.exists());
        assert!(!stale.exists());

        // A missing directory is not an error: nothing has been uploaded yet.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(purge_expired_uploads(&dir, now).unwrap(), 0);
    }

    fn asset_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-assets-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The temp dir is itself a symlink on macOS, so the fixture is
        // canonicalized once here: every check downstream compares canonical
        // paths, and a test must not be the one place that does not.
        std::fs::canonicalize(&dir).unwrap()
    }

    fn test_asset_entry(path: &FsPath, root: &FsPath, modified_unix_ms: u128) -> AssetEntry {
        AssetEntry {
            id: asset_id(path),
            path: path.to_path_buf(),
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            size: 1,
            modified_unix_ms,
            root: root.to_path_buf(),
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: Some("wA:t1".into()),
            pane_id: Some("wA:p1".into()),
        }
    }

    #[test]
    fn asset_reads_are_fenced_inside_the_workspace_roots() {
        let root = asset_test_dir("fence");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("docs/report.md"), b"# report\n").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let roots = vec![workspace.clone()];

        assert!(resolve_asset_path(&workspace.join("docs/report.md"), &roots).is_some());

        // Traversal out of the root, whatever shape it arrives in.
        assert!(
            resolve_asset_path(&workspace.join("docs/../../outside/secret.txt"), &roots).is_none()
        );
        assert!(resolve_asset_path(&outside.join("secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(FsPath::new("/etc/hosts"), &roots).is_none());

        // A sibling whose name merely starts with the root's is not inside it.
        let neighbour = root.join("workspace-notes");
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("note.txt"), b"note\n").unwrap();
        assert!(resolve_asset_path(&neighbour.join("note.txt"), &roots).is_none());

        // A directory is not an asset, and neither is the root itself.
        assert!(resolve_asset_path(&workspace, &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("docs"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("missing.md"), &roots).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_indexed_asset_outlives_the_workspace_that_made_it() {
        // The worktree an agent wrote into was removed hours later. The roots
        // resolve to nothing now, which is the whole of what changed: the file
        // is still there and still the thing the user tapped.
        let root = asset_test_dir("provenance");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();

        // While the workspace is a root, nothing about the read changed.
        let live = vec![workspace.clone()];
        assert_eq!(
            resolve_indexed_asset_path(&report, &live),
            Some(report.clone())
        );

        // With no roots at all -- the workspace closed -- the stored path is
        // replayed and answers the same bytes.
        assert_eq!(
            resolve_indexed_asset_path(&report, &[]),
            Some(report.clone())
        );

        // A file that is gone is gone, roots or no roots.
        let deleted = workspace.join("gone.md");
        std::fs::write(&deleted, b"bye\n").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        assert!(resolve_indexed_asset_path(&deleted, &[]).is_none());

        // A directory left where the file was is not a file.
        std::fs::create_dir_all(workspace.join("was-a-file")).unwrap();
        assert!(resolve_indexed_asset_path(&workspace.join("was-a-file"), &[]).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_into_a_stored_asset_path_is_not_that_asset() {
        // The attack the equality guard exists for: the workspace closes, the
        // real file is replaced by a link to somewhere the gateway would never
        // have indexed, and the old id is presented again.
        let root = asset_test_dir("provenance-symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let stored = workspace.join("report.md");
        std::fs::write(&stored, b"# report\n").unwrap();
        assert!(resolve_indexed_asset_path(&stored, &[]).is_some());

        std::fs::remove_file(&stored).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), &stored).unwrap();

        // The path canonicalizes to the link's target, which is not the path
        // that was stored, so the replay refuses it -- and refuses it the same
        // way an unknown id is refused.
        assert!(resolve_indexed_asset_path(&stored, &[]).is_none());
        assert!(resolve_indexed_asset_path(&stored, std::slice::from_ref(&workspace)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_that_was_never_indexed_has_no_entry_to_replay() {
        // The fallback is reached through an index lookup and nowhere else, so
        // provenance is what it serves. A file the gateway never scanned has no
        // entry, and the read stops at the lookup with the same 404 as a bad id.
        let root = asset_test_dir("provenance-unindexed");
        std::fs::create_dir_all(&root).unwrap();
        let indexed = root.join("indexed.md");
        let never = root.join("never-scanned.md");
        std::fs::write(&indexed, b"# indexed\n").unwrap();
        std::fs::write(&never, b"# private\n").unwrap();

        let mut index = AssetIndex::default();
        index.upsert(test_asset_entry(&indexed, &root, 1));

        assert!(index.get(&asset_id(&indexed)).is_some());
        assert!(index.get(&asset_id(&never)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_uploads_directory_ingests_like_any_other_root_and_survives_a_cold_start() {
        // The bug this guards against: an upload lives in the gateway's own
        // uploads directory, which is not under any pane's cwd, so a rebuild
        // that only walks pane roots never finds it again once the in-memory
        // index is gone (e.g. after a restart). The cold-start path in
        // `asset_content` now adds the uploads directory as one more root per
        // session -- this proves that root ingests the same way a pane root
        // does, and that the resulting entry answers a lookup with none of the
        // session's live roots present, exactly the state a fresh process is
        // in right after startup.
        let uploads = asset_test_dir("uploads-cold-start");
        let upload = uploads.join("5969c1e3.webp");
        std::fs::write(&upload, b"fake webp bytes").unwrap();

        let root = AssetRoot {
            path: uploads.clone(),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        };
        let index: Mutex<AssetIndex> = Mutex::new(AssetIndex::default());
        let created = ingest_root(&index, &root);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].path, upload);
        assert_eq!(created[0].session_id, "default");

        // Simulate the state right after a restart answering `asset_content`:
        // the entry is in the index (just rebuilt), but the session's live
        // roots -- the pane cwds -- do not include the uploads directory and
        // never will. `resolve_indexed_asset_path`'s own-path fallback is what
        // actually serves it; this proves the entry it is given exists to
        // fall back on at all.
        let id = asset_id(&upload);
        let entry = index.lock().unwrap().get(&id).unwrap();
        assert_eq!(entry.path, upload);
        let no_live_pane_roots: Vec<PathBuf> = Vec::new();
        assert_eq!(
            resolve_indexed_asset_path(&entry.path, &no_live_pane_roots),
            Some(upload.clone())
        );

        std::fs::remove_dir_all(&uploads).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_workspace_root_cannot_be_read_or_scanned() {
        let root = asset_test_dir("symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("escape.txt"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape-dir")).unwrap();
        std::fs::write(workspace.join("own.txt"), b"mine\n").unwrap();
        let roots = vec![workspace.clone()];

        // The link resolves to where it points, which is outside the root.
        assert!(resolve_asset_path(&workspace.join("escape.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("escape-dir/secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("own.txt"), &roots).is_some());

        // The scan never offers such a path in the first place.
        let names: Vec<String> = scan_workspace_root(&workspace, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert_eq!(names, vec![String::from("own.txt")]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn asset_kind_comes_from_the_bytes_and_the_extension_only_splits_the_text_kinds() {
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.png").kind,
            AssetKind::Image
        );
        // A name that lies about the content does not change what it is.
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.md").kind,
            AssetKind::Image
        );
        assert_eq!(
            sniff_asset_type(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n", "report.pdf").kind,
            AssetKind::Pdf
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.md").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.MARKDOWN").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.txt").kind,
            AssetKind::Text
        );
        assert_eq!(
            sniff_asset_type("日本語とemoji 🎈\n".as_bytes(), "notes").kind,
            AssetKind::Text
        );
        // A markdown extension over bytes that are not text is still binary.
        assert_eq!(
            sniff_asset_type(b"\x00\x01\x02binary\x00", "notes.md").kind,
            AssetKind::Binary
        );
        assert_eq!(
            sniff_asset_type(&[0xff, 0xfe, 0xfd, 0xfc], "blob.bin").kind,
            AssetKind::Binary
        );
        assert_eq!(sniff_asset_type(b"", "empty.txt").kind, AssetKind::Text);

        assert!(AssetKind::Markdown.previewable());
        assert!(AssetKind::Image.previewable());
        assert!(AssetKind::Pdf.previewable());
        assert!(!AssetKind::Binary.previewable());

        // A UTF-8 character cut in half by the sniff window is a truncation,
        // not a binary file.
        let mut truncated = "héllo".as_bytes().to_vec();
        truncated.pop();
        assert!(looks_textual(&truncated));
        assert!(!looks_textual(b"text\x00text"));
    }

    #[test]
    fn a_scan_stays_shallow_and_skips_heavy_directories() {
        let root = asset_test_dir("scan");
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
        std::fs::write(root.join("report.md"), b"# report\n").unwrap();
        std::fs::write(root.join(".hidden"), b"hidden\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), b"module\n").unwrap();
        std::fs::write(root.join("target/debug/binary"), b"binary\n").unwrap();
        std::fs::write(root.join(".git/objects/blob"), b"blob\n").unwrap();
        std::fs::write(root.join("a/b/c/deep.txt"), b"deep\n").unwrap();
        std::fs::write(root.join("a/b/c/d/e/too-deep.txt"), b"too deep\n").unwrap();

        let mut names: Vec<String> = scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![String::from("deep.txt"), String::from("report.md")]
        );

        // The file budget is a hard stop, not a suggestion.
        assert_eq!(scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 1).len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_index_dedupes_by_path_and_answers_newest_first() {
        let root = asset_test_dir("index");
        let nested = root.join("nested");
        let mut index = AssetIndex::default();

        let old = root.join("old.txt");
        let new = root.join("new.txt");
        assert!(index.upsert(test_asset_entry(&old, &root, 1_000)));
        assert!(index.upsert(test_asset_entry(&new, &root, 2_000)));
        // The same file seen again by a later scan updates it in place.
        let mut rescanned = test_asset_entry(&old, &root, 3_000);
        rescanned.size = 4_096;
        assert!(!index.upsert(rescanned));
        assert_eq!(index.entries.len(), 2);

        // `test_asset_entry` puts everything in workspace "wA".
        let scope_a = AssetScope::Workspace("wA".into());
        let scope_b = AssetScope::Workspace("wB".into());
        let listed = index.session_assets("default", &scope_a, None, 10);
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>(),
            vec![String::from("old.txt"), String::from("new.txt")]
        );
        assert_eq!(listed[0].size, 4_096);
        assert_eq!(listed[0].modified_unix_ms, 3_000);

        // `since` is exclusive, and `limit` cuts the newest page.
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(2_000), 10)
                .len(),
            1
        );
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(3_000), 10)
                .len(),
            0
        );
        assert_eq!(index.session_assets("default", &scope_a, None, 1).len(), 1);
        assert_eq!(index.session_assets("other", &scope_a, None, 10).len(), 0);
        // Same session, a different workspace: none of "wA"'s files leak into
        // it. This is the defect the workspace scope closes -- everything
        // above proves the index still works exactly as it did, this proves
        // it no longer answers wider than the workspace asked for.
        assert_eq!(index.session_assets("default", &scope_b, None, 10).len(), 0);

        // Nested roots see the same file; the deeper one owns it.
        let shared = nested.join("shared.txt");
        assert!(index.upsert(test_asset_entry(&shared, &root, 4_000)));
        let mut deeper = test_asset_entry(&shared, &nested, 4_000);
        deeper.workspace_id = Some("wB".into());
        assert!(!index.upsert(deeper));
        let owned = index.get(&asset_id(&shared)).unwrap();
        assert_eq!(owned.root, nested);
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));
        // A shallower root does not take it back.
        let mut shallower = test_asset_entry(&shared, &root, 5_000);
        shallower.workspace_id = Some("wA".into());
        assert!(!index.upsert(shallower));
        assert_eq!(index.get(&asset_id(&shared)).unwrap().root, nested);

        // A removed worktree takes its files with it.
        index.forget_under(&nested);
        assert!(index.get(&asset_id(&shared)).is_none());
        assert_eq!(index.entries.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_kind_allow_list_is_normalized_and_an_absent_one_filters_nothing() {
        assert!(asset_kind_filter(None).is_empty());
        assert!(asset_kind_filter(Some("")).is_empty());
        // A trailing comma is a client's join, not a kind.
        assert!(asset_kind_filter(Some(",, ,")).is_empty());
        assert_eq!(
            asset_kind_filter(Some("image")),
            vec![String::from("image")]
        );
        assert_eq!(
            asset_kind_filter(Some(" Markdown , PDF ")),
            vec![String::from("markdown"), String::from("pdf")]
        );
        // A kind outside the taxonomy is carried as asked and simply matches
        // nothing, the way an unknown name in the events allow-list does.
        assert_eq!(
            asset_kind_filter(Some("document")),
            vec![String::from("document")]
        );
    }

    /// The listing's own state, pointed at a socket that is not there: no roots
    /// come back and none are remembered, so nothing rescans and the entries
    /// under test stay as they were put in. Their mtimes are what the ordering
    /// is asserted on, which a run of files written in the same millisecond
    /// could not give.
    fn asset_listing_state(root: &FsPath, entries: Vec<AssetEntry>) -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path =
            root.join("herdr.sock").to_string_lossy().to_string();
        {
            let mut index = state.assets.lock().unwrap();
            for entry in entries {
                index.upsert(entry);
            }
        }
        state
    }

    fn asset_listing_query(kind: Option<&str>, limit: usize) -> AssetsQuery {
        AssetsQuery {
            since: None,
            limit: Some(limit),
            kind: kind.map(str::to_owned),
            path: None,
        }
    }

    async fn listed_asset_names(state: &AppState, kind: Option<&str>, limit: usize) -> Vec<String> {
        // `test_asset_entry` puts every fixture in workspace "wA".
        let response = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(kind, limit)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        response.0["data"]["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_kind_filter_answers_the_newest_of_that_kind_not_the_kind_among_the_newest() {
        // The shape that made the filter necessary: an agent editing source code
        // writes files faster than it writes artifacts, so the image and the
        // documents sit well behind the newest page.
        let root = asset_test_dir("kind-listing");
        std::fs::write(root.join("chart.png"), png_bytes()).unwrap();
        std::fs::write(root.join("notes.md"), b"# notes\n").unwrap();
        std::fs::write(root.join("report.pdf"), b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n").unwrap();
        for index in 0..3 {
            std::fs::write(root.join(format!("mod{index}.rs")), b"fn main() {}\n").unwrap();
        }
        let state = asset_listing_state(
            &root,
            vec![
                test_asset_entry(&root.join("chart.png"), &root, 1_000),
                test_asset_entry(&root.join("notes.md"), &root, 2_000),
                test_asset_entry(&root.join("report.pdf"), &root, 3_000),
                test_asset_entry(&root.join("mod0.rs"), &root, 4_000),
                test_asset_entry(&root.join("mod1.rs"), &root, 5_000),
                test_asset_entry(&root.join("mod2.rs"), &root, 6_000),
            ],
        );

        // No kind is the old answer exactly: the newest files, whatever they are.
        assert_eq!(
            listed_asset_names(&state, None, 3).await,
            vec![
                String::from("mod2.rs"),
                String::from("mod1.rs"),
                String::from("mod0.rs")
            ]
        );
        assert_eq!(listed_asset_names(&state, Some(""), 3).await.len(), 3);

        // The image is the fourth-oldest file of six, so a page of three would
        // never have shown it. Filtering during the scan is what finds it.
        assert_eq!(
            listed_asset_names(&state, Some("image"), 3).await,
            vec![String::from("chart.png")]
        );
        // One request for a client filter that spans two kinds, still newest
        // first across both.
        assert_eq!(
            listed_asset_names(&state, Some("markdown,pdf"), 3).await,
            vec![String::from("report.pdf"), String::from("notes.md")]
        );
        // The page is still cut to the limit, and cut from the matches.
        assert_eq!(
            listed_asset_names(&state, Some("text"), 2).await,
            vec![String::from("mod2.rs"), String::from("mod1.rs")]
        );
        // A kind the gateway does not have matches nothing rather than erroring
        // or quietly widening back to everything.
        assert!(listed_asset_names(&state, Some("document"), 3)
            .await
            .is_empty());

        // The applied allow-list comes back, so a client can tell this gateway
        // from one old enough to have ignored the parameter.
        let response = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(Some("Image"), 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(response.0["data"]["kind"], json!(["image"]));
        assert_eq!(response.0["data"]["assets"][0]["kind"], "image");
        let unfiltered = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(unfiltered.0["data"]["kind"], json!([]));

        // A different workspace in the same session sees none of it: the
        // scope this fix adds is the workspace, not the session, and this is
        // the reported defect in miniature -- another workspace's files never
        // showing up in this one's listing.
        let other_workspace = session_assets(
            State(state.clone()),
            Path(("default".into(), "wB".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert!(other_workspace.0["data"]["assets"]
            .as_array()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_page_is_filled_from_the_matches_and_sniffs_no_further_than_it_has_to() {
        let root = asset_test_dir("kind-page");
        std::fs::write(root.join("a.png"), png_bytes()).unwrap();
        std::fs::write(root.join("b.png"), png_bytes()).unwrap();
        std::fs::write(root.join("c.txt"), b"plain\n").unwrap();
        let ordered = vec![
            test_asset_entry(&root.join("a.png"), &root, 3_000),
            test_asset_entry(&root.join("b.png"), &root, 2_000),
            test_asset_entry(&root.join("c.txt"), &root, 1_000),
        ];

        let names = |page: Vec<Value>| -> Vec<String> {
            page.iter()
                .map(|asset| asset["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            names(asset_page(ordered.clone(), &[], 2)),
            vec![String::from("a.png"), String::from("b.png")]
        );
        assert_eq!(
            names(asset_page(ordered.clone(), &[String::from("image")], 1)),
            vec![String::from("a.png")]
        );
        // A name that lies is not what the filter goes on: the kind is the one
        // sniffed from the bytes, the same one the asset carries on the wire.
        std::fs::write(root.join("a.png"), b"not an image at all\n").unwrap();
        assert_eq!(
            names(asset_page(ordered, &[String::from("image")], 2)),
            vec![String::from("b.png")]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_exact_path_lookup_reaches_an_upload_only_through_the_uploads_root() {
        // The path a client is guaranteed to hold for an upload is the one the
        // upload response returned -- the gateway's own directory, never under
        // a pane's cwd. Without the fold-in, that exact path was the one
        // lookup that could never answer.
        let base = asset_test_dir("uploads-by-path");
        let uploads = base.join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        let stored = uploads.join("9126cf50.webp");
        std::fs::write(&stored, b"fake webp bytes").unwrap();
        let pane_roots = vec![AssetRoot {
            path: base.join("workspace"),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        }];

        // Pane roots alone miss it; the composed lookup roots answer it.
        assert!(asset_entry_for_path(&stored.to_string_lossy(), &pane_roots).is_none());
        let composed = with_uploads_root(pane_roots.clone(), "default", Some(uploads.clone()));
        let found = asset_entry_for_path(&stored.to_string_lossy(), &composed).unwrap();
        assert_eq!(found.path, stored);
        assert_eq!(found.session_id, "default");

        // No uploads directory resolved leaves the roots untouched.
        assert_eq!(
            with_uploads_root(pane_roots.clone(), "default", None).len(),
            1
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_exact_path_lookup_answers_one_asset_or_none_and_never_leaves_the_roots() {
        let base = asset_test_dir("lookup");
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(workspace.join("a/b/c/d/e/f")).unwrap();
        std::fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();
        // Deeper than the scan goes, and inside a directory the scan skips: the
        // user pointed at these, so an exact lookup still resolves them.
        let deep = workspace.join("a/b/c/d/e/f/deep.txt");
        std::fs::write(&deep, b"deep\n").unwrap();
        let skipped = workspace.join("node_modules/pkg/index.js");
        std::fs::write(&skipped, b"module\n").unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"secret\n").unwrap();
        let roots = vec![AssetRoot {
            path: workspace.clone(),
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: Some("wA:t1".into()),
            pane_id: Some("wA:p1".into()),
        }];

        let found = asset_entry_for_path(&report.to_string_lossy(), &roots).unwrap();
        assert_eq!(found.path, report);
        assert_eq!(found.id, asset_id(&report));
        assert_eq!(found.name, "report.md");
        assert_eq!(found.workspace_id.as_deref(), Some("wA"));
        assert!(asset_entry_for_path(&deep.to_string_lossy(), &roots).is_some());
        assert!(asset_entry_for_path(&skipped.to_string_lossy(), &roots).is_some());

        // The fence still holds, and a fenced-out path is simply a miss.
        assert!(asset_entry_for_path(&secret.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(
            &workspace.join("../outside/secret.txt").to_string_lossy(),
            &roots
        )
        .is_none());
        assert!(asset_entry_for_path("/etc/hosts", &roots).is_none());
        assert!(asset_entry_for_path(&workspace.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(&workspace.join("a").to_string_lossy(), &roots).is_none());
        assert!(
            asset_entry_for_path(&workspace.join("gone.md").to_string_lossy(), &roots).is_none()
        );
        assert!(asset_entry_for_path("report.md", &roots).is_none());
        assert!(asset_entry_for_path(&report.to_string_lossy(), &[]).is_none());

        // Nested roots: the deepest one owns the file it contains.
        let nested = AssetRoot {
            path: workspace.join("a"),
            session_id: "default".into(),
            workspace_id: Some("wB".into()),
            tab_id: Some("wB:t1".into()),
            pane_id: Some("wB:p1".into()),
        };
        let mut both = roots.clone();
        both.push(nested);
        let owned = asset_entry_for_path(&deep.to_string_lossy(), &both).unwrap();
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn asset_ids_are_stable_per_path_and_carry_no_path() {
        let first = asset_id(FsPath::new("/tmp/workspace/report.md"));
        assert_eq!(first, asset_id(FsPath::new("/tmp/workspace/report.md")));
        assert_ne!(first, asset_id(FsPath::new("/tmp/workspace/report2.md")));
        assert!(first.starts_with("as_"));
        assert!(!first.contains("report"));
        assert!(first[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn workspace_roots_come_from_pane_cwds_and_never_widen_to_the_whole_machine() {
        let home = dirs::home_dir().unwrap();
        let response = json!({
            "result": { "panes": [
                { "pane_id": "wA:p1", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                // A second pane in the same directory is the same root.
                { "pane_id": "wA:p2", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                { "pane_id": "wB:p1", "workspace_id": "wB", "tab_id": "wB:t1", "foreground_cwd": "/Users/okk/.ws/api" },
                { "pane_id": "wC:p1", "workspace_id": "wC", "cwd": "/" },
                { "pane_id": "wD:p1", "workspace_id": "wD", "cwd": home.to_string_lossy() },
                { "pane_id": "wE:p1", "workspace_id": "wE" }
            ] }
        });
        let roots = pane_list_roots("default", &response);
        assert_eq!(
            roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![
                String::from("/Users/okk/.repos/muqun"),
                String::from("/Users/okk/.ws/api")
            ]
        );
        assert_eq!(roots[0].pane_id.as_deref(), Some("wA:p1"));
        assert_eq!(roots[0].tab_id.as_deref(), Some("wA:t1"));
        assert_eq!(roots[1].workspace_id.as_deref(), Some("wB"));
        assert_eq!(roots[1].tab_id.as_deref(), Some("wB:t1"));
        assert!(pane_list_roots("default", &json!({ "result": {} })).is_empty());
    }

    #[test]
    fn asset_scope_narrows_to_a_tab_or_to_a_workspace_and_never_to_the_other() {
        // The invariant card #802's tab-scoping stands on: a `Tab` scope only
        // ever matches its own tmux window, and a `Workspace` scope (what a
        // herdr session resolves its tab to) matches every tab inside that
        // workspace -- so a herdr workspace with more than one tab keeps
        // seeing all of them, exactly as it did before tabs existed here.
        let root_in_tab_a = AssetRoot {
            path: PathBuf::from("/work/a"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t1".into()),
            pane_id: Some("wM:t1:p1".into()),
        };
        let root_in_tab_b = AssetRoot {
            path: PathBuf::from("/work/b"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t2".into()),
            pane_id: Some("wM:t2:p1".into()),
        };
        let root_elsewhere = AssetRoot {
            path: PathBuf::from("/work/c"),
            session_id: "default".into(),
            workspace_id: Some("wN".into()),
            tab_id: Some("wN:t1".into()),
            pane_id: Some("wN:t1:p1".into()),
        };

        let tab_scope = AssetScope::Tab("wM:t1".into());
        assert!(tab_scope.matches_root(&root_in_tab_a));
        assert!(!tab_scope.matches_root(&root_in_tab_b));
        assert!(!tab_scope.matches_root(&root_elsewhere));

        // A herdr session's tab id resolves to its workspace before scoping,
        // so both of that workspace's tabs match -- this is the "herdr must
        // not narrow" guarantee, proven at the layer that actually filters.
        let workspace_scope = AssetScope::Workspace("wM".into());
        assert!(workspace_scope.matches_root(&root_in_tab_a));
        assert!(workspace_scope.matches_root(&root_in_tab_b));
        assert!(!workspace_scope.matches_root(&root_elsewhere));
    }

    #[tokio::test]
    async fn a_tmux_session_scopes_by_tab_directly_and_a_herdr_session_resolves_it_to_a_workspace()
    {
        // tmux: the tab id is used verbatim, no lookup involved.
        let mut session = test_config("token").sessions[0].clone();
        session.backend = BackendKind::Tmux;
        assert_eq!(
            resolve_asset_scope(&session, "@3").await,
            AssetScope::Tab("@3".into())
        );

        // herdr: with no live socket to ask (as in every other test in this
        // file), the tab id can't be translated to its owning workspace, so
        // it is kept as-is rather than silently widened to "no scope at all".
        // This is also exactly what makes every pre-existing
        // `session_assets(Path(("default", "wA")))` test call in this file
        // keep behaving as a workspace-scoped call after this change: they
        // never had a live socket either.
        let mut herdr_session = test_config("token").sessions[0].clone();
        herdr_session.backend = BackendKind::Herdr;
        herdr_session.socket_path = "/tmp/herdr-does-not-exist.sock".into();
        assert_eq!(
            resolve_asset_scope(&herdr_session, "wA").await,
            AssetScope::Workspace("wA".into())
        );
    }

    #[test]
    fn worktree_events_name_a_root_to_scan_and_never_a_file() {
        // The payload Herdr actually sends on protocol 17: the worktree's
        // checkout path and its workspace, with no file information at all.
        let created = json!({
            "event": "worktree_created",
            "data": {
                "type": "worktree_created",
                "workspace": { "workspace_id": "wM", "number": 3, "label": "muqun" },
                "worktree": {
                    "path": "/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463",
                    "branch": "wip/live-activity",
                    "is_bare": false,
                    "is_detached": false,
                    "is_prunable": false,
                    "is_linked_worktree": true,
                    "label": "muqun"
                }
            }
        })
        .to_string();
        let root = worktree_event_root("default", &created).unwrap();
        assert_eq!(
            root.path,
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463")
        );
        assert_eq!(root.workspace_id.as_deref(), Some("wM"));
        assert_eq!(root.session_id, "default");

        let opened = json!({
            "event": "worktree_opened",
            "data": { "type": "worktree_opened", "already_open": false,
                "workspace": { "workspace_id": "wZ" },
                "worktree": { "path": "/Users/okk/.repos/muqun", "label": "muqun" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_root("default", &opened).unwrap().path,
            PathBuf::from("/Users/okk/.repos/muqun")
        );

        let removed = json!({
            "event": "worktree_removed",
            "data": { "type": "worktree_removed", "forced": false, "workspace_id": "wZ",
                "worktree": { "path": "/Users/okk/.repos/muqun/.claude/worktrees/gone" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_removed_root(&removed).unwrap(),
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/gone")
        );
        assert!(worktree_event_root("default", &removed).is_none());

        // Everything else on the stream leaves the index alone.
        assert!(worktree_event_root("default", r#"{"event":"pane_updated","data":{}}"#).is_none());
        assert!(worktree_event_root("default", "not json").is_none());
        assert!(worktree_event_removed_root(r#"{"event":"pane_updated","data":{}}"#).is_none());
    }

    #[test]
    fn an_asset_envelope_is_versioned_and_declares_capabilities() {
        let root = PathBuf::from("/tmp/workspace");
        let entry = test_asset_entry(&root.join("report.md"), &root, 1_785_100_000_000);
        let envelope: Value = serde_json::from_str(&asset_created_payload(
            &entry,
            sniff_asset_type(b"# report\n", "report.md"),
        ))
        .unwrap();
        assert_eq!(envelope["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(envelope["capabilities"]["assets"], true);
        let asset = &envelope["data"]["asset"];
        assert_eq!(asset["id"], entry.id);
        assert_eq!(asset["kind"], "markdown");
        assert_eq!(asset["mime"], "text/markdown; charset=utf-8");
        assert_eq!(asset["previewable"], true);
        assert_eq!(asset["origin"]["session_id"], "default");
        assert_eq!(asset["origin"]["workspace_id"], "wA");
        assert_eq!(asset["origin"]["pane_id"], "wA:p1");
        assert_eq!(asset["modified_unix_ms"], 1_785_100_000_000_u64);
    }

    #[test]
    fn the_content_envelope_declares_parts_at_the_version_that_added_them() {
        // One envelope and one version across the content model: a client reads
        // the version once and knows both endpoints answer it.
        let envelope = content_envelope(json!({}));
        assert_eq!(envelope["schema_version"], "1.4.0");
        assert_eq!(envelope["capabilities"]["parts"], true);
        assert_eq!(envelope["capabilities"]["assets"], true);
        assert_eq!(envelope["capabilities"]["image_upload"], true);
        assert_eq!(envelope["capabilities"]["composer"], true);
    }

    /// A native adapter answers `parts: "native"`, and only when it actually
    /// answered. The distinction matters: an operator who has not pointed the
    /// gateway at an opencode server still gets the dictionary, and a client
    /// must be able to tell "could have" from "did".
    #[test]
    fn a_pane_says_native_only_when_a_protocol_actually_answered() {
        let read = native::NativeRead {
            parts: Vec::new(),
            session: Some("ses_1".into()),
            version: Some("1.18.0".into()),
        };
        let native_pane = pane_capabilities(
            "wA:p1",
            Some("opencode"),
            parts::dictionary_for(Some("opencode")),
            Some(&read),
            None,
        );
        assert_eq!(native_pane["parts"], "native");
        assert_eq!(native_pane["native"]["protocol"], "opencode-server");
        assert_eq!(native_pane["native"]["version"], "1.18.0");
        assert_eq!(native_pane["native"]["session"], "ses_1");
        // The dictionary is still named, because it is still what answers when
        // the server is not up.
        assert_eq!(native_pane["dictionary"], "opencode");

        // Same agent, no endpoint reached: the pane falls back and says so.
        let fallback = pane_capabilities(
            "wA:p1",
            Some("opencode"),
            parts::dictionary_for(Some("opencode")),
            None,
            None,
        );
        assert_eq!(fallback["parts"], "dictionary");
        assert_eq!(fallback["native"], Value::Null);
    }

    /// One shape for both sources: the approval endpoints answer the same keys
    /// whether the request was read off a menu or reported by a protocol, and
    /// `pane.approvals` is the only thing that says which.
    #[test]
    fn a_reported_approval_answers_in_the_same_shape_a_drawn_one_does() {
        let pending = native::NativeApproval {
            adapter: &native::OPENCODE,
            base: "http://127.0.0.1:1".into(),
            session: "ses_1".into(),
            request: parts::ApprovalRequest {
                id: "per_1".into(),
                prompt: "Allow bash?".into(),
                tool: Some("bash".into()),
                context: vec!["echo hi".into()],
                options: vec![parts::ApprovalChoice {
                    index: 1,
                    label: "Approve".into(),
                    decision: "allow",
                }],
            },
        };
        let data = native_approval_data("default", "wM:p1", Some("opencode"), Some(&pending));
        assert_eq!(data["state"], "pending");
        assert_eq!(data["approval"]["approval_id"], "per_1");
        assert_eq!(data["approval"]["options"][0]["decision"], "allow");
        // The label is the gateway's own, so the command in `context` is the
        // only agent-authored text on this payload.
        assert_eq!(data["approval"]["options"][0]["label"], "Approve");
        assert_eq!(data["pane"]["approvals"], "protocol");

        let idle = native_approval_data("default", "wM:p1", Some("opencode"), None);
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["approval"], Value::Null);
        assert_eq!(idle["pane"]["approvals"], "protocol");
    }

    /// The drawn-menu spelling, which is what every existing assertion means.
    fn approval_data_menu(
        session_id: &str,
        pane_id: &str,
        agent: Option<&str>,
        approval: Option<&approvals::Approval>,
    ) -> Value {
        approval_data(session_id, pane_id, agent, approval, "menu")
    }

    #[test]
    fn a_pane_carries_a_composer_descriptor_only_for_an_agent_with_a_table() {
        let known = pane_capabilities(
            "wA:p1",
            Some("Claude Code"),
            parts::dictionary_for(Some("claude")),
            None,
            composer::descriptor(Some("Claude Code"), None),
        );
        assert_eq!(known["parts"], "dictionary");
        assert_eq!(known["composer"]["table"], "claude");
        assert_eq!(known["composer"]["file_mentions"], true);
        assert!(known["composer"]["slash_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "/compact" && entry["source"] == "builtin"));

        // An agent with no table carries no key at all -- not a null, which a
        // client would have to tell apart from "no commands".
        let unknown = pane_capabilities(
            "wA:p2",
            Some("aider"),
            parts::dictionary_for(Some("aider")),
            None,
            composer::descriptor(Some("aider"), None),
        );
        assert_eq!(unknown["parts"], "text");
        assert!(unknown.as_object().unwrap().get("composer").is_none());
    }

    /// The file search can only ever look in a root the asset API would also
    /// serve, because it takes its root from the same place: the pane cwds
    /// Herdr reports, filtered by the same "this is not the whole machine"
    /// rule. A pane id that is not in that list has no root to search.
    #[test]
    fn file_search_takes_its_root_from_the_panes_the_session_actually_has() {
        let roots = pane_list_roots(
            "default",
            &json!({ "result": { "panes": [
                { "pane_id": "wA:p1", "cwd": "/Users/dev/src/project", "workspace_id": "wA" },
                { "pane_id": "wA:p2", "cwd": "/" }
            ] } }),
        );
        let root_for = |pane: &str| {
            roots
                .iter()
                .find(|root| root.pane_id.as_deref() == Some(pane))
                .map(|root| root.path.clone())
        };
        assert_eq!(
            root_for("wA:p1"),
            Some(PathBuf::from("/Users/dev/src/project"))
        );
        // The pane sitting at the filesystem root never became a root, so the
        // search has nothing to look in rather than the whole machine.
        assert_eq!(root_for("wA:p2"), None);
        assert_eq!(root_for("wB:p9"), None);
    }

    #[test]
    fn a_file_search_limit_is_clamped_whatever_the_client_asks_for() {
        let clamp = |limit: Option<usize>| {
            limit
                .unwrap_or(composer::FILE_SEARCH_DEFAULT_LIMIT)
                .clamp(1, composer::FILE_SEARCH_MAX_LIMIT)
        };
        assert_eq!(clamp(None), 20);
        assert_eq!(clamp(Some(0)), 1);
        assert_eq!(clamp(Some(5)), 5);
        assert_eq!(clamp(Some(10_000)), 50);
    }

    #[test]
    fn an_asset_file_name_cannot_break_out_of_a_response_header() {
        assert_eq!(header_safe_name("report.md"), "report.md");
        assert_eq!(
            header_safe_name("re\"port\r\nX-Evil: 1.md"),
            "reportX-Evil 1.md"
        );
        assert_eq!(header_safe_name("../../etc/passwd"), "....etcpasswd");
        assert_eq!(header_safe_name("图片.png"), ".png");
        assert_eq!(header_safe_name("\u{202e}"), "asset");
    }

    #[test]
    fn openapi_spec_contains_docs_routes_and_auth() {
        let spec = openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        let output = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"];
        assert!(output.is_object());
        assert_eq!(output["parameters"][4]["name"], "start");
        assert_eq!(output["parameters"][5]["name"], "end");
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/zoom"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/events"].is_object());
        assert!(spec["paths"]["/api/pair/request"].is_object());
        assert!(spec["paths"]["/api/pair/claim"].is_object());
        assert!(spec["paths"]["/api/meta"].is_object());
        assert!(spec["paths"]["/api/devices/push-token"]["delete"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/workspaces/{workspaceId}"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/agents/{target}/send"].is_object());
        assert!(
            spec["paths"]["/api/uploads"]["post"]["requestBody"]["content"]["multipart/form-data"]
                .is_object()
        );
        assert!(spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"].is_object());
        let parts = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/parts"]["get"];
        assert!(parts.is_object());
        assert_eq!(parts["parameters"][2]["name"], "lines");
        let part = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["parts"]["items"];
        // A client dispatches on `type` and falls back on `fallback_text`, so
        // the spec has to require exactly those two of every part.
        assert_eq!(part["required"], json!(["type", "fallback_text"]));
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("tool-block")));
        // v2's one addition to the closed set. It has to be in the spec's enum
        // or a client has no way to learn the type exists without meeting one.
        assert!(part["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("approval")));
        assert!(part["properties"]["approval_id"].is_object());
        assert_eq!(
            part["properties"]["options"]["items"]["required"],
            json!(["index", "label", "decision"])
        );
        // Which source answered is a value of an enum that already had two.
        let pane = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"];
        assert_eq!(
            pane["parts"]["enum"],
            json!(["native", "dictionary", "text"])
        );
        assert!(pane["native"].is_object());
        assert_eq!(
            parts["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["data"]["properties"]["source"]["enum"],
            json!(["recent-unwrapped", "native"])
        );
        // The composer descriptor rides on the pane, and its source vocabulary
        // is closed the same way the part types are.
        let composer = &parts["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["data"]["properties"]["pane"]["properties"]["composer"];
        assert_eq!(
            composer["properties"]["slash_commands"]["items"]["properties"]["source"]["enum"],
            json!(["builtin", "workspace"])
        );
        let files = &spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/files"]["get"];
        assert!(files.is_object());
        assert_eq!(files["parameters"][2]["name"], "query");
        assert_eq!(files["parameters"][3]["name"], "limit");
        assert_eq!(
            files["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["schema_version"]["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/tabs/{tabId}/assets"]["get"]["responses"]
                ["200"]["content"]["application/json"]["schema"]["properties"]["schema_version"]
                ["const"],
            CONTENT_SCHEMA_VERSION
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["415"].is_object()
        );
        assert!(
            spec["paths"]["/api/assets/{assetId}/content"]["get"]["responses"]["413"].is_object()
        );
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["413"].is_object());
        assert!(spec["paths"]["/api/uploads"]["post"]["responses"]["415"].is_object());
        assert_eq!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"]["get"]["parameters"]
                [6]["name"],
            "format"
        );

        // Task dispatch, including the partial answer, which a client that only
        // handles 200 and "error" would silently mishandle.
        let tasks = &spec["paths"]["/api/sessions/{sessionId}/tasks"]["post"];
        assert!(tasks.is_object());
        assert!(tasks["responses"]["207"].is_object());
        assert!(tasks["responses"]["403"].is_object());
        assert_eq!(
            tasks["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["repo_path", "agent"])
        );
        assert!(spec["paths"]["/api/agents/catalog"]["get"].is_object());
    }

    #[test]
    fn the_api_version_and_capabilities_announce_task_dispatch() {
        // A minor bump: the routes are additive, so an older client keeps
        // working, and a newer one can gate on the capability rather than on
        // probing for a 404.
        assert!(GATEWAY_API_VERSION.starts_with("1.7."));
        assert_eq!(GATEWAY_API_MAJOR, 1);
        assert!(API_CAPABILITIES.contains(&"tasks"));
        assert!(API_CAPABILITIES.contains(&"agent_catalog"));
        assert!(API_CAPABILITIES.contains(&"terminal_backends"));
        assert!(API_CAPABILITIES.contains(&"multiple_terminal_backends"));
    }

    #[test]
    fn the_digest_endpoint_is_documented_and_announced() {
        let spec = openapi_spec();
        let events = &spec["paths"]["/api/sessions/{sessionId}/agent-events"]["get"];
        assert!(events.is_object());
        let names: Vec<&str> = events["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sessionId", "since"]);
        // A client must be able to tell a gateway that keeps this from one old
        // enough to answer 404, without probing for the 404.
        assert!(API_CAPABILITIES.contains(&"agent_events"));
    }

    #[test]
    fn a_config_without_agent_commands_still_loads_and_round_trips_unchanged() {
        // Every gateway already in the field has a config.json written before
        // this field existed. Reading one must not fail, and rewriting one must
        // not add noise to it.
        let existing = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }]
        });
        let config: Config = serde_json::from_value(existing.clone()).unwrap();
        assert!(config.agent_commands.is_empty());
        assert_eq!(serde_json::to_value(&config).unwrap(), existing);

        let with_override = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }],
            "agent_commands": { "claude": "claude-canary" }
        });
        let config: Config = serde_json::from_value(with_override).unwrap();
        assert_eq!(
            config.agent_commands.get("claude").map(String::as_str),
            Some("claude-canary")
        );
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

    #[test]
    fn tmux_sessions_sort_ahead_of_herdr_whatever_order_they_were_added() {
        let mut config = test_config("token");
        config.sessions.clear();
        upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
        upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
        // Neither `GET /api/sessions` nor `gateway_metadata`'s "primary"
        // session for `/health` and `/api/meta` serve in this order any more
        // -- both reorder by liveness on every request (see
        // `session_order_key` / `ordered_sessions`). This stable storage
        // order is what's left to matter: the order `configure_backend list`
        // prints. Keeping tmux first there too is the harmless,
        // previously-established default, not a competing ordering rule for
        // the app.
        assert_eq!(config.sessions[0].backend, BackendKind::Tmux);
        assert_eq!(config.sessions[1].backend, BackendKind::Herdr);
    }

    fn liveness_session(id: &str, backend: BackendKind) -> SessionConfig {
        SessionConfig {
            id: id.into(),
            label: id.into(),
            socket_path: String::new(),
            backend,
        }
    }

    #[test]
    fn liveness_outranks_backend_kind_in_the_sessions_ordering_key() {
        use SessionLiveness::{Empty, HasPanes, Unreachable};
        // A reachable-and-populated herdr session outranks an empty or dead
        // tmux one -- liveness is the significant digit, backend kind is only
        // the tiebreaker.
        // Index 0 is the preferred entry; even so, being down loses to a
        // live one further down the list.
        assert!(session_order_key(1, HasPanes) < session_order_key(0, Empty));
        assert!(session_order_key(1, HasPanes) < session_order_key(0, Unreachable));
        assert!(session_order_key(1, Empty) < session_order_key(0, Unreachable));
    }

    /// The behaviour `muqun-gateway backend default` promises, and did not
    /// have: whichever entry the reader put first wins a tie, whatever kind of
    /// backend it is. Replaces a test that asserted tmux always won, which is
    /// what made the command a no-op.
    #[test]
    fn the_configured_order_breaks_ties_at_every_liveness_level() {
        for liveness in [
            SessionLiveness::HasPanes,
            SessionLiveness::Empty,
            SessionLiveness::Unreachable,
        ] {
            assert!(
                session_order_key(0, liveness) < session_order_key(1, liveness),
                "the first configured session should win a tie when both are {liveness:?}"
            );
        }
    }

    /// Sorting a full session list by the key, including every configured
    /// session staying present when none of them are reachable -- ordering
    /// must never turn into filtering.
    #[test]
    fn sorting_by_the_key_orders_without_dropping_anyone() {
        let sessions = [
            liveness_session("herdr-empty", BackendKind::Herdr),
            liveness_session("tmux-dead", BackendKind::Tmux),
            liveness_session("herdr-live", BackendKind::Herdr),
            liveness_session("tmux-empty", BackendKind::Tmux),
        ];
        let liveness = [
            SessionLiveness::Empty,
            SessionLiveness::Unreachable,
            SessionLiveness::HasPanes,
            SessionLiveness::Empty,
        ];
        let mut order: Vec<usize> = (0..sessions.len()).collect();
        order.sort_by_key(|&index| session_order_key(index, liveness[index]));
        let ids: Vec<&str> = order
            .iter()
            .map(|&index| sessions[index].id.as_str())
            .collect();
        // Liveness first; among the two equally-empty entries the one
        // configured earlier wins, which is the reader's stated preference
        // rather than a rule about backend kinds.
        assert_eq!(
            ids,
            vec!["herdr-live", "herdr-empty", "tmux-empty", "tmux-dead"]
        );
        assert_eq!(
            order.len(),
            sessions.len(),
            "every session stays in the list"
        );
    }

    /// All-unreachable is the case that matters most: a client reading only
    /// `sessions[0]` must still get a session back, not `undefined`.
    #[test]
    fn every_session_survives_when_all_are_unreachable() {
        let sessions = [
            liveness_session("a", BackendKind::Herdr),
            liveness_session("b", BackendKind::Tmux),
            liveness_session("c", BackendKind::Herdr),
        ];
        let mut order: Vec<usize> = (0..sessions.len()).collect();
        order.sort_by_key(|&index| session_order_key(index, SessionLiveness::Unreachable));
        assert_eq!(order.len(), 3);
        // All equally unreachable, so the configured order stands -- ordering
        // must never turn into filtering, and it must not reshuffle either.
        let ids: Vec<&str> = order
            .iter()
            .map(|&index| sessions[index].id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn session_liveness_reports_panes_present() {
        let panes = vec![Pane {
            id: BackendPaneId::new("p1"),
            terminal_id: None,
            workspace_id: BackendWorkspaceId::new("w1"),
            tab_id: BackendTabId::new("t1"),
            label: None,
            terminal_title: None,
            cwd: None,
            focused: false,
            width: None,
            height: None,
            revision: None,
            foreground_command: None,
            agent: None,
            agent_status: BackendAgentStatus::Unknown,
            max_offset_from_bottom: None,
            viewport_rows: None,
            alternate_on: None,
        }];
        let outcome = session_liveness(
            Box::pin(async move { Ok(panes) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::HasPanes);
    }

    #[tokio::test]
    async fn session_liveness_reports_reachable_but_empty() {
        let outcome = session_liveness(
            Box::pin(async { Ok(Vec::new()) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Empty);
    }

    #[tokio::test]
    async fn session_liveness_reports_unreachable_on_a_backend_error() {
        let outcome = session_liveness(
            Box::pin(async { Err(BackendError::Unavailable) }),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Unreachable);
    }

    /// The case a failed connect does not cover: a backend that accepts and
    /// then never answers must not hang `GET /api/sessions` -- it has to be
    /// discovered by timeout.
    #[tokio::test]
    async fn session_liveness_reports_unreachable_on_timeout_without_waiting_for_the_probe() {
        let outcome = session_liveness(
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(Vec::new())
            }),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(outcome, SessionLiveness::Unreachable);
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
    fn a_renamed_standalone_dir_migrates_once_and_never_again() {
        let parent = std::env::temp_dir().join(format!("gateway-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
        std::fs::create_dir_all(&old_dir).unwrap();
        // A real, parseable config so the pre-migration liveness check has a
        // port to look at -- this is the ordinary case: an install whose old
        // gateway has already been stopped, so nothing is listening on that
        // port under the old name and the migration proceeds. (The refusal
        // path -- a process actually found listening -- is not exercised
        // here: this file does not spawn real OS processes to unit-test
        // process introspection anywhere else either, and that path was
        // verified operationally when this fix was written, against a
        // gateway genuinely still running under the pre-rename name.)
        std::fs::write(
            old_dir.join(CONFIG_FILE),
            serde_json::to_vec(&test_config("migration-token")).unwrap(),
        )
        .unwrap();

        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!old_dir.exists());
        assert!(migrated.join(CONFIG_FILE).exists());

        // A directory recreated under the old name afterward is left alone:
        // once the new name exists, migration never looks at the old one again.
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("marker"), b"should not move").unwrap();
        let migrated_again = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated_again, migrated);
        assert!(old_dir.join("marker").exists());
        assert!(!migrated.join("marker").exists());

        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn no_old_dir_and_no_new_dir_migrates_nothing() {
        let parent =
            std::env::temp_dir().join(format!("gateway-rename-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!migrated.exists());
        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn an_unparseable_old_config_does_not_block_migration() {
        // No port can be read from this, so the liveness check has nothing to
        // ask about and degrades to "proceed" rather than "refuse forever" --
        // an install should not be permanently stuck migrating because one
        // file did not parse.
        let parent = std::env::temp_dir().join(format!(
            "gateway-rename-unparseable-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join(CONFIG_FILE), b"{}").unwrap();

        let migrated = migrate_renamed_standalone_dir(&parent).unwrap();
        assert_eq!(migrated, parent.join("muqun-gateway"));
        assert!(!old_dir.exists());

        std::fs::remove_dir_all(&parent).ok();
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
    fn transport_metadata_distinguishes_tls_tailscale_and_plain_http() {
        let mut config = test_config("token");
        config.public_url = "https://host.tailnet.ts.net".into();
        config.listen = "127.0.0.1:23847".into();
        assert_eq!(transport_protection(&config), "https");

        config.public_url = "http://host.tailnet.ts.net:23847".into();
        config.listen = "100.118.124.50:23847".into();
        assert_eq!(transport_protection(&config), "tailscale-wireguard");

        config.listen = "0.0.0.0:23847".into();
        assert_eq!(transport_protection(&config), "unencrypted-http");
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
        assert_eq!(merged.sessions[0].backend, BackendKind::Tmux);
        assert_eq!(merged.sessions[1].backend, BackendKind::Herdr);
        assert!(target_config_dir.join(HERDR_PLUGIN_IMPORT_MARKER).exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_run_that_got_part_of_the_way_answers_207_with_the_same_body() {
        let payload = json!({ "workspace_id": "ws-1", "pane_id": "pane-1" });

        let mut steps = tasks::StepLog::new();
        steps.ok("worktree", json!({ "path": "/tmp/wt" }));
        steps.ok("workspace", json!({ "workspace_id": "ws-1" }));
        steps.ok("agent", json!({ "kind": "claude" }));
        steps.skipped("prompt", "no prompt was given");
        assert_eq!(
            task_partial(payload.clone(), &steps).status(),
            StatusCode::OK,
            "a skipped step is not a failure"
        );

        steps.failed("prompt", "herdr_error", "pane vanished");
        assert_eq!(
            task_partial(payload, &steps).status(),
            StatusCode::MULTI_STATUS
        );
    }

    #[test]
    fn nothing_created_is_an_error_whose_status_says_whose_fault_it_was() {
        let steps = tasks::StepLog::new();
        // Herdr refusing is the request being wrong.
        let refused = HerdrCallError::Herdr {
            method: "worktree.create".into(),
            error: json!({ "code": "not_a_repo", "message": "not a git repository" }),
        };
        assert_eq!(refused.code(), "not_a_repo");
        assert!(refused.message().contains("worktree.create"));
        assert_eq!(
            task_failure(refused, &steps).status(),
            StatusCode::BAD_REQUEST
        );

        // The socket being down, or Herdr answering off-schema, is not.
        assert_eq!(
            task_failure(
                HerdrCallError::Unavailable("Herdr is unavailable".into()),
                &steps
            )
            .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            task_failure(HerdrCallError::malformed("workspace.create"), &steps).status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            HerdrCallError::malformed("workspace.create").code(),
            "invalid_herdr_response"
        );
    }

    #[test]
    fn repo_roots_come_from_the_repos_this_session_has_and_never_widen_to_the_machine() {
        // The gathering half of task_repo_roots, which is the part that decides
        // what the fence lets through. A workspace names its repo root as well
        // as its checkout, which is why a pane sitting deep inside a repo still
        // lets the repo's top level be branched from.
        let workspaces = json!({
            "id": "1",
            "result": { "type": "workspace_list", "workspaces": [
                { "workspace_id": "ws-1", "worktree": {
                    "repo_key": "k", "repo_name": "muqun",
                    "repo_root": "/Users/dev/code/muqun",
                    "checkout_path": "/Users/dev/code/muqun-task",
                    "is_linked_worktree": true } },
                { "workspace_id": "ws-2" },
                { "workspace_id": "ws-3", "worktree": {
                    "repo_key": "k2", "repo_name": "home", "repo_root": "/",
                    "checkout_path": "/", "is_linked_worktree": false } }
            ] }
        });
        let mut found: Vec<String> = Vec::new();
        for workspace in workspaces["result"]["workspaces"].as_array().unwrap() {
            for key in ["repo_root", "checkout_path"] {
                if let Some(path) = workspace
                    .pointer(&format!("/worktree/{key}"))
                    .and_then(Value::as_str)
                {
                    if is_scannable_root(FsPath::new(path)) {
                        found.push(path.to_owned());
                    }
                }
            }
        }
        assert_eq!(
            found,
            vec!["/Users/dev/code/muqun", "/Users/dev/code/muqun-task"]
        );
        // A workspace with no worktree contributes nothing, and "/" is refused
        // by the same guard the asset roots use.
        assert!(!found.iter().any(|path| path == "/"));
    }

    /// A state whose Herdr socket cannot answer, which is how a test reaches
    /// the checks that happen before anything is created.
    fn unreachable_state() -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path = std::env::temp_dir()
            .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        state
    }

    /// A Herdr socket that answers `pane.list` with a fixed set of panes (or
    /// none), for driving the real `HerdrBackend` -> `list_panes` path that
    /// `sessions()` probes end to end, without needing Herdr installed or a
    /// live tmux server.
    struct FakePaneListHerdr {
        socket_path: PathBuf,
    }

    impl FakePaneListHerdr {
        fn start(panes: Value) -> Self {
            let socket_path = std::env::temp_dir().join(format!(
                "herdr-panelist-{}.sock",
                uuid::Uuid::new_v4().simple()
            ));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap_or_default();
                    let response = json!({
                        "id": request["id"],
                        "result": { "panes": panes.clone() }
                    })
                    .to_string();
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(b"\n").await;
                    let _ = stream.flush().await;
                }
            });
            Self { socket_path }
        }

        fn session(&self, id: &str) -> SessionConfig {
            SessionConfig {
                id: id.into(),
                label: id.into(),
                socket_path: self.socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Herdr,
            }
        }
    }

    fn session_ids(response: &Value) -> Vec<String> {
        response["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// End to end through the real handler: a herdr session that actually has
    /// a pane outranks a tmux session configured but not running -- the
    /// motivating regression for this card, where the app reads
    /// `sessions[0]` and the old static tmux-first order pointed it at the
    /// dead backend.
    #[tokio::test]
    async fn sessions_endpoint_puts_a_live_backend_ahead_of_a_configured_but_dead_one() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            herdr.session("herdr-live"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["herdr-live", "tmux-dead"]);
    }

    /// The dual-backend defect the final review caught: with tmux dead (or
    /// merely empty) and herdr live, the app connects to whichever session
    /// `GET /api/sessions` leads with -- but it validates that connection
    /// against the metadata `/health`/`/api/meta` describe as "primary".
    /// Before this fix `gateway_metadata` always read
    /// `config.sessions.first()`, which is stored order (tmux-first) and
    /// ignores liveness entirely, so it kept describing the dead tmux
    /// session -- whose `session_metadata` hardcodes `compatible: true` --
    /// while the app was actually talking to herdr. The version fence
    /// `assertSupportedHerdr` exists to enforce was defeated for exactly this
    /// configuration. `gateway_metadata`'s primary must agree with
    /// `sessions()`'s first entry.
    #[tokio::test]
    async fn gateway_metadata_primary_agrees_with_the_sessions_endpoint() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            herdr.session("herdr-live"),
        ];

        let metadata = gateway_metadata(&state).await.unwrap();
        assert_eq!(metadata["backend"]["sessionId"], "herdr-live");
        assert_eq!(metadata["backend"]["kind"], json!(BackendKind::Herdr));
    }

    /// Ordering must never turn into filtering: every configured session is
    /// still in the response when none of them are reachable, so a client
    /// reading `sessions[0]` finds a session object instead of `undefined`.
    #[tokio::test]
    async fn sessions_endpoint_keeps_every_session_when_nothing_is_reachable() {
        let mut state = unreachable_state();
        state.config.sessions.push(SessionConfig {
            id: "tmux-also-dead".into(),
            label: "tmux".into(),
            socket_path: std::env::temp_dir()
                .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            backend: BackendKind::Tmux,
        });
        let configured = state.config.sessions.len();
        let preferred = state.config.sessions[0].id.clone();

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        let ids = session_ids(&response.0);
        assert_eq!(ids.len(), configured, "no session drops out of the list");
        // Both are genuinely SessionLiveness::Unreachable here -- the herdr
        // entry via a refused connection, the tmux entry via
        // `probe_reachable` catching the same "no such file or directory"
        // that `list_output` would otherwise fold into an empty topology
        // (see `probe_reachable_tells_no_server_apart_from_list_panes_reporting_empty`
        // in `backend/tmux.rs`). So this genuinely exercises the tiebreak
        // between two unreachable entries, not an accident of tmux
        // misreporting as merely empty -- and the tiebreak is now the order
        // the reader configured, so the first entry stays first.
        assert_eq!(ids[0], preferred);
    }

    /// The regression `sessions_endpoint_keeps_every_session_when_nothing_is_reachable`
    /// could not have caught on its own: before `probe_reachable`, a tmux
    /// session pointed at a socket nothing is listening on classified as
    /// `SessionLiveness::Empty` (via `list_output`'s "no server is an empty
    /// topology" masking), not `Unreachable`. That happened to still sort
    /// tmux first against an `Unreachable` herdr entry -- Empty outranks
    /// Unreachable regardless of the tiebreak -- so the bug was invisible
    /// there. Pairing the dead tmux session with a *reachable-but-empty*
    /// herdr session instead exposes it directly: a herdr session that is
    /// genuinely `Empty` must outrank a tmux session that is genuinely
    /// `Unreachable`, which only holds if tmux's "no server" case is actually
    /// classified as `Unreachable` and not conflated with `Empty`.
    #[tokio::test]
    async fn sessions_endpoint_ranks_a_reachable_empty_backend_ahead_of_a_dead_tmux_one() {
        let empty_herdr = FakePaneListHerdr::start(json!([]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "tmux-dead".into(),
                label: "tmux".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("tmux-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Tmux,
            },
            empty_herdr.session("herdr-empty"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["herdr-empty", "tmux-dead"]);
    }

    /// A reachable session with no panes open still outranks an unreachable
    /// one, and still trails a session that actually has something in it.
    #[tokio::test]
    async fn sessions_endpoint_ranks_reachable_empty_between_live_and_dead() {
        let empty_herdr = FakePaneListHerdr::start(json!([]));
        let busy_herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1" }
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            empty_herdr.session("empty"),
            SessionConfig {
                id: "dead".into(),
                label: "dead".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Herdr,
            },
            busy_herdr.session("busy"),
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["busy", "empty", "dead"]);
    }

    /// Same regression as `sessions_endpoint_puts_a_live_backend_ahead_of_a_configured_but_dead_one`,
    /// but against a genuinely live tmux server instead of a fake -- on a
    /// private socket this test creates and owns, never the developer's
    /// default tmux server. Requires `tmux` on `PATH` and permission to
    /// create a Unix socket, so it is `--ignored` like the other isolated
    /// tmux contract tests.
    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn sessions_endpoint_puts_a_live_isolated_tmux_session_ahead_of_a_dead_herdr_one() {
        if tokio::process::Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .is_err()
        {
            eprintln!("skipping: no tmux on PATH");
            return;
        }
        let socket_path = std::env::temp_dir().join(format!(
            "gateway-sessions-live-{}.sock",
            uuid::Uuid::new_v4()
        ));
        let tmux = backend::TmuxBackend::new(Some(socket_path.clone()));
        let workspace = tmux
            .create_workspace(&BackendCreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-sessions-live".into()),
                focus: true,
            })
            .await
            .unwrap();

        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions = vec![
            SessionConfig {
                id: "herdr-dead".into(),
                label: "herdr".into(),
                socket_path: std::env::temp_dir()
                    .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
                    .to_string_lossy()
                    .into_owned(),
                backend: BackendKind::Herdr,
            },
            SessionConfig {
                id: "tmux-live".into(),
                label: "tmux".into(),
                socket_path: socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Tmux,
            },
        ];

        let response = sessions(State(state), bearer_headers("token"))
            .await
            .unwrap();
        assert_eq!(session_ids(&response.0), vec!["tmux-live", "herdr-dead"]);

        tmux.close_workspace(&workspace.id).await.unwrap();
    }

    fn spawn_body(agent: &str, cwd: Option<&str>) -> SpawnBody {
        SpawnBody {
            agent: agent.to_owned(),
            cwd: cwd.map(str::to_owned),
            tab_id: None,
            prompt: None,
        }
    }

    #[tokio::test]
    async fn a_spawn_is_refused_before_anything_is_created() {
        let state = unreachable_state();

        // An agent this gateway does not offer, answered in the reader's own
        // language and pointing at the list that would have said so.
        let refusal = spawn_agent(
            State(state.clone()),
            Path("default".into()),
            locale_headers("token", "zh-TW"),
            Json(spawn_body("definitely-not-an-agent", None)),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(error_body(&refusal)["error"]["code"], "unknown_agent");
        assert!(error_body(&refusal)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("GET /api/agents/catalog"));

        // A real agent, but a directory this session does not work in. The
        // socket is unreachable here, so the session has no roots at all --
        // which is exactly the case that must refuse rather than fall open.
        let refusal = spawn_agent(
            State(state.clone()),
            Path("default".into()),
            bearer_headers("token"),
            Json(spawn_body("claude", Some("/etc"))),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);
        assert_eq!(error_body(&refusal)["error"]["code"], "cwd_not_allowed");

        // And none of it is reachable without a paired device.
        assert_eq!(
            spawn_agent(
                State(state),
                Path("default".into()),
                bearer_headers("not-a-token"),
                Json(spawn_body("claude", None)),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn recent_cwds_lists_the_panes_directories_and_is_not_a_directory_browser() {
        // The picker for spawn. It answers with what the panes are already in
        // and nothing around it: a phone must not be able to walk the host from
        // here, and the list it can pick from is exactly the list `cwd` takes.
        let root = asset_test_dir("recent-cwds");
        let repo = root.join("repo");
        let plain = root.join("notes");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();

        let state = unreachable_state();
        state.assets.lock().unwrap().remember_roots(
            "default",
            None,
            vec![
                AssetRoot {
                    path: repo.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wA".into()),
                    tab_id: Some("wA:t1".into()),
                    pane_id: Some("wA:p1".into()),
                },
                AssetRoot {
                    path: plain.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wB".into()),
                    tab_id: Some("wB:t1".into()),
                    pane_id: Some("wB:p1".into()),
                },
            ],
        );

        let answer = recent_cwds(
            State(state.clone()),
            Path("default".into()),
            bearer_headers("token"),
        )
        .await
        .unwrap()
        .0;
        let cwds = answer["cwds"].as_array().unwrap();
        assert_eq!(cwds.len(), 2);
        assert_eq!(cwds[0]["path"], repo.to_string_lossy().as_ref());
        assert_eq!(cwds[0]["name"], "repo");
        assert_eq!(cwds[0]["pane_id"], "wA:p1");
        assert_eq!(cwds[0]["workspace_id"], "wA");
        // Which of them is a checkout, because "start an agent here" usually
        // means a repo.
        assert_eq!(cwds[0]["git"], true);
        assert_eq!(cwds[1]["git"], false);
        // Nothing above or below what the panes are in.
        assert!(!cwds
            .iter()
            .any(|entry| entry["path"] == root.to_string_lossy().as_ref()));

        assert_eq!(
            recent_cwds(
                State(state),
                Path("default".into()),
                bearer_headers("not-a-token"),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_blocked_push_says_nothing_the_agent_wrote_until_the_owner_asks_it_to() {
        let approval = approvals::detect(concat!(
            "Bash command\n",
            "\n",
            "  rm -rf build/\n",
            "\n",
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. Yes, and don't ask again for rm commands\n",
            "  3. No, and tell Claude what to do differently (esc)\n",
        ))
        .expect("the fixture draws a menu");

        let mut statuses = HashMap::new();
        let notice = notification_for_agent_status_event(
            &status_event("w1:p1", "claude", "blocked"),
            &mut statuses,
            "server-1",
            "Studio",
            "default",
        )
        .unwrap();

        // The default, and what every gateway sends until someone changes it:
        // that something needs answering, and never what.
        let plain = notice.render(Locale::En);
        assert_eq!(plain.title, "Agent blocked · Studio");
        assert_eq!(plain.body, "claude needs your input.");
        assert!(plain.data.get("question").is_none());
        assert!(!plain.body.contains("rm -rf"));

        // Opted in: the agent's own question, verbatim, plus the answers it is
        // offering. Not translated, because it is a quotation.
        let mut rich = notice.clone();
        rich.detail = Some(PushDetail::from_approval(&approval));
        let opted_in = rich.render(Locale::ZhTw);
        assert_eq!(opted_in.title, "代理程式等待中 · Studio");
        assert_eq!(opted_in.body, "Do you want to proceed?");
        assert_eq!(opted_in.data["question"], "Do you want to proceed?");
        let labels = opted_in.data["option_labels"].as_array().unwrap();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "Yes");

        // A question longer than a glance is cut, and a menu with more answers
        // than a notification row shows is cut too.
        let long = approvals::Approval {
            prompt: "x".repeat(400),
            options: (1..=6)
                .map(|index| approvals::ApprovalOption {
                    index,
                    label: "y".repeat(80),
                    selected: false,
                    decision: approvals::Decision::Allow,
                })
                .collect(),
            ..approval
        };
        let detail = PushDetail::from_approval(&long);
        // Cut, and visibly cut: the ellipsis is how a reader knows there is
        // more rather than believing they have read the whole question.
        assert!(detail
            .question
            .starts_with(&"x".repeat(MAX_PUSH_QUESTION_CHARS)));
        assert!(detail.question.ends_with("..."));
        assert_eq!(detail.question.chars().count(), MAX_PUSH_QUESTION_CHARS + 3);
        assert_eq!(detail.option_labels.len(), MAX_PUSH_OPTIONS);
        assert_eq!(
            detail.option_labels[0].chars().count(),
            MAX_PUSH_OPTION_CHARS + 3
        );
    }

    #[test]
    fn rich_pushes_are_off_until_a_config_says_otherwise() {
        // The one switch that puts terminal text on a lock screen. A config
        // written before it existed must read as off, and a gateway that has
        // not been told otherwise must not start saying more than it did.
        let config = test_config("admin");
        assert!(!config.rich_agent_pushes);

        let existing = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [{ "id": "default", "label": "Default", "socket_path": "/tmp/h.sock" }]
        });
        let parsed: Config = serde_json::from_value(existing.clone()).unwrap();
        assert!(!parsed.rich_agent_pushes);
        // And writing it back does not add the key, so an untouched config file
        // stays untouched.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), existing);

        let mut object = existing.as_object().cloned().unwrap();
        object.insert("rich_agent_pushes".into(), json!(true));
        let opted_in: Config = serde_json::from_value(Value::Object(object)).unwrap();
        assert!(opted_in.rich_agent_pushes);
        assert_eq!(
            serde_json::to_value(&opted_in).unwrap()["rich_agent_pushes"],
            json!(true)
        );
    }

    #[test]
    fn dispatch_and_stop_are_documented_and_announced() {
        let spec = openapi_spec();
        let spawn = &spec["paths"]["/api/sessions/{sessionId}/spawn"]["post"];
        assert!(spawn.is_object());
        assert_eq!(
            spawn["requestBody"]["content"]["application/json"]["schema"]["required"],
            json!(["agent"])
        );
        assert!(spawn["responses"]["207"].is_object());
        assert!(spec["paths"]["/api/sessions/{sessionId}/recent-cwds"]["get"].is_object());
        assert!(
            spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/interrupt"]["post"].is_object()
        );

        for capability in ["agent_spawn", "recent_cwds", "pane_interrupt"] {
            assert!(
                API_CAPABILITIES.contains(&capability),
                "{capability} is not announced"
            );
        }
    }

    /// A Herdr socket that answers from a script.
    ///
    /// Every gateway request is its own short-lived connection, so this accepts
    /// in a loop and answers one line per connection. `pane.read` walks the
    /// scripted screens and then repeats the last one forever, which is what
    /// lets a test say "and from here on the pane holds still".
    ///
    /// `agent.list` is scripted by how many Enters have arrived rather than by
    /// call count, because that is the real relationship: the agent's state
    /// sequence moves when a keystroke is finally taken, not after some number
    /// of polls.
    struct FakeHerdr {
        socket_path: PathBuf,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    /// Seq the fake agent sits on until it accepts an Enter. Arbitrary, but not
    /// zero, so "advanced past the baseline" cannot pass by accident.
    const FAKE_AGENT_SEQ: u64 = 100;

    impl FakeHerdr {
        /// `advance_after` is the number of Enters it takes for the agent's
        /// state sequence to move; `None` makes `agent.list` come back empty,
        /// which is a pane running no agent Herdr knows.
        fn start(screens: Vec<&str>, advance_after: Option<usize>) -> Self {
            let socket_path = std::env::temp_dir().join(format!(
                "herdr-submit-{}.sock",
                uuid::Uuid::new_v4().simple()
            ));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let screens: Vec<String> = screens.into_iter().map(str::to_owned).collect();
            tokio::spawn(async move {
                let mut reads = 0usize;
                let mut enters = 0usize;
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap_or_default().to_owned();
                    let result = match method.as_str() {
                        "pane.read" => {
                            let screen = screens
                                .get(reads)
                                .or_else(|| screens.last())
                                .cloned()
                                .unwrap_or_default();
                            reads += 1;
                            json!({ "read": { "text": screen, "revision": reads } })
                        }
                        "agent.list" => {
                            let agents = match advance_after {
                                None => json!([]),
                                Some(threshold) => {
                                    let seq = FAKE_AGENT_SEQ + u64::from(enters >= threshold);
                                    json!([
                                        { "pane_id": "wZ:p9", "state_change_seq": 1 },
                                        {
                                            "pane_id": "w1:p1",
                                            "agent": "claude",
                                            "agent_status": "idle",
                                            "state_change_seq": seq
                                        }
                                    ])
                                }
                            };
                            json!({ "agents": agents })
                        }
                        "pane.send_keys" => {
                            enters += 1;
                            json!({ "ok": true })
                        }
                        _ => json!({ "ok": true }),
                    };
                    recorded.lock().unwrap().push(request.clone());
                    let response = json!({ "id": request["id"], "result": result }).to_string();
                    let mut stream = reader.into_inner();
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(b"\n").await.unwrap();
                    stream.flush().await.unwrap();
                }
            });
            Self { socket_path, calls }
        }

        fn session(&self) -> SessionConfig {
            SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: self.socket_path.to_string_lossy().into_owned(),
                backend: BackendKind::Herdr,
            }
        }

        /// The call order with the state polling filtered out, so a test can say
        /// what happened to the pane without counting how many times the agent
        /// was asked about.
        fn pane_methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].as_str().unwrap_or_default().to_owned())
                .filter(|method| method != "agent.list")
                .collect()
        }

        fn enters(&self) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call["method"] == "pane.send_keys")
                .cloned()
                .collect()
        }
    }

    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    #[tokio::test]
    async fn an_enter_the_agent_took_is_not_repeated() {
        let herdr = FakeHerdr::start(
            // The screen keeps moving after the Enter and then keeps moving
            // again, which on its own proves nothing either way.
            vec![
                "> review this",
                "> review this",
                "reviewing...",
                "reviewing",
            ],
            Some(1),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        let enters = herdr.enters();
        assert_eq!(enters.len(), 1);
        assert_eq!(enters[0]["params"]["pane_id"], "w1:p1");
        assert_eq!(enters[0]["params"]["keys"], json!(["Enter"]));
    }

    #[tokio::test]
    async fn enter_waits_for_the_pane_to_stop_redrawing() {
        // The middle screens are Claude Code staging an image: the input line is
        // rewritten while the file is read, and an Enter in there is swallowed.
        let herdr = FakeHerdr::start(
            vec![
                "> look at /tmp/a.jpg",
                "> look at /tmp/a.jpg (reading)",
                "> look at [Image #1]",
                "> look at [Image #1]",
                "analyzing image...",
            ],
            Some(1),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 1);
        assert_eq!(
            herdr.pane_methods(),
            vec![
                "pane.read",
                "pane.read",
                "pane.read",
                "pane.read",
                "pane.send_keys"
            ]
        );
    }

    #[tokio::test]
    async fn a_screen_that_moved_without_submitting_does_not_pass_for_a_submission() {
        // The exact false positive that broke the first version of this: three
        // large images stage in bursts, so the pane looks still, then different,
        // then still again, while the prompt never leaves the input box. Only
        // the agent's state sequence knows, and here it moves on the third
        // Enter.
        let herdr = FakeHerdr::start(
            vec![
                "> look at [Image #1]",
                "> look at [Image #1]",
                "> look at [Image #1] [Image #2]",
                "> look at [Image #1] [Image #2] [Image #3]",
            ],
            Some(3),
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 3);
    }

    #[tokio::test]
    async fn enters_stop_at_the_budget_when_the_agent_never_takes_one() {
        let herdr = FakeHerdr::start(vec!["> review this"], Some(usize::MAX));

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), SUBMIT_MAX_ATTEMPTS as usize);
    }

    /// A pane that repaints a four-row screen, scrolling one row per read: the
    /// shape of the pane in card #646, small enough to assert on exactly.
    fn repainting_screens() -> Vec<String> {
        (0..4)
            .map(|top| {
                (top..top + 4)
                    .map(|row| format!("row {row}"))
                    .collect::<Vec<String>>()
                    .join("\n")
            })
            .collect()
    }

    fn output_state(herdr: &FakeHerdr) -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path = herdr.session().socket_path;
        state
    }

    async fn read_output(state: &AppState, lines: u32) -> String {
        let response = pane_output(
            State(state.clone()),
            Path(("default".into(), "wM:p1".into())),
            Query(OutputQuery {
                source: Some("recent-unwrapped".into()),
                lines: Some(lines),
                format: Some("text".into()),
                start: None,
                end: None,
            }),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        pane_read_text(&response.0).unwrap_or_default()
    }

    /// The whole point, end to end: Herdr keeps one screen, the gateway watched
    /// four, and the reader can ask for all four.
    #[tokio::test]
    async fn a_zero_backlog_pane_hands_back_more_than_herdr_kept() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } }),
        );

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        // Herdr's own answer is the last screen alone.
        assert_eq!(screens.last().unwrap(), "row 3\nrow 4\nrow 5\nrow 6");
        assert_eq!(served, "row 0\nrow 1\nrow 2\nrow 3\nrow 4\nrow 5\nrow 6");
    }

    async fn read_output_range(state: &AppState, start: u32, end: u32) -> String {
        let response = pane_output(
            State(state.clone()),
            Path(("default".into(), "wM:p1".into())),
            Query(OutputQuery {
                source: Some("recent-unwrapped".into()),
                lines: None,
                format: Some("text".into()),
                start: Some(start),
                end: Some(end),
            }),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        pane_read_text(&response.0).unwrap_or_default()
    }

    /// The bug this pins: a pane the scrollback store is keeping rows for is
    /// exactly the condition the tail-path stitching above exists for, and
    /// `keeps()` is decided from session/pane identity and Herdr's own scroll
    /// telemetry alone -- nothing about it depends on whether the *current*
    /// request happens to be range-addressed. Without gating on that, this
    /// same "kept" pane, read with an explicit `[start, end)`, would come back
    /// windowed by the plain `lines` default (200) rather than sliced to the
    /// requested range, while nothing about the response said so.
    ///
    /// Reuses the exact setup `a_zero_backlog_pane_hands_back_more_than_herdr_kept`
    /// uses to prove stitching *does* widen a tail read for this pane, then
    /// shows a range-addressed read of the same pane is answered with exactly
    /// what Herdr served -- the last screen alone, not the seven-row window.
    #[tokio::test]
    async fn a_range_addressed_read_is_never_widened_by_local_scrollback() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } }),
        );

        // Feed the store the same four repaints that, in the tail-path test,
        // make a fifth plain read come back as all seven kept rows.
        for _ in 0..4 {
            read_output(&state, 240).await;
        }

        let served = read_output_range(&state, 0, 240).await;

        // Herdr's own answer for this read is the last screen alone -- the
        // range-addressed request must get exactly that, not the stitched span.
        assert_eq!(screens.last().unwrap(), "row 3\nrow 4\nrow 5\nrow 6");
        assert_eq!(served, "row 3\nrow 4\nrow 5\nrow 6");
    }

    /// And having kept them, it says so where the reader's affordance looks --
    /// on the pane, not on the output.
    #[tokio::test]
    async fn the_pane_listing_reports_what_was_kept() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        let pane = json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } });
        state.scrollback.lock().unwrap().observe("default", &pane);

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let listing =
            note_and_amend_panes(&state, "default", json!({ "result": { "panes": [pane] } }));

        // Seven rows kept, four of them on screen: three to reach back for.
        assert_eq!(
            listing.pointer("/result/panes/0/scroll/max_offset_from_bottom"),
            Some(&json!(3))
        );
    }

    /// The panes that already worked have to keep working exactly as they did.
    #[tokio::test]
    async fn a_pane_with_scrollback_is_answered_as_herdr_answered_it() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        state.scrollback.lock().unwrap().observe(
            "default",
            &json!({ "pane_id": "wM:p1", "scroll": { "max_offset_from_bottom": 908, "viewport_rows": 4 } }),
        );

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        assert_eq!(served, screens.last().unwrap().as_str());
    }

    /// And so does a pane nobody has reported on: not knowing is a reason to
    /// stay out of the way.
    #[tokio::test]
    async fn an_unreported_pane_is_never_buffered() {
        let screens = repainting_screens();
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);

        for _ in 0..4 {
            read_output(&state, 240).await;
        }
        let served = read_output(&state, 240).await;

        assert_eq!(served, screens.last().unwrap().as_str());
    }

    #[tokio::test]
    async fn a_pane_herdr_lists_no_agent_for_falls_back_to_watching_the_screen() {
        let herdr = FakeHerdr::start(
            vec!["$ ls", "$ ls", "Cargo.toml  src", "Cargo.toml  src"],
            None,
        );

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), 1);
        assert_eq!(
            herdr.pane_methods(),
            vec!["pane.read", "pane.read", "pane.send_keys", "pane.read"]
        );
    }

    #[tokio::test]
    async fn a_blind_submit_presses_enter_no_more_than_the_small_budget() {
        let herdr = FakeHerdr::start(vec!["$ ls"], None);

        submit_keypress(&herdr.session(), "w1:p1").await;

        assert_eq!(herdr.enters().len(), SUBMIT_BLIND_MAX_ATTEMPTS as usize);
    }
}
