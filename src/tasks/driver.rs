//! Moves tasks nobody is waiting on. `tasks.wait` advances a task only while
//! its creator waits; this pass, at startup and every 60 s, settles what
//! finished or vanished meanwhile, through [`TaskService`] so every move is
//! published.
//!
//! - A command whose pane is gone fails ("pane closed"); one whose pane is
//!   there gets one marker read, which completes it (and closes an exit-0
//!   tab) if it has exited.
//! - A delegation whose session the agent no longer has fails ("session
//!   gone"); one whose session failed fails with the error line as its
//!   summary; one whose session went idle after the task was created is
//!   done when the agent's last message carries the completion marker and
//!   `idle_unconfirmed` (the last message's line as a hint) otherwise; a
//!   paused one whose session runs again is working.
//!
//! - A terminal agent's delegation (bound to a pane, with `agent_id`) gets
//!   its pane follower started if none runs (after a restart, say), which
//!   settles it from the pane -- see `pane_agent`.
//!
//! - A task bound to nothing (the gateway stopped between recording it and
//!   dispatching it) fails ("never dispatched") once it is
//!   [`NEVER_DISPATCHED_AFTER`] old.
//!
//! Everything else is left alone -- including tasks whose terminal backend or
//! agent cannot be asked right now, since unreachable is not gone.
//!
//! Between passes, [`spawn_follower`] settles delegations from the agents'
//! own events as they arrive (see [`TaskService::follow`]); the pass catches
//! whatever the follower missed (a lagged channel, a gateway restart).
//!
//! [`TaskService::follow`]: super::service::TaskService::follow

use std::collections::HashMap;
use std::time::Duration;

use super::domain::one_line;
use super::domain::{now_ms, Task, TaskBinding, TaskChange, TaskFilter, TaskKind, TaskState};
use super::pane_agent;
use super::service::{finished_since, summary_timeline, turn_end, OPEN_STATES, SUMMARY_CHARS};
use crate::agents::domain::AgentSessionStatus;
use crate::agents::ports::agent::AgentError;
use crate::{terminal_backend, AppState};

const INTERVAL: Duration = Duration::from_secs(60);
/// How long a task may stay bound to nothing before it is given up on;
/// dispatching takes seconds.
const NEVER_DISPATCHED_AFTER: Duration = Duration::from_secs(10 * 60);
const SOURCE: &str = "task_driver";
/// The store caps a page at 1000.
const PAGE: usize = 1000;

/// Reconcile at once, then every [`INTERVAL`].
pub(crate) fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let moved = reconcile(&state).await;
            if moved > 0 {
                tracing::info!(moved, "settled tasks nobody was waiting on");
            }
        }
    });
}

/// Follow the agents' events for the life of the gateway, settling the
/// delegation each one is about.
pub(crate) fn spawn_follower(state: AppState) {
    let mut events = state.agent_runtime.subscribe_events();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(event) => {
                    state.tasks.follow(&state, &event).await;
                }
                // The next driver pass settles whatever the skipped events moved.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "task follower fell behind");
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}

/// One pass over the open tasks. Returns how many moved.
pub(crate) async fn reconcile(state: &AppState) -> usize {
    let filter = TaskFilter {
        created_by: None,
        states: Some(OPEN_STATES.to_vec()),
        limit: PAGE,
        ..Default::default()
    };
    let tasks = match state.tasks.open_tasks(filter).await {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(%error, "task driver: could not list tasks");
            return 0;
        }
    };
    // Each session's panes, listed once per pass; `None` when unreachable.
    let mut panes: PaneCache = HashMap::new();
    let mut moved = 0;
    for task in tasks {
        let outcome = match &task.binding {
            TaskBinding::Pane {
                session_id,
                pane_id,
                shell_pid,
                ..
            } => match pane_alive(state, &mut panes, session_id, pane_id, *shell_pid).await {
                // A terminal agent's delegation: its follower moves it, and
                // fails it ("the pane closed") when its pane is gone.
                _ if task.kind == TaskKind::Delegation => {
                    pane_agent::follow(state, &task);
                    false
                }
                Some(false) => fail(state, &task, "pane closed").await,
                Some(true) => poll_command(state, &task).await,
                None => false,
            },
            TaskBinding::AgentSession { agent_id, asid } => {
                settle_delegation(state, &task, agent_id, asid).await
            }
            TaskBinding::None => {
                let age = now_ms().saturating_sub(task.created_ms);
                age > NEVER_DISPATCHED_AFTER.as_millis() as u64
                    && fail(state, &task, "never dispatched").await
            }
        };
        moved += usize::from(outcome);
    }
    moved
}

/// Each session's `(pane id, shell pid)` pairs; `None` when unreachable.
type PaneCache = HashMap<String, Option<Vec<(String, Option<u32>)>>>;

/// Whether `pane_id` still exists in `session_id` and, when the task was
/// bound with a shell pid, still runs that shell (tmux reuses `%N` ids after
/// a restart). `None` when that cannot be told (no such session configured,
/// its backend does not answer, or the pane's shell pid is not known now).
async fn pane_alive(
    state: &AppState,
    cache: &mut PaneCache,
    session_id: &str,
    pane_id: &str,
    shell_pid: Option<u32>,
) -> Option<bool> {
    if !cache.contains_key(session_id) {
        let listed = match state.config.sessions.iter().find(|s| s.id == session_id) {
            Some(session) => terminal_backend(session).list_panes().await.ok(),
            None => None,
        };
        let ids = listed.map(|panes| {
            panes
                .into_iter()
                .map(|pane| (pane.id.as_str().to_owned(), pane.shell_pid))
                .collect()
        });
        cache.insert(session_id.to_owned(), ids);
    }
    let panes = cache.get(session_id)?.as_ref()?;
    let Some((_, current)) = panes.iter().find(|(id, _)| id == pane_id) else {
        return Some(false);
    };
    match (shell_pid, current) {
        (None, _) => Some(true),
        (Some(expected), Some(current)) => Some(expected == *current),
        (Some(_), None) => None,
    }
}

/// One marker read of a command's pane. Whether the task moved.
async fn poll_command(state: &AppState, task: &Task) -> bool {
    match state.tasks.poll_once(state, task).await {
        Ok(handle) => handle.task.state != task.state,
        Err(error) => {
            tracing::debug!(task = %task.id.0, %error, "task driver: poll failed");
            false
        }
    }
}

async fn settle_delegation(state: &AppState, task: &Task, agent_id: &str, asid: &str) -> bool {
    // By the agent the task names: right after a start, before that agent is
    // attached, there is nobody to ask and nothing is concluded.
    let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await else {
        return false;
    };
    let session = match manager.sessions().get_session(asid).await {
        Ok(session) => session,
        Err(error) if session_gone(&error) => return fail(state, task, "session gone").await,
        Err(error) => {
            tracing::debug!(task = %task.id.0, %error, "task driver: session read failed");
            return false;
        }
    };
    if session.status == AgentSessionStatus::Failed {
        let why = session
            .error
            .as_ref()
            .map(|e| format!("{}: {}", e.name, e.message));
        let why = one_line(
            why.as_deref().unwrap_or("the agent session failed"),
            SUMMARY_CHARS,
        );
        return fail(state, task, &why).await;
    }
    let running = matches!(
        session.status,
        AgentSessionStatus::Busy | AgentSessionStatus::Retry
    );
    if running && task.state == TaskState::IdleUnconfirmed {
        return moved(state, task, change(TaskState::Working)).await;
    }
    if !finished_since(&session, task.created_ms) {
        return false;
    }
    // Marker first: a turn that ended while the gateway was down is done
    // only if the agent said so.
    let ended = turn_end(&summary_timeline(&manager, asid, task.created_ms).await);
    if ended.to == task.state {
        return false;
    }
    let ended = TaskChange {
        source: SOURCE.into(),
        ..ended
    };
    moved(state, task, ended).await
}

/// A session the agent says it does not have. OpenCode answers a deleted
/// session with a plain HTTP 404 rather than `SessionNotFound`.
fn session_gone(error: &AgentError) -> bool {
    match error {
        AgentError::SessionNotFound(_) => true,
        AgentError::RequestFailed(message) => message.starts_with("HTTP 404"),
        _ => false,
    }
}

async fn fail(state: &AppState, task: &Task, why: &str) -> bool {
    let failed = TaskChange {
        note: Some(why.to_owned()),
        summary: Some(why.to_owned()),
        ..change(TaskState::Failed)
    };
    moved(state, task, failed).await
}

async fn moved(state: &AppState, task: &Task, to: TaskChange) -> bool {
    match state.tasks.move_to(&task.id, to).await {
        Ok(_) => true,
        // Moved meanwhile (a waiter, a cancel): nothing to do.
        Err(error) => {
            tracing::debug!(task = %task.id.0, %error, "task driver: move refused");
            false
        }
    }
}

fn change(to: TaskState) -> TaskChange {
    TaskChange {
        to,
        source: SOURCE.into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::domain::{now_ms, PrincipalKind, TaskId, TaskKind, TaskPrincipal};
    use crate::test_support::{fake_session, test_state, FakeAgent, FakePaneListHerdr};
    use serde_json::json;

    fn task(binding: TaskBinding, kind: TaskKind) -> Task {
        let now = now_ms();
        Task {
            id: TaskId::new(),
            kind,
            goal: "g".into(),
            cwd: None,
            binding,
            state: TaskState::Queued,
            summary: None,
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Assistant,
                id: "a".into(),
            },
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: None,
            origin: Default::default(),
            label: None,
            requester_name: None,
        }
    }

    /// Store `task` and move it to Working.
    async fn working(state: &AppState, task: Task) -> TaskId {
        let id = state.tasks.insert_for_test(&task).await;
        let to = TaskChange {
            to: TaskState::Working,
            source: "test".into(),
            ..Default::default()
        };
        state.tasks.move_to(&id, to).await.unwrap();
        id
    }

    async fn state_of(state: &AppState, id: &TaskId) -> Task {
        state.tasks.get_for_test(id).await
    }

    fn delegation(asid: &str) -> Task {
        let binding = TaskBinding::AgentSession {
            agent_id: "opencode".into(),
            asid: asid.into(),
        };
        task(binding, TaskKind::Delegation)
    }

    #[tokio::test]
    async fn a_command_whose_pane_closed_fails() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1" }
        ]));
        let mut state = test_state("admin", Vec::new());
        state.config.sessions[0].socket_path = herdr.session("default").socket_path;
        let pane = |pane_id: &str| TaskBinding::Pane {
            session_id: "default".into(),
            pane_id: pane_id.into(),
            tab_id: None,
            shell_pid: None,
            agent_id: None,
        };
        let gone = working(&state, task(pane("w1:p7"), TaskKind::Command)).await;
        let unknown_session = TaskBinding::Pane {
            session_id: "elsewhere".into(),
            pane_id: "w1:p7".into(),
            tab_id: None,
            shell_pid: None,
            agent_id: None,
        };
        let unknown = working(&state, task(unknown_session, TaskKind::Command)).await;

        assert_eq!(reconcile(&state).await, 1);
        let failed = state_of(&state, &gone).await;
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.summary.as_deref(), Some("pane closed"));
        let left = state_of(&state, &unknown).await;
        assert_eq!(left.state, TaskState::Working, "cannot tell: untouched");
    }

    #[tokio::test]
    async fn a_command_whose_pane_id_now_runs_another_shell_fails() {
        let herdr = FakePaneListHerdr::start(json!([
            { "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "shell_pid": 200 }
        ]));
        let mut state = test_state("admin", Vec::new());
        state.config.sessions[0].socket_path = herdr.session("default").socket_path;
        let pane = |shell_pid: Option<u32>| TaskBinding::Pane {
            session_id: "default".into(),
            pane_id: "w1:p1".into(),
            tab_id: None,
            shell_pid,
            agent_id: None,
        };
        let reused = working(&state, task(pane(Some(100)), TaskKind::Command)).await;
        let same = working(&state, task(pane(Some(200)), TaskKind::Command)).await;
        let unpinned = working(&state, task(pane(None), TaskKind::Command)).await;

        assert_eq!(reconcile(&state).await, 1);
        let failed = state_of(&state, &reused).await;
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.summary.as_deref(), Some("pane closed"));
        assert_eq!(state_of(&state, &same).await.state, TaskState::Working);
        assert_eq!(state_of(&state, &unpinned).await.state, TaskState::Working);
    }

    #[tokio::test]
    async fn a_delegation_whose_session_is_gone_fails_and_one_idle_with_its_marker_is_done() {
        let state = test_state("admin", Vec::new());
        let mut idle = fake_session("ses_idle", 1);
        idle.time_idle = Some(now_ms() + 60_000);
        let busy = {
            let mut busy = fake_session("ses_busy", 1);
            busy.status = crate::agents::domain::AgentSessionStatus::Busy;
            busy
        };
        let reply = crate::agents::domain::TimelineItem {
            id: "p1".into(),
            message_id: "m1".into(),
            role: crate::agents::domain::TimelineRole::Assistant,
            part: crate::agents::domain::AgentPart::Text {
                text: "all green\n\nMUQUN_TASK_DONE: the build is green".into(),
            },
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        };
        let mut agent = FakeAgent::new("opencode").with_timeline(vec![reply]);
        agent.sessions = vec![idle, busy];
        state.agent_runtime.attach_for_test(agent.manager()).await;

        let gone = working(&state, delegation("ses_deleted")).await;
        let done = working(&state, delegation("ses_idle")).await;
        let running = working(&state, delegation("ses_busy")).await;

        assert_eq!(reconcile(&state).await, 2);
        let failed = state_of(&state, &gone).await;
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.summary.as_deref(), Some("session gone"));
        let finished = state_of(&state, &done).await;
        assert_eq!(finished.state, TaskState::Done);
        // Marker first, after a restart too: the agent's own summary.
        assert_eq!(finished.summary.as_deref(), Some("the build is green"));
        assert_eq!(state_of(&state, &running).await.state, TaskState::Working);
    }

    #[tokio::test]
    async fn a_delegation_whose_session_failed_fails_with_the_error_line() {
        let state = test_state("admin", Vec::new());
        let mut failed = fake_session("ses_failed", 1);
        failed.status = crate::agents::domain::AgentSessionStatus::Failed;
        failed.error = Some(crate::agents::domain::AgentErrorInfo {
            name: "ProviderError".into(),
            message: "quota\nexceeded".into(),
            status: None,
        });
        let mut agent = FakeAgent::new("opencode");
        agent.sessions = vec![failed];
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let id = working(&state, delegation("ses_failed")).await;
        let to = TaskChange {
            to: TaskState::IdleUnconfirmed,
            source: "test".into(),
            ..Default::default()
        };
        state.tasks.move_to(&id, to).await.unwrap();

        assert_eq!(reconcile(&state).await, 1);
        let task = state_of(&state, &id).await;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(
            task.summary.as_deref(),
            Some("ProviderError: quota exceeded")
        );
    }

    #[tokio::test]
    async fn a_delegation_to_an_agent_not_attached_is_left_alone() {
        let state = test_state("admin", Vec::new());
        let id = working(&state, delegation("ses_x")).await;
        assert_eq!(reconcile(&state).await, 0);
        assert_eq!(state_of(&state, &id).await.state, TaskState::Working);
    }

    #[tokio::test]
    async fn a_task_bound_to_nothing_fails_once_it_is_old() {
        let state = test_state("admin", Vec::new());
        let mut stale = task(TaskBinding::None, TaskKind::Delegation);
        stale.created_ms = now_ms() - 11 * 60 * 1000;
        let stale = state.tasks.insert_for_test(&stale).await;
        let fresh = state
            .tasks
            .insert_for_test(&task(TaskBinding::None, TaskKind::Delegation))
            .await;

        assert_eq!(reconcile(&state).await, 1);
        let failed = state_of(&state, &stale).await;
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.summary.as_deref(), Some("never dispatched"));
        assert_eq!(state_of(&state, &fresh).await.state, TaskState::Queued);
        // Queued -> Dispatching -> Failed, every step the driver's.
        let events = state.tasks.events_for_test(&stale).await;
        let sources: Vec<&str> = events[1..].iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, ["task_driver", "task_driver"]);
    }

    #[test]
    fn a_404_from_the_agent_is_a_gone_session() {
        assert!(session_gone(&AgentError::SessionNotFound("s".into())));
        assert!(session_gone(&AgentError::RequestFailed(
            "HTTP 404 Not Found: no such session".into()
        )));
        assert!(!session_gone(&AgentError::RequestFailed(
            "HTTP 500 Internal Server Error: x".into()
        )));
        assert!(!session_gone(&AgentError::Network("down".into())));
    }
}
