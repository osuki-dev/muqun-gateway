//! Backend-neutral terminal workspace boundary.
//!
//! HTTP handlers and application workflows speak the types in this module.
//! Herdr JSON-RPC and tmux format strings are outbound adapter details and must
//! not leak through this boundary.

mod herdr;
mod model;
mod tmux;

pub mod compat;

pub use herdr::HerdrBackend;
pub use model::{
    AgentStatus, BackendError, BackendFuture, BackendKind, BackendMetadata, CreateTab,
    CreateWorkspace, OutputFormat, OutputSource, Pane, PaneId, PaneOutput, ReadPane,
    SplitDirection, SplitPane, Tab, TabId, TerminalBackend, Workspace, WorkspaceId,
};
pub use tmux::TmuxBackend;
