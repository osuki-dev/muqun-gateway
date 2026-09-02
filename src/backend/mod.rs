//! Backend-neutral terminal workspace boundary.
//!
//! HTTP handlers and application workflows speak the types in this module.
//! Herdr JSON-RPC and tmux format strings are outbound adapter details and must
//! not leak through this boundary.

mod herdr;
mod model;
mod registry;
mod tmux;
mod tmux_wire;

pub mod compat;

pub use herdr::HerdrBackend;
pub use model::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, CreateTab, CreateWorkspace, OutputFormat, OutputSource, Pane,
    PaneId, PaneOutput, PaneRange, ReadPane, SplitDirection, SplitPane, StartAgent, StartedAgent,
    Tab, TabId, TerminalBackend, Workspace, WorkspaceId, Worktree, WorktreePlacement,
    WorktreeRequest,
};
pub use registry::BackendRegistry;
pub use tmux::{TmuxBackend, TMUX_PROGRAM};
pub use tmux_wire::TmuxWireIds;
