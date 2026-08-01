use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub label: String,
    pub focused: bool,
    pub active_tab_id: Option<TabId>,
    pub tab_count: Option<u32>,
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
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub label: Option<String>,
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
    /// True when the backend itself retains output above the viewport.
    pub has_scrollback: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOutput {
    pub text: String,
    pub revision: Option<u64>,
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
}

#[derive(Debug)]
pub enum BackendError {
    Unavailable,
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

/// Backend-neutral port used by HTTP use cases and background workflows.
///
/// Implementations own transport details and translate their native IDs and
/// output into this model. They must not expose Herdr JSON or tmux format
/// strings to callers.
pub trait TerminalBackend: Send + Sync {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata>;
    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>>;
    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>>;
    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>>;
    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane>;
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
