use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

opaque_id!(WorkspaceId);
opaque_id!(TabId);
opaque_id!(PaneId);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Herdr,
    Tmux,
}

impl BackendKind {
    pub fn is_herdr(&self) -> bool {
        *self == Self::Herdr
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Tmux => "tmux",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendMetadata {
    pub kind: BackendKind,
    pub version: Option<String>,
    pub protocol: Option<u64>,
    /// Native metadata retained only for the legacy compatibility envelope.
    /// Terminal use cases must not inspect it.
    pub compatibility_response: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackendActivity {
    /// Normalized underscore name used by gateway filters.
    pub name: String,
    /// Existing Herdr-compatible event envelope retained at the HTTP edge.
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub number: Option<u32>,
    pub label: String,
    pub focused: bool,
    pub active_tab_id: Option<TabId>,
    pub tab_count: Option<u32>,
    pub pane_count: Option<u32>,
    pub agent_status: AgentStatus,
    pub repo_root: Option<PathBuf>,
    pub checkout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: TabId,
    pub workspace_id: WorkspaceId,
    pub label: String,
    pub focused: bool,
    pub active_pane_id: Option<PaneId>,
    pub pane_count: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentStatus {
    Starting,
    Working,
    Idle,
    Blocked,
    Completed,
    #[default]
    Unknown,
}

impl AgentStatus {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("starting") => Self::Starting,
            Some("working") => Self::Working,
            Some("idle") => Self::Idle,
            Some("blocked") => Self::Blocked,
            Some("done" | "completed") => Self::Completed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    /// Native terminal identifier when it differs from the pane identifier.
    /// Herdr exposes both; tmux uses the pane id for both roles.
    pub terminal_id: Option<String>,
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub label: Option<String>,
    pub terminal_title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub focused: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub revision: Option<u64>,
    /// The foreground executable as reported by the backend. Agent detection
    /// is an application concern because the same catalogue serves both
    /// Herdr and tmux.
    pub foreground_command: Option<String>,
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    /// Display rows available above the current viewport.
    pub max_offset_from_bottom: Option<u32>,
    pub viewport_rows: Option<u32>,
    /// Whether the pane's foreground program owns an alternate screen (tmux's
    /// `#{alternate_on}`) -- a modal editor, an agent wrapped in one, anything
    /// that repaints in place rather than prints and scrolls. `None` where the
    /// backend cannot say (Herdr; a tmux pane before its first list).
    ///
    /// Gateway-internal, and currently unread. Card #795 first added this for
    /// `ScrollbackStore` (`src/scrollback.rs`) to gate its replace-vs-
    /// accumulate fold on, but measured wrong live: `alternate_on` is 1 for an
    /// agent pane exactly as it is for an editor's -- Claude Code, opencode
    /// and codex all own an alternate screen too (see that module's own
    /// doc) -- so it cannot tell an editor's pane from an agent's, and a card
    /// #795 follow-up dropped it as that switch in favour of
    /// `foreground_command` (`is_editor_command` in `src/scrollback.rs`).
    /// Left here, still riding the same envelope `ScrollbackStore::observe`
    /// already walks (`compat::pane_list`), as a raw signal a future reader
    /// may still want.
    pub alternate_on: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub target: String,
    pub pane_id: PaneId,
    pub workspace_id: Option<WorkspaceId>,
    pub tab_id: Option<TabId>,
    pub kind: Option<String>,
    pub display_agent: Option<String>,
    pub status: AgentStatus,
    pub state_change_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartAgent {
    pub pane_id: PaneId,
    pub kind: String,
    pub command: String,
    pub executable: Option<PathBuf>,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedAgent {
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRequest {
    pub cwd: PathBuf,
    pub branch: String,
    pub label: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlacement {
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Ansi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPane {
    pub pane_id: PaneId,
    pub source: OutputSource,
    pub format: OutputFormat,
    pub lines: u32,
    /// Absolute half-open range to read. `None` keeps the tail-of-`lines`
    /// behaviour every existing caller relies on.
    pub start: Option<u32>,
    pub end: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOutput {
    pub text: String,
    pub revision: Option<u64>,
    /// Which absolute lines this text is, where the backend can say.
    pub range: Option<PaneRange>,
}

/// Which absolute lines of a pane a read actually returned.
///
/// Line 0 is the oldest line the pane still holds and `total` is one past the
/// newest, so `start == 0` means the reader has reached the top. This is
/// `Some` only where a backend actually served the range it was asked for --
/// tmux, whose `capture-pane -S/-E` can address an absolute span. A backend
/// that cannot honour a requested range (herdr, whose `pane.read` has no
/// range parameter) reports `None` rather than describing a tail it merely
/// happened to serve: a range built from an unrequested tail is
/// indistinguishable, on the wire, from one that genuinely satisfied the
/// request, and `start == 0` on the former would be read as "top reached"
/// when it is nothing of the kind. `Option<PaneRange>` exists exactly to keep
/// that distinction, so a client on `None` falls back to the measured paging
/// behaviour it already has rather than trusting a range that was never
/// honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRange {
    pub start: u32,
    pub end: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateWorkspace {
    pub cwd: Option<PathBuf>,
    pub label: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTab {
    pub workspace_id: Option<WorkspaceId>,
    pub cwd: Option<PathBuf>,
    pub label: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitPane {
    pub pane_id: PaneId,
    pub direction: SplitDirection,
    pub ratio: Option<f64>,
    pub cwd: Option<PathBuf>,
    /// Environment requested for the new pane. Backends that support native
    /// pane environments apply it without routing through a shell.
    pub env: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug)]
pub enum BackendError {
    Unavailable,
    Unsupported(&'static str),
    InvalidResponse(&'static str),
    InvalidTarget(&'static str),
    Refused {
        code: Option<String>,
        message: String,
    },
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("terminal backend is unavailable"),
            Self::Unsupported(capability) => {
                write!(formatter, "terminal backend does not support {capability}")
            }
            Self::InvalidResponse(context) => {
                write!(formatter, "terminal backend returned an invalid {context}")
            }
            Self::InvalidTarget(kind) => write!(formatter, "invalid {kind} id"),
            Self::Refused { code, message } => match code {
                Some(code) => write!(formatter, "terminal backend refused ({code}): {message}"),
                None => write!(formatter, "terminal backend refused: {message}"),
            },
        }
    }
}

impl std::error::Error for BackendError {}

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send + 'a>>;
pub type BackendActivityStream =
    Pin<Box<dyn Stream<Item = Result<BackendActivity, BackendError>> + Send>>;

/// Backend-neutral port used by HTTP use cases and background workflows.
///
/// Implementations own transport details and translate their native IDs and
/// output into this model. They must not expose Herdr JSON or tmux format
/// strings to callers.
pub trait TerminalBackend: Send + Sync {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata>;
    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream>;
    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>>;
    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>>;
    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>>;
    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>>;
    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane>;
    fn get_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, Agent>;
    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput>;
    fn create_workspace<'a>(&'a self, request: &'a CreateWorkspace)
        -> BackendFuture<'a, Workspace>;
    fn focus_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()>;
    fn rename_workspace<'a>(&'a self, id: &'a WorkspaceId, label: &'a str)
        -> BackendFuture<'a, ()>;
    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()>;
    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab>;
    fn focus_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()>;
    fn rename_tab<'a>(&'a self, id: &'a TabId, label: &'a str) -> BackendFuture<'a, ()>;
    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()>;
    fn focus_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()>;
    fn rename_pane<'a>(&'a self, id: &'a PaneId, label: &'a str) -> BackendFuture<'a, ()>;
    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()>;
    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane>;
    fn send_text<'a>(&'a self, id: &'a PaneId, text: &'a str) -> BackendFuture<'a, ()>;
    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()>;
    fn focus_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, ()>;
    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()>;
    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent>;
    fn list_worktrees<'a>(&'a self, _cwd: &'a PathBuf) -> BackendFuture<'a, Vec<Worktree>> {
        Box::pin(async { Err(BackendError::Unsupported("worktrees")) })
    }
    fn open_worktree<'a>(
        &'a self,
        _request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async { Err(BackendError::Unsupported("worktrees")) })
    }
    fn create_worktree<'a>(
        &'a self,
        _request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async { Err(BackendError::Unsupported("worktrees")) })
    }

    /// Whether the backend's own server is there at all, apart from whether
    /// it currently holds anything.
    ///
    /// For most backends `list_panes`'s own success/failure already answers
    /// this on its own -- a connection that cannot be made surfaces as an
    /// error, full stop. This exists only because tmux's `list_output`
    /// deliberately folds "no server running" into an *empty* topology for
    /// every other caller (closing the last workspace must not start looking
    /// like an error), which would otherwise make a dead tmux server
    /// indistinguishable from a live one that happens to hold nothing. A
    /// caller that needs to tell those two apart -- `GET /api/sessions`'s
    /// liveness probe -- asks here first.
    ///
    /// Defaults to "reachable", so a backend that never overrides this keeps
    /// relying purely on `list_panes`'s own error path, exactly as before
    /// this method existed.
    fn probe_reachable(&self) -> BackendFuture<'_, bool> {
        Box::pin(async { Ok(true) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_defaults_to_herdr_for_old_configs() {
        assert_eq!(BackendKind::default(), BackendKind::Herdr);
        assert_eq!(
            serde_json::from_str::<BackendKind>(r#""tmux""#).unwrap(),
            BackendKind::Tmux
        );
    }

    #[test]
    fn agent_status_accepts_the_existing_backend_vocabulary() {
        assert_eq!(AgentStatus::parse(Some("WORKING")), AgentStatus::Working);
        assert_eq!(AgentStatus::parse(Some("done")), AgentStatus::Completed);
        assert_eq!(
            AgentStatus::parse(Some("something-new")),
            AgentStatus::Unknown
        );
        assert_eq!(AgentStatus::parse(None), AgentStatus::Unknown);
    }
}
