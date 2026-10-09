use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct TaskId(pub String);

impl TaskId {
    /// `t_` followed by 22 URL-safe characters.
    pub(crate) fn new() -> TaskId {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(uuid::Uuid::new_v4().as_bytes());
        TaskId(format!("t_{raw}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskKind {
    Delegation,
    Command,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskState {
    #[default]
    Queued,
    Dispatching,
    Working,
    Waiting,
    IdleUnconfirmed,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// Once it works, a task moves freely among working, waiting and
    /// idle_unconfirmed, and can end from any of them: a confirm of a waiting
    /// task, or a pane closing under a paused one, is a direct move, never a
    /// walk through a `working` that did not happen.
    pub(crate) fn can_move_to(self, to: TaskState) -> bool {
        use TaskState::*;
        match self {
            Queued => matches!(to, Dispatching | Cancelled),
            Dispatching => matches!(to, Working | Failed | Cancelled),
            Working | Waiting | IdleUnconfirmed => {
                to != self && !matches!(to, Queued | Dispatching)
            }
            Done | Failed | Cancelled => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TaskBinding {
    None,
    AgentSession {
        agent_id: String,
        asid: String,
    },
    Pane {
        session_id: String,
        pane_id: String,
        tab_id: Option<String>,
        /// The pane's shell process when the task was bound. tmux reuses
        /// `%N` ids after a server restart, so a pane with this id but
        /// another shell is not the task's pane. `None` (rows written before
        /// this existed, or a backend that could not tell) matches by id only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_pid: Option<u32>,
        /// The terminal agent kind (`codex`, `claude`, ...) a delegation
        /// started in this pane; absent for a command task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrincipalKind {
    Device,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskPrincipal {
    pub kind: PrincipalKind,
    pub id: String,
}

/// Where a task came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskOrigin {
    /// A paired device opened it (`POST /api/tasks`).
    Device,
    /// Work the gateway already tracks asked for it: its own agent service,
    /// or the pane of an open task.
    Agent,
    /// Any other MCP host, through `exec.run` or `tasks.delegate`. Rows
    /// stored before origins existed read as this.
    #[default]
    Mcp,
}

/// Who asks for a new task, on whose behalf, and under which task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requested {
    /// Owns the task: only it (or a device) may read, wait for or cancel it.
    pub by: TaskPrincipal,
    pub origin: TaskOrigin,
    /// The open task the request was made from, when one could be told.
    pub parent: Option<TaskId>,
    /// The requester's display name (a device's name), when it has one.
    pub name: Option<String>,
}

impl Requested {
    /// An MCP host's request with no parent.
    #[cfg(test)]
    pub(crate) fn mcp(by: TaskPrincipal) -> Self {
        Self {
            by,
            origin: TaskOrigin::Mcp,
            parent: None,
            name: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Task {
    pub id: TaskId,
    pub kind: TaskKind,
    pub goal: String,
    pub cwd: Option<String>,
    pub binding: TaskBinding,
    pub state: TaskState,
    pub summary: Option<String>,
    pub exit_code: Option<i32>,
    pub created_by: TaskPrincipal,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub seq: u64,
    pub parent: Option<TaskId>,
    pub origin: TaskOrigin,
    /// A short title the requester gave, if any.
    pub label: Option<String>,
    /// The requester's display name (a device's name), if it had one.
    pub requester_name: Option<String>,
}

/// A task's direct children by state, computed when the task is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ChildCounts {
    pub total: u32,
    /// Not finished and not waiting on the user.
    pub active: u32,
    pub waiting: u32,
    pub failed: u32,
}

impl ChildCounts {
    pub(crate) fn add(&mut self, state: TaskState, count: u32) {
        self.total += count;
        match state {
            TaskState::Waiting => self.waiting += count,
            TaskState::Failed => self.failed += count,
            state if !state.is_terminal() => self.active += count,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskEvent {
    pub task_id: TaskId,
    pub seq: u64,
    pub at_ms: u64,
    pub from: Option<TaskState>,
    pub to: TaskState,
    pub source: String,
    pub note: Option<String>,
}

/// A requested move. `Some` fields replace the task's value, `None` keeps it.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskChange {
    pub to: TaskState,
    pub source: String,
    pub note: Option<String>,
    pub summary: Option<String>,
    pub exit_code: Option<i32>,
    pub binding: Option<TaskBinding>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TaskFilter {
    pub created_by: Option<TaskPrincipal>,
    pub states: Option<Vec<TaskState>>,
    /// `0` means the store's default page size.
    pub limit: usize,
    /// Only this task and its descendants.
    pub root: Option<TaskId>,
    /// Only tasks bound to an agent session of this agent.
    pub agent_id: Option<String>,
    /// Only tasks bound to this agent session.
    pub asid: Option<String>,
    /// Only tasks bound to this pane: `(session_id, pane_id)`.
    pub pane: Option<(String, String)>,
}

/// `text` cut to at most `max` bytes on a char boundary, keeping the end.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// `text` as one line of at most `max` characters: whitespace runs become one
/// space, and a longer text keeps its start and ends in `…`.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_keeps_the_end_on_a_char_boundary() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "…def");
        let clipped = clip("ééé", 3);
        assert!(clipped.ends_with('é') && clipped.len() <= 3 + "…".len());
    }

    #[test]
    fn one_line_collapses_whitespace_and_keeps_the_start() {
        assert_eq!(one_line("  all\n\n green \t ok ", 240), "all green ok");
        let long = "word ".repeat(100);
        let line = one_line(&long, 240);
        assert_eq!(line.chars().count(), 240);
        assert!(line.starts_with("word word") && line.ends_with('…'));
        assert_eq!(one_line("ééé", 2), "é…");
    }

    #[test]
    fn child_counts_sort_states_into_buckets() {
        let mut counts = ChildCounts::default();
        counts.add(TaskState::Working, 2);
        counts.add(TaskState::Queued, 1);
        counts.add(TaskState::Waiting, 1);
        counts.add(TaskState::Failed, 3);
        counts.add(TaskState::Done, 4);
        counts.add(TaskState::Cancelled, 1);
        assert_eq!(
            counts,
            ChildCounts {
                total: 12,
                active: 3,
                waiting: 1,
                failed: 3
            }
        );
    }

    #[test]
    fn a_pane_binding_stored_without_a_shell_pid_still_parses() {
        let stored = r#"{"kind":"pane","session_id":"s","pane_id":"%3","tab_id":null}"#;
        let binding: TaskBinding = serde_json::from_str(stored).unwrap();
        assert_eq!(
            binding,
            TaskBinding::Pane {
                session_id: "s".into(),
                pane_id: "%3".into(),
                tab_id: None,
                shell_pid: None,
                agent_id: None,
            }
        );
        assert_eq!(serde_json::to_string(&binding).unwrap(), stored);
        let pinned = TaskBinding::Pane {
            session_id: "s".into(),
            pane_id: "%3".into(),
            tab_id: None,
            shell_pid: Some(42),
            agent_id: None,
        };
        let json = serde_json::to_string(&pinned).unwrap();
        assert_eq!(serde_json::from_str::<TaskBinding>(&json).unwrap(), pinned);
    }

    #[test]
    fn terminal_states_do_not_move() {
        for s in [TaskState::Done, TaskState::Failed, TaskState::Cancelled] {
            assert!(s.is_terminal());
            for t in [TaskState::Working, TaskState::Done, TaskState::Queued] {
                assert!(!s.can_move_to(t), "{s:?}->{t:?}");
            }
        }
    }

    #[test]
    fn the_spec_paths_are_legal() {
        use TaskState::*;
        for (a, b) in [
            (Queued, Dispatching),
            (Dispatching, Working),
            (Working, Waiting),
            (Waiting, Working),
            (Working, IdleUnconfirmed),
            (IdleUnconfirmed, Done),
            (Working, Failed),
            (Queued, Cancelled),
            // Real outcomes of a paused task, with no `working` between.
            (Waiting, Done),
            (Waiting, Failed),
            (IdleUnconfirmed, Failed),
            (IdleUnconfirmed, Waiting),
        ] {
            assert!(a.can_move_to(b), "{a:?}->{b:?}");
        }
        assert!(!Queued.can_move_to(Working), "queued must dispatch first");
        assert!(!Waiting.can_move_to(Dispatching), "never dispatched twice");
        assert!(!Working.can_move_to(Working), "a move is a change");
        assert!(!Done.can_move_to(Working));
    }

    #[test]
    fn ids_are_prefixed_and_url_safe() {
        let id = TaskId::new().0;
        assert!(
            id.starts_with("t_")
                && id.len() == 24
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }
}
