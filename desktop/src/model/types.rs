//! Plain data shared by the core side and the UI.

/// How many sessions the sidebar lists. P0 has no paging.
pub const SESSION_LIST_LIMIT: usize = 100;

/// A listed session's id and folder, for resolving projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub asid: String,
    pub directory: Option<String>,
}

/// One changed path from `get_vcs_diff`, for the top-bar diff panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Unified patch text when the agent provided it (may be empty).
    pub patch: String,
}

/// What `get_vcs_diff` says about the open session's working tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub asid: String,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    /// Paths shown in the panel (capped); empty when the agent only gave counts.
    pub paths: Vec<DiffFile>,
}
