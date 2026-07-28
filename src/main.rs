#![recursion_limit = "256"]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{stdout, Write as _};
use std::net::SocketAddr;
use std::process::{Command as ProcessCommand, Stdio};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse as _, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use base64::Engine as _;
use clap::{Parser, Subcommand};
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_stream::Stream;

mod agent_events;
mod approvals;
mod composer;
mod i18n;
mod native;
mod parts;
mod scrollback;
mod shortcuts;
mod tasks;

use crate::i18n::Locale;

#[cfg(unix)]
use tokio::net::UnixStream;

const CONFIG_FILE: &str = "config.json";
const PAIRING_FILE: &str = "pairing.json";
const PUSH_TOKENS_FILE: &str = "push-tokens.json";
const DEVICES_FILE: &str = "devices.json";
const PID_FILE: &str = "gateway.pid";
const LOG_FILE: &str = "gateway.log";
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
const PAIRING_CODE_LENGTH: usize = PAIRING_CODE_CHARACTER_COUNT + 1;
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
const GATEWAY_API_VERSION: &str = "1.5.0";
const GATEWAY_API_MAJOR: u64 = 1;
const HERDR_PROTOCOL_MIN: u64 = 17;
const HERDR_PROTOCOL_MAX: u64 = 17;
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
    "terminal_input",
];

#[derive(Parser)]
#[command(name = "gateway", about = "Mobile gateway for Herdr")]
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
    },
    Run {
        #[arg(long)]
        config: Option<String>,
    },
    Start,
    Stop,
    Status,
    Manage,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    server_id: String,
    label: String,
    listen: String,
    public_url: String,
    /// Hash of the admin token. The admin token lives in `pairing.json` and is
    /// used by the local `manage` UI; paired devices get their own tokens.
    token_hash: String,
    sessions: Vec<SessionConfig>,
    /// Herdr agent kind -> the executable `GET /api/agents/catalog` looks for on
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingPayload {
    kind: String,
    server_id: String,
    label: String,
    url: String,
    token: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingPairing {
    request_id: String,
    device_name: String,
    #[serde(default)]
    install_id: Option<String>,
    code: String,
    code_hash: String,
    created_unix_ms: u128,
    #[serde(default)]
    failed_attempts: u8,
}

#[derive(Debug, PartialEq, Eq)]
enum PairingCodeError {
    Missing,
    Expired,
    Invalid,
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

/// One paired device. Each successful pairing mints a token that only this
/// record can authenticate, so a single device can be revoked on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeviceRecord {
    id: String,
    name: String,
    token_hash: String,
    paired_unix_ms: u128,
    #[serde(default)]
    last_seen_unix_ms: u128,
    /// The client's stable install identifier, used to replace an earlier
    /// record for the same device instead of piling up duplicates.
    #[serde(default)]
    install_id: Option<String>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup {
            public_url,
            port,
            socket_path,
        } => setup(public_url, port, socket_path)?,
        Command::Run { config } => run(config).await?,
        Command::Start => start_background()?,
        Command::Stop => stop_background()?,
        Command::Status => status()?,
        Command::Manage => manage()?,
        Command::Devices => list_devices()?,
        Command::Revoke { device_id, all } => revoke_device(device_id, all)?,
    }
    Ok(())
}

fn setup(public_url: Option<String>, port: u16, socket_path: Option<String>) -> anyhow::Result<()> {
    let config_dir = config_dir()?;
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create config dir {}", config_dir.display()))?;

    // Reuse an existing install's identity so re-running setup (after an update
    // or a retry) refreshes settings without minting a new server id or token --
    // that would orphan every already-paired device. Only a consistent config +
    // pairing pair is trusted; a half-written state falls back to a fresh mint.
    let existing = load_existing_identity(
        &config_dir.join(CONFIG_FILE),
        &config_dir.join(PAIRING_FILE),
    );

    let (server_id, token, token_hash) = match &existing {
        Some(id) => (
            id.server_id.clone(),
            id.token.clone(),
            id.token_hash.clone(),
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
        (Some(url), _) => (
            validate_public_url(&url)?,
            format!("0.0.0.0:{port}"),
            String::from("manual --public-url"),
        ),
        (None, Some(id)) => (
            id.public_url.clone(),
            id.listen.clone(),
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
    let socket_path = socket_path
        .or_else(|| std::env::var("HERDR_SOCKET_PATH").ok())
        .unwrap_or_else(default_socket_path);

    let config = Config {
        server_id,
        label: hostname_label(),
        listen,
        public_url: public_url.clone(),
        token_hash,
        sessions: vec![SessionConfig {
            id: "default".into(),
            label: "Default".into(),
            socket_path,
        }],
        agent_commands: BTreeMap::new(),
        rich_agent_pushes: false,
    };

    let path = config_dir.join(CONFIG_FILE);
    write_secret_file(&path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", path.display()))?;

    let payload = PairingPayload {
        kind: "herdr-gateway".into(),
        server_id: config.server_id.clone(),
        label: config.label.clone(),
        url: public_url,
        token,
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
    if payload.url.contains("127.0.0.1") || payload.url.contains("localhost") {
        println!("warning: pairing URL is local-only; rerun setup after starting Tailscale or pass --public-url");
    }
    println!("pairing identity is ready");
    println!("open the Gateway Manager pane to scan the QR code");
    Ok(())
}

/// The parts of a prior install that setup preserves so a rerun keeps devices
/// paired. Pulled from both files: the server id and admin token hash live in
/// the config, the raw admin token lives in the pairing file.
struct ExistingIdentity {
    server_id: String,
    token: String,
    token_hash: String,
    public_url: String,
    listen: String,
}

fn load_existing_identity(
    config_path: &std::path::Path,
    pairing_path: &std::path::Path,
) -> Option<ExistingIdentity> {
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).ok()?).ok()?;
    let pairing: PairingFile = serde_json::from_slice(&std::fs::read(pairing_path).ok()?).ok()?;
    // A config whose pairing file points at a different server is inconsistent;
    // treat it as no identity so setup mints a clean one rather than stitching
    // two mismatched halves together.
    if pairing.payload.server_id != config.server_id {
        return None;
    }
    Some(ExistingIdentity {
        server_id: config.server_id,
        token: pairing.payload.token,
        token_hash: config.token_hash,
        public_url: config.public_url,
        listen: config.listen,
    })
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
        // Tailscale Serve at the gateway. The listener still binds to the IP, and
        // MagicDNS resolves the name to it, so the phone reaches it either way.
        let magic_dns = status
            .pointer("/Self/DNSName")
            .and_then(Value::as_str)
            .map(|name| name.trim_end_matches('.'))
            .filter(|name| !name.is_empty());
        return match magic_dns {
            Some(name) => PublicUrlSelection {
                url: format!("http://{name}:{port}"),
                source: String::from("tailscale magicdns (http; set up Serve for https)"),
                listen_host: ip.to_string(),
            },
            None => PublicUrlSelection {
                url: format!("http://{ip}:{port}"),
                source: String::from("tailscale ip"),
                listen_host: ip.to_string(),
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
    if let Some(pid) = read_pid()? {
        if process_running(pid) {
            stop_pid(pid)?;
            stopped = true;
            if verbose {
                println!("gateway stopped pid {pid}");
            }
        } else if verbose {
            println!("gateway pid file exists, but pid {pid} is not running");
        }
    }

    let port = configured_port();
    for pid in gateway_listener_pids(port)? {
        if process_running(pid) {
            stop_pid(pid)?;
            stopped = true;
            if verbose {
                println!("gateway stopped listener pid {pid}");
            }
        }
    }

    remove_pid_file()?;
    if verbose && !stopped {
        println!("gateway is not running");
    }
    Ok(())
}

async fn run(config_path: Option<String>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;

    let state = AppState {
        config,
        pending_pairing: Arc::new(Mutex::new(None)),
        pairing_requests: Arc::new(Mutex::new(VecDeque::new())),
        push_tokens: Arc::new(Mutex::new(read_push_tokens().unwrap_or_default())),
        devices: Arc::new(Mutex::new(read_devices().unwrap_or_default())),
        assets: Arc::new(Mutex::new(AssetIndex::default())),
        scrollback: Arc::new(Mutex::new(scrollback::ScrollbackStore::default())),
        agent_events: Arc::new(Mutex::new(agent_events::AgentEventLog::default())),
        approval_events: tokio::sync::broadcast::channel(APPROVAL_EVENT_CAPACITY).0,
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
        .route("/api/sessions/{session_id}/assets", get(session_assets))
        .route("/api/assets/{asset_id}/content", get(asset_content))
        // A route-level limit is applied inside the router-wide one, so uploads
        // get their own ceiling while every JSON route keeps the small one.
        .route(
            "/api/uploads",
            post(upload_file).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            known_hosts(&state.config),
            known_host,
        ))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(request_locale))
        .with_state(state);

    println!("herdr gateway listening on http://{addr}");
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
    Json(body): Json<PairRequestBody>,
) -> ApiResult<Json<Value>> {
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
        if !pairing_code_expired(pending, now) {
            if pending.request_id == body.request_id {
                return Ok(Json(pair_request_response(&state.config, &body.request_id)));
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
    Ok(Json(pair_request_response(&state.config, &body.request_id)))
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

async fn pair_claim(
    State(state): State<AppState>,
    Json(body): Json<PairClaimBody>,
) -> ApiResult<Json<Value>> {
    let (device_name, install_id) = {
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
        consume_pairing_code(&mut pending, &body.request_id, &code, now_unix_ms()).map_err(
            |error| match error {
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
            },
        )?;
        (device_name, install_id)
    };

    // Each device gets its own token so it can be revoked without disturbing
    // the others. The admin token in pairing.json is never handed out.
    let token = generate_token();
    let record = DeviceRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: device_name,
        token_hash: hash_token(&token),
        paired_unix_ms: now_unix_ms(),
        last_seen_unix_ms: now_unix_ms(),
        install_id: install_id.clone(),
    };
    {
        let mut devices = lock_devices(&state)?;
        // One record per install: replace an earlier pairing from the same
        // device rather than accumulating duplicates.
        if let Some(install_id) = install_id.as_deref() {
            devices.retain(|device| device.install_id.as_deref() != Some(install_id));
        }
        devices.push(record);
        devices.sort_by_key(|item| item.paired_unix_ms);
        if devices.len() > MAX_DEVICES {
            let excess = devices.len() - MAX_DEVICES;
            devices.drain(..excess);
        }
        write_devices(&devices).map_err(|err| {
            eprintln!("failed to write device tokens: {err:#}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device_write_failed",
                "failed to save the new device token",
            )
        })?;
    }

    Ok(Json(json!({
        "kind": "herdr-gateway",
        "server_id": state.config.server_id,
        "label": state.config.label,
        "url": state.config.public_url,
        "token": token
    })))
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

fn consume_pairing_code(
    pending: &mut Option<PendingPairing>,
    request_id: &str,
    code: &str,
    now_unix_ms: u128,
) -> Result<(), PairingCodeError> {
    let Some(current) = pending.as_mut() else {
        return Err(PairingCodeError::Missing);
    };
    if pairing_code_expired(current, now_unix_ms) {
        *pending = None;
        return Err(PairingCodeError::Expired);
    }
    let request_matches = constant_time_eq(request_id.as_bytes(), current.request_id.as_bytes());
    let valid = valid_pairing_code(code)
        && request_matches
        && constant_time_eq(hash_token(code).as_bytes(), current.code_hash.as_bytes());
    if !valid {
        current.failed_attempts = current.failed_attempts.saturating_add(1);
        if current.failed_attempts >= MAX_PAIRING_CODE_ATTEMPTS {
            *pending = None;
        }
        return Err(PairingCodeError::Invalid);
    }
    *pending = None;
    Ok(())
}

fn pairing_code_expired(pending: &PendingPairing, now_unix_ms: u128) -> bool {
    now_unix_ms.saturating_sub(pending.created_unix_ms) >= PAIRING_CODE_TTL_MS
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
    if pending
        .as_ref()
        .is_some_and(|value| pairing_code_expired(value, now_unix_ms()))
    {
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
    // language. "Herdr Gateway" is the product's name and stays in Latin script
    // in every locale, the same way the app leaves "Gateway" alone.
    let locale = Locale::from_headers(&headers);
    let result = send_expo_push_notifications(
        &tokens,
        body.title.unwrap_or_else(|| "Herdr Gateway".into()),
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

fn status() -> anyhow::Result<()> {
    let config = load_config(None)?;
    println!("server_id: {}", config.server_id);
    println!("label: {}", config.label);
    println!("listen: {}", config.listen);
    println!("public_url: {}", config.public_url);
    for session in config.sessions {
        println!("session {}: {}", session.id, session.socket_path);
    }
    match read_pid()? {
        Some(pid) if process_running(pid) => println!("gateway: running pid {pid}"),
        Some(pid) => println!("gateway: stale pid {pid}"),
        None => println!("gateway: stopped"),
    }
    Ok(())
}

fn manage() -> anyhow::Result<()> {
    let _terminal = TerminalModeGuard::enter()?;
    let mut message = auto_upgrade_local_public_url()?.unwrap_or_else(|| String::from("ready"));
    let mut pending_pairing = fetch_pending_pairing().ok().flatten();
    let mut devices = read_devices().unwrap_or_default();
    // False by default so a finished pairing lands on the device list; `p` flips
    // it on to add another device.
    let mut show_qr = false;
    print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;

    loop {
        if !poll_event(MANAGE_REFRESH_INTERVAL)? {
            let next_pending_pairing = fetch_pending_pairing().ok().flatten();
            let next_devices = read_devices().unwrap_or_default();
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
                start_background_inner(false)?;
                message = String::from("start requested");
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
                    if revoke_managed_device(&device.id)? {
                        message =
                            format!("revoked {}; scan to pair again", truncate(&device.name, 32));
                        // Revocation usually means the user is replacing this
                        // app's credential. Return straight to the pairing QR
                        // instead of leaving them on the remaining device list.
                        show_qr = true;
                    } else {
                        message = String::from("device was already revoked");
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
                    update_public_url(&url)?;
                    message = format!("url updated: {}", truncate(&url, 36));
                }
                None => {
                    message = String::from("url unchanged");
                }
            },
            "a" | "auto" => {
                let selection = auto_public_url(configured_port());
                update_public_url(&selection.url)?;
                message = format!("auto url: {}", truncate(&selection.url, 36));
            }
            "q" | "quit" => break,
            other => message = format!("unknown command: {other}"),
        }

        pending_pairing = fetch_pending_pairing().ok().flatten();
        devices = read_devices().unwrap_or_default();
        print_manage_screen(&message, pending_pairing.as_ref(), &devices, show_qr)?;
    }
    Ok(())
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

fn update_public_url(public_url: &str) -> anyhow::Result<()> {
    let public_url = validate_public_url(public_url)?;
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.public_url = public_url.clone();
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
    update_public_url(&selection.url)?;
    Ok(Some(format!("auto url: {}", truncate(&selection.url, 36))))
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
        .map(|config| truncate(&config.public_url, 44))
        .unwrap_or_else(|| "run setup first".into());
    let status = match read_pid()? {
        Some(pid) if process_running(pid) => format!("running ({pid})"),
        Some(_) => String::from("stale pid"),
        None => String::from("stopped"),
    };

    let mut lines = vec![
        String::from("Herdr Gateway for Muqun"),
        String::from(""),
        String::from("keys   : [s] start  [t] stop  [p] pair  [x] revoke"),
        String::from("         [u] url    [a] auto  [r] refresh  [q] close"),
        format!("server : {server}"),
        format!("url    : {url}"),
        format!("status : {status}"),
        format!("message: {}", truncate(message, 58)),
        String::from(""),
    ];

    // A device mid-pairing takes priority: show its name + the code to enter.
    if let Some(pending) = pending_pairing {
        lines.extend([
            String::from("Pairing request"),
            format!("device : {}", truncate(&pending.device_name, 48)),
            format!("code   : {}", pending.code),
            String::from(""),
            String::from("Enter this code in Muqun to finish pairing."),
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
        let qr_controls = vec![
            String::from("Muqun Gateway"),
            String::from(""),
            String::from("[s] start   [t] stop"),
            String::from("[p] pair    [x] revoke"),
            String::from("[u] URL     [a] auto URL"),
            String::from("[r] devices / refresh"),
            String::from("[q/Esc] close"),
            String::from(""),
            format!("server: {}", truncate(&server, 18)),
            format!("url: {}", truncate(&url, 21)),
            format!("status: {}", truncate(&status, 18)),
            format!("message: {}", truncate(message, 17)),
        ];
        let mut qr_lines = vec![
            String::from("Scan with Muqun"),
            String::from("Code appears after scan"),
            String::from(""),
        ];
        // Config is authoritative for the advertised URL and server id. Older
        // pairing files can retain a stale URL even though their admin token is
        // still valid; rendering from that file made `p` show the wrong server.
        let encoded = pairing_qr_offer(&config.public_url, &config.server_id);
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

fn pairing_qr_offer(url: &str, server_id: &str) -> String {
    format!(
        "muqun://pair?u={}&s={}",
        url_component(url),
        url_component(server_id)
    )
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
    let session = find_session(&state.config, "default")?;
    let herdr = match herdr_request(session, "ping", json!({})).await {
        Ok(value) => {
            let version = value.pointer("/result/version").and_then(Value::as_str);
            let protocol = value.pointer("/result/protocol").and_then(Value::as_u64);
            let compatible = protocol
                .is_some_and(|value| (HERDR_PROTOCOL_MIN..=HERDR_PROTOCOL_MAX).contains(&value));
            json!({
                "connected": true,
                "version": version,
                "protocol": protocol,
                "compatible": compatible,
                "supportedProtocolMin": HERDR_PROTOCOL_MIN,
                "supportedProtocolMax": HERDR_PROTOCOL_MAX,
                "response": value
            })
        }
        Err(err) => {
            eprintln!("Herdr metadata request failed: {err:#}");
            json!({ "connected": false, "error": "Herdr is unavailable" })
        }
    };
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
        "herdr": herdr
    }))
}

async fn sessions(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    Ok(Json(json!({ "sessions": state.config.sessions })))
}

async fn snapshot(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let answer =
        call_session_method(&state.config, &session_id, "session.snapshot", json!({})).await;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer?.0)))
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
    call_session_method(&state.config, &session_id, "workspace.list", json!({})).await
}

async fn panes(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let answer = call_session_method(&state.config, &session_id, "pane.list", json!({})).await;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer?.0)))
}

async fn agents(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "agent.list", json!({})).await
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
    for ptr in ["/result/read/text", "/result/text"] {
        if let Some(text) = value.pointer(ptr).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    value
        .pointer("/result")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn stream_pane_frame(value: &Value) -> Option<StreamPaneFrame> {
    let revision = ["/result/read/revision", "/result/revision"]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))?;
    let output = pane_read_text(value)?;
    Some(StreamPaneFrame { revision, output })
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
    session: &SessionConfig,
    opts: &StreamOutputOpts,
) -> Option<StreamPaneFrame> {
    let pane = opts.pane.as_deref()?;
    let read = tokio::time::timeout(
        STREAM_OUTPUT_READ_TIMEOUT,
        herdr_request(
            session,
            "pane.read",
            json!({
                "pane_id": pane,
                "source": opts.source,
                "lines": opts.lines,
                "format": opts.format,
            }),
        ),
    )
    .await
    .ok()?
    .ok()?;
    stream_pane_frame(&read)
}

/// If `line` is a `pane.updated` for the streamed pane, read that pane's output
/// and fold it into the event as `data.output`. Returns `None` to forward the
/// line untouched (wrong pane, wrong event, or a read failure -- the client
/// still has its revision and can fall back to a read).
async fn enrich_pane_update(
    line: &str,
    session: &SessionConfig,
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
        herdr_request(
            session,
            "pane.read",
            json!({
                "pane_id": pane,
                "source": opts.source,
                "lines": opts.lines,
                "format": opts.format,
            }),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let text = pane_read_text(&read)?;
    value
        .get_mut("data")
        .and_then(Value::as_object_mut)?
        .insert("output".into(), Value::String(text));
    serde_json::to_string(&value).ok()
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
    if output.is_empty() {
        return;
    }
    let Ok(mut store) = store.lock() else { return };
    if !store.keeps(session_id, pane_id) {
        return;
    }
    let key = scrollback::read_key(session_id, pane_id, &opts.source, &opts.format);
    store.record(&key, output);
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

/// The `event` field of a forwarded Herdr line, used to decide whether a
/// subscribed client asked for it.
fn herdr_event_name(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("event")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

async fn events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<Value>),
> {
    require_device(&state, &headers)?;
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
    let mut approvals_rx = state.approval_events.subscribe();
    let assets = state.assets.clone();
    let scrollback_store = state.scrollback.clone();
    let stream = async_stream::stream! {
        match herdr_event_stream(&session).await {
            Ok(mut reader) => {
                let mut line = Vec::new();
                let mut output_interval = tokio::time::interval(STREAM_OUTPUT_POLL_INTERVAL);
                output_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_stream_output: Option<String> = None;
                loop {
                    line.clear();
                    tokio::select! {
                        read = reader.read_until(b'\n', &mut line) => match read {
                            Ok(0) => break,
                            Ok(_) => {
                            let data = String::from_utf8_lossy(&line);
                            let data = data.trim();
                            if !data.is_empty() {
                                // Filter at the point of forwarding rather than
                                // by unsubscribing: a subscribed event a client
                                // did not ask for costs nothing here, but waking
                                // the phone for it costs battery.
                                let keep = match &wanted {
                                    Some(set) => herdr_event_name(data)
                                        .map(|name| set.contains(&name))
                                        .unwrap_or(true),
                                    None => true,
                                };
                                if keep {
                                    // Fold the viewed pane's output into its
                                    // update so the client paints on arrival,
                                    // with no follow-up read hop. Everything
                                    // else forwards untouched.
                                    let payload = if stream_opts.pane.is_some() {
                                        enrich_pane_update(data, &session, &stream_opts)
                                            .await
                                            .unwrap_or_else(|| data.to_owned())
                                    } else {
                                        data.to_owned()
                                    };
                                    // An enriched update is a second source of
                                    // fresh output for the watched pane. Feeding
                                    // only the tick would drop frames on a pane
                                    // busy enough to beat it.
                                    if let Some(pane_id) = stream_opts.pane.as_deref() {
                                        if let Some(output) = enriched_pane_output(&payload) {
                                            keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &output);
                                        }
                                    }
                                    yield Ok(Event::default().event("herdr").data(payload));
                                }
                                // A worktree event is consumed whether or not
                                // the client asked to see it: the asset index
                                // is fed by the same stream that filters.
                                if let Some(root) = worktree_event_root(&session_id, data) {
                                    let created = ingest_roots(assets.clone(), vec![root]).await;
                                    if asset_events {
                                        let now = now_unix_ms();
                                        for entry in created
                                            .into_iter()
                                            .filter(|entry| now.saturating_sub(entry.modified_unix_ms) <= ASSET_EVENT_MAX_AGE_MS)
                                            .take(MAX_ASSET_EVENTS_PER_WORKTREE)
                                        {
                                            let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
                                            yield Ok(Event::default()
                                                .event("asset.created")
                                                .data(asset_created_payload(&entry, asset_type)));
                                        }
                                    }
                                } else if let Some(removed) = worktree_event_removed_root(data) {
                                    if let Ok(mut index) = assets.lock() {
                                        index.forget_under(&removed);
                                    }
                                }
                            }
                            }
                            Err(err) => {
                            eprintln!("Herdr event stream read failed: {err:#}");
                            yield Ok(Event::default().event("gateway.error").data("Herdr event stream unavailable"));
                            break;
                            }
                        },
                        _ = output_interval.tick(), if stream_opts.pane.is_some() => {
                            if let Some(frame) = poll_stream_pane_update(&session, &stream_opts).await {
                                if last_stream_output.as_deref() != Some(frame.output.as_str()) {
                                    last_stream_output = Some(frame.output.clone());
                                    if let Some(pane_id) = stream_opts.pane.as_deref() {
                                        // The frame the reader is watching is
                                        // also the only chance to keep it: for a
                                        // pane Herdr holds no scrollback for,
                                        // this tick is where its history comes
                                        // from, and it was being dropped.
                                        keep_stream_frame(&scrollback_store, &session_id, pane_id, &stream_opts, &frame.output);
                                        if let Some(payload) = stream_pane_update_payload(&frame, pane_id) {
                                            yield Ok(Event::default().event("herdr").data(payload));
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
                                        yield Ok(Event::default().event(approval.name).data(approval.payload));
                                    }
                                }
                                // A client that fell behind has missed
                                // transitions; it re-reads the pane's approval
                                // rather than being told a stale one.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        },
                    }
                }
            }
            Err(err) => {
                eprintln!("Herdr event stream connection failed: {err:#}");
                yield Ok(Event::default().event("gateway.error").data("Herdr event stream unavailable"));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
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
        let Ok(response) = herdr_request(&session, "pane.list", json!({})).await else {
            continue;
        };
        // This listing is already being fetched, and it is the only place that
        // says which panes Herdr keeps no scrollback for. Reading it here means
        // the buffer knows what to keep before the reader ever opens the pane,
        // and costs Herdr nothing extra.
        if let Some(mut store) = lock_scrollback(&state) {
            store.observe(&session.id, &response);
        }

        let panes = response
            .pointer("/result/panes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut seen: Vec<String> = Vec::new();
        for pane in &panes {
            let Some(pane_id) = pane.get("pane_id").and_then(Value::as_str) else {
                continue;
            };
            let agent = pane
                .get("agent")
                .and_then(Value::as_str)
                .filter(|agent| !agent.is_empty());
            let Some(agent) = agent else { continue };
            seen.push(pane_id.to_owned());

            let Ok(read) = herdr_request(
                &session,
                "pane.read",
                json!({
                    "pane_id": pane_id,
                    "source": "visible",
                    "lines": APPROVAL_READ_LINES,
                    "format": "text"
                }),
            )
            .await
            else {
                continue;
            };
            let text = pane_read_text(&read).unwrap_or_default();
            match approvals::detect(&text) {
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

    loop {
        for mut notification in poll_agent_notifications(&state, &session, &mut statuses).await {
            enrich_blocked_notification(&state, &session, &mut notification).await;
            deliver_agent_notification(&state, notification).await;
        }

        match herdr_agent_event_stream(&session).await {
            Ok(mut reader) => {
                let mut line = String::new();
                loop {
                    line.clear();
                    match tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
                        .await
                    {
                        Err(_) | Ok(Ok(0)) => break,
                        Ok(Ok(_)) => {
                            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                                continue;
                            };
                            if let Some(mut notification) = absorb_agent_status_event(
                                &state,
                                &session.id,
                                &event,
                                &mut statuses,
                            ) {
                                enrich_blocked_notification(&state, &session, &mut notification)
                                    .await;
                                deliver_agent_notification(&state, notification).await;
                            }
                        }
                        Ok(Err(err)) => {
                            eprintln!(
                                "Herdr event stream failed for session {}: {err}",
                                session.id
                            );
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "Herdr event subscription failed for session {}: {err:#}",
                    session.id
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn poll_agent_notifications(
    state: &AppState,
    session: &SessionConfig,
    statuses: &mut HashMap<String, String>,
) -> Vec<AgentPushNotice> {
    let Ok(value) = herdr_request(session, "agent.list", json!({})).await else {
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
    let Ok(value) = herdr_request(session, "agent.list", json!({})).await else {
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
    let mut params = serde_json::Map::new();
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "label", body.label);
    insert_opt(&mut params, "focus", body.focus);
    call_session_method(
        &state.config,
        &session_id,
        "workspace.create",
        Value::Object(params),
    )
    .await
}

async fn focus_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.focus",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

async fn rename_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameWorkspaceBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.rename",
        json!({ "workspace_id": workspace_id, "label": body.label }),
    )
    .await
}

async fn close_workspace(
    State(state): State<AppState>,
    Path((session_id, workspace_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "workspace.close",
        json!({ "workspace_id": workspace_id }),
    )
    .await
}

async fn tabs(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(&state.config, &session_id, "tab.list", json!({})).await
}

async fn create_tab(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mut params = serde_json::Map::new();
    insert_opt(&mut params, "workspace_id", body.workspace_id);
    insert_opt(&mut params, "label", body.label);
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "focus", body.focus);
    call_session_method(
        &state.config,
        &session_id,
        "tab.create",
        Value::Object(params),
    )
    .await
}

async fn focus_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.focus",
        json!({ "tab_id": tab_id }),
    )
    .await
}

async fn rename_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenameTabBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.rename",
        json!({ "tab_id": tab_id, "label": body.label }),
    )
    .await
}

async fn close_tab(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "tab.close",
        json!({ "tab_id": tab_id }),
    )
    .await
}

async fn pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let answer = call_session_method(
        &state.config,
        &session_id,
        "pane.get",
        json!({ "pane_id": pane_id }),
    )
    .await;
    Ok(Json(note_and_amend_panes(&state, &session_id, answer?.0)))
}

async fn focus_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.focus",
        json!({ "pane_id": pane_id }),
    )
    .await
}

async fn rename_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RenamePaneBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.rename",
        json!({ "pane_id": pane_id, "label": body.label }),
    )
    .await
}

async fn close_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "pane.close",
        json!({ "pane_id": pane_id }),
    )
    .await
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
    let command = body
        .command
        .map(|parts| parts.join(" "))
        .filter(|text| !text.trim().is_empty());
    if let Some(text) = command.as_deref() {
        validate_text(text)?;
    }
    let mut params = serde_json::Map::new();
    params.insert("target_pane_id".into(), json!(pane_id));
    params.insert("direction".into(), json!(body.direction));
    insert_opt(&mut params, "ratio", body.ratio);
    insert_opt(&mut params, "cwd", body.cwd);
    insert_opt(&mut params, "env", body.env);
    let result = call_session_method(
        &state.config,
        &session_id,
        "pane.split",
        Value::Object(params),
    )
    .await?;
    if let Some(text) = command {
        let created_pane_id = created_pane_id(&result).ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "invalid_split_response",
                "Herdr did not return the created pane id",
            )
        })?;
        let _ = call_session_method(
            &state.config,
            &session_id,
            "pane.send_text",
            json!({ "pane_id": created_pane_id, "text": text }),
        )
        .await?;
        let _ = call_session_method(
            &state.config,
            &session_id,
            "pane.send_keys",
            json!({ "pane_id": created_pane_id, "keys": ["Enter"] }),
        )
        .await?;
    }
    Ok(result)
}

fn created_pane_id(value: &Value) -> Option<&str> {
    value
        .pointer("/result/pane/pane_id")
        .and_then(Value::as_str)
}

async fn zoom_pane(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
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
    call_session_method(
        &state.config,
        &session_id,
        "pane.zoom",
        json!({ "pane_id": pane_id, "mode": mode }),
    )
    .await
}

async fn agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "agent.get",
        json!({ "target": target }),
    )
    .await
}

async fn focus_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    call_session_method(
        &state.config,
        &session_id,
        "agent.focus",
        json!({ "target": target }),
    )
    .await
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
    herdr_call(
        session,
        "agent.prompt",
        json!({ "target": target, "text": text }),
    )
    .await
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
            "agent submit for pane {pane_id}: Herdr lists no agent state for it, \
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
        if let Err(err) = herdr_call(
            session,
            "pane.send_keys",
            json!({ "pane_id": pane_id, "keys": ["Enter"] }),
        )
        .await
        {
            eprintln!(
                "agent submit for pane {pane_id} failed to send Enter: {}",
                err.message()
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

/// Herdr's own count of how many times this pane's agent has changed state.
///
/// `None` means Herdr lists no agent for the pane, which a send to a plain shell
/// pane legitimately is, or that it could not be asked.
async fn agent_state_change_seq(session: &SessionConfig, pane_id: &str) -> Option<u64> {
    let value = match herdr_request(session, "agent.list", json!({})).await {
        Ok(value) => value,
        Err(err) => {
            eprintln!("Herdr request agent.list failed: {err:#}");
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

    let mut agent_params = serde_json::Map::new();
    agent_params.insert(
        "name".into(),
        json!(label.as_deref().unwrap_or(body.agent.as_str())),
    );
    agent_params.insert("kind".into(), json!(body.agent));
    agent_params.insert("pane_id".into(), json!(place.pane_id));
    agent_params.insert("timeout_ms".into(), json!(timeout_ms));
    if let Some(args) = body.agent_args.as_ref() {
        agent_params.insert("args".into(), json!(args));
    }
    match herdr_call(&session, "agent.start", Value::Object(agent_params)).await {
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
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(repo_path.to_string_lossy()));
    params.insert("focus".into(), json!(false));
    insert_opt(&mut params, "label", label);
    let value = match herdr_call(session, "workspace.create", Value::Object(params)).await {
        Ok(value) => value,
        Err(err) => {
            steps.failed("workspace", err.code(), &err.message());
            return Err(err);
        }
    };
    let place = workspace_place(&value, None, false).ok_or_else(|| {
        let err = HerdrCallError::malformed("workspace.create");
        steps.failed("workspace", err.code(), &err.message());
        err
    })?;
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

    let mut params = serde_json::Map::new();
    params.insert("name".into(), json!(body.agent));
    params.insert("kind".into(), json!(body.agent));
    params.insert("pane_id".into(), json!(place.pane_id));
    params.insert(
        "timeout_ms".into(),
        json!(tasks::DEFAULT_AGENT_START_TIMEOUT_MS),
    );
    match herdr_call(&session, "agent.start", Value::Object(params)).await {
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
    {
        let mut params = serde_json::Map::new();
        insert_opt(&mut params, "cwd", cwd);
        params.insert("focus".into(), json!(false));
        let value = herdr_call(session, "tab.create", Value::Object(params))
            .await
            .map_err(|err| {
                steps.failed("pane", err.code(), &err.message());
                err.into_api_error("tab.create")
            })?;
        let pane_id = created_pane_id(&value)
            .or_else(|| {
                value
                    .pointer("/result/root_pane/pane_id")
                    .and_then(Value::as_str)
            })
            .ok_or_else(|| {
                api_error(
                    StatusCode::BAD_GATEWAY,
                    "herdr_malformed_response",
                    "Herdr did not return the created pane id",
                )
            })?
            .to_owned();
        let tab_id = value
            .pointer("/result/tab/tab_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        steps.ok("pane", json!({ "pane_id": pane_id, "tab_id": tab_id }));
        Ok(SpawnPlace { pane_id, tab_id })
    }
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
    let mut params = serde_json::Map::new();
    params.insert("pane_id".into(), json!(host));
    params.insert("direction".into(), json!("down"));
    insert_opt(&mut params, "cwd", cwd);
    params.insert("focus".into(), json!(false));
    // A split that Ghostty refuses -- a tab already carrying as many panes as
    // its layout will hold, which is the ordinary state of a tab someone works
    // in -- must not lose the task. The tab was a preference, not the request:
    // the request was "start this agent". So a refusal falls back to a tab of
    // its own, recorded as such rather than passed off as the split that was
    // asked for.
    let split = herdr_call(session, "pane.split", Value::Object(params)).await;
    let value = match split {
        Ok(value) => value,
        Err(err) => {
            steps.skipped(
                "split",
                &format!("{} -- starting in a tab of its own instead", err.message()),
            );
            return spawn_in_new_tab(session, cwd, steps).await;
        }
    };
    let pane_id = created_pane_id(&value)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "herdr_malformed_response",
                "Herdr did not return the created pane id",
            )
        })?
        .to_owned();
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
    let value = herdr_request(session, "pane.list", json!({})).await.ok()?;
    pane_to_split(&value, tab_id)
}

/// Which pane of a `pane.list` a split should hang off, given the tab.
///
/// The focused one, because that is the one the user was looking at and the one
/// a new pane should appear next to. A tab this session does not have answers
/// with nothing, which is what makes an unknown `tab_id` a refusal rather than
/// a spawn somewhere else.
fn pane_to_split(response: &Value, tab_id: &str) -> Option<String> {
    let panes = response
        .pointer("/result/panes")
        .and_then(Value::as_array)?;
    let in_tab: Vec<&Value> = panes
        .iter()
        .filter(|pane| pane.get("tab_id").and_then(Value::as_str) == Some(tab_id))
        .collect();
    in_tab
        .iter()
        .find(|pane| pane.get("focused").and_then(Value::as_bool) == Some(true))
        .or_else(|| in_tab.first())
        .and_then(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
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
    let roots = session_asset_roots(&state, &session).await;

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

    herdr_call(
        &session,
        "pane.send_keys",
        json!({ "pane_id": pane_id, "keys": [key] }),
    )
    .await
    .map_err(|err| err.into_api_error("pane.send_keys"))?;

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
    let cwd = repo_path.to_string_lossy().into_owned();

    let existing = herdr_call(session, "worktree.list", json!({ "cwd": cwd }))
        .await
        .ok()
        .and_then(|value| worktree_for_branch(&value, branch));

    if let Some(path) = existing {
        let mut params = serde_json::Map::new();
        params.insert("cwd".into(), json!(cwd));
        params.insert("branch".into(), json!(branch));
        params.insert("focus".into(), json!(false));
        insert_opt(&mut params, "label", label);
        match herdr_call(session, "worktree.open", Value::Object(params)).await {
            Ok(value) => {
                if let Some(place) = workspace_place(&value, Some(path.clone()), true) {
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
            }
            // Reuse is an optimisation, not a contract. If opening the existing
            // checkout fails, fall through and let the create path report a
            // real error rather than masking it with this one.
            Err(err) => eprintln!("task: worktree.open for {branch} failed: {}", err.message()),
        }
    }

    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(cwd));
    params.insert("branch".into(), json!(branch));
    params.insert("focus".into(), json!(false));
    insert_opt(&mut params, "label", label);
    match herdr_call(session, "worktree.create", Value::Object(params)).await {
        Ok(value) => {
            let path = value
                .pointer("/result/worktree/path")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let place = workspace_place(&value, path.clone(), false).ok_or_else(|| {
                let err = HerdrCallError::malformed("worktree.create");
                steps.failed("worktree", err.code(), &err.message());
                err
            })?;
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
        Err(HerdrCallError::Herdr { error, .. }) if tasks::is_unknown_method_error(&error) => {
            prepare_worktree_with_git(session, repo_path, branch, label, steps).await
        }
        Err(err) => {
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

    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(path));
    params.insert("focus".into(), json!(false));
    insert_opt(&mut params, "label", label);
    let created = match herdr_call(session, "workspace.create", Value::Object(params)).await {
        // Herdr's own refusal is the useful message here, so it is kept rather
        // than flattened into "something went wrong".
        Err(err) => Err(err),
        Ok(value) => workspace_place(&value, Some(path.clone()), !added.created)
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

/// Herdr answers `workspace.create`, `worktree.create` and `worktree.open` with
/// the same workspace/tab/root_pane trio, so one reader covers all three.
fn workspace_place(
    value: &Value,
    worktree_path: Option<String>,
    reused: bool,
) -> Option<TaskPlace> {
    Some(TaskPlace {
        workspace_id: value
            .pointer("/result/workspace/workspace_id")
            .and_then(Value::as_str)?
            .to_owned(),
        pane_id: value
            .pointer("/result/root_pane/pane_id")
            .and_then(Value::as_str)?
            .to_owned(),
        worktree_path: value
            .pointer("/result/worktree/path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(worktree_path),
        reused,
    })
}

/// The path of an existing checkout of `branch` in a `worktree.list` answer.
fn worktree_for_branch(value: &Value, branch: &str) -> Option<String> {
    value
        .pointer("/result/worktrees")
        .and_then(Value::as_array)?
        .iter()
        .find(|worktree| {
            worktree
                .get("branch")
                .and_then(Value::as_str)
                .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name))
                == Some(branch)
        })
        .and_then(|worktree| worktree.get("path").and_then(Value::as_str))
        .map(str::to_owned)
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

    match herdr_request(session, "workspace.list", json!({})).await {
        Ok(value) => {
            if let Some(workspaces) = value
                .pointer("/result/workspaces")
                .and_then(Value::as_array)
            {
                for workspace in workspaces {
                    for key in ["repo_root", "checkout_path"] {
                        if let Some(path) = workspace
                            .pointer(&format!("/worktree/{key}"))
                            .and_then(Value::as_str)
                        {
                            push(PathBuf::from(path));
                        }
                    }
                }
            }
        }
        Err(err) => eprintln!("task roots: workspace.list failed: {err:#}"),
    }

    for root in session_asset_roots(state, session).await {
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

async fn herdr_call(
    session: &SessionConfig,
    method: &str,
    params: Value,
) -> Result<Value, HerdrCallError> {
    let value = herdr_request(session, method, params)
        .await
        .map_err(|err| {
            eprintln!("Herdr request {method} failed: {err:#}");
            HerdrCallError::Unavailable("Herdr is unavailable".into())
        })?;
    if let Some(error) = value.get("error") {
        return Err(HerdrCallError::Herdr {
            method: method.to_owned(),
            error: error.clone(),
        });
    }
    Ok(value)
}

async fn pane_output(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<OutputQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
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
    let params = json!({
        "pane_id": pane_id,
        "source": herdr_source,
        "lines": lines,
        "format": format
    });
    let mut answer = call_session_method(&state.config, &session_id, "pane.read", params)
        .await?
        .0;

    // Herdr answered with everything it has. For a pane it keeps nothing above
    // the viewport for, everything it has is one screen -- and this is the read
    // that both feeds what the gateway kept and hands it back.
    if let Some(text) = pane_read_text(&answer) {
        let key = scrollback::read_key(&session_id, &pane_id, herdr_source, &format);
        let served = {
            let Some(mut store) = lock_scrollback(&state) else {
                return Ok(Json(answer));
            };
            if !store.keeps(&session_id, &pane_id) {
                return Ok(Json(answer));
            }
            store.record(&key, &text);
            store.window(&key, lines as usize)
        };
        if let Some(served) = served {
            if served.len() > text.len() {
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
    let read = herdr_request(
        &session,
        "pane.read",
        json!({
            "pane_id": pane_id,
            "source": "recent_unwrapped",
            "lines": lines,
            "format": "text"
        }),
    )
    .await
    .map_err(|err| {
        eprintln!("Herdr request pane.read failed: {err:#}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "herdr_unavailable",
            "Herdr is unavailable",
        )
    })?;
    let herdr_text = pane_read_text(&read).unwrap_or_default();
    // The transcript reads the same pane through a different endpoint, so it
    // has to be given the same rows: two views that disagree about where
    // history ends is the bug this whole thing is trying not to introduce.
    let text = {
        let key = scrollback::read_key(&session_id, &pane_id, "recent_unwrapped", "text");
        match lock_scrollback(&state) {
            Some(mut store) if store.keeps(&session_id, &pane_id) => {
                store.record(&key, &herdr_text);
                store
                    .window(&key, lines as usize)
                    .filter(|served| served.len() > herdr_text.len())
                    .unwrap_or(herdr_text)
            }
            _ => herdr_text,
        }
    };
    let revision = ["/result/read/revision", "/result/revision"]
        .into_iter()
        .find_map(|pointer| read.pointer(pointer).and_then(Value::as_u64));

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
    let pane = herdr_request(session, "pane.get", json!({ "pane_id": pane_id }))
        .await
        .ok();
    let pane = pane
        .as_ref()
        .map(|response| response.pointer("/result/pane").unwrap_or(response));
    let agent = pane
        .and_then(|pane| pane.get("agent").and_then(Value::as_str))
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned);
    let root = pane
        .and_then(|pane| {
            pane.get("cwd")
                .and_then(Value::as_str)
                .or_else(|| pane.get("foreground_cwd").and_then(Value::as_str))
        })
        .map(PathBuf::from)
        .filter(|path| is_scannable_root(path))
        .and_then(|path| std::fs::canonicalize(path).ok());
    (agent, root)
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
    // assets API would refuse.
    let roots = session_asset_roots(&state, &session).await;
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
    let response = herdr_request(session, "pane.get", json!({ "pane_id": pane_id }))
        .await
        .map_err(|err| {
            eprintln!("Herdr request pane.get failed: {err:#}");
            api_error(
                StatusCode::BAD_GATEWAY,
                "herdr_unavailable",
                "Herdr is unavailable",
            )
        })?;
    Ok(response
        .pointer("/result/pane")
        .cloned()
        .unwrap_or(response))
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
    herdr_request(
        session,
        "pane.send_keys",
        json!({ "pane_id": pane_id, "keys": keys }),
    )
    .await
    .map_err(|err| {
        eprintln!("Herdr request pane.send_keys failed: {err:#}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "herdr_unavailable",
            "Herdr is unavailable",
        )
    })
}

/// What a pane is drawing right now, as plain text.
///
/// `visible` rather than the transcript source the parts endpoint reads: both
/// callers care about the current screen, and the scrollback of an answered menu
/// or an already-submitted prompt would only mislead them.
async fn read_pane_visible_text(session: &SessionConfig, pane_id: &str) -> ApiResult<String> {
    let read = herdr_request(
        session,
        "pane.read",
        json!({
            "pane_id": pane_id,
            "source": "visible",
            "lines": APPROVAL_READ_LINES,
            "format": "text"
        }),
    )
    .await
    .map_err(|err| {
        eprintln!("Herdr request pane.read failed: {err:#}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "herdr_unavailable",
            "Herdr is unavailable",
        )
    })?;
    Ok(pane_read_text(&read).unwrap_or_default())
}

/// Read a pane's agent and whatever menu it is drawing.
async fn read_pane_approval(
    session: &SessionConfig,
    pane_id: &str,
) -> ApiResult<(Option<String>, Option<approvals::Approval>)> {
    let agent = herdr_request(session, "pane.get", json!({ "pane_id": pane_id }))
        .await
        .ok()
        .and_then(|response| {
            let pane = response.pointer("/result/pane").unwrap_or(&response);
            pane.get("agent").and_then(Value::as_str).map(str::to_owned)
        })
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
    call_session_method(
        &state.config,
        &session_id,
        "pane.send_text",
        json!({ "pane_id": pane_id, "text": body.text }),
    )
    .await
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
    call_session_method(
        &state.config,
        &session_id,
        "pane.send_keys",
        json!({ "pane_id": pane_id, "keys": body.keys }),
    )
    .await
}

/// A file type the gateway is willing to store, with the extension and MIME
/// type derived from the bytes themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadKind {
    extension: &'static str,
    mime: &'static str,
}

/// Accept an image from the phone, park it in the gateway's own upload
/// directory and hand back a local path. The app then sends that path to an
/// agent as ordinary text, so the agent reads the file straight off this
/// machine.
///
/// This first version takes images only. The stored name is generated here and
/// the extension comes from the sniffed content, so nothing the client sends
/// reaches the filesystem.
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

    // Executables are checked separately from the image allow-list so the
    // refusal says what it means, and so the rule still holds if the allow-list
    // ever grows a format that a script could hide inside.
    if looks_executable(&bytes) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "executable_rejected",
            "executables and scripts are not accepted",
        ));
    }
    let Some(kind) = sniff_upload_kind(&bytes) else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_file_type",
            "only png, jpeg, gif, webp, and heic images are accepted",
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
    pane_id: Option<String>,
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
    roots: HashMap<String, Vec<AssetRoot>>,
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

    fn remember_roots(&mut self, session_id: &str, roots: Vec<AssetRoot>) {
        self.roots.insert(session_id.to_owned(), roots);
    }

    fn known_roots(&self, session_id: &str) -> Vec<AssetRoot> {
        self.roots.get(session_id).cloned().unwrap_or_default()
    }

    /// Newest first, cut to one page.
    fn session_assets(
        &self,
        session_id: &str,
        since_unix_ms: Option<u128>,
        limit: usize,
    ) -> Vec<AssetEntry> {
        let mut entries = self.session_assets_ordered(session_id, since_unix_ms);
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
    fn session_assets_ordered(
        &self,
        session_id: &str,
        since_unix_ms: Option<u128>,
    ) -> Vec<AssetEntry> {
        let mut entries: Vec<AssetEntry> = self
            .entries
            .values()
            .filter(|entry| entry.session_id == session_id)
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
async fn session_asset_roots(state: &AppState, session: &SessionConfig) -> Vec<AssetRoot> {
    let fresh = match herdr_request(session, "pane.list", json!({})).await {
        Ok(value) => pane_list_roots(&session.id, &value),
        Err(err) => {
            eprintln!("asset roots: pane.list failed: {err:#}");
            Vec::new()
        }
    };
    if fresh.is_empty() {
        return match state.assets.lock() {
            Ok(index) => index.known_roots(&session.id),
            Err(_) => Vec::new(),
        };
    }
    if let Ok(mut index) = state.assets.lock() {
        index.remember_roots(&session.id, fresh.clone());
    }
    fresh
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

/// List what a session's workspaces produced recently, newest first.
async fn session_assets(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<AssetsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let roots = session_asset_roots(&state, &session).await;

    // An exact path lookup asks about one file, so it neither waits for a scan
    // nor pages: it answers with that file or with nothing.
    if let Some(wanted) = query.path.clone() {
        let lookup_roots = roots.clone();
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
    // has to be given the whole session in order instead, because the index
    // does not know what a file is -- its bytes do -- and the page has to be
    // filled from the newest matches rather than from whatever the newest
    // handful of files happened to be.
    let entries = {
        let index = lock_assets(&state)?;
        if kinds.is_empty() {
            index.session_assets(&session_id, since, limit)
        } else {
            index.session_assets_ordered(&session_id, since)
        }
    };
    let filter = kinds.clone();
    let assets = tokio::task::spawn_blocking(move || asset_page(entries, &filter, limit))
        .await
        .unwrap_or_default();

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
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
        // the index from the live sessions once before answering.
        for session in state.config.sessions.clone() {
            let roots = session_asset_roots(&state, &session).await;
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
    let roots = canonical_roots(&session_asset_roots(&state, &session).await);
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

async fn call_session_method(
    config: &Config,
    session_id: &str,
    method: &str,
    params: Value,
) -> ApiResult<Json<Value>> {
    let session = find_session(config, session_id)?;
    let value = herdr_request(session, method, params)
        .await
        .map_err(|err| {
            eprintln!("Herdr request {method} failed: {err:#}");
            api_error(
                StatusCode::BAD_GATEWAY,
                "herdr_unavailable",
                "Herdr is unavailable",
            )
        })?;
    Ok(Json(value))
}

async fn herdr_request(
    session: &SessionConfig,
    method: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let request = build_herdr_request(method, params);

    #[cfg(unix)]
    {
        let mut stream = UnixStream::connect(&session.socket_path)
            .await
            .with_context(|| format!("failed to connect Herdr socket {}", session.socket_path))?;
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).await?;
        let value = serde_json::from_str(&response)?;
        Ok(value)
    }

    #[cfg(not(unix))]
    {
        let _ = (session, method, params, request);
        anyhow::bail!("direct Herdr socket access is not implemented on this platform yet");
    }
}

#[cfg(unix)]
async fn herdr_event_stream(session: &SessionConfig) -> anyhow::Result<BufReader<UnixStream>> {
    let pane_ids = session_pane_ids(session).await?;
    open_herdr_event_stream(session, event_subscriptions(&pane_ids)).await
}

#[cfg(unix)]
async fn herdr_agent_event_stream(
    session: &SessionConfig,
) -> anyhow::Result<BufReader<UnixStream>> {
    let pane_ids = session_pane_ids(session).await?;
    open_herdr_event_stream(session, agent_event_subscriptions(&pane_ids)).await
}

#[cfg(unix)]
async fn session_pane_ids(session: &SessionConfig) -> anyhow::Result<Vec<String>> {
    let pane_response = herdr_request(session, "pane.list", json!({})).await?;
    Ok(pane_response
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .context("pane.list response is missing result.panes")?
        .iter()
        .filter_map(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

#[cfg(unix)]
async fn open_herdr_event_stream(
    session: &SessionConfig,
    subscriptions: Vec<Value>,
) -> anyhow::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(&session.socket_path)
        .await
        .with_context(|| format!("failed to connect Herdr socket {}", session.socket_path))?;
    let request = build_herdr_request(
        "events.subscribe",
        json!({
            "subscriptions": subscriptions
        }),
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(BufReader::new(stream))
}

fn event_subscriptions(pane_ids: &[String]) -> Vec<Value> {
    let mut subscriptions = vec![
        json!({ "type": "workspace.created" }),
        json!({ "type": "workspace.updated" }),
        json!({ "type": "workspace.metadata_updated" }),
        json!({ "type": "workspace.renamed" }),
        json!({ "type": "workspace.moved" }),
        json!({ "type": "workspace.closed" }),
        json!({ "type": "workspace.focused" }),
        json!({ "type": "tab.created" }),
        json!({ "type": "tab.closed" }),
        json!({ "type": "tab.focused" }),
        json!({ "type": "tab.renamed" }),
        json!({ "type": "tab.moved" }),
        json!({ "type": "pane.created" }),
        json!({ "type": "pane.updated" }),
        json!({ "type": "pane.closed" }),
        json!({ "type": "pane.focused" }),
        json!({ "type": "pane.moved" }),
        json!({ "type": "pane.exited" }),
        json!({ "type": "pane.agent_detected" }),
        json!({ "type": "layout.updated" }),
        json!({ "type": "worktree.created" }),
        json!({ "type": "worktree.opened" }),
        json!({ "type": "worktree.removed" }),
    ];
    subscriptions.extend(
        pane_ids
            .iter()
            .map(|pane_id| json!({ "type": "pane.agent_status_changed", "pane_id": pane_id })),
    );
    subscriptions
}

fn agent_event_subscriptions(pane_ids: &[String]) -> Vec<Value> {
    pane_ids
        .iter()
        .map(|pane_id| json!({ "type": "pane.agent_status_changed", "pane_id": pane_id }))
        .collect()
}

#[cfg(not(unix))]
async fn herdr_event_stream(
    _session: &SessionConfig,
) -> anyhow::Result<BufReader<tokio::io::Empty>> {
    anyhow::bail!("direct Herdr event streaming is not implemented on this platform yet");
}

#[cfg(not(unix))]
async fn herdr_agent_event_stream(
    _session: &SessionConfig,
) -> anyhow::Result<BufReader<tokio::io::Empty>> {
    anyhow::bail!("direct Herdr event streaming is not implemented on this platform yet");
}

fn build_herdr_request(method: &str, params: Value) -> String {
    json!({
        "id": format!("gateway:{}", uuid::Uuid::new_v4()),
        "method": method,
        "params": params
    })
    .to_string()
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

/// Match a presented token against every device token. Every candidate is
/// compared in constant time and the loop is not short-circuited, so a caller
/// cannot learn which device matched from timing.
fn identify_device(devices: &[DeviceRecord], token: &str) -> Option<String> {
    if token.len() > 256 {
        return None;
    }
    let presented = hash_token(token);
    let mut matched = None;
    for device in devices {
        if constant_time_eq(presented.as_bytes(), device.token_hash.as_bytes()) {
            matched = Some(device.id.clone());
        }
    }
    matched
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
    let now = now_unix_ms();
    if let Some(device) = devices.iter_mut().find(|device| device.id == device_id) {
        let stale = now.saturating_sub(device.last_seen_unix_ms) >= DEVICE_LAST_SEEN_FLUSH_MS;
        device.last_seen_unix_ms = now;
        if stale {
            if let Err(err) = write_devices(&devices) {
                // Losing a last-seen timestamp must not fail the request.
                eprintln!("failed to persist device last-seen: {err:#}");
            }
        }
    }
    Ok(device_id)
}

/// The local manage UI's credential, which authorises nothing but reading the
/// pending pairing code.
fn require_admin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    let token = bearer_token(headers)?;
    if token.len() > 256
        || !constant_time_eq(hash_token(token).as_bytes(), config.token_hash.as_bytes())
    {
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
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

fn insert_opt<T: Serialize>(map: &mut serde_json::Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
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
    let path = config_path
        .map(Into::into)
        .unwrap_or(config_dir()?.join(CONFIG_FILE));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::config_dir()
        .context("failed to locate config directory")?
        .join("herdr-gateway"))
}

fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::data_dir()
        .or_else(dirs::config_dir)
        .context("failed to locate state directory")?
        .join("herdr-gateway"))
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
    let path = state_dir()?.join(DEVICES_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read device file {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_devices(devices: &[DeviceRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    write_secret_file(
        &dir.join(DEVICES_FILE),
        &serde_json::to_vec_pretty(devices)?,
    )
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
        let mut devices = read_devices()?;
        let count = devices.len();
        devices.clear();
        write_devices(&devices)?;
        println!("revoked {count} device token(s)");
        return Ok(());
    }
    let Some(device_id) = device_id else {
        anyhow::bail!("pass a device id from `gateway devices`, or --all");
    };
    if !revoke_device_by_id(&device_id)? {
        anyhow::bail!("device {device_id} not found");
    }
    println!("revoked device {device_id}");
    Ok(())
}

fn revoke_device_by_id(device_id: &str) -> anyhow::Result<bool> {
    let mut devices = read_devices()?;
    let previous_len = devices.len();
    devices.retain(|device| device.id != device_id);
    if devices.len() == previous_len {
        return Ok(false);
    }
    write_devices(&devices)?;
    Ok(true)
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

fn read_pairing_file() -> anyhow::Result<PairingFile> {
    let bytes = std::fs::read(config_dir()?.join(PAIRING_FILE))?;
    Ok(serde_json::from_slice(&bytes)?)
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

const GATEWAY_PROCESS_NAME: &str = "herdr-gateway";

/// Find gateway processes listening on `port`. Anything that is not the gateway
/// is filtered out by process name: `stop` must never kill an unrelated service
/// that happens to hold the port.
fn gateway_listener_pids(port: u16) -> anyhow::Result<Vec<u32>> {
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
            .filter(|pid| process_is_gateway(*pid))
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
            if !line.contains(&port_suffix) || !line.contains(GATEWAY_PROCESS_NAME) {
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

/// Confirm a pid really belongs to the gateway before signalling it.
fn process_is_gateway(pid: u32) -> bool {
    ProcessCommand::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(GATEWAY_PROCESS_NAME))
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

fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        write_secret_dir_gitignore(dir);
        lock_down_secret_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies when the file is created, so a file written by an
        // older build keeps its old permissions until they are set explicitly.
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
    // generic "Herdr Server". Fall back to the real machine name so a fresh
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
    "Herdr Server".into()
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

fn valid_pairing_code(code: &str) -> bool {
    code.len() == PAIRING_CODE_LENGTH
        && code.as_bytes()[4] == b'-'
        && code
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || PAIRING_CODE_ALPHABET.contains(&byte))
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

fn hash_token(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

fn openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Herdr Gateway API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Token-protected mobile API for controlling a local Herdr server through the Herdr socket API. Human-readable text is localized: send X-Muqun-Locale (or Accept-Language) with `en` or `zh-TW`. Error `code` values, decision names and other wire vocabulary are the same bytes in every locale."
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
            "/health": { "get": simple_endpoint("Gateway and Herdr health") },
            "/api/meta": { "get": simple_endpoint("Gateway API and Herdr compatibility metadata") },
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
            "/api/sessions/{sessionId}/assets": {
                "get": {
                    "summary": "List files this session's workspaces produced recently, newest first",
                    "description": "Unified content model, schema version 1.0.0. The response is the versioned envelope: schema_version, capabilities, and data, with the assets under data.assets. Assets are fed by the Herdr worktree events the gateway subscribes to, and by an mtime scan of the session's workspace roots, which is what a cold start uses. The scan is shallow, budgeted, and skips dot directories, dependency directories, and build output.",
                    "parameters": [
                        path_param("sessionId"),
                        query_param("since", "Unix milliseconds, the same unit as modified_unix_ms; only files modified strictly after this are returned"),
                        query_param("limit", "How many assets to return, 1 to 200, default 50"),
                        query_param("kind", "Comma-separated allow-list of kinds -- image, markdown, text, pdf, binary -- filtered during the scan, so kind=image&limit=50 answers with the 50 newest images rather than the images among the 50 newest files. Absent or empty means every kind; a value outside the taxonomy matches nothing rather than erroring. The applied list is echoed back as data.kind"),
                        query_param("path", "Resolve one absolute path exactly, for a file path tapped in terminal output. Takes precedence over since and limit. Answers with one asset, or with none when the path does not canonicalize to a file inside a workspace root -- a fenced-out path is a miss, not an error")
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
            "/api/sessions": { "get": simple_endpoint("List configured Herdr sessions") },
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
            "/api/sessions/{sessionId}/snapshot": { "get": session_endpoint("Return Herdr session.snapshot") },
            "/api/sessions/{sessionId}/workspaces": {
                "get": session_endpoint("List Herdr workspaces"),
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
                "get": session_endpoint("List Herdr tabs"),
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
            "/api/sessions/{sessionId}/panes": { "get": session_endpoint("List Herdr panes") },
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
                    "summary": "Zoom a pane for a single-panel viewport",
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
                    "description": "With `branch_name`, the task gets its own git worktree; without it, the task runs in the repo as it stands. `repo_path` must be, or be inside, a workspace this session already has open -- anything else is 403, and a symlink out of one resolves to the outside path and fails there too. `branch_name` is held to letters, digits, dot, underscore, dash and slash, with `..`, a leading dash, and dot-leading segments refused, so it can only ever be a ref and never an argument. The agent is started with Herdr's `agent.start`, which waits for it to become interactive before the prompt is sent. Asking twice for the same branch reuses the existing checkout rather than making a second one.",
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
                            "cwd": { "type": "string", "description": "Absolute path, inside a workspace this session has open; omit to take Herdr's default" },
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
                    "description": "A picker for spawn, and deliberately not a directory browser: it answers with the cwds Herdr reports for the panes that exist right now, deduplicated and held to the same rule the asset scan uses, so the filesystem root and a bare home directory are not on it. Each entry carries path, name, the pane and workspace it came from, and git, which says whether the directory is a checkout. Nothing here can be used to walk the host.",
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
            "/api/sessions/{sessionId}/agents": { "get": session_endpoint("List Herdr agents") },
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
                        query_param("format", "Output format: text or ansi")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/parts": {
                "get": {
                    "summary": "Read the pane's transcript normalized into content-model parts",
                    "description": "Unified content model, schema version 1.4.0. Same envelope as the asset endpoints: schema_version, capabilities, and data, with the ordered parts under data.parts. Two sources can answer, and data.pane.parts says which did. native: the agent runs a protocol the gateway was pointed at (opencode's server API today), so a tool's exit code, the patch an edit produced, the checklist a todo write submitted and any pending permission arrive as data; range then spans the adapter's own rendering, which is the parts' fallback_text joined by newlines. dictionary: the pane's recent-unwrapped text read through the marker table of whichever agent Herdr reports -- Claude Code, Qoder, Codex and opencode. text: no table covers this pane, so everything degraded to prose, which is an answer and not an error. Whichever source answered, every part carries fallback_text verbatim, so an unknown type still renders and a source that drifts loses structure and never loses content. data.pane.composer carries the slash commands this agent understands and whether @ file mentions make sense, and is absent entirely for an agent the gateway has no table for. The raw output endpoint is unchanged and remains the fallback path.",
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
                    "description": "Answers paths only -- no contents, no sizes, no absolute paths. The only directory searched is the pane's own working directory as Herdr reports it, canonicalized, which is the same fence the asset API is gated on; a pane sitting at the filesystem root or straight in the home directory has no workspace and answers with an empty list rather than an error, so this cannot be used to probe the host. Symlinks are never followed, and dot, dependency and build directories are skipped, so nothing outside the root can be named. The query is a fuzzy subsequence match over the relative path, ranked so that the file name beats the directories above it; an empty query answers with the shallowest files, which is what a picker shows before anything is typed. kind is decided from the name alone because nothing is read -- the asset content endpoint sniffs the bytes again when a file is actually opened.",
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
                    "summary": "Send Herdr key names to a pane",
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
        "502": { "description": "Herdr socket unavailable or returned an error" }
    })
}

const DOCS_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Herdr Gateway API Docs</title>
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

    fn test_config(token: &str) -> Config {
        Config {
            server_id: "server-1".into(),
            label: "test".into(),
            listen: "127.0.0.1:23100".into(),
            public_url: "http://127.0.0.1:23100".into(),
            token_hash: hash_token(token),
            sessions: vec![SessionConfig {
                id: "default".into(),
                label: "Default".into(),
                socket_path: "/tmp/herdr.sock".into(),
            }],
            agent_commands: BTreeMap::new(),
            rich_agent_pushes: false,
        }
    }

    fn test_device(id: &str, token: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: format!("device {id}"),
            token_hash: hash_token(token),
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

    #[test]
    fn event_filter_matches_dot_and_underscore_and_reads_the_event_field() {
        assert_eq!(normalize_event_name("pane.updated"), "pane_updated");
        assert_eq!(normalize_event_name(" pane_updated "), "pane_updated");
        assert_eq!(
            herdr_event_name(r#"{"event":"pane_updated","data":{}}"#).as_deref(),
            Some("pane_updated")
        );
        // A line the gateway cannot parse is forwarded rather than dropped, so a
        // filter never silently swallows an event shape we did not anticipate.
        assert_eq!(herdr_event_name("not json"), None);
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
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(valid_pairing_code(&code));
        assert!(!valid_pairing_code("ABCD2345"));
        assert!(!valid_pairing_code("abcD-2345"));
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
            assert!(valid_pairing_code(&code), "{code} is not a pairing code");
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

    #[test]
    fn pairing_code_is_consumed_after_one_successful_claim() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_pairing_code(&mut pending, "request-1", "2345-6789", 1_001),
            Ok(())
        );
        assert!(pending.is_none());
        assert_eq!(
            consume_pairing_code(&mut pending, "request-1", "2345-6789", 1_002),
            Err(PairingCodeError::Missing)
        );
    }

    #[test]
    fn expired_pairing_code_is_rejected_and_cleared() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_pairing_code(
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
                consume_pairing_code(&mut pending, "request-1", "AAAA-AAAA", 1_001),
                Err(PairingCodeError::Invalid)
            );
        }
        assert!(pending.is_none());
    }

    #[test]
    fn herdr_request_shape_matches_socket_api() {
        let encoded = build_herdr_request("pane.read", json!({ "pane_id": "w1:p1" }));
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["method"], "pane.read");
        assert_eq!(value["params"]["pane_id"], "w1:p1");
        assert!(value["id"].as_str().unwrap().starts_with("gateway:"));
    }

    #[test]
    fn stream_pane_read_becomes_inline_update() {
        let read = json!({
            "result": {
                "read": {
                    "pane_id": "w1:p2",
                    "revision": 42,
                    "text": "hello\n"
                }
            }
        });
        let frame = stream_pane_frame(&read).unwrap();
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
    fn stream_pane_update_requires_a_revision() {
        let read = json!({ "result": { "read": { "text": "hello" } } });
        assert!(stream_pane_frame(&read).is_none());
    }

    #[test]
    fn agent_status_subscriptions_are_scoped_to_each_pane() {
        let subscriptions = event_subscriptions(&["w1:p1".into(), "w2:p3".into()]);
        let agent_subscriptions = subscriptions
            .iter()
            .filter(|subscription| subscription["type"] == "pane.agent_status_changed")
            .collect::<Vec<_>>();

        assert_eq!(agent_subscriptions.len(), 2);
        assert_eq!(agent_subscriptions[0]["pane_id"], "w1:p1");
        assert_eq!(agent_subscriptions[1]["pane_id"], "w2:p3");
        assert!(agent_subscriptions
            .iter()
            .all(|subscription| subscription["pane_id"].is_string()));

        let watcher_subscriptions = agent_event_subscriptions(&["w1:p1".into(), "w2:p3".into()]);
        assert_eq!(
            watcher_subscriptions,
            vec![
                json!({ "type": "pane.agent_status_changed", "pane_id": "w1:p1" }),
                json!({ "type": "pane.agent_status_changed", "pane_id": "w2:p3" })
            ]
        );
    }

    #[test]
    fn herdr_compatibility_requires_protocol_17() {
        assert_eq!(HERDR_PROTOCOL_MIN, 17);
        assert_eq!(HERDR_PROTOCOL_MAX, 17);
    }

    #[test]
    fn split_result_uses_protocol_17_pane_shape() {
        let response = json!({
            "result": {
                "type": "pane_created",
                "pane": { "pane_id": "w1:p2" }
            }
        });
        assert_eq!(created_pane_id(&response), Some("w1:p2"));
    }

    #[test]
    fn pairing_payload_contains_mobile_connection_fields() {
        let payload = PairingPayload {
            kind: "herdr-gateway".into(),
            server_id: "server-1".into(),
            label: "machine".into(),
            url: "http://100.1.2.3:23100".into(),
            token: "secret".into(),
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["kind"], "herdr-gateway");
        assert_eq!(value["url"], "http://100.1.2.3:23100");
        assert_eq!(value["token"], "secret");
    }

    #[test]
    fn manager_qr_uses_the_current_config_fields() {
        assert_eq!(
            pairing_qr_offer("http://100.1.2.3:23847", "server-1"),
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
    fn everything_that_is_not_an_image_is_refused() {
        // This first version carries images only. A document or a text file is
        // a 415 like any other unrecognised content, so nothing without an
        // image magic number can be parked on the host.
        assert!(sniff_upload_kind(b"%PDF-1.7\n1 0 obj").is_none());
        assert!(sniff_upload_kind(b"# notes\n\nplain, with no magic number\n").is_none());
        assert!(sniff_upload_kind(b"{\"a\":1}").is_none());
        assert!(sniff_upload_kind(b"a,b,c\n1,2,3\n").is_none());
        assert!(sniff_upload_kind(b"2026-07-25 12:00:00 INFO started\n").is_none());
        assert!(sniff_upload_kind(b"PK\x03\x04").is_none());
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

        let listed = index.session_assets("default", None, 10);
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
        assert_eq!(index.session_assets("default", Some(2_000), 10).len(), 1);
        assert_eq!(index.session_assets("default", Some(3_000), 10).len(), 0);
        assert_eq!(index.session_assets("default", None, 1).len(), 1);
        assert_eq!(index.session_assets("other", None, 10).len(), 0);

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
        let response = session_assets(
            State(state.clone()),
            Path("default".into()),
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
            Path("default".into()),
            Query(asset_listing_query(Some("Image"), 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(response.0["data"]["kind"], json!(["image"]));
        assert_eq!(response.0["data"]["assets"][0]["kind"], "image");
        let unfiltered = session_assets(
            State(state.clone()),
            Path("default".into()),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(unfiltered.0["data"]["kind"], json!([]));

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
                { "pane_id": "wA:p1", "workspace_id": "wA", "cwd": "/Users/okk/.repos/muqun" },
                // A second pane in the same directory is the same root.
                { "pane_id": "wA:p2", "workspace_id": "wA", "cwd": "/Users/okk/.repos/muqun" },
                { "pane_id": "wB:p1", "workspace_id": "wB", "foreground_cwd": "/Users/okk/.ws/api" },
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
        assert_eq!(roots[1].workspace_id.as_deref(), Some("wB"));
        assert!(pane_list_roots("default", &json!({ "result": {} })).is_empty());
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
        assert!(spec["paths"]["/api/sessions/{sessionId}/panes/{paneId}/output"].is_object());
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
        assert!(spec["paths"]["/api/sessions/{sessionId}/assets"]["get"].is_object());
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
            spec["paths"]["/api/sessions/{sessionId}/assets"]["get"]["responses"]["200"]["content"]
                ["application/json"]["schema"]["properties"]["schema_version"]["const"],
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
                [4]["name"],
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
        assert!(GATEWAY_API_VERSION.starts_with("1.5."));
        assert_eq!(GATEWAY_API_MAJOR, 1);
        assert!(API_CAPABILITIES.contains(&"tasks"));
        assert!(API_CAPABILITIES.contains(&"agent_catalog"));
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

    /// Protocol 17 answers `workspace.create`, `worktree.create` and
    /// `worktree.open` with the same workspace/tab/root_pane trio.
    fn worktree_created_response() -> Value {
        json!({
            "id": "1",
            "result": {
                "type": "worktree_created",
                "workspace": { "workspace_id": "ws-9", "number": 2, "label": "task/594",
                               "focused": false, "pane_count": 1, "tab_count": 1,
                               "active_tab_id": "tab-9", "agent_status": "unknown" },
                "tab": { "tab_id": "tab-9" },
                "root_pane": { "pane_id": "pane-9", "terminal_id": "t-9", "workspace_id": "ws-9",
                               "tab_id": "tab-9", "focused": false, "agent_status": "unknown",
                               "revision": 1 },
                "worktree": { "path": "/Users/dev/code/muqun-task-594", "branch": "task/594",
                              "is_bare": false, "is_detached": false, "is_prunable": false,
                              "is_linked_worktree": true, "label": "task/594" }
            }
        })
    }

    #[test]
    fn a_created_place_is_read_out_of_the_protocol_17_response() {
        let place = workspace_place(&worktree_created_response(), None, false).unwrap();
        assert_eq!(place.workspace_id, "ws-9");
        assert_eq!(place.pane_id, "pane-9");
        assert_eq!(
            place.worktree_path.as_deref(),
            Some("/Users/dev/code/muqun-task-594")
        );
        assert!(!place.reused);

        // workspace.create carries no worktree, so the caller's own path, if it
        // has one, is what fills the field.
        let created = json!({
            "id": "1",
            "result": {
                "type": "workspace_created",
                "workspace": { "workspace_id": "ws-1" },
                "tab": { "tab_id": "tab-1" },
                "root_pane": { "pane_id": "pane-1" }
            }
        });
        let place = workspace_place(&created, None, false).unwrap();
        assert_eq!(place.pane_id, "pane-1");
        assert_eq!(place.worktree_path, None);
        let place = workspace_place(&created, Some("/tmp/wt".into()), true).unwrap();
        assert_eq!(place.worktree_path.as_deref(), Some("/tmp/wt"));
        assert!(place.reused);

        // A response missing the pane is not silently treated as a success:
        // there would be nowhere to start the agent.
        let no_pane = json!({ "id": "1", "result": { "workspace": { "workspace_id": "ws-1" } } });
        assert!(workspace_place(&no_pane, None, false).is_none());
        let herdr_error = json!({ "id": "1", "error": { "code": "not_found", "message": "no" } });
        assert!(workspace_place(&herdr_error, None, false).is_none());
    }

    #[test]
    fn an_existing_checkout_of_the_branch_is_found_before_another_one_is_made() {
        let listing = json!({
            "id": "1",
            "result": {
                "type": "worktree_list",
                "source": { "repo_key": "k", "repo_name": "muqun", "repo_root": "/repo",
                            "source_checkout_path": "/repo" },
                "worktrees": [
                    { "path": "/repo", "branch": "refs/heads/main", "is_bare": false,
                      "is_detached": false, "is_prunable": false, "is_linked_worktree": false,
                      "label": "main" },
                    { "path": "/repo-task-594", "branch": "task/594", "is_bare": false,
                      "is_detached": false, "is_prunable": false, "is_linked_worktree": true,
                      "label": "task/594" },
                    { "path": "/repo-detached", "branch": null, "is_bare": false,
                      "is_detached": true, "is_prunable": false, "is_linked_worktree": true,
                      "label": "detached" }
                ]
            }
        });
        // Herdr reports refs either way round, and both have to match.
        assert_eq!(
            worktree_for_branch(&listing, "task/594").as_deref(),
            Some("/repo-task-594")
        );
        assert_eq!(
            worktree_for_branch(&listing, "main").as_deref(),
            Some("/repo")
        );
        assert_eq!(worktree_for_branch(&listing, "nope"), None);
        // A detached checkout has no branch and must never be matched by one.
        assert_eq!(worktree_for_branch(&listing, ""), None);
        assert_eq!(worktree_for_branch(&json!({}), "main"), None);
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

    #[test]
    fn a_split_hangs_off_the_focused_pane_of_the_tab_that_was_named() {
        let panes = json!({
            "result": { "panes": [
                { "pane_id": "w1:p1", "tab_id": "t1", "focused": false },
                { "pane_id": "w1:p2", "tab_id": "t1", "focused": true },
                { "pane_id": "w2:p1", "tab_id": "t2", "focused": true }
            ] }
        });

        // The focused pane, because that is the one the user was looking at.
        assert_eq!(pane_to_split(&panes, "t1").as_deref(), Some("w1:p2"));
        assert_eq!(pane_to_split(&panes, "t2").as_deref(), Some("w2:p1"));
        // A tab this session does not have is nothing to split, which is what
        // makes an unknown tab_id a refusal rather than a spawn elsewhere.
        assert_eq!(pane_to_split(&panes, "t9"), None);
        assert_eq!(pane_to_split(&json!({}), "t1"), None);

        // No pane in the tab is focused: any of them will do, in Herdr's order.
        let unfocused = json!({
            "result": { "panes": [{ "pane_id": "w3:p1", "tab_id": "t3" }] }
        });
        assert_eq!(pane_to_split(&unfocused, "t3").as_deref(), Some("w3:p1"));
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
            vec![
                AssetRoot {
                    path: repo.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wA".into()),
                    pane_id: Some("wA:p1".into()),
                },
                AssetRoot {
                    path: plain.clone(),
                    session_id: "default".into(),
                    workspace_id: Some("wB".into()),
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

    #[ignore = "the gateway keeps no rows while scrollback::SCROLLBACK_ENABLED is false (1.2.0)"]
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
            }),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        pane_read_text(&response.0).unwrap_or_default()
    }

    /// The whole point, end to end: Herdr keeps one screen, the gateway watched
    /// four, and the reader can ask for all four.
    #[ignore = "the gateway keeps no rows while scrollback::SCROLLBACK_ENABLED is false (1.2.0)"]
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

    /// And having kept them, it says so where the reader's affordance looks --
    /// on the pane, not on the output.
    #[ignore = "the gateway keeps no rows while scrollback::SCROLLBACK_ENABLED is false (1.2.0)"]
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
