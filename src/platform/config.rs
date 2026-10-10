//! Application configuration: CLI-facing types, persisted configuration, and
//! the shared tuning constants.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::agents;
use crate::backend::BackendKind;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SetupBackend {
    Herdr,
    Tmux,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TransportEncryptionMode {
    #[default]
    Required,
    Disabled,
}

impl TransportEncryptionMode {
    pub(crate) const fn as_str(self) -> &'static str {
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
pub(crate) struct Config {
    /// Captured-prefix storage selection; changing it requires a restart.
    #[serde(default, skip_serializing_if = "HistoryConfig::is_default")]
    pub(crate) history: HistoryConfig,
    pub(crate) server_id: String,
    pub(crate) label: String,
    pub(crate) listen: String,
    pub(crate) public_url: String,
    /// Hash of the admin token. The admin token lives in `pairing.json` and is
    /// used by the local `manage` UI; paired devices get their own tokens.
    pub(crate) token_hash: String,
    /// Controls how newly paired devices authenticate their HTTP transport.
    /// Existing encrypted device records keep working after this changes.
    #[serde(default, skip_serializing_if = "is_required_transport")]
    pub(crate) transport_encryption: TransportEncryptionMode,
    /// Answer every device route without asking for a token at all.
    ///
    /// For a mock or test double that has no pairing to offer and talks to a
    /// gateway bound to loopback. It is not a transport setting and is not
    /// implied by `transport_encryption: disabled`: cleartext means no
    /// envelope, never no authentication. Off unless the owner writes it, and
    /// said loudly at startup when it is on.
    ///
    /// Omitted from a written config when false, so an existing `config.json`
    /// round-trips untouched.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) dev_unauthenticated: bool,
    pub(crate) sessions: Vec<SessionConfig>,
    /// Explicit opt-in by session ID; adding a backend never enables startup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) autostart_backends: Vec<String>,
    /// Agent kind -> the executable `GET /api/agents/catalog` looks for on
    /// `PATH`. Only needed when a kind's binary is named something else on this
    /// machine; absent, every kind probes for its own name. Optional and
    /// omitted when empty, so an existing `config.json` keeps working untouched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) agent_commands: BTreeMap<String, String>,
    /// Agent kind -> the arguments every agent the gateway launches for that
    /// kind starts with, in place of the built-in defaults (bypass-permission
    /// flags for `codex` and `claude`; see `agents::tasks::agent_launch_args`).
    /// A non-empty list replaces the defaults; an empty list turns them off
    /// for that kind. Omitted when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) agent_launch_args: BTreeMap<String, Vec<String>>,
    /// Variables every agent the gateway launches starts with -- typically a
    /// proxy (`HTTPS_PROXY`, ...) that the owner's interactive shell sets but
    /// the gateway's own environment lacks. Values are never logged.
    #[serde(default, skip_serializing_if = "AgentEnv::is_empty")]
    pub(crate) agent_env: AgentEnv,
    /// Mark the directory an agent is launched in as trusted, so it does not
    /// stop at "Do you trust the files in this folder?" with nobody at the
    /// terminal: in Claude Code's own config
    /// (`projects["<dir>"].hasTrustDialogAccepted` in `~/.claude.json`), and
    /// with Codex's `-c projects=…` for that run. On by default -- the
    /// owner's choice, for agents launched from their phone -- and only ever
    /// for directories inside their home; `false` does neither. Omitted from
    /// a written config while on.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub(crate) agent_trust_folders: bool,
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
    pub(crate) rich_agent_pushes: bool,
    /// How this gateway gets an OpenCode agent to talk to. Absent, it starts
    /// one when it cannot find a running service, which is the behaviour the
    /// owner asked for; `{"autostart": false}` leaves that to them.
    #[serde(default, skip_serializing_if = "is_default_opencode")]
    pub(crate) opencode: agents::OpencodeConfig,
    /// Configuration for the DeepSeek agent adapter
    #[serde(default, skip_serializing_if = "is_default_deepseek")]
    pub(crate) deepseek: agents::DeepseekConfig,
    /// The T3 Code agent adapter: off unless `enabled` or `url` is set.
    #[serde(default, skip_serializing_if = "is_default_t3")]
    pub(crate) t3: agents::T3Config,
    /// The shared SQLite store. Omitted from a written config while it is the
    /// default, so an existing `config.json` round-trips untouched.
    #[serde(default, skip_serializing_if = "StoreConfig::is_default")]
    pub(crate) store: StoreConfig,
    /// The `/mcp` endpoint for agents on this machine. On unless switched off.
    #[serde(default, skip_serializing_if = "McpConfig::is_default")]
    pub(crate) mcp: McpConfig,
    /// The owner's other computers. See `FleetConfig`.
    #[serde(default, skip_serializing_if = "FleetConfig::is_default")]
    pub(crate) fleet: FleetConfig,
}

/// `{"mcp": {"enabled": false}}` removes `/mcp` and `/api/mcp/tokens*`, stops
/// the token sweeper and gives OpenCode no `MUQUN_MCP_*` environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpConfig {
    #[serde(default = "mcp_enabled_default")]
    pub(crate) enabled: bool,
}

fn mcp_enabled_default() -> bool {
    true
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl McpConfig {
    pub(crate) fn is_default(&self) -> bool {
        *self == McpConfig::default()
    }
}

/// `{"fleet": {"accept_delegations": false}}`: other computers this one is
/// linked with may not open tasks here (`POST /api/tasks` with a gateway
/// token answers 403 `delegations_disabled`). On by default; the phone's
/// "Accept work from other computers" switch writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FleetConfig {
    #[serde(default = "default_true")]
    pub(crate) accept_delegations: bool,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            accept_delegations: true,
        }
    }
}

impl FleetConfig {
    pub(crate) fn is_default(&self) -> bool {
        *self == FleetConfig::default()
    }
}

/// Whether the shared SQLite store is encrypted, and where its key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StoreEncryption {
    /// Plain SQLite. For machines where the state directory is already encrypted.
    Off,
    /// SQLCipher, keyed by `store.key` in the state directory.
    #[default]
    File,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoreConfig {
    #[serde(default)]
    pub(crate) encryption: StoreEncryption,
}

impl StoreConfig {
    pub(crate) fn is_default(&self) -> bool {
        *self == StoreConfig::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HistoryStorage {
    #[default]
    Memory,
    Sqlite,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryConfig {
    #[serde(default)]
    pub(crate) storage: HistoryStorage,
}

impl HistoryConfig {
    fn is_default(&self) -> bool {
        self.storage == HistoryStorage::Memory
    }
}

/// `skip_serializing_if` for the OpenCode block, so an existing `config.json`
/// round-trips untouched until someone changes something.
pub(crate) fn is_default_opencode(config: &agents::OpencodeConfig) -> bool {
    config.enabled && config.autostart && config.binary.is_none()
}

pub(crate) fn is_default_deepseek(config: &agents::DeepseekConfig) -> bool {
    !config.enabled && config.endpoint.is_none() && config.token.is_none()
}

pub(crate) fn is_default_t3(config: &agents::T3Config) -> bool {
    *config == agents::T3Config::default()
}

pub(crate) fn is_required_transport(mode: &TransportEncryptionMode) -> bool {
    *mode == TransportEncryptionMode::Required
}

/// `skip_serializing_if` for a flag whose absence is its default.
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

/// `skip_serializing_if` for a flag that is on when absent.
fn is_true(value: &bool) -> bool {
    *value
}

fn default_true() -> bool {
    true
}

/// `agent_env`: a map whose `Debug` names the variables but never shows a
/// value, so a logged config cannot leak a proxy credential.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct AgentEnv(pub(crate) BTreeMap<String, String>);

impl AgentEnv {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The variables whose names a shell and `env` accept
    /// (`[A-Za-z_][A-Za-z0-9_]*`); anything else is dropped with a warning
    /// that names the key only.
    pub(crate) fn usable(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .filter(|(key, _)| {
                let ok = is_env_name(key);
                if !ok {
                    tracing::warn!("agent_env: ignoring {key:?}, not a valid variable name");
                }
                ok
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

impl std::fmt::Debug for AgentEnv {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_set()
            .entries(self.0.keys().map(|key| format!("{key}=<redacted>")))
            .finish()
    }
}

fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

impl Config {
    pub(crate) fn port(&self) -> u16 {
        self.listen
            .parse::<SocketAddr>()
            .map(|addr| addr.port())
            .unwrap_or(DEFAULT_PORT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionConfig {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) socket_path: String,
    /// Omitted for every existing config, where Herdr remains the default.
    #[serde(default, skip_serializing_if = "BackendKind::is_herdr")]
    pub(crate) backend: BackendKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PairingPayload {
    pub(crate) kind: String,
    pub(crate) server_id: String,
    pub(crate) label: String,
    pub(crate) url: String,
    pub(crate) token: String,
    /// QR-only bootstrap secret used to protect pairing before a device token
    /// exists. Older pairing files are upgraded the next time setup runs.
    #[serde(default)]
    pub(crate) transport_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PairingFile {
    pub(crate) payload: PairingPayload,
}

pub(crate) struct PublicUrlSelection {
    pub(crate) url: String,
    pub(crate) source: String,
    pub(crate) listen_host: String,
}

pub(crate) const CONFIG_FILE: &str = "config.json";

pub(crate) const PAIRING_FILE: &str = "pairing.json";

pub(crate) const PUSH_TOKENS_FILE: &str = "push-tokens.json";

pub(crate) const DEVICES_FILE: &str = "devices.json";

/// The generation of `devices.json` that the last write replaced, kept so a
/// device file that goes bad is recoverable rather than terminal.
pub(crate) const DEVICES_BACKUP_FILE: &str = "devices.json.bak";

pub(crate) const PID_FILE: &str = "gateway.pid";

pub(crate) const LOG_FILE: &str = "gateway.log";

pub(crate) const HERDR_PLUGIN_IMPORT_MARKER: &str = ".herdr-plugin-imported";

// Deliberately outside the common-service range and outside the OS ephemeral
// ranges (Linux 32768-60999, macOS 49152-65535) so setup rarely collides.
pub(crate) const DEFAULT_PORT: u16 = 23847;

// Scrollback a phone can page back through. Herdr keeps far more; this is the
// ceiling on a single read, and 1000 ran out after a few screens of an agent
// transcript.
pub(crate) const MAX_OUTPUT_LINES: u32 = 5000;

/// Normalizing costs more than streaming text does, and a phone opens on the
/// last screen or two, so the parts endpoint asks for less than the raw one by
/// default. A client that wants the whole scrollback still says so.
pub(crate) const PARTS_DEFAULT_LINES: u32 = 400;

pub(crate) const MAX_SEND_TEXT_BYTES: usize = 64 * 1024;

pub(crate) const PAIRING_CODE_CHARACTER_COUNT: usize = 8;

pub(crate) const PAIRING_CODE_TTL_MS: u128 = 5 * 60 * 1000;

pub(crate) const MAX_PAIRING_CODE_ATTEMPTS: u8 = 8;

pub(crate) const PAIRING_RATE_LIMIT_WINDOW_MS: u128 = 10 * 60 * 1000;

pub(crate) const MAX_PAIRING_REQUESTS_PER_WINDOW: usize = 6;

pub(crate) const MAX_SEND_KEYS: usize = 32;

/// The argv a task may add to the agent's own command line. Generous enough for
/// any flag list a person types, bounded because everything a client sends is.
pub(crate) const MAX_AGENT_ARGS: usize = 32;

pub(crate) const MAX_AGENT_ARG_CHARS: usize = 512;

pub(crate) const MAX_WORKSPACE_LABEL_CHARS: usize = 120;

/// How often each agent pane is checked for a permission menu. An approval
/// blocks the agent outright, so the phone should hear about one in about the
/// time it takes to glance at the screen; the cost is one `pane.read` per agent
/// pane per tick.
pub(crate) const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// A permission menu is a screenful. Reading more only slows the poll down.
pub(crate) const APPROVAL_READ_LINES: u32 = 60;

/// Approval events are published from a background watcher and fanned out to
/// whatever event streams happen to be open. A slow client falls behind rather
/// than holding the watcher up.
pub(crate) const APPROVAL_EVENT_CAPACITY: usize = 64;

/// How many activity events one session's hub buffers for a slow subscriber.
///
/// Generous, because a subscriber that falls behind is dropped events, not
/// backpressure on the producer: a phone on a bad connection would otherwise
/// miss layout changes during a burst. Small enough that a session nobody is
/// reading holds nothing worth counting.
pub(crate) const ACTIVITY_EVENT_CAPACITY: usize = 128;

pub(crate) const MAX_PUSH_TOKENS: usize = 64;

pub(crate) const MAX_DEVICES: usize = 32;

pub(crate) const MAX_DEVICE_NAME_CHARS: usize = 80;

/// Device `last_seen` is kept in memory and only flushed to disk past this
/// interval so that routine polling does not rewrite the file on every request.
pub(crate) const DEVICE_LAST_SEEN_FLUSH_MS: u128 = 5 * 60 * 1000;

pub(crate) const MANAGE_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Herdr's general `pane.updated` subscription is intentionally coalesced and
/// can arrive several seconds after terminal output is already readable. While
/// a phone is actively viewing one pane, sample that pane locally and publish a
/// frame only when its actual content changes. Herdr's revision is coalesced
/// along with `pane.updated`, so it can remain stale even after `pane.read`
/// already exposes new text. This keeps the mobile SSE live without polling
/// every pane or sending unchanged terminal frames over the network.
pub(crate) const STREAM_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(150);

pub(crate) const STREAM_OUTPUT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// How often a live event stream re-checks that the device holding it is
/// still paired.
///
/// Every other route is authorised once, per request, and that is enough
/// because the request is over in milliseconds. An event stream is not: it is
/// authorised once and then runs for as long as the phone keeps it open,
/// which for a phone on a charger is days. So `DELETE /api/pairings/{id}` --
/// the button whose entire purpose is cutting off a device somebody no longer
/// controls -- removed the record while the revoked device carried on
/// receiving that session's terminal output, keystrokes and all, through the
/// stream it already had. Nothing timed it out either: the 15s keep-alive is
/// the gateway writing to the socket, so an idle connection never lapses.
///
/// Short enough that revocation is effective while somebody is still looking
/// at the screen they pressed it on; long enough that a stream costs one
/// mutex acquisition every few seconds and nothing else.
pub(crate) const STREAM_DEVICE_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

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
pub(crate) const HERDR_PROTOCOL_MIN: u64 = 17;

pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;

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
/// 1.5.0 adds three read-only pane routes in the same envelope -- `context`,
/// `git/status` and `git/diff` -- behind the `pane_context` and `git_diff`
/// capabilities; no existing payload changes.
pub(crate) const CONTENT_SCHEMA_VERSION: &str = "1.5.0";

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn the_store_block_is_left_out_when_default_and_strict_when_present() {
        let base = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": []
        });
        let config: Config = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(config.store.encryption, StoreEncryption::File);
        assert_eq!(serde_json::to_value(&config).unwrap(), base);

        let mut off = base.clone();
        off["store"] = json!({"encryption": "off"});
        let config: Config = serde_json::from_value(off.clone()).unwrap();
        assert_eq!(config.store.encryption, StoreEncryption::Off);
        assert_eq!(serde_json::to_value(&config).unwrap(), off);

        let mut unknown = base;
        unknown["store"] = json!({"foo": 1});
        assert!(serde_json::from_value::<Config>(unknown).is_err());
    }

    #[test]
    fn the_mcp_block_is_left_out_when_default_and_strict_when_present() {
        let base = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": []
        });
        let config: Config = serde_json::from_value(base.clone()).unwrap();
        assert!(config.mcp.enabled);
        assert_eq!(serde_json::to_value(&config).unwrap(), base);

        let mut off = base.clone();
        off["mcp"] = json!({"enabled": false});
        let config: Config = serde_json::from_value(off.clone()).unwrap();
        assert!(!config.mcp.enabled);
        assert_eq!(serde_json::to_value(&config).unwrap(), off);

        let mut empty = base.clone();
        empty["mcp"] = json!({});
        assert!(serde_json::from_value::<Config>(empty).unwrap().mcp.enabled);

        let mut unknown = base;
        unknown["mcp"] = json!({"path": "/x"});
        assert!(serde_json::from_value::<Config>(unknown).is_err());
    }

    #[test]
    fn history_storage_defaults_to_memory_roundtrips_sqlite_and_rejects_unknown_modes() {
        let base = json!({"server_id":"s", "label":"fixture", "listen":"127.0.0.1:23847", "public_url":"", "token_hash":"fixture", "sessions":[]});
        let default: Config = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(default.history.storage, super::HistoryStorage::Memory);
        assert!(serde_json::to_value(default)
            .unwrap()
            .get("history")
            .is_none());
        for storage in ["sqlite", "unknown"] {
            let mut configured = base.clone();
            configured["history"] = json!({"storage":storage});
            let parsed = serde_json::from_value::<Config>(configured);
            if storage == "sqlite" {
                assert_eq!(
                    serde_json::to_value(parsed.unwrap()).unwrap()["history"]["storage"],
                    "sqlite"
                );
            } else {
                assert!(parsed.is_err());
            }
        }
    }

    #[test]
    fn the_t3_block_is_read_from_config_json_and_left_out_when_default() {
        let base = json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": []
        });
        let config: Config = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(config.t3, agents::T3Config::default());
        assert!(!config.t3.wanted());
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            base,
            "an absent t3 block stays absent"
        );

        let mut with_t3 = base.clone();
        with_t3["t3"] = json!({
            "enabled": true,
            "url": "http://127.0.0.1:3773",
            "pairing_token": "from-t3-pair",
            "runtime_mode": "approval-required"
        });
        let config: Config = serde_json::from_value(with_t3.clone()).unwrap();
        assert!(config.t3.wanted());
        assert_eq!(config.t3.url.as_deref(), Some("http://127.0.0.1:3773"));
        assert_eq!(config.t3.pairing_token.as_deref(), Some("from-t3-pair"));
        assert_eq!(serde_json::to_value(&config).unwrap(), with_t3);
        assert!(
            !format!("{config:?}").contains("from-t3-pair"),
            "the logged config never shows the token"
        );
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
    fn agent_launch_args_and_agent_env_load_and_never_show_a_value() {
        let config: Config = serde_json::from_value(json!({
            "server_id": "s1",
            "label": "mac",
            "listen": "127.0.0.1:23847",
            "public_url": "https://example.ts.net",
            "token_hash": "abc",
            "sessions": [],
            "agent_launch_args": { "codex": [], "claude": ["--model", "opus"] },
            "agent_env": {
                "HTTPS_PROXY": "http://user:pw-q7xz@proxy:7890",
                "bad-name": "x",
                "1X": "y"
            }
        }))
        .unwrap();
        assert_eq!(config.agent_launch_args["codex"], Vec::<String>::new());
        assert_eq!(config.agent_launch_args["claude"], ["--model", "opus"]);
        assert!(config.agent_trust_folders, "on unless switched off");
        assert!(!format!("{config:?}").contains("pw-q7xz"));
        assert!(format!("{config:?}").contains("HTTPS_PROXY"));
        let usable = config.agent_env.usable();
        assert_eq!(usable.keys().collect::<Vec<_>>(), ["HTTPS_PROXY"]);
    }
}
