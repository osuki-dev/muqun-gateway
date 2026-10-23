//! Terminal composer capabilities and file mention search.
//!
//! Slash commands come exclusively from the downloaded agent-command catalog.
//! Workspace skills and commands are not merged into the command suggestions;
//! the skills a pane's agent can run are a separate list (`pane_skills`).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Bumped when the descriptor's source or shape changes.
pub const COMPOSER_VERSION: u32 = 3;

/// One command the agent understands out of the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinCommand {
    /// The literal text to send, leading slash included, so a client can put it
    /// straight into the composer.
    pub name: &'static str,
    pub description: &'static str,
    /// What may follow the command, written the way the agent's own help writes
    /// it. `None` means it runs exactly as typed, so a client may send it on a
    /// single tap; anything else should land in the composer first.
    pub args_hint: Option<&'static str>,
}

const fn cmd(name: &'static str, description: &'static str) -> BuiltinCommand {
    BuiltinCommand {
        name,
        description,
        args_hint: None,
    }
}

const fn cmd_with(
    name: &'static str,
    description: &'static str,
    hint: &'static str,
) -> BuiltinCommand {
    BuiltinCommand {
        name,
        description,
        args_hint: Some(hint),
    }
}

/// How a workspace directory stores the commands it adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One directory per skill, each holding a `SKILL.md` whose front matter
    /// names and describes it. The directory name is the fallback name.
    Skills,
    /// One markdown file per command, with optional `description:` and
    /// `argument-hint:` front matter. The file stem is the command name.
    Commands,
}

/// A directory, relative to the pane's workspace root, that this agent reads
/// its extra commands from.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceDir {
    #[cfg_attr(not(test), allow(dead_code))]
    pub path: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub layout: Layout,
}

const fn skills(path: &'static str) -> WorkspaceDir {
    WorkspaceDir {
        path,
        layout: Layout::Skills,
    }
}

const fn commands(path: &'static str) -> WorkspaceDir {
    WorkspaceDir {
        path,
        layout: Layout::Commands,
    }
}

/// One agent family's composer capabilities.
pub struct CommandTable {
    /// Stable id on the wire, the same id `parts.rs` uses for the same agent,
    /// so a client can key a cache on one name.
    pub id: &'static str,
    /// Matched as a substring against the agent name Herdr reports, the same
    /// discipline `parts.rs` and `shortcuts.rs` use.
    pub agent_match: &'static [&'static str],
    /// The release this table was read off. A table is only as true as the
    /// version it was captured from, and saying which one makes the drift
    /// reviewable.
    pub captured_from: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub commands: &'static [BuiltinCommand],
    /// Where this agent reads workspace-local commands and skills from.
    #[cfg_attr(not(test), allow(dead_code))]
    pub workspace: &'static [WorkspaceDir],
    /// Whether `@` in the composer means "mention a file" to this agent. False
    /// would mean a client should not offer the file picker at all.
    pub file_mentions: bool,
}

/// Claude Code 2.1.220. Read off the shipped binary's command definitions;
/// descriptions and argument hints are Anthropic's own wording.
///
/// `/agents` is gone in this release ("(removed)"), `/cost` is an alias of
/// `/usage`, and the debug commands (`/heapdump`, `/pro-trial-expired`, …) are
/// not something to put on a phone.
const CLAUDE_COMMANDS: &[BuiltinCommand] = &[
    cmd_with("/add-dir", "Add a new working directory", "<path>"),
    cmd_with(
        "/bug",
        "Report a bug or share your conversation",
        "[report]",
    ),
    cmd_with(
        "/clear",
        "Start a new session with empty context; previous session stays on disk",
        "[name]",
    ),
    cmd_with(
        "/compact",
        "Free up context by summarizing the conversation so far",
        "[instructions]",
    ),
    cmd_with("/config", "Open settings", "[key=value]"),
    cmd_with(
        "/context",
        "Visualize current context usage as a colored grid",
        "[all]",
    ),
    cmd("/diff", "View uncommitted changes and per-turn diffs"),
    cmd("/doctor", "Diagnose the Claude Code installation"),
    cmd_with(
        "/export",
        "Export the current conversation to a file or clipboard",
        "[filename]",
    ),
    cmd("/help", "Show help and available commands"),
    cmd("/hooks", "View hook configurations for tool events"),
    cmd_with("/ide", "Manage IDE integrations and show status", "[open]"),
    cmd("/init", "Initialize a new CLAUDE.md file for this project"),
    cmd("/login", "Sign in with your Anthropic account"),
    cmd("/logout", "Sign out from your Anthropic account"),
    cmd_with("/mcp", "Manage MCP servers", "[reconnect|enable|disable]"),
    cmd("/memory", "Open a memory file in your editor"),
    cmd_with("/model", "Set the AI model for Claude Code", "[model]"),
    cmd("/output-style", "Change the output style"),
    cmd(
        "/permissions",
        "Manage allow and deny tool permission rules",
    ),
    cmd("/plan", "Enable plan mode or view the current session plan"),
    cmd("/privacy-settings", "View and update your privacy settings"),
    cmd("/release-notes", "Show release notes"),
    cmd("/reload-skills", "Pick up skills added or changed on disk"),
    cmd_with(
        "/resume",
        "Resume a previous conversation",
        "[conversation id or search term]",
    ),
    cmd_with(
        "/review",
        "Review a GitHub pull request; for your working diff use /code-review",
        "[pr number]",
    ),
    cmd("/rewind", "Rewind the conversation to a checkpoint"),
    cmd("/skills", "List available skills"),
    cmd(
        "/status",
        "Show Claude Code status: version, model, account, tools",
    ),
    cmd("/statusline", "Configure the status line"),
    cmd("/terminal-setup", "Configure the terminal key bindings"),
    cmd("/theme", "Change the theme"),
    cmd(
        "/usage",
        "Show session cost, plan usage, and activity stats",
    ),
    cmd("/vim", "Toggle vim mode"),
];

/// Codex CLI 0.145.0. Names are the `#[strum]` serializations of `SlashCommand`
/// at tag `rust-v0.145.0`; descriptions are that enum's `description()`.
///
/// Left out on purpose: `/rollout`, `/test-approval`, `/debug-m-drop` and
/// `/debug-m-update` are debug-only, `/setup-default-sandbox` and
/// `/sandbox-add-read-dir` are Windows-only, and `/exit` is a second spelling of
/// `/quit`.
const CODEX_COMMANDS: &[BuiltinCommand] = &[
    cmd("/agent", "Switch the active agent thread"),
    cmd("/app", "Continue this session in the Desktop app"),
    cmd(
        "/approve",
        "Approve one retry of a recent auto-review denial",
    ),
    cmd("/apps", "Manage apps"),
    cmd("/archive", "Archive this session and exit"),
    cmd("/clear", "Clear the terminal and start a new chat"),
    cmd(
        "/compact",
        "Summarize conversation to prevent hitting the context limit",
    ),
    cmd("/copy", "Copy last response as markdown"),
    cmd("/delete", "Permanently delete this session and exit"),
    cmd("/diff", "Show git diff (including untracked files)"),
    cmd("/experimental", "Toggle experimental features"),
    cmd("/fork", "Fork the current chat"),
    cmd_with(
        "/goal",
        "Set or view the goal for a long-running task",
        "[objective|clear]",
    ),
    cmd("/hooks", "View and manage lifecycle hooks"),
    cmd(
        "/ide",
        "Include current selection, open files, and other context from your IDE",
    ),
    cmd(
        "/import",
        "Import setup, this project, and recent chats from Claude Code",
    ),
    cmd(
        "/init",
        "Create an AGENTS.md file with instructions for Codex",
    ),
    cmd("/keymap", "Remap TUI shortcuts"),
    cmd("/logout", "Log out of Codex"),
    cmd_with(
        "/mcp",
        "List configured MCP tools; use /mcp verbose for details",
        "[verbose]",
    ),
    cmd("/memories", "Configure memory use and generation"),
    cmd_with("/mention", "Mention a file", "[file]"),
    cmd("/model", "Choose what model and reasoning effort to use"),
    cmd("/new", "Start a new chat during a conversation"),
    cmd("/permissions", "Choose what Codex is allowed to do"),
    cmd("/personality", "Choose a communication style for Codex"),
    cmd_with("/pets", "Choose or hide the terminal pet", "[pet]"),
    cmd("/plan", "Switch to Plan mode"),
    cmd("/plugins", "Browse plugins"),
    cmd("/ps", "List background terminals"),
    cmd("/quit", "Exit Codex"),
    cmd(
        "/raw",
        "Toggle raw scrollback mode for copy-friendly selection",
    ),
    cmd_with("/rename", "Rename the current thread", "[title]"),
    cmd_with(
        "/resume",
        "Resume a saved chat",
        "[conversation id or search term]",
    ),
    cmd_with(
        "/review",
        "Review my current changes and find issues",
        "[instructions]",
    ),
    cmd_with(
        "/side",
        "Start a side conversation in an ephemeral fork",
        "[question]",
    ),
    cmd("/skills", "Use skills to improve how Codex performs tasks"),
    cmd(
        "/status",
        "Show current session configuration and token usage",
    ),
    cmd(
        "/statusline",
        "Configure which items appear in the status line",
    ),
    cmd("/stop", "Stop all background terminals"),
    cmd("/theme", "Choose a syntax highlighting theme"),
    cmd(
        "/title",
        "Configure which items appear in the terminal title",
    ),
    cmd("/usage", "View account usage or use a usage limit reset"),
    cmd("/vim", "Toggle Vim mode for the composer"),
];

/// opencode 1.18.0. Its palette entries carry the slash name they answer to,
/// as `slashName`/`slashAliases` or as `slash: { name, aliases }`; a palette
/// entry with neither is a key binding, not something typed with a slash, and
/// is not listed here. `/debug`, `/warp` and `/org` are, and are left out.
///
/// opencode's published command list is behind its binary -- the docs still
/// name `/init`, which 1.18.0 no longer registers -- so the binary is what this
/// table follows.
const OPENCODE_COMMANDS: &[BuiltinCommand] = &[
    cmd("/agents", "Switch agent"),
    cmd("/compact", "Compact the session into a summary"),
    cmd("/connect", "Add a provider and its API key"),
    cmd("/copy", "Copy the session transcript"),
    cmd("/diff", "Open the diff viewer"),
    cmd("/editor", "Compose the message in your external editor"),
    cmd("/exit", "Close opencode"),
    cmd("/export", "Export the session transcript"),
    cmd("/fork", "Fork the session"),
    cmd("/help", "Open the help dialog"),
    cmd("/mcps", "Toggle MCP servers"),
    cmd("/models", "Switch model"),
    cmd("/move", "Move the session"),
    cmd("/new", "Start a new session"),
    cmd("/redo", "Redo the message that was undone"),
    cmd_with("/rename", "Rename the session", "[title]"),
    cmd("/sessions", "List and switch sessions"),
    cmd("/share", "Share the session"),
    cmd("/skills", "Browse available skills"),
    cmd("/status", "View status"),
    cmd("/themes", "Switch theme"),
    cmd("/thinking", "Toggle visibility of reasoning blocks"),
    cmd("/timeline", "Jump to a message in the session"),
    cmd("/timestamps", "Toggle message timestamps"),
    cmd("/undo", "Undo the previous message"),
    cmd("/unshare", "Stop sharing the session"),
    cmd("/variants", "Switch model variant"),
    cmd("/workspaces", "Manage workspaces"),
];

/// Qoder CLI 1.1.5. Read off the shipped binary's command definitions. Qoder is
/// a Claude Code-shaped CLI, so the vocabulary overlaps; the wording is Qoder's
/// own. `/corgi` and the `debug-*` commands are left out.
const QODER_COMMANDS: &[BuiltinCommand] = &[
    cmd_with(
        "/add-dir",
        "Add a directory to the workspace context",
        "[directory]",
    ),
    cmd("/agents", "Manage agents"),
    cmd(
        "/branch",
        "Create a new session branch from the current conversation",
    ),
    cmd_with(
        "/btw",
        "Ask a quick side question without interrupting the conversation",
        "<question>",
    ),
    cmd("/clear", "Clear the screen and start a new conversation"),
    cmd("/commands", "Reload and list all available slash commands"),
    cmd("/commit", "Generate a commit message and commit changes"),
    cmd(
        "/compact",
        "Compress the context by replacing it with a summary",
    ),
    cmd("/config", "View and modify CLI configuration"),
    cmd("/context", "Visualize current context window usage"),
    cmd_with(
        "/copy",
        "Copy the last assistant response to the clipboard",
        "[N]",
    ),
    cmd("/diff", "Show uncommitted git changes"),
    cmd_with(
        "/effort",
        "Set reasoning effort for the current model",
        "[auto|low|medium|high|max|off]",
    ),
    cmd_with(
        "/export",
        "Export the current conversation to a file or clipboard",
        "[filename]",
    ),
    cmd_with(
        "/fast",
        "Toggle fast mode for the current model",
        "[on|off]",
    ),
    cmd_with(
        "/goal",
        "Manage persistent goals for the current session",
        "<set|status|clear|pause|resume|take>",
    ),
    cmd("/help", "Show help for this prompt"),
    cmd("/hooks", "Manage hooks"),
    cmd(
        "/init",
        "Analyze the project and create a tailored context file",
    ),
    cmd("/login", "Sign in to your account"),
    cmd("/logout", "Sign out and clear cached credentials"),
    cmd("/mcp", "Configure and manage MCP servers"),
    cmd("/memory", "Commands for interacting with memory"),
    cmd("/model", "Set or manage model configuration"),
    cmd("/new", "Start a new conversation"),
    cmd("/permissions", "Manage permissions"),
    cmd("/plan", "Toggle Plan Mode"),
    cmd("/plugins", "Manage plugins"),
    cmd(
        "/release-notes",
        "Show release notes for recent CLI versions",
    ),
    cmd("/rename", "Set a custom title for the current session"),
    cmd(
        "/resume",
        "Resume a session by identifier, or open the session browser",
    ),
    cmd("/review", "Review code changes and find actionable issues"),
    cmd("/skills", "Manage agent skills"),
    cmd("/status", "Show session status"),
    cmd("/theme", "Change the theme"),
    cmd(
        "/usage",
        "Show usage statistics for the current billing period",
    ),
    cmd("/vim", "Toggle vim mode on/off for this session"),
];

/// Where each agent reads workspace-local commands from. `.agents/skills` is
/// the cross-agent convention and appears under every agent that honours it;
/// the rest are that agent's own directories. Nothing here is guessed: each
/// path is one the agent it is listed under loads from. Claude Code mentions
/// `.agents/skills` only to import it into `.claude/skills`; its loader reads
/// `.claude/skills` alone, so the cross-agent directory is not listed for it.
const CLAUDE_WORKSPACE: &[WorkspaceDir] = &[skills(".claude/skills"), commands(".claude/commands")];
const CODEX_WORKSPACE: &[WorkspaceDir] = &[skills(".codex/skills"), skills(".agents/skills")];
const OPENCODE_WORKSPACE: &[WorkspaceDir] = &[
    skills(".opencode/skills"),
    skills(".opencode/skill"),
    skills(".agents/skills"),
    commands(".opencode/command"),
];
const QODER_WORKSPACE: &[WorkspaceDir] = &[
    skills(".qoder/skills"),
    skills(".agents/skills"),
    commands(".qoder/commands"),
];

/// The tables, matched the same way `parts.rs` matches its dictionaries: as a
/// substring of the agent name Herdr reports, so "Claude Code", "claude-code"
/// and "claude" all land on one table. The aliases mirror the `id` and
/// `aliases` of Herdr's agent-detection manifests, which is the
/// ecosystem-maintained list this tracks.
pub const TABLES: &[CommandTable] = &[
    CommandTable {
        id: "claude",
        agent_match: &["claude"],
        captured_from: "claude 2.1.220",
        commands: CLAUDE_COMMANDS,
        workspace: CLAUDE_WORKSPACE,
        file_mentions: true,
    },
    CommandTable {
        id: "codex",
        agent_match: &["codex"],
        captured_from: "codex-cli 0.145.0",
        commands: CODEX_COMMANDS,
        workspace: CODEX_WORKSPACE,
        file_mentions: true,
    },
    CommandTable {
        id: "opencode",
        agent_match: &["opencode", "open-code"],
        captured_from: "opencode 1.18.0",
        commands: OPENCODE_COMMANDS,
        workspace: OPENCODE_WORKSPACE,
        file_mentions: true,
    },
    CommandTable {
        id: "qoder",
        agent_match: &["qoder"],
        captured_from: "qodercli 1.1.5",
        commands: QODER_COMMANDS,
        workspace: QODER_WORKSPACE,
        file_mentions: true,
    },
];

/// Which table, if any, describes this agent's composer.
///
/// `None` is the supported answer for an agent with no table: the pane carries
/// no `composer` descriptor at all rather than an empty one, so a client can
/// tell "this gateway knows nothing about this agent" from "this agent has no
/// slash commands".
pub fn table_for(agent: Option<&str>) -> Option<&'static CommandTable> {
    let agent = agent?.trim().to_ascii_lowercase();
    if agent.is_empty() {
        return None;
    }
    TABLES
        .iter()
        .find(|table| table.agent_match.iter().any(|name| agent.contains(name)))
}

/// The table with this id, for callers that already resolved an agent profile.
#[cfg(test)]
pub fn table_with_id(id: &str) -> Option<&'static CommandTable> {
    TABLES.iter().find(|table| table.id == id)
}

// ---------------------------------------------------------------------------
// Workspace discovery
//
// Read-only, budgeted, and fenced: every path read has to canonicalize inside
// its root -- the workspace root for a repo's directories, the home directory
// for the user's own -- which is the same rule the asset content endpoint is
// gated on.
// ---------------------------------------------------------------------------

/// A stray directory cannot turn one request into thousands of file reads.
#[cfg(test)]
const MAX_WORKSPACE_COMMANDS: usize = 64;
/// Front matter is at the head of the file, so refusing to read further loses
/// nothing. Without a cap, a symlink to `/dev/zero` in a skills directory turns
/// one request into an unbounded read.
const MAX_COMMAND_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 160;

/// A command a workspace added, as it goes out on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
pub struct WorkspaceCommand {
    pub name: String,
    pub description: String,
    pub args_hint: Option<String>,
}

/// Read the commands and skills this workspace adds for this agent.
///
/// `root` is expected to be canonical already -- it is the pane's workspace
/// root, canonicalized by the caller the same way the asset roots are. Every
/// directory and every file below it is canonicalized again and checked to
/// still be inside `root`, so a symlinked `.claude/skills` pointing at another
/// checkout, or a skill directory that is a link out of the repo, is skipped
/// rather than read.
#[cfg(test)]
pub fn workspace_commands(root: &Path, dirs: &[WorkspaceDir]) -> Vec<WorkspaceCommand> {
    let mut found: Vec<WorkspaceCommand> = Vec::new();
    for dir in dirs {
        if found.len() >= MAX_WORKSPACE_COMMANDS {
            break;
        }
        let Some(path) = fenced(&root.join(dir.path), root) else {
            continue;
        };
        match dir.layout {
            Layout::Skills => {
                let mut skills = Vec::new();
                collect_skills(
                    &path,
                    root,
                    MAX_WORKSPACE_COMMANDS - found.len(),
                    &mut skills,
                );
                found.extend(skills.into_iter().map(|skill| WorkspaceCommand {
                    name: format!("/{}", skill.name),
                    description: if skill.description.is_empty() {
                        "Workspace skill".to_owned()
                    } else {
                        skill.description
                    },
                    args_hint: skill.args_hint,
                }));
            }
            Layout::Commands => collect_commands(&path, root, &mut found),
        }
    }
    // One name, one command: the first directory listed for an agent wins, so a
    // repo's `.opencode/skills` beats the same name under `.agents/skills`.
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// The one gate on reading anything from a workspace: whatever the path
/// claimed, it has to canonicalize to something inside the root. Canonicalizing
/// first is what closes symlink escapes -- a link inside the root that points
/// outside it resolves to the outside path, and fails here.
fn fenced(path: &Path, root: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    (canonical != root && canonical.starts_with(root)).then_some(canonical)
}

/// One skill directory's manifest, read but not yet placed on the wire.
struct FoundSkill {
    name: String,
    description: String,
    #[cfg_attr(not(test), allow(dead_code))]
    args_hint: Option<String>,
}

/// Every `<name>/SKILL.md` under `dir`, at most `limit` of them, each fenced
/// inside `root`. An unreadable directory or manifest is skipped, never an
/// error: one broken skill must not hide the rest.
fn collect_skills(dir: &Path, root: &Path, limit: usize, out: &mut Vec<FoundSkill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut taken = 0;
    for entry in entries.flatten() {
        if taken >= limit {
            return;
        }
        // A skill directory may be a link; the fence below decides whether
        // its target may be read.
        let Some(fallback) = command_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let Some(manifest) = fenced(&entry.path().join("SKILL.md"), root) else {
            continue;
        };
        let Some(text) = read_head(&manifest) else {
            continue;
        };
        // A skill names itself in its front matter; the directory name is only
        // the fallback, and a front-matter name that is not a usable command
        // name is ignored rather than trusted.
        let name = field(&text, "name")
            .and_then(|value| command_name(&value))
            .unwrap_or(fallback);
        taken += 1;
        out.push(FoundSkill {
            name,
            description: field(&text, "description").unwrap_or_default(),
            args_hint: field(&text, "argument-hint"),
        });
    }
}

#[cfg(test)]
fn collect_commands(dir: &Path, root: &Path, out: &mut Vec<WorkspaceCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_WORKSPACE_COMMANDS {
            return;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(command_name)
        else {
            continue;
        };
        let Some(path) = fenced(&path, root) else {
            continue;
        };
        let Some(text) = read_head(&path) else {
            continue;
        };
        let description = field(&text, "description").unwrap_or_default();
        out.push(WorkspaceCommand {
            name: format!("/{name}"),
            description: if description.is_empty() {
                "Workspace command".to_owned()
            } else {
                description
            },
            args_hint: field(&text, "argument-hint"),
        });
    }
}

/// A name typed into a live agent, so it may only be a name.
fn command_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(value.to_owned())
}

/// Regular files only, and small ones. A fifo here would block the request
/// forever, and the workspace root is chosen by whoever created the pane.
fn read_head(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_COMMAND_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Reads one flat string key out of a file's YAML front matter. Deliberately
/// not a YAML parser: these are flat string keys, and pulling in a parser to
/// read them would be the larger risk. A folded or literal block (`key: >`,
/// `key: |-`, ...) is joined into one line, since skill descriptions are
/// commonly written that way.
pub fn field(text: &str, key: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let mut lines = rest.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        let Some(value) = trimmed
            .strip_prefix(key)
            .and_then(|value| value.strip_prefix(':'))
        else {
            continue;
        };
        let value = if matches!(value.trim(), ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            lines
                .take_while(|next| next.starts_with([' ', '\t']) || next.trim().is_empty())
                .map(str::trim)
                .filter(|next| !next.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            value.to_owned()
        };
        let value = sanitize(&value);
        return (!value.is_empty()).then_some(value);
    }
    None
}

/// Anything read off disk is drawn into a client's UI, so it is stripped of
/// quoting and of control characters and capped before it goes on the wire.
pub fn sanitize(value: &str) -> String {
    value
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DESCRIPTION_CHARS)
        .collect()
}

// ---------------------------------------------------------------------------
// Pane skills
//
// The skills a pane's agent can be asked to run by name, for a composer that
// lists them apart from the slash commands. Only agents whose prompt runs a
// skill typed at the start of a line have a profile: listing a skill the
// client cannot deliver would be offering a button that does nothing.
// ---------------------------------------------------------------------------

/// The most skills one answer carries.
const MAX_PANE_SKILLS: usize = 500;
/// How many manifests one answer may read, duplicates included: the same skill
/// is often linked into several agents' directories.
const MAX_SKILL_READS: usize = 2_000;

/// A user-level skills directory: `<base>/skills`, where `base` is `$env` when
/// that is set and `~/<home>` otherwise.
struct UserSkills {
    env: Option<&'static str>,
    home: &'static str,
}

/// How one agent family takes a skill typed into its prompt.
struct SkillProfile {
    /// The `CommandTable` id this profile extends; that table's
    /// `Layout::Skills` workspace directories are where a repo's skills are
    /// read from.
    table: &'static str,
    /// What the agent's prompt reads as "run this skill" at the start of a line.
    prefix: &'static str,
    user: &'static [UserSkills],
    /// Whether the user's skill wins a name both define. False means the
    /// workspace's does.
    user_wins: bool,
}

/// Claude Code runs a skill as `/name`, loads `$CLAUDE_CONFIG_DIR/skills`
/// (default `~/.claude/skills`), and lets a personal skill beat a project one
/// of the same name. Codex takes `$name` as a skill mention, loads
/// `~/.agents/skills` and, as its older home, `$CODEX_HOME/skills`, and names
/// no precedence (both appear in its own picker), so the workspace's wins.
/// Qoder runs a skill as `/name` from `~/.qoder/skills`, the user's winning.
///
/// OpenCode has no profile: its skills reach the model through its `skill`
/// tool, and the only typed reference is an `@` picker entry its prompt keeps
/// as a mark rather than as text, which text typed into the pane cannot make.
const SKILL_PROFILES: &[SkillProfile] = &[
    SkillProfile {
        table: "claude",
        prefix: "/",
        user: &[UserSkills {
            env: Some("CLAUDE_CONFIG_DIR"),
            home: ".claude",
        }],
        user_wins: true,
    },
    SkillProfile {
        table: "codex",
        prefix: "$",
        user: &[
            UserSkills {
                env: None,
                home: ".agents",
            },
            UserSkills {
                env: Some("CODEX_HOME"),
                home: ".codex",
            },
        ],
        user_wins: false,
    },
    SkillProfile {
        table: "qoder",
        prefix: "/",
        user: &[UserSkills {
            env: None,
            home: ".qoder",
        }],
        user_wins: true,
    },
];

/// One skill as it goes out on the wire.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PaneSkill {
    pub name: String,
    pub description: String,
    /// `user` or `workspace`.
    pub source: &'static str,
}

/// The skills a pane's agent can run, and how to type one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PaneSkills {
    /// `None` for an agent with no typed skill invocation; `skills` is then
    /// empty.
    pub prefix: Option<&'static str>,
    pub skills: Vec<PaneSkill>,
}

/// The skills this pane's agent can run, read from the gateway user's skill
/// directories and the pane's workspace. Blocking filesystem work: call it off
/// the async runtime.
pub fn pane_skills(agent: Option<&str>, cwd: Option<&Path>) -> PaneSkills {
    let home = dirs::home_dir().and_then(|home| std::fs::canonicalize(home).ok());
    pane_skills_in(agent, cwd, home.as_deref(), |key| std::env::var_os(key))
}

fn pane_skills_in(
    agent: Option<&str>,
    cwd: Option<&Path>,
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> PaneSkills {
    let Some(table) = table_for(agent) else {
        return PaneSkills::default();
    };
    let Some(profile) = SKILL_PROFILES.iter().find(|entry| entry.table == table.id) else {
        return PaneSkills::default();
    };

    let mut budget = MAX_SKILL_READS;
    let mut user = Vec::new();
    for dir in profile.user {
        let base = match dir.env.and_then(&env).filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => match home {
                Some(home) => home.join(dir.home),
                None => continue,
            },
        };
        let Ok(base) = std::fs::canonicalize(&base) else {
            continue;
        };
        // The user's skills are commonly links between agents' directories
        // (`~/.claude/skills/x -> ~/.agents/skills/x`), so the fence is the
        // home directory rather than the skills directory; a base its variable
        // puts outside home is its own fence.
        let root = match home {
            Some(home) if base.starts_with(home) => home.to_path_buf(),
            _ => base.clone(),
        };
        let Some(path) = fenced(&base.join("skills"), &root) else {
            continue;
        };
        read_into(&path, &root, "user", &mut budget, &mut user);
    }

    let mut workspace = Vec::new();
    for level in cwd
        .map(|cwd| workspace_levels(cwd, home))
        .unwrap_or_default()
    {
        for dir in table.workspace {
            if dir.layout != Layout::Skills {
                continue;
            }
            let Some(path) = fenced(&level.join(dir.path), &level) else {
                continue;
            };
            read_into(&path, &level, "workspace", &mut budget, &mut workspace);
        }
    }

    let mut skills = if profile.user_wins {
        user.into_iter().chain(workspace).collect::<Vec<_>>()
    } else {
        workspace.into_iter().chain(user).collect::<Vec<_>>()
    };
    // Stable, so the first one read under a name -- the one that wins -- is
    // the one kept.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.dedup_by(|a, b| a.name == b.name);
    skills.truncate(MAX_PANE_SKILLS);
    PaneSkills {
        prefix: Some(profile.prefix),
        skills,
    }
}

fn read_into(
    dir: &Path,
    root: &Path,
    source: &'static str,
    budget: &mut usize,
    out: &mut Vec<PaneSkill>,
) {
    let mut found = Vec::new();
    collect_skills(dir, root, *budget, &mut found);
    *budget -= found.len();
    out.extend(found.into_iter().map(|skill| PaneSkill {
        name: skill.name,
        description: skill.description,
        source,
    }));
}

/// The directories a repo's skills are read from for a pane in `cwd`, nearest
/// first: the directory itself and every parent up to its checkout root, the
/// way Claude Code and Codex walk them. Outside a checkout, only `cwd`. Never
/// the home directory, whose skills are the user's own.
fn workspace_levels(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Ok(cwd) = std::fs::canonicalize(cwd) else {
        return Vec::new();
    };
    let mut levels = Vec::new();
    let mut dir = cwd.as_path();
    while Some(dir) != home {
        levels.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            return levels;
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }
    levels.truncate(1);
    levels
}

// ---------------------------------------------------------------------------
// The descriptor
// ---------------------------------------------------------------------------

/// The `composer` object on a pane's capability descriptor, or `None` for an
/// agent with neither a known profile nor a downloaded command catalog.
/// Commands come only from the catalog; a known profile without a cached
/// catalog still advertises file mentions, with an empty command list.
/// Workspace files do not contribute command suggestions.
pub fn descriptor(agent: Option<&str>, _root: Option<&Path>) -> Option<Value> {
    let table = table_for(agent);
    let snapshot = agent
        .and_then(crate::command_catalog::profile_for)
        .and_then(crate::command_catalog::load);
    if table.is_none() && snapshot.is_none() {
        return None;
    }
    let slash_commands: Vec<Value> = if let Some(snapshot) = &snapshot {
        snapshot
            .commands
            .iter()
            .map(|command| {
                json!({
                    "name": command.name,
                    "description": command.description,
                    "args_hint": command.args_hint,
                    "source": "catalog",
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(json!({
        "version": COMPOSER_VERSION,
        "table": table.map_or_else(|| snapshot.as_ref().map_or("", |entry| entry.agent.as_str()), |entry| entry.id),
        "captured_from": snapshot.as_ref().map_or_else(|| table.map_or("", |entry| entry.captured_from).to_owned(), |entry| format!("{} {}", entry.agent, entry.version)),
        "slash_commands": slash_commands,
        "file_mentions": table.is_none_or(|entry| entry.file_mentions),
    }))
}

// ---------------------------------------------------------------------------
// File search
//
// For `@` mentions: a fuzzy path query over one workspace root, paths only.
// ---------------------------------------------------------------------------

pub const FILE_SEARCH_DEFAULT_LIMIT: usize = 20;
pub const FILE_SEARCH_MAX_LIMIT: usize = 50;
/// Deeper than the asset scan -- a mention names a source file, which lives
/// further down than an artifact -- but still bounded, and still budgeted by
/// entries so a monorepo cannot turn one request into a full crawl.
const FILE_SEARCH_MAX_DEPTH: usize = 12;
const FILE_SEARCH_MAX_ENTRIES: usize = 40_000;

/// One match: a path relative to the workspace root and what kind of file it
/// is. No contents, no absolute path, no size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub name: String,
    pub kind: &'static str,
}

/// Fuzzy path search inside one workspace root.
///
/// Never leaves the root: symlinks are not followed (a link is how a walk would
/// leave the root), dot directories and dependency and build directories are
/// skipped, and every result is emitted as a path relative to the root, so
/// there is nothing on the wire that could name a file outside it.
///
/// An empty query is not an error: it answers with the shallowest files in the
/// root, which is what an `@` with nothing typed after it should show.
pub fn search_files(root: &Path, query: &str, limit: usize) -> Vec<FileHit> {
    let limit = limit.clamp(1, FILE_SEARCH_MAX_LIMIT);
    let needle = query.trim().to_ascii_lowercase();
    let mut scored: Vec<(i32, String, String)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;

    while let Some((dir, depth)) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            visited += 1;
            if visited > FILE_SEARCH_MAX_ENTRIES {
                stack.clear();
                break;
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
                if depth + 1 >= FILE_SEARCH_MAX_DEPTH
                    || crate::ASSET_SKIP_DIRS.contains(&name.as_str())
                {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().to_string();
            let Some(score) = score(&relative, &name, &needle) else {
                continue;
            };
            scored.push((score, relative, name));
        }
    }

    // Best score first; ties go to the shorter path, which is the one nearer
    // the root and almost always the one meant.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path, name)| FileHit {
            kind: kind_for_name(&name),
            path,
            name,
        })
        .collect()
}

/// How well a path answers the query, or `None` when it does not.
///
/// A subsequence match, the way every fuzzy file finder works: the query's
/// characters have to appear in order. What separates a good match from a bad
/// one is where they landed -- consecutive beats scattered, a word boundary
/// beats mid-word, and the file name beats the directories above it.
fn score(relative: &str, name: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        // Nothing typed: prefer the shallowest, shortest paths.
        return Some(-(relative.matches('/').count() as i32) * 10 - relative.len() as i32 / 8);
    }
    let haystack = relative.to_ascii_lowercase();
    let mut score = subsequence_score(&haystack, needle)?;
    // The file name is what the user is thinking of, so a match that fits
    // inside it outranks one smeared across the directories above it.
    if let Some(name_score) = subsequence_score(&name.to_ascii_lowercase(), needle) {
        score += name_score + 40;
    }
    if name.to_ascii_lowercase() == needle {
        score += 60;
    }
    Some(score - relative.len() as i32 / 8)
}

fn subsequence_score(haystack: &str, needle: &str) -> Option<i32> {
    let mut score = 0i32;
    let mut chars = haystack.char_indices().peekable();
    let mut previous: Option<char> = None;
    let mut last_matched: Option<usize> = None;
    for wanted in needle.chars() {
        loop {
            let (index, current) = chars.next()?;
            if current == wanted {
                score += 1;
                if last_matched == Some(index.saturating_sub(1)) {
                    score += 8;
                }
                if previous.is_none_or(|c| matches!(c, '/' | '-' | '_' | '.' | ' ')) {
                    score += 6;
                }
                last_matched = Some(index);
                previous = Some(current);
                break;
            }
            previous = Some(current);
        }
    }
    Some(score)
}

/// What kind of file this is, from the name alone.
///
/// The search reads no file contents -- that is the point of it -- so the kind
/// is a hint for an icon, not the authority. `GET /api/assets/{id}/content`
/// sniffs the bytes again when a file is actually opened, and it is that answer
/// a viewer is chosen from.
pub fn kind_for_name(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "avif" | "tiff" => {
            "image"
        }
        "pdf" => "pdf",
        "md" | "markdown" | "mdx" => "markdown",
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" | "exe" | "dll"
        | "so" | "dylib" | "a" | "o" | "bin" | "wasm" | "class" | "jar" | "pyc" | "db"
        | "sqlite" | "mp3" | "mp4" | "mov" | "wav" | "ttf" | "otf" | "woff" | "woff2" | "ico"
        | "icns" | "node" => "binary",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway tree, canonicalized once, because every check downstream
    /// compares canonical paths and `/tmp` is a symlink on macOS.
    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-composer-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn skill(root: &Path, dir: &str, name: &str, body: &str) {
        let path = root.join(dir).join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), body).unwrap();
    }

    fn names(found: &[WorkspaceCommand]) -> Vec<&str> {
        found.iter().map(|entry| entry.name.as_str()).collect()
    }

    // -- the tables ---------------------------------------------------------

    const CLAUDE_SNAPSHOT: &str = include_str!("../../tests/fixtures/claude-commands.json");
    const CODEX_SNAPSHOT: &str = include_str!("../../tests/fixtures/codex-commands.json");
    const OPENCODE_SNAPSHOT: &str = include_str!("../../tests/fixtures/opencode-commands.json");
    const QODER_SNAPSHOT: &str = include_str!("../../tests/fixtures/qoder-commands.json");

    /// Tables are pinned per agent version, the same way the part dictionaries
    /// are: an agent that renames or drops a command has to show up as a diff in
    /// review rather than as a composer offering something the pane rejects.
    /// Re-pin deliberately, after checking against the installed CLI, with
    /// `UPDATE_COMMAND_SNAPSHOTS=1 cargo test`.
    fn assert_snapshot(id: &str, pinned: &str) {
        let table = table_with_id(id).unwrap();
        let actual = serde_json::to_string_pretty(&json!({
            "id": table.id,
            "captured_from": table.captured_from,
            "agent_match": table.agent_match,
            "file_mentions": table.file_mentions,
            "workspace": table.workspace.iter().map(|dir| json!({
                "path": dir.path,
                "layout": if dir.layout == Layout::Skills { "skills" } else { "commands" },
            })).collect::<Vec<_>>(),
            "commands": table.commands.iter().map(|command| json!({
                "name": command.name,
                "description": command.description,
                "args_hint": command.args_hint,
            })).collect::<Vec<_>>(),
        }))
        .unwrap();
        let name = format!("{id}-commands.json");
        if std::env::var_os("UPDATE_COMMAND_SNAPSHOTS").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(&name);
            std::fs::write(&path, format!("{actual}\n")).unwrap();
            return;
        }
        assert_eq!(actual.trim(), pinned.trim(), "{name} drifted");
    }

    #[test]
    fn the_claude_table_matches_its_pinned_snapshot() {
        assert_snapshot("claude", CLAUDE_SNAPSHOT);
    }

    #[test]
    fn the_codex_table_matches_its_pinned_snapshot() {
        assert_snapshot("codex", CODEX_SNAPSHOT);
    }

    #[test]
    fn the_opencode_table_matches_its_pinned_snapshot() {
        assert_snapshot("opencode", OPENCODE_SNAPSHOT);
    }

    #[test]
    fn the_qoder_table_matches_its_pinned_snapshot() {
        assert_snapshot("qoder", QODER_SNAPSHOT);
    }

    #[test]
    fn every_command_is_something_a_pane_could_actually_be_sent() {
        for table in TABLES {
            let mut seen: Vec<&str> = Vec::new();
            for command in table.commands {
                assert!(
                    command.name.starts_with('/'),
                    "{}: {} has no slash",
                    table.id,
                    command.name
                );
                assert!(
                    !command.name.contains(' '),
                    "{}: {} has a space",
                    table.id,
                    command.name
                );
                assert!(
                    !command.description.is_empty(),
                    "{}: {} has no description",
                    table.id,
                    command.name
                );
                assert!(
                    command.args_hint.is_none_or(|hint| !hint.is_empty()),
                    "{}: {} has an empty hint",
                    table.id,
                    command.name
                );
                assert!(
                    !seen.contains(&command.name),
                    "{}: {} listed twice",
                    table.id,
                    command.name
                );
                seen.push(command.name);
            }
            let mut sorted = seen.clone();
            sorted.sort_unstable();
            assert_eq!(seen, sorted, "{}: table is not in name order", table.id);
            assert!(
                table.commands.len() >= 10,
                "{} types only {} commands",
                table.id,
                table.commands.len()
            );
        }
    }

    #[test]
    fn an_agent_resolves_to_one_table_however_herdr_spells_it() {
        assert_eq!(table_for(Some("Claude Code")).unwrap().id, "claude");
        assert_eq!(table_for(Some("claude-code")).unwrap().id, "claude");
        assert_eq!(table_for(Some("qodercli")).unwrap().id, "qoder");
        assert_eq!(table_for(Some("open-code")).unwrap().id, "opencode");
        assert!(table_for(Some("aider")).is_none());
        assert!(table_for(Some("")).is_none());
        assert!(table_for(None).is_none());
    }

    /// The two dictionaries in this crate answer about the same agents, and a
    /// pane that types parts but offers no commands (or the reverse) is a table
    /// that drifted rather than a decision anybody made.
    #[test]
    fn every_table_has_a_part_dictionary_and_the_same_id() {
        for table in TABLES {
            let dictionary = crate::parts::dictionary_for(Some(table.id))
                .unwrap_or_else(|| panic!("{} has no part dictionary", table.id));
            assert_eq!(dictionary.id, table.id);
            for alias in table.agent_match {
                assert_eq!(
                    crate::parts::dictionary_for(Some(alias)).map(|entry| entry.id),
                    Some(table.id),
                    "{} is keyed on {alias}, which the part dictionaries read differently",
                    table.id
                );
            }
        }
    }

    /// Same drift check `parts.rs` runs, for the same reason: Herdr keeps the
    /// ecosystem's agent list as detection manifests it updates out of band, and
    /// a table keyed on a name Herdr does not know describes a pane that would
    /// never reach us. The manifests live on the machine Herdr runs on, so the
    /// check is skipped where they are absent. The other direction -- an agent
    /// with no table yet -- is the roadmap, not a failure, so it is reported.
    /// Run with `cargo test -- --nocapture` to read it.
    #[test]
    fn no_table_is_keyed_on_an_agent_herdr_does_not_know() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let root = std::path::Path::new(&home).join(".local/state/herdr/agent-detection/remote");
        let Ok(entries) = std::fs::read_dir(&root) else {
            eprintln!("herdr manifests not on this machine; drift check skipped");
            return;
        };
        let mut known: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines().take_while(|line| !line.starts_with("[[")) {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                if !matches!(key.trim(), "id" | "aliases") {
                    continue;
                }
                known.extend(
                    value
                        .split('"')
                        .skip(1)
                        .step_by(2)
                        .map(|name| name.to_ascii_lowercase()),
                );
            }
        }
        if known.is_empty() {
            eprintln!("herdr manifests unreadable; drift check skipped");
            return;
        }
        for table in TABLES {
            for alias in table.agent_match {
                assert!(
                    known.iter().any(|name| name.contains(alias)),
                    "table {} is keyed on {alias}, which no herdr manifest reports",
                    table.id
                );
            }
        }
        let uncovered: Vec<&String> = known
            .iter()
            .filter(|name| table_for(Some(name)).is_none())
            .collect();
        eprintln!("herdr agents with no command table yet: {uncovered:?}");
    }

    // -- workspace discovery ------------------------------------------------

    #[test]
    fn a_workspace_skill_is_read_from_its_front_matter() {
        let root = workspace("skills");
        skill(
            &root,
            ".claude/skills",
            "ship-release",
            "---\nname: ship-release\ndescription: Cut and publish a release\n---\nbody\n",
        );
        skill(
            &root,
            ".claude/skills",
            "dev-workflow",
            "---\nname: dev-workflow\ndescription: The card-backed development flow\n---\n",
        );
        std::fs::create_dir_all(root.join(".claude/commands")).unwrap();
        std::fs::write(
            root.join(".claude/commands/deploy.md"),
            "---\ndescription: Deploy to production\nargument-hint: [environment]\n---\n",
        )
        .unwrap();

        let found = workspace_commands(&root, CLAUDE_WORKSPACE);
        assert_eq!(
            names(&found),
            vec!["/deploy", "/dev-workflow", "/ship-release"]
        );
        let deploy = found.iter().find(|c| c.name == "/deploy").unwrap();
        assert_eq!(deploy.description, "Deploy to production");
        assert_eq!(deploy.args_hint.as_deref(), Some("[environment]"));
        let skill = found.iter().find(|c| c.name == "/ship-release").unwrap();
        assert_eq!(skill.description, "Cut and publish a release");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_skill_without_front_matter_still_answers_under_its_directory_name() {
        let root = workspace("bare");
        skill(&root, ".claude/skills", "no-matter", "just a body\n");
        // A front-matter name that is not a usable command name is ignored, and
        // the directory name answers instead.
        skill(
            &root,
            ".claude/skills",
            "hostile",
            "---\nname: rm -rf /; echo\ndescription: nope\n---\n",
        );
        let found = workspace_commands(&root, CLAUDE_WORKSPACE);
        assert_eq!(names(&found), vec!["/hostile", "/no-matter"]);
        assert_eq!(
            found
                .iter()
                .find(|c| c.name == "/no-matter")
                .unwrap()
                .description,
            "Workspace skill"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovery_never_leaves_the_workspace_root() {
        let root = workspace("fence");
        let outside = workspace("outside");
        std::fs::create_dir_all(outside.join("secret")).unwrap();
        std::fs::write(
            outside.join("secret/SKILL.md"),
            "---\nname: secret\ndescription: not yours\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(outside.join("commands")).unwrap();
        std::fs::write(
            outside.join("commands/leak.md"),
            "---\ndescription: no\n---\n",
        )
        .unwrap();

        // The whole skills directory is a link out of the repo.
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".claude/skills")).unwrap();
        // And one skill inside a real skills directory is a link out of it.
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join(".agents/skills/borrowed"))
            .unwrap();
        assert!(workspace_commands(&root, CODEX_WORKSPACE).is_empty());
        // And one command file is a link out of it.
        std::fs::create_dir_all(root.join(".claude/commands")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("commands/leak.md"),
            root.join(".claude/commands/leak.md"),
        )
        .unwrap();

        assert!(workspace_commands(&root, CLAUDE_WORKSPACE).is_empty());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn discovery_is_capped_and_skips_anything_that_is_not_a_small_regular_file() {
        let root = workspace("caps");
        for index in 0..(MAX_WORKSPACE_COMMANDS + 20) {
            skill(
                &root,
                ".claude/skills",
                &format!("skill-{index:03}"),
                "---\ndescription: one of many\n---\n",
            );
        }
        assert_eq!(
            workspace_commands(&root, CLAUDE_WORKSPACE).len(),
            MAX_WORKSPACE_COMMANDS
        );

        let other = workspace("caps-files");
        std::fs::create_dir_all(other.join(".claude/commands")).unwrap();
        std::fs::write(
            other.join(".claude/commands/good.md"),
            "---\ndescription: Fine\n---\n",
        )
        .unwrap();
        std::fs::write(
            other.join(".claude/commands/big.md"),
            vec![b'x'; (MAX_COMMAND_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        std::fs::create_dir_all(other.join(".claude/commands/nested.md")).unwrap();
        std::fs::write(other.join(".claude/commands/notes.txt"), "ignored").unwrap();
        assert_eq!(
            names(&workspace_commands(&other, CLAUDE_WORKSPACE)),
            vec!["/good"]
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn a_workspace_with_none_of_the_directories_answers_with_nothing() {
        let root = workspace("empty");
        assert!(workspace_commands(&root, CLAUDE_WORKSPACE).is_empty());
        assert!(workspace_commands(&root, CODEX_WORKSPACE).is_empty());
        assert!(workspace_commands(&root, OPENCODE_WORKSPACE).is_empty());
        assert!(workspace_commands(&root, QODER_WORKSPACE).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn front_matter_is_stripped_of_quoting_and_control_characters() {
        assert_eq!(
            field(
                "---\ndescription: \"line\u{1b}[2Kbreak\"\n---\n",
                "description"
            )
            .as_deref(),
            Some("line[2Kbreak")
        );
        assert!(field("no front matter\n", "description").is_none());
        assert!(field("---\ndescription:\n---\n", "description").is_none());
    }

    // -- pane skills --------------------------------------------------------

    fn pane(agent: &str, cwd: &Path, home: &Path) -> PaneSkills {
        pane_skills_in(Some(agent), Some(cwd), Some(home), |_| None)
    }

    fn listed(found: &PaneSkills) -> Vec<(&str, &str)> {
        found
            .skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.source))
            .collect()
    }

    #[test]
    fn a_skill_reads_its_name_and_a_block_description_from_front_matter() {
        let home = workspace("skills-home");
        skill(
            &home,
            ".claude/skills",
            "dir-name",
            "---\nname: ship-it\ndescription: >-\n  Cut a release\n  and publish it.\nother: x\n---\nbody\n",
        );
        skill(&home, ".claude/skills", "bare", "no front matter\n");
        let found = pane_skills_in(Some("claude"), None, Some(&home), |_| None);
        assert_eq!(
            found.skills,
            vec![
                PaneSkill {
                    name: "bare".into(),
                    description: String::new(),
                    source: "user",
                },
                PaneSkill {
                    name: "ship-it".into(),
                    description: "Cut a release and publish it.".into(),
                    source: "user",
                },
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn only_agents_that_run_a_typed_skill_get_a_prefix_and_a_list() {
        let home = workspace("skills-prefix");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        for dir in [".claude", ".agents", ".codex", ".qoder", ".opencode"] {
            skill(&repo, &format!("{dir}/skills"), "probe", "---\n---\n");
        }
        for (agent, prefix) in [
            ("Claude Code", Some("/")),
            ("codex", Some("$")),
            ("qoder", Some("/")),
            ("opencode", None),
            ("aider", None),
        ] {
            let found = pane(agent, &repo, &home);
            assert_eq!(found.prefix, prefix, "{agent}");
            assert_eq!(found.skills.is_empty(), prefix.is_none(), "{agent}");
        }
        assert_eq!(
            pane_skills_in(None, Some(&repo), Some(&home), |_| None),
            PaneSkills::default()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_name_both_define_goes_to_the_side_the_agent_prefers() {
        let home = workspace("skills-precedence");
        let repo = home.join("repo");
        let cwd = repo.join("packages/app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        // Claude Code: a personal skill beats a project one.
        skill(
            &home,
            ".claude/skills",
            "deploy",
            "---\ndescription: mine\n---\n",
        );
        skill(
            &repo,
            ".claude/skills",
            "deploy",
            "---\ndescription: repo\n---\n",
        );
        skill(&repo, ".claude/skills", "lint", "---\n---\n");
        let claude = pane("claude", &cwd, &home);
        assert_eq!(
            listed(&claude),
            vec![("deploy", "user"), ("lint", "workspace")]
        );
        assert_eq!(claude.skills[0].description, "mine");

        // Codex names no precedence, so the workspace's wins, and the level
        // nearest the pane beats the checkout root.
        skill(&home, ".agents/skills", "deploy", "---\n---\n");
        skill(
            &repo,
            ".agents/skills",
            "deploy",
            "---\ndescription: root\n---\n",
        );
        skill(
            &cwd,
            ".agents/skills",
            "deploy",
            "---\ndescription: near\n---\n",
        );
        let codex = pane("codex", &cwd, &home);
        assert_eq!(listed(&codex), vec![("deploy", "workspace")]);
        assert_eq!(codex.skills[0].description, "near");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_skill_linked_out_of_its_fence_is_skipped() {
        let home = workspace("skills-fence");
        let outside = workspace("skills-fence-outside");
        skill(&outside, "", "secret", "---\ndescription: not yours\n---\n");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        // In the user's directories the fence is home: a link between agents'
        // directories is read, a link out of home is not.
        skill(&home, ".agents/skills", "shared", "---\n---\n");
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(
            home.join(".agents/skills/shared"),
            home.join(".claude/skills/shared"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), home.join(".claude/skills/secret"))
            .unwrap();
        // In a workspace the fence is the directory it was found under, so a
        // link from the repo into home is skipped too.
        std::fs::create_dir_all(repo.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(
            home.join(".agents/skills/shared"),
            repo.join(".claude/skills/borrowed"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), repo.join(".claude/skills/leak"))
            .unwrap();

        assert_eq!(
            listed(&pane("claude", &repo, &home)),
            vec![("shared", "user")]
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    // -- the descriptor -----------------------------------------------------

    #[test]
    fn an_unknown_agent_has_no_composer_descriptor_at_all() {
        assert!(descriptor(Some("aider"), None).is_none());
        assert!(descriptor(None, None).is_none());
    }

    #[test]
    fn workspace_commands_do_not_enter_the_catalog_descriptor() {
        let root = workspace("descriptor");
        skill(
            &root,
            ".claude/skills",
            "review",
            "---\nname: review\ndescription: This repo's own review\n---\n",
        );
        let value = descriptor(Some("Claude Code"), Some(&root)).unwrap();
        assert_eq!(value["table"], "claude");
        assert_eq!(value["file_mentions"], true);
        let commands = value["slash_commands"].as_array().unwrap();
        assert!(commands.iter().all(|entry| entry["source"] == "catalog"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_descriptor_without_a_root_uses_only_the_catalog() {
        let value = descriptor(Some("codex"), None).unwrap();
        let commands = value["slash_commands"].as_array().unwrap();
        assert!(commands
            .iter()
            .all(|entry| entry["source"] == "catalog" && entry["name"].is_string()));
    }

    // -- file search --------------------------------------------------------

    fn touch(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    fn paths(hits: &[FileHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.path.as_str()).collect()
    }

    #[test]
    fn search_ranks_the_file_the_query_names_first() {
        let root = workspace("search");
        touch(&root, "src/main.rs");
        touch(&root, "src/parts.rs");
        touch(&root, "docs/content-model.md");
        touch(&root, "tests/fixtures/main-transcript.txt");

        let hits = search_files(&root, "main.rs", 10);
        assert_eq!(hits[0].path, "src/main.rs");
        assert_eq!(hits[0].name, "main.rs");
        assert_eq!(hits[0].kind, "text");

        // A directory fragment still finds it, and the markdown is typed.
        let hits = search_files(&root, "content", 10);
        assert_eq!(hits[0].path, "docs/content-model.md");
        assert_eq!(hits[0].kind, "markdown");

        // Nothing typed answers with the shallowest files rather than an error.
        assert!(!search_files(&root, "", 10).is_empty());
        // A query nothing matches is empty, not everything.
        assert!(search_files(&root, "zzzznotathing", 10).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_never_leaves_the_workspace_root() {
        let root = workspace("search-fence");
        let outside = workspace("search-outside");
        touch(&outside, "secrets.env");
        touch(&root, "inside.txt");
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.join("secrets.env"), root.join("secrets.env")).unwrap();

        let hits = search_files(&root, "", 50);
        assert_eq!(paths(&hits), vec!["inside.txt"]);
        assert!(search_files(&root, "secrets", 50).is_empty());
        // Every path that does come back is relative, so nothing on the wire
        // can name a file outside the root.
        assert!(hits.iter().all(|hit| !hit.path.starts_with('/')));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn search_skips_dependency_build_and_dot_directories() {
        let root = workspace("search-skip");
        touch(&root, "keeper.rs");
        touch(&root, ".git/objects/keeper.rs");
        touch(&root, "node_modules/pkg/keeper.rs");
        touch(&root, "target/debug/keeper.rs");
        touch(&root, "dist/keeper.rs");
        touch(&root, ".env");

        assert_eq!(paths(&search_files(&root, "keeper", 50)), vec!["keeper.rs"]);
        assert!(search_files(&root, "env", 50).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_is_capped_whatever_the_caller_asks_for() {
        let root = workspace("search-cap");
        for index in 0..(FILE_SEARCH_MAX_LIMIT + 30) {
            touch(&root, &format!("note-{index:03}.txt"));
        }
        assert_eq!(search_files(&root, "note", 5).len(), 5);
        assert_eq!(
            search_files(&root, "note", 10_000).len(),
            FILE_SEARCH_MAX_LIMIT
        );
        assert_eq!(search_files(&root, "note", 0).len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_kind_is_decided_from_the_name_and_never_from_the_bytes() {
        assert_eq!(kind_for_name("diagram.PNG"), "image");
        assert_eq!(kind_for_name("report.pdf"), "pdf");
        assert_eq!(kind_for_name("README.md"), "markdown");
        assert_eq!(kind_for_name("libfoo.dylib"), "binary");
        assert_eq!(kind_for_name("Makefile"), "text");
        assert_eq!(kind_for_name("main.rs"), "text");
    }
}
