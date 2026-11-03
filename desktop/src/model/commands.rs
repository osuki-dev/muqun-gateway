//! UI → core. The UI thread only ever sends one of these; it never awaits,
//! never takes a runtime lock, never calls the hub (architecture §2).

use std::path::PathBuf;

use muqun_gateway::agents::{AgentSessionId, PermissionDecision};
use tokio::sync::mpsc;

use crate::config::{ConfigEdit, Scheme};

#[derive(Debug, Clone)]
pub enum Command {
    /// Re-read agent status and the session list.
    Refresh,
    /// Make a session the active one and read its snapshot.
    OpenSession(AgentSessionId),
    /// Start a session for `agent` in `cwd`, remember `cwd`, open it. (The
    /// composer starts sessions through `Compose`; the QA harness uses this.)
    CreateSession {
        agent: String,
        cwd: PathBuf,
    },
    SendPrompt {
        asid: AgentSessionId,
        text: String,
    },
    Interrupt(AgentSessionId),
    ReplyPermission {
        asid: AgentSessionId,
        id: String,
        decision: PermissionDecision,
    },
    /// The next older page of the session's history (architecture §3
    /// `earlier`).
    LoadEarlier(AgentSessionId),
    ReplyForm {
        asid: AgentSessionId,
        id: String,
        /// `{ field key: value }`, as the gateway's form route takes it.
        answers: serde_json::Value,
    },
    /// Folders under a typed prefix. Debounced on the core side; only the
    /// answer for the newest prefix is delivered. (The start page picks
    /// folders with the system dialog; this waits for a typed path field.)
    #[allow(dead_code)]
    CompleteDirectory {
        prefix: String,
    },
    /// The folders `agent` already knows as projects.
    LoadProjects {
        agent: String,
    },
    /// The owner changed a setting or arranged the window. Applied at once
    /// in memory, written to disk after 500 ms without another edit.
    Edit(ConfigEdit),
    /// The composer's work (`model/compose.rs`): files, catalogue, staging,
    /// delivery, the queue.
    Compose(super::compose::ComposeCommand),
    /// Themes (`theme/service.rs`). The library list, and the gallery index
    /// (from its 1 h cache when fresh) with its card pictures.
    ThemeRefresh,
    /// Download theme `id` from the gallery, check its SHA-256, install it.
    ThemeInstall(String),
    /// Remove an installed theme (built-ins cannot be).
    ThemeRemove(String),
    /// Make theme `id` current in `scheme`, and remember both.
    ThemeApply(String, Scheme),
    /// Install a `.muqun-theme` from disk (file dialog or drop).
    ThemeImportFile(PathBuf),
}

pub type CommandSender = mpsc::UnboundedSender<Command>;
pub type CommandReceiver = mpsc::UnboundedReceiver<Command>;

pub fn channel() -> (CommandSender, CommandReceiver) {
    mpsc::unbounded_channel()
}

/// Send, from the UI thread. The receiver only goes away when the core
/// thread has ended, at which point there is nothing left to tell.
pub fn send(tx: &CommandSender, command: Command) {
    if tx.send(command).is_err() {
        tracing::warn!("the core thread is gone; a UI command was dropped");
    }
}
