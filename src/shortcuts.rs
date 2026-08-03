//! What a pane can be driven with: the key row a client should offer, and the
//! slash commands the program in the pane understands.
//!
//! This lives in the gateway rather than in each client because it changes
//! whenever an agent changes, and the gateway is the piece a developer already
//! updates on their own machine. A client that reads it here picks up a new
//! agent without shipping a new build.
//!
//! Everything here is taken from what the programs themselves advertise in
//! their own footers and help output, not guessed.
//!
//! The slash-command tables themselves live in `composer.rs`, one per agent,
//! pinned by a snapshot. This endpoint and the pane's composer descriptor
//! answer out of the same table: two lists of the same agent's commands that
//! could disagree would be one list too many.
//!
//! # Adding an agent
//!
//! The tables below are only the defaults that ship with the gateway. They are
//! overlaid by `agents.json` in the config directory, so supporting a new agent
//! is an edit to a JSON file -- no rebuild of the gateway, and certainly no
//! release of any client:
//!
//! ```json
//! {
//!   "opencode": {
//!     "match": ["opencode"],
//!     "keys": [{ "label": "⇧tab", "key": "shift+tab", "description": "Cycle mode" }],
//!     "interrupt": "esc",
//!     "commands": [{ "command": "/model", "description": "Switch model",
//!                    "argumentHint": "[model]" }],
//!     "commandDirs": [{ "path": "~/.opencode/commands", "format": "markdown",
//!                       "source": "user" }]
//!   }
//! }
//! ```
//!
//! Every field is optional. A profile with only `match` still gets the shared
//! base keys, which is enough to drive any prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Bumped whenever the tables below change, so a client can cache a response
/// and know when to drop it. 4 moved the slash-command tables into
/// `composer.rs`, so this endpoint and the pane composer descriptor answer out
/// of one table, and added the opencode profile. 5 adds `interrupt` to every
/// profile and to this endpoint's answer: which key stops this particular
/// agent, which is not `ctrl+c` on any of them.
pub const KEYMAP_VERSION: u32 = 5;

/// Overlay file, read from the gateway's config directory on every request.
/// Re-read rather than cached so an edit takes effect on the next pane switch
/// instead of on the next gateway restart.
pub const AGENTS_FILE: &str = "agents.json";

/// A command as it goes out over the API, whether it came from the built-in
/// table or was found on disk.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCommand {
    pub command: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// "builtin" for the table below, "user"/"project"/"plugin" for a command
    /// found on disk. Lets a client show where a command came from, and makes
    /// it obvious when discovery found nothing.
    pub source: &'static str,
}

/// A key as it goes out over the API, from either the built-in table or the
/// overlay file.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedShortcut {
    pub label: String,
    pub key: String,
    pub description: String,
}

impl From<&Shortcut> for ResolvedShortcut {
    fn from(value: &Shortcut) -> Self {
        Self {
            label: value.label.to_owned(),
            key: value.key.to_owned(),
            description: value.description.to_owned(),
        }
    }
}

/// One agent's entry in `agents.json`. Everything is optional so a profile can
/// add commands without restating the keys, or vice versa.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentOverlay {
    /// Substrings matched against the agent name Herdr reports. Defaults to the
    /// profile's own key.
    #[serde(default)]
    r#match: Vec<String>,
    #[serde(default)]
    keys: Option<Vec<OverlayShortcut>>,
    /// The key that stops this agent mid-answer, when it is not `esc`. The one
    /// field here whose default is wrong loudly rather than quietly: a Stop
    /// button that sends the wrong key looks broken.
    #[serde(default)]
    interrupt: Option<String>,
    #[serde(default)]
    commands: Option<Vec<OverlayCommand>>,
    /// Accepted as `commandDirs` or `command_dirs`: the file is hand-written,
    /// and rejecting it over a casing choice would be a poor trade.
    #[serde(default, alias = "commandDirs")]
    command_dirs: Option<Vec<CommandDir>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayShortcut {
    label: String,
    key: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayCommand {
    command: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, alias = "argumentHint")]
    argument_hint: Option<String>,
}

/// Where an agent keeps the commands a developer wrote themselves. Data rather
/// than code, so a new agent's directory is one more line in `agents.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandDir {
    /// `~` expands to the home directory, `.` is relative to the pane's cwd.
    path: String,
    /// "markdown" for a directory of one file per command, "qoder-registry"
    /// for Qoder's single JSON file.
    #[serde(default = "default_dir_format")]
    format: String,
    #[serde(default = "default_dir_source")]
    source: String,
}

fn default_dir_format() -> String {
    "markdown".to_owned()
}

fn default_dir_source() -> String {
    "user".to_owned()
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Shortcut {
    /// What to print on the key. Short enough for a phone-sized button.
    pub label: &'static str,
    /// The key name to hand back to `pane.send_keys`.
    pub key: &'static str,
    /// Spoken form, for screen readers and tooltips.
    pub description: &'static str,
}

/// The shape of a built-in command here and in the composer descriptor is the
/// same shape, because it is the same table.
use crate::composer::BuiltinCommand as SlashCommand;

const fn key(label: &'static str, key: &'static str, description: &'static str) -> Shortcut {
    Shortcut {
        label,
        key,
        description,
    }
}

const fn cmd(name: &'static str, description: &'static str) -> SlashCommand {
    SlashCommand {
        name,
        description,
        args_hint: None,
    }
}

/// Answering a prompt, getting out of one, and deleting. Every pane needs
/// these. `shift+enter` is the only way to write a multi-line message from a
/// phone, so it is not optional.
const PRIMARY: &[Shortcut] = &[
    key("↵", "enter", "Enter"),
    key("⇧↵", "shift+enter", "Newline without sending"),
    key("ESC", "esc", "Escape"),
];

/// Still needed everywhere, but reached for less often than what the agent
/// itself advertises, so they sit after it.
const SECONDARY: &[Shortcut] = &[
    key("TAB", "tab", "Tab"),
    key("⌃C", "ctrl+c", "Interrupt"),
    key("⌫", "backspace", "Backspace"),
];

/// Readline editing and history. Only meaningful at a shell prompt.
const SHELL: &[Shortcut] = &[
    key("⌃D", "ctrl+d", "End of input"),
    key("⌃A", "ctrl+a", "Start of line"),
    key("⌃E", "ctrl+e", "End of line"),
    key("⌃K", "ctrl+k", "Clear to end of line"),
    key("⌃U", "ctrl+u", "Clear line"),
    key("⌃W", "ctrl+w", "Delete word"),
    key("⌃Y", "ctrl+y", "Paste"),
    key("⌃P", "ctrl+p", "Previous command"),
    key("⌃N", "ctrl+n", "Next command"),
    key("⌃R", "ctrl+r", "Reverse search"),
    key("⌃Z", "ctrl+z", "Suspend"),
    key("⌃L", "ctrl+l", "Clear screen"),
];

/// A modal editor wants none of the shell's line editing; these are the window
/// and scroll motions that are awkward to type on a phone.
/// A modal editor wants none of the shell's line editing; these are the window
/// and scroll motions that are awkward to type, plus the two commands you
/// cannot leave without. `:q` and `:wq` are sent as literal text rather than as
/// key names -- there is no "quit" key, and hunting for `:` on a phone keyboard
/// to escape an editor you opened by accident is the single worst moment in
/// driving nvim from a phone.
const EDITOR: &[Shortcut] = &[
    key("⌃W", "ctrl+w", "Window prefix"),
    key("⌃D", "ctrl+d", "Half page down"),
    key("⌃U", "ctrl+u", "Half page up"),
    key("⌃O", "ctrl+o", "Jump back"),
    key("⌃R", "ctrl+r", "Redo"),
    key("⌃V", "ctrl+v", "Visual block"),
];

/// Sent as text, not as keys. Only offered for a full-screen editor.
const EDITOR_COMMANDS: &[SlashCommand] = &[
    cmd(":w", "Write the file"),
    cmd(":q", "Quit"),
    cmd(":wq", "Write and quit"),
    cmd(":q!", "Quit without saving"),
];

/// Caps on what discovery will read, so a stray directory cannot turn one API
/// call into thousands of file reads.
const MAX_DISCOVERED_COMMANDS: usize = 64;
/// Front matter is at the head of the file, so nothing is lost by refusing to
/// read further. Without a cap, a command directory containing a large file --
/// or a symlink to `/dev/zero` -- turns one request into an unbounded read.
const MAX_COMMAND_FILE_BYTES: u64 = 64 * 1024;

/// Herdr rejects `home`, `end`, `pageup`, `pagedown`, `delete` and `insert`
/// with `invalid_key` -- checked by sending every key in this file to a real
/// pane. Word motions are the accepted equivalents and are what a phone
/// actually needs: jumping a word at a time beats hunting for a caret position.
const NAVIGATION: &[Shortcut] = &[
    key("←", "left", "Left"),
    key("↓", "down", "Down"),
    key("↑", "up", "Up"),
    key("→", "right", "Right"),
    key("⌥←", "alt+left", "Back one word"),
    key("⌥→", "alt+right", "Forward one word"),
];

/// From Claude Code's own footer: "esc to interrupt · ctrl+t to hide tasks ·
/// ctrl+b to run in background", and collapsed blocks marked "(ctrl+o to
/// expand)".
const CLAUDE_KEYS: &[Shortcut] = &[
    key("⇧TAB", "shift+tab", "Cycle permission mode"),
    key("⌃O", "ctrl+o", "Expand output"),
    key("⌃T", "ctrl+t", "Toggle tasks"),
    key("⌃B", "ctrl+b", "Run in background"),
    key("⌃R", "ctrl+r", "Transcript"),
    key("⌃L", "ctrl+l", "Clear screen"),
];

/// From Codex's footer: "Esc to cancel · Tab to amend · ctrl+e to explain".
const CODEX_KEYS: &[Shortcut] = &[
    key("⇧TAB", "shift+tab", "Cycle approval mode"),
    key("⌃E", "ctrl+e", "Explain"),
    key("⌃R", "ctrl+r", "Transcript"),
    key("⌃L", "ctrl+l", "Clear screen"),
];

/// From Qoder CLI's collapsed rows, marked "… +24 rows (Ctrl+O)".
const QODER_KEYS: &[Shortcut] = &[
    key("⇧TAB", "shift+tab", "Cycle mode"),
    key("⌃O", "ctrl+o", "Expand rows"),
    key("⌃R", "ctrl+r", "Transcript"),
    key("⌃L", "ctrl+l", "Clear screen"),
];

/// From opencode 1.18.0's own keybind defaults (`agent_cycle_reverse`,
/// `command_list`, `variant_cycle`, `session_rename`, `session_background`) and
/// its leader key, which is `ctrl+x`. The leader-prefixed bindings need two
/// keystrokes and are not what a phone row is for; these are the ones that are
/// one key on their own.
const OPENCODE_KEYS: &[Shortcut] = &[
    key("⇧TAB", "shift+tab", "Cycle agent"),
    key("⌃P", "ctrl+p", "List commands"),
    key("⌃T", "ctrl+t", "Cycle model variant"),
    key("⌃R", "ctrl+r", "Rename session"),
    key("⌃B", "ctrl+b", "Background subagents"),
    key("⌃X", "ctrl+x", "Leader key"),
];

/// What stops whatever is running, per profile.
///
/// `ctrl+c` is the shell's answer and the wrong one for an agent: in a TUI it
/// is at best ignored and at worst kills the session the user was in the middle
/// of. Every agent here says `esc` in its own footer -- Claude Code's "esc to
/// interrupt", Codex's "Esc to cancel", opencode's `session_interrupt` -- so a
/// Stop button that sends the same key everywhere is wrong on four agents out
/// of four.
pub const SHELL_INTERRUPT: &str = "ctrl+c";
const AGENT_INTERRUPT: &str = "esc";

struct Profile {
    id: &'static str,
    /// Matched against the agent name reported by Herdr, lowercased, as a
    /// substring: "Claude Code" and "claude-code" both resolve to "claude".
    agent_match: &'static [&'static str],
    keys: &'static [Shortcut],
    /// Which `composer.rs` table lists this agent's slash commands. The id
    /// differs from the profile's own where Herdr's agent name does
    /// ("qodercli" runs Qoder CLI), so it is named rather than assumed.
    commands: &'static str,
    /// The key that stops this agent mid-answer, from its own footer.
    interrupt: &'static str,
}

const AGENT_PROFILES: &[Profile] = &[
    Profile {
        id: "claude",
        agent_match: &["claude"],
        keys: CLAUDE_KEYS,
        commands: "claude",
        interrupt: AGENT_INTERRUPT,
    },
    Profile {
        id: "codex",
        agent_match: &["codex"],
        keys: CODEX_KEYS,
        commands: "codex",
        interrupt: AGENT_INTERRUPT,
    },
    Profile {
        id: "opencode",
        agent_match: &["opencode", "open-code"],
        keys: OPENCODE_KEYS,
        commands: "opencode",
        interrupt: AGENT_INTERRUPT,
    },
    Profile {
        id: "qodercli",
        agent_match: &["qoder"],
        keys: QODER_KEYS,
        commands: "qoder",
        interrupt: AGENT_INTERRUPT,
    },
];

/// The built-in commands of a profile, from the one table that has them.
fn profile_commands(profile: Option<&Profile>) -> &'static [SlashCommand] {
    profile
        .and_then(|profile| crate::composer::table_with_id(profile.commands))
        .map(|table| table.commands)
        .unwrap_or(&[])
}

/// Programs that take over the whole screen. An editor is not an agent, so the
/// agent field never names it.
///
/// Shared with `scrollback.rs`'s `is_editor_command`, which asks the same "is
/// this pane an editor" question against a different signal
/// (`foreground_command`, tmux's own report of the running process, rather
/// than the pane title matched here): `ScrollbackStore` needs to tell an
/// editor's pane from an agent's to decide whether a read replaces or
/// accumulates, and an agent pane must never answer yes (card #795 -- see
/// that module's own doc on `record`).
pub(crate) const EDITOR_PROGRAMS: &[&str] = &["vim", "nvim", "nvi", "helix", "hx", "emacs", "nano"];

/// Recognised from the pane title.
fn is_editor_title(title: &str) -> bool {
    let head = title
        .trim()
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    EDITOR_PROGRAMS.contains(&head.as_str())
}

/// Resolves what a pane is running into the keys and commands it responds to.
///
/// An unrecognised agent falls back to the shell set rather than to nothing:
/// the shell keys are a superset of what any prompt needs, so a brand new agent
/// stays usable before it is listed here.
pub fn resolve(agent: Option<&str>, pane_title: Option<&str>, cwd: Option<&str>) -> Value {
    let overlays = load_overlays(crate::config_dir);
    resolve_with(agent, pane_title, cwd, &overlays)
}

/// Which profile answers for a pane, and everything that follows from it.
///
/// One selection, read by two callers: the key row and the Stop button. They
/// disagreeing about which agent a pane is running would mean a client offering
/// Claude's keys and the shell's interrupt on the same pane.
struct Selection<'a> {
    id: String,
    keys: &'static [Shortcut],
    commands: &'static [SlashCommand],
    /// A `String` rather than a `&'static str` because `agents.json` can name
    /// it, and a key a developer wrote is as real as one in the table.
    interrupt: String,
    overlay: Option<&'a AgentOverlay>,
    /// True when `agents.json` named this profile, so it is obvious which edits
    /// are taking effect.
    configured: bool,
}

fn select_profile<'a>(
    agent: Option<&str>,
    pane_title: Option<&str>,
    overlays: &'a HashMap<String, AgentOverlay>,
) -> Selection<'a> {
    let agent_lower = agent.unwrap_or("").trim().to_ascii_lowercase();

    // The overlay file wins over the built-in table, so a developer can correct
    // a profile that ships wrong without waiting for a gateway release.
    let overlay_id = overlays
        .iter()
        .find(|(id, entry)| {
            if entry.r#match.is_empty() {
                return !agent_lower.is_empty() && agent_lower.contains(id.as_str());
            }
            entry.r#match.iter().any(|needle| {
                !needle.is_empty() && agent_lower.contains(&needle.to_ascii_lowercase())
            })
        })
        .map(|(id, _)| id.clone());

    let builtin_profile = AGENT_PROFILES.iter().find(|profile| {
        profile
            .agent_match
            .iter()
            .any(|needle| agent_lower.contains(needle))
    });

    let (id, keys, commands, interrupt): (String, &[Shortcut], &[SlashCommand], &str) =
        match (&overlay_id, builtin_profile) {
            (Some(id), _) => {
                // An overlay may extend a built-in profile of the same name, so
                // fall back to that profile's keys when it does not set its own.
                let base = AGENT_PROFILES.iter().find(|profile| profile.id == id);
                (
                    id.clone(),
                    base.map(|profile| profile.keys).unwrap_or(SHELL),
                    profile_commands(base),
                    // An overlay for an agent with no built-in profile is still
                    // an agent, so it gets an agent's interrupt rather than the
                    // shell's -- and can say otherwise with `interrupt`.
                    base.map_or(AGENT_INTERRUPT, |profile| profile.interrupt),
                )
            }
            (None, Some(profile)) => (
                profile.id.to_owned(),
                profile.keys,
                profile_commands(Some(profile)),
                profile.interrupt,
            ),
            (None, None) if pane_title.is_some_and(is_editor_title) => (
                "editor".to_owned(),
                EDITOR,
                EDITOR_COMMANDS,
                AGENT_INTERRUPT,
            ),
            (None, None) => ("shell".to_owned(), SHELL, &[][..], SHELL_INTERRUPT),
        };

    let overlay = overlay_id.as_ref().and_then(|id| overlays.get(id));
    Selection {
        id,
        keys,
        commands,
        // Sanitised like any other key read off disk: it is sent verbatim to
        // `pane.send_keys`, and a name Herdr refuses would make Stop a 400.
        interrupt: overlay
            .and_then(|entry| entry.interrupt.as_deref())
            .filter(|key| valid_key_name(key))
            .unwrap_or(interrupt)
            .to_owned(),
        overlay,
        configured: overlay_id.is_some(),
    }
}

/// The key that stops whatever this pane is running.
///
/// Resolved here rather than in a client for the same reason the key row is: it
/// changes when an agent does, and the gateway is what a developer updates.
pub fn interrupt_key(agent: Option<&str>, pane_title: Option<&str>) -> String {
    let overlays = load_overlays(crate::config_dir);
    select_profile(agent, pane_title, &overlays).interrupt
}

fn resolve_with(
    agent: Option<&str>,
    pane_title: Option<&str>,
    cwd: Option<&str>,
    overlays: &HashMap<String, AgentOverlay>,
) -> Value {
    let selection = select_profile(agent, pane_title, overlays);
    let id = selection.id.clone();
    let specific = selection.keys;
    let builtin = selection.commands;
    let overlay = selection.overlay;

    // Ordered by how often a thumb reaches for them, not by category. Answering
    // and interrupting come first because every pane needs them; then whatever
    // this particular agent advertises, which is the reason the row is dynamic
    // at all; then the rest, then arrows, which are the least used on a phone
    // with a keyboard.
    let agent_keys: Vec<ResolvedShortcut> = match overlay.and_then(|entry| entry.keys.as_ref()) {
        Some(keys) => keys
            .iter()
            // Sanitised like anything else read off disk: a key row is drawn
            // into a client's UI, and `key` is sent verbatim to send_keys.
            .filter(|entry| valid_key_name(&entry.key))
            .map(|entry| ResolvedShortcut {
                label: unquote(&entry.label),
                key: entry.key.clone(),
                description: entry
                    .description
                    .as_deref()
                    .map(unquote)
                    .unwrap_or_else(|| unquote(&entry.label)),
            })
            .collect(),
        None => specific.iter().map(ResolvedShortcut::from).collect(),
    };

    let keys: Vec<ResolvedShortcut> = PRIMARY
        .iter()
        .map(ResolvedShortcut::from)
        .chain(agent_keys)
        .chain(SECONDARY.iter().map(ResolvedShortcut::from))
        .chain(NAVIGATION.iter().map(ResolvedShortcut::from))
        .collect();

    let commands = merge_commands(&id, builtin, overlay, cwd);

    json!({
        "version": KEYMAP_VERSION,
        "profile": id,
        "agent": agent.unwrap_or_default(),
        // True when this profile came from `agents.json` rather than the
        // built-in table, so it is obvious which edits are taking effect.
        "configured": selection.configured,
        "keys": keys,
        "commands": commands,
        // The key a Stop button sends on this pane. Named here as well as on
        // the interrupt endpoint so a client can label the button honestly
        // without a second round trip.
        "interrupt": selection.interrupt,
    })
}

/// Every agent Herdr can integrate with, so a client can show which of them
/// have a profile and which will fall back to the shared shell keys. Taken from
/// Herdr's own `IntegrationTarget` enum.
pub const HERDR_AGENTS: &[&str] = &[
    "pi",
    "omp",
    "claude",
    "codex",
    "copilot",
    "devin",
    "droid",
    "kimi",
    "opencode",
    "kilo",
    "hermes",
    "qodercli",
    "cursor",
    "mastracode",
];

/// Whether this gateway has any notion of the named agent at all.
///
/// The catalogue's own question, asked of one name: a kind Herdr integrates
/// with, a profile in the built-in table, or an entry in `agents.json`. It is
/// deliberately generous about the last of those -- someone who wrote a profile
/// for an agent this build has never heard of has said, in the clearest way
/// available, that it is one they want to run.
pub fn is_known_agent(agent: &str) -> bool {
    let name = agent.trim().to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if HERDR_AGENTS.contains(&name.as_str()) {
        return true;
    }
    if AGENT_PROFILES
        .iter()
        .any(|profile| profile.agent_match.iter().any(|needle| name == *needle))
    {
        return true;
    }
    load_overlays(crate::config_dir)
        .keys()
        .any(|id| id.to_ascii_lowercase() == name)
}

/// The whole table at once: which agents have a profile, where each came from,
/// and where to put an overlay. Answers "is my new agent supported yet" without
/// having to open a pane running it.
pub fn catalog() -> Value {
    let overlays = load_overlays(crate::config_dir);
    let path = crate::config_dir()
        .map(|dir| dir.join(AGENTS_FILE).display().to_string())
        .unwrap_or_default();

    let agents: Vec<Value> = HERDR_AGENTS
        .iter()
        .map(|name| {
            let configured = overlays.contains_key(*name);
            let builtin = AGENT_PROFILES.iter().any(|profile| {
                profile
                    .agent_match
                    .iter()
                    .any(|needle| name.contains(needle))
            });
            json!({
                "agent": name,
                "source": if configured {
                    "configured"
                } else if builtin {
                    "builtin"
                } else {
                    "fallback"
                },
            })
        })
        .collect();

    // Anything in the overlay file that Herdr does not list, e.g. an agent
    // added to Herdr after this gateway build.
    let extra: Vec<Value> = overlays
        .keys()
        .filter(|id| !HERDR_AGENTS.contains(&id.as_str()))
        .map(|id| json!({ "agent": id, "source": "configured" }))
        .collect();

    json!({
        "version": KEYMAP_VERSION,
        "overlayPath": path,
        "agents": agents.into_iter().chain(extra).collect::<Vec<_>>(),
    })
}

/// Reads `agents.json` from the gateway's config directory. A missing or
/// malformed file is not an error: the built-in tables still work, and failing
/// a pane switch over a typo in a config file would be worse than ignoring it.
fn load_overlays(config_dir: fn() -> anyhow::Result<PathBuf>) -> HashMap<String, AgentOverlay> {
    let Ok(dir) = config_dir() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join(AGENTS_FILE)) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, AgentOverlay>>(&text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{AGENTS_FILE} is not valid: {err}");
            HashMap::new()
        }
    }
}

/// Built-in commands first, then anything found on disk that the table does not
/// already name. Discovered commands win on description: they are the file the
/// agent actually reads.
fn merge_commands(
    profile: &str,
    builtin: &[SlashCommand],
    overlay: Option<&AgentOverlay>,
    cwd: Option<&str>,
) -> Vec<ResolvedCommand> {
    let mut discovered = discover_commands(profile, overlay, cwd);

    // A profile that lists its own commands replaces the built-in list rather
    // than adding to it: the file is the correction, not a supplement.
    if let Some(commands) = overlay.and_then(|entry| entry.commands.as_ref()) {
        let mut out: Vec<ResolvedCommand> = commands
            .iter()
            .filter(|entry| valid_command_name(&entry.command))
            .map(|entry| ResolvedCommand {
                command: entry.command.clone(),
                description: entry
                    .description
                    .as_deref()
                    .map(unquote)
                    .unwrap_or_default(),
                argument_hint: entry.argument_hint.as_deref().map(unquote),
                source: "configured",
            })
            .collect();
        discovered.retain(|found| !out.iter().any(|entry| entry.command == found.command));
        discovered.sort_by(|a, b| a.command.cmp(&b.command));
        out.extend(discovered);
        return out;
    }

    let mut out: Vec<ResolvedCommand> = Vec::new();

    for entry in builtin {
        let name = entry.name.to_owned();
        if let Some(index) = discovered.iter().position(|found| found.command == name) {
            out.push(discovered.remove(index));
            continue;
        }
        out.push(ResolvedCommand {
            command: name,
            description: entry.description.to_owned(),
            argument_hint: entry.args_hint.map(str::to_owned),
            source: "builtin",
        });
    }

    discovered.sort_by(|a, b| a.command.cmp(&b.command));
    out.extend(discovered);
    out
}

/// Where each agent keeps the commands a user has written.
///
/// `claude --help` does not list slash commands and there is no non-interactive
/// way to ask for them, so the built-in table above stays hand-maintained.
/// Custom commands are a different story: they are plain files, and reading
/// them is the only way to know about commands this developer wrote themselves.
fn discover_commands(
    profile: &str,
    overlay: Option<&AgentOverlay>,
    cwd: Option<&str>,
) -> Vec<ResolvedCommand> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut found = Vec::new();

    // A configured profile says where to look, so a new agent needs no code.
    if let Some(dirs) = overlay.and_then(|entry| entry.command_dirs.as_ref()) {
        for dir in dirs {
            let Some(path) = expand_path(&dir.path, home.as_deref(), cwd) else {
                continue;
            };
            let source: &'static str = match dir.source.as_str() {
                "project" => "project",
                "plugin" => "plugin",
                _ => "user",
            };
            match dir.format.as_str() {
                "qoder-registry" => collect_qoder_commands(&path, &mut found),
                _ => collect_markdown_commands(&path, source, &mut found),
            }
        }
        return found;
    }

    match profile {
        // Markdown files, one per command, with optional YAML front matter.
        "claude" => {
            if let Some(home) = home.as_ref() {
                collect_markdown_commands(&home.join(".claude/commands"), "user", &mut found);
            }
            if let Some(cwd) = cwd {
                collect_markdown_commands(
                    &Path::new(cwd).join(".claude/commands"),
                    "project",
                    &mut found,
                );
            }
        }
        "codex" => {
            if let Some(home) = home.as_ref() {
                collect_markdown_commands(&home.join(".codex/prompts"), "user", &mut found);
            }
        }
        // Qoder registers external commands in a single JSON file.
        "qodercli" => {
            if let Some(home) = home.as_ref() {
                collect_qoder_commands(
                    &home.join(".qoder/external-commands/registry.json"),
                    &mut found,
                );
            }
        }
        _ => {}
    }

    found
}

/// Expands `~` and resolves a relative path against the pane's working
/// directory, which is what makes a project-local command directory possible.
fn expand_path(value: &str, home: Option<&Path>, cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    if value.starts_with('/') {
        return Some(PathBuf::from(value));
    }
    cwd.map(|cwd| Path::new(cwd).join(value))
}

fn collect_markdown_commands(dir: &Path, source: &'static str, out: &mut Vec<ResolvedCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Counted on accepted commands rather than on directory entries: a folder
    // holding 64 unrelated files would otherwise yield nothing at all.
    let mut accepted = 0usize;
    for entry in entries.flatten() {
        if accepted >= MAX_DISCOVERED_COMMANDS {
            break;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        // Regular files only, and small ones. A fifo here would block the
        // request forever, and the pane's working directory -- which decides
        // where this looks -- is chosen by whoever created the pane.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_COMMAND_FILE_BYTES {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let (description, argument_hint) = front_matter(&text);
        accepted += 1;
        out.push(ResolvedCommand {
            command: format!("/{name}"),
            description: if description.is_empty() {
                format!("Custom {source} command")
            } else {
                description
            },
            argument_hint,
            source,
        });
    }
}

/// Reads `description:` and `argument-hint:` out of a command file's YAML front
/// matter, with the same reader the composer's workspace discovery uses.
fn front_matter(text: &str) -> (String, Option<String>) {
    (
        crate::composer::field(text, "description").unwrap_or_default(),
        crate::composer::field(text, "argument-hint"),
    )
}

/// A slash command is typed into a live agent, so it may only be a name.
/// Applied to the overlay file as well as to discovered files: a config snippet
/// copied from somewhere else is not more trustworthy than a command file.
fn valid_command_name(value: &str) -> bool {
    // `:` for an editor's ex commands, `/` for an agent's slash commands.
    let Some(name) = value.strip_prefix('/').or_else(|| value.strip_prefix(':')) else {
        return false;
    };
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' || c == '!')
}

/// A key name is sent straight to `pane.send_keys`, which is why it is checked
/// here rather than trusted. Herdr rejects anything it does not know, but a
/// client should not be asked to draw a button that cannot work.
fn valid_key_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '_')
}

/// Anything read off disk or out of a config file is drawn into a client's UI,
/// so it is stripped and capped the same way the composer's is.
fn unquote(value: &str) -> String {
    crate::composer::sanitize(value)
}

fn collect_qoder_commands(path: &Path, out: &mut Vec<ResolvedCommand>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() > MAX_COMMAND_FILE_BYTES {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(commands) = value.get("commands").and_then(Value::as_object) else {
        return;
    };
    for (name, entry) in commands.iter().take(MAX_DISCOVERED_COMMANDS) {
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        out.push(ResolvedCommand {
            command: format!("/{name}"),
            description: entry
                .get("description")
                .and_then(Value::as_str)
                .map(unquote)
                .unwrap_or_else(|| "External Qoder command".to_owned()),
            argument_hint: None,
            source: "user",
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_names(value: &Value) -> Vec<String> {
        value["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["key"].as_str().unwrap().to_owned())
            .collect()
    }

    fn overlays(json: &str) -> HashMap<String, AgentOverlay> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_config_file_cannot_smuggle_control_characters_or_bogus_keys() {
        let table = overlays(
            r#"{"evil": {
                 "match": ["evil"],
                 "keys": [{"label": "ok\u001b[2K", "key": "ctrl+a"},
                          {"label": "nope", "key": "echo pwned; reboot"}],
                 "commands": [{"command": "/fine", "description": "line\nbreak"},
                              {"command": "not-a-command"},
                              {"command": "/bad; reboot"}]
               }}"#,
        );
        let value = resolve_with(Some("evil"), None, None, &table);

        // The one valid key survives, with its label stripped of the escape.
        let keys = value["keys"].as_array().unwrap();
        let configured: Vec<&Value> = keys.iter().filter(|k| k["key"] == "ctrl+a").collect();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0]["label"], "ok[2K");
        assert!(!keys.iter().any(|k| k["key"] == "echo pwned; reboot"));

        // Only the well-formed command survives, without the newline.
        let commands = value["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "/fine");
        assert_eq!(commands[0]["description"], "linebreak");
    }

    #[test]
    fn discovery_skips_anything_that_is_not_a_small_regular_file() {
        let dir = std::env::temp_dir().join(format!("herdr-cmds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good.md"), "---\ndescription: Fine\n---\n").unwrap();
        std::fs::write(
            dir.join("big.md"),
            vec![b'x'; (MAX_COMMAND_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("nested.md")).unwrap();

        let mut found = Vec::new();
        collect_markdown_commands(&dir, "user", &mut found);
        let names: Vec<&str> = found.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(names, vec!["/good"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_agent_the_gateway_has_never_heard_of_works_from_the_config_file() {
        let table = overlays(
            r#"{"opencode": {
                 "match": ["opencode"],
                 "keys": [{"label": "⇧tab", "key": "shift+tab", "description": "Cycle mode"}],
                 "commands": [{"command": "/model", "description": "Switch model",
                               "argumentHint": "[model]"}]
               }}"#,
        );
        let value = resolve_with(Some("opencode"), None, None, &table);
        assert_eq!(value["profile"], "opencode");
        assert_eq!(value["configured"], true);
        let keys = key_names(&value);
        assert_eq!(&keys[..3], &["enter", "shift+enter", "esc"]);
        assert_eq!(keys[3], "shift+tab");
        let commands = value["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "/model");
        assert_eq!(commands[0]["argument_hint"], "[model]");
        assert_eq!(commands[0]["source"], "configured");
    }

    #[test]
    fn a_config_entry_overrides_the_built_in_profile_of_the_same_name() {
        let table = overlays(r#"{"claude": {"commands": [{"command": "/only"}]}}"#);
        let value = resolve_with(Some("Claude Code"), None, None, &table);
        assert_eq!(value["configured"], true);
        let commands = value["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "/only");
        // Keys were not overridden, so the built-in Claude row still applies.
        assert!(key_names(&value).contains(&"ctrl+t".to_string()));
    }

    #[test]
    fn an_empty_config_leaves_the_built_in_tables_alone() {
        let value = resolve_with(Some("claude"), None, None, &HashMap::new());
        assert_eq!(value["configured"], false);
        assert!(!value["commands"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_path_expands_against_home_and_the_panes_cwd() {
        let home = Path::new("/home/dev");
        assert_eq!(
            expand_path("~/.claude/commands", Some(home), None),
            Some(PathBuf::from("/home/dev/.claude/commands"))
        );
        assert_eq!(
            expand_path(".claude/commands", Some(home), Some("/work/repo")),
            Some(PathBuf::from("/work/repo/.claude/commands"))
        );
        assert_eq!(
            expand_path("/etc/commands", None, None),
            Some(PathBuf::from("/etc/commands"))
        );
    }

    #[test]
    fn the_key_row_leads_with_the_agents_own_actions() {
        let keys = key_names(&resolve(Some("claude"), None, None));
        // enter / shift+enter / esc, then what Claude Code itself advertises.
        assert_eq!(&keys[..3], &["enter", "shift+enter", "esc"]);
        assert_eq!(keys[3], "shift+tab");
        assert!(
            keys.iter().position(|k| k == "ctrl+t").unwrap()
                < keys.iter().position(|k| k == "tab").unwrap(),
            "agent keys should come before the generic ones"
        );
    }

    #[test]
    fn discovery_reads_a_command_file_and_its_front_matter() {
        let (description, hint) =
            front_matter("---\ndescription: Ship a release\nargument-hint: [version]\n---\nbody\n");
        assert_eq!(description, "Ship a release");
        assert_eq!(hint.as_deref(), Some("[version]"));
    }

    #[test]
    fn a_file_without_front_matter_still_yields_a_command() {
        let (description, hint) = front_matter("just a prompt body\n");
        assert!(description.is_empty());
        assert!(hint.is_none());
    }

    #[test]
    fn built_in_commands_are_labelled_as_such() {
        let value = resolve(Some("claude"), None, None);
        let commands = value["commands"].as_array().unwrap();
        assert!(commands.iter().any(|entry| entry["source"] == "builtin"));
    }

    #[test]
    fn a_command_that_takes_an_argument_says_so() {
        let value = resolve(Some("claude"), None, None);
        let commands = value["commands"].as_array().unwrap();
        let compact = commands
            .iter()
            .find(|entry| entry["command"] == "/compact")
            .unwrap();
        assert_eq!(compact["argument_hint"], "[instructions]");
        // And one that runs exactly as typed, which is what lets a client send
        // it on a single tap instead of opening the composer.
        let help = commands
            .iter()
            .find(|entry| entry["command"] == "/help")
            .unwrap();
        assert!(help["argument_hint"].is_null());
    }

    #[test]
    fn every_command_starts_with_a_slash_and_has_a_description() {
        for agent in [Some("claude"), Some("codex"), Some("qodercli")] {
            for entry in resolve(agent, None, None)["commands"].as_array().unwrap() {
                let command = entry["command"].as_str().unwrap();
                assert!(command.starts_with('/'), "{command}");
                assert!(!command.contains(' '), "{command}");
                assert!(!entry["description"].as_str().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn resolves_agents_by_substring() {
        let value = resolve(Some("Claude Code"), Some("okk@mac-mini:~/src"), None);
        assert_eq!(value["profile"], "claude");
        assert!(key_names(&value).contains(&"ctrl+t".to_string()));
        assert!(value["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["command"] == "/compact"));
    }

    #[test]
    fn an_editor_pane_gets_editor_motions_not_shell_editing() {
        let value = resolve(None, Some("nvim src/main.rs"), None);
        assert_eq!(value["profile"], "editor");
        let keys = key_names(&value);
        assert!(keys.contains(&"ctrl+v".to_string()));
        assert!(!keys.contains(&"ctrl+p".to_string()));
        // Getting out of an editor must not require hunting for `:` on a phone.
        let commands = value["commands"].as_array().unwrap();
        assert!(commands.iter().any(|entry| entry["command"] == ":q"));
        assert!(commands.iter().any(|entry| entry["command"] == ":wq"));
    }

    #[test]
    fn an_unknown_agent_falls_back_to_shell() {
        let value = resolve(Some("some-new-agent"), None, None);
        assert_eq!(value["profile"], "shell");
        assert!(key_names(&value).contains(&"ctrl+r".to_string()));
        assert!(value["commands"].as_array().unwrap().is_empty());
    }

    #[test]
    fn every_profile_starts_with_the_base_keys_and_ends_with_navigation() {
        for agent in [Some("claude"), Some("codex"), Some("qodercli"), None] {
            let keys = key_names(&resolve(agent, None, None));
            assert_eq!(keys[0], "enter");
            assert_eq!(keys[1], "shift+enter");
            assert_eq!(keys.last().unwrap(), "alt+right");
        }
    }

    #[test]
    fn stop_sends_the_key_this_particular_agent_stops_on() {
        // The whole reason the interrupt endpoint exists: `ctrl+c` is right at a
        // shell and wrong in every agent, where it is at best ignored and at
        // worst kills the session the user was in the middle of.
        let empty = HashMap::new();
        for agent in ["claude", "Claude Code", "codex", "opencode", "qodercli"] {
            assert_eq!(
                select_profile(Some(agent), None, &empty).interrupt,
                "esc",
                "{agent}"
            );
        }
        // A pane with no agent is a shell prompt, and a shell stops on ctrl+c.
        assert_eq!(
            select_profile(None, None, &empty).interrupt,
            SHELL_INTERRUPT
        );
        assert_eq!(
            select_profile(Some("some-new-agent"), None, &empty).interrupt,
            SHELL_INTERRUPT
        );
        // An editor is not stopped with ctrl+c either.
        assert_eq!(
            select_profile(None, Some("/usr/bin/nvim ."), &empty).interrupt,
            "esc"
        );
        // And the key row says the same thing the endpoint would send, so a
        // client can label the button without a second round trip.
        assert_eq!(resolve(Some("claude"), None, None)["interrupt"], "esc");
        assert_eq!(
            resolve(Some("some-new-agent"), None, None)["interrupt"],
            SHELL_INTERRUPT
        );
    }

    #[test]
    fn a_config_file_can_correct_an_interrupt_key_but_not_invent_a_command() {
        // An agent that stops on something else is one line of `agents.json`,
        // the same way its keys and commands are.
        let table = overlays(
            r#"{"weird": {"match": ["weird"], "interrupt": "ctrl+d"},
                "hostile": {"match": ["hostile"], "interrupt": "pkill -9 agent; echo"}}"#,
        );
        assert_eq!(
            select_profile(Some("weird-agent"), None, &table).interrupt,
            "ctrl+d"
        );
        // The key is sent verbatim to `pane.send_keys`, so it is sanitised like
        // every other key read off disk and a bad one falls back to the default
        // rather than travelling.
        assert_eq!(
            select_profile(Some("hostile-agent"), None, &table).interrupt,
            "esc"
        );
        // An overlay for an agent with no built-in profile is still an agent.
        assert_eq!(
            select_profile(Some("weird-agent"), None, &table).id,
            "weird"
        );
    }

    #[test]
    fn an_agent_is_known_when_herdr_integrates_with_it_or_a_profile_names_it() {
        assert!(is_known_agent("claude"));
        assert!(is_known_agent("Codex"));
        assert!(is_known_agent("opencode"));
        assert!(is_known_agent("qodercli"));
        // Herdr's own list is wider than the profile table, and a kind with no
        // key row still runs -- it just falls back to the shell keys.
        assert!(is_known_agent("droid"));
        // Nothing else, and nothing shaped like an argument.
        assert!(!is_known_agent(""));
        assert!(!is_known_agent("   "));
        assert!(!is_known_agent("claude; rm -rf /"));
        assert!(!is_known_agent("../../bin/sh"));
        assert!(!is_known_agent("not-an-agent"));
    }

    #[test]
    fn a_shell_title_is_not_mistaken_for_an_editor() {
        assert!(!is_editor_title("okk@mac-mini:~/.repos/muqun"));
        assert!(!is_editor_title("npm run dev"));
        assert!(is_editor_title("/usr/bin/nvim ."));
    }
}
