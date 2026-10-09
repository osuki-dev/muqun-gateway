//! Which open task an MCP call is made from, so the work it starts is filed
//! under that task (`parent_task_id`) and the phone sees one tree.
//!
//! A token says who is calling, not from which task, so the parent is told
//! from what the gateway can see, and only when exactly one task fits:
//!
//! - **Pane-bound token**: the open task whose pane the caller runs in (an
//!   assistant started by `exec.run`).
//! - **Agent-bound token** (the agent service the gateway runs; every one of
//!   its sessions shares the token): among the open delegations to that
//!   agent, the one whose session is running this very call right now -- its
//!   timeline holds a running call of the same tool with the same arguments
//!   (`command` and `cwd`, or `goal`). An untracked session making another
//!   call of the tool meanwhile is not mistaken for it.
//!
//! None or several fit: no parent. A wrong parent would misfile work on the
//! phone; a missing one only leaves it at the top level.
//!
//! `tasks.complete` finds the caller's own task the same way ([`own_task`]):
//! a session may complete only the task it is working on.

use futures::future::join_all;

use super::tokens::{AssistantSession, TokenBinding};
use crate::agents::domain::{AgentPart, ToolCallStatus};
use crate::tasks::domain::{
    Requested, Task, TaskBinding, TaskFilter, TaskId, TaskKind, TaskOrigin,
};
use crate::tasks::service::OPEN_STATES;
use crate::AppState;

/// Open delegations probed for an agent-bound caller, newest first.
const MAX_CANDIDATES: usize = 20;
/// Timeline entries read per candidate: the calling turn is the last one.
const PROBE_ITEMS: usize = 20;

/// The MCP call that asks for a task: the tool (`exec.run`,
/// `tasks.delegate`) and the arguments that tell this call from another of
/// the same tool, as the caller sent them.
pub(crate) struct Call<'a> {
    pub tool: &'a str,
    pub args: Vec<(&'a str, &'a str)>,
}

/// `session`'s request to start a task through `call`.
pub(crate) async fn requested(
    state: &AppState,
    session: &AssistantSession,
    call: &Call<'_>,
) -> Requested {
    let parent = parent_of(state, &session.binding, call).await;
    let origin = match (&session.binding, &parent) {
        (TokenBinding::Agent { .. }, _) | (_, Some(_)) => TaskOrigin::Agent,
        _ => TaskOrigin::Mcp,
    };
    Requested {
        by: session.principal(),
        origin,
        parent,
        name: None,
    }
}

async fn parent_of(state: &AppState, binding: &TokenBinding, call: &Call<'_>) -> Option<TaskId> {
    match <[Task; 1]>::try_from(fitting(state, binding, call).await) {
        Ok([task]) => Some(task.id),
        Err(_) => None,
    }
}

/// The open delegation `session` works on, for `tasks.complete` (`call`):
/// `wanted` when it is given and fits, else the one task that fits. Fits
/// the way a parent does -- the open task bound to a pane token's pane, or
/// the open delegation of an agent token's agent whose session is running
/// this very call -- so a session can only complete its own task.
pub(crate) async fn own_task(
    state: &AppState,
    session: &AssistantSession,
    call: &Call<'_>,
    wanted: Option<&TaskId>,
) -> Option<TaskId> {
    let fits = fitting(state, &session.binding, call).await;
    let mut fits = fits
        .into_iter()
        .filter(|task| task.kind == TaskKind::Delegation)
        .filter(|task| wanted.is_none_or(|id| task.id == *id));
    match (fits.next(), fits.next()) {
        (Some(task), None) => Some(task.id),
        _ => None,
    }
}

/// The open tasks `binding` could be calling from (see the module docs).
async fn fitting(state: &AppState, binding: &TokenBinding, call: &Call<'_>) -> Vec<Task> {
    let filter = match binding {
        TokenBinding::Pane {
            session_id,
            pane_id,
        } => TaskFilter {
            pane: Some((session_id.clone(), pane_id.clone())),
            ..open(2)
        },
        TokenBinding::Agent { agent_id } => TaskFilter {
            agent_id: Some(agent_id.clone()),
            ..open(MAX_CANDIDATES)
        },
    };
    let candidates = match state.tasks.open_tasks(filter).await {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::debug!(%error, "MCP call: task lookup failed");
            return Vec::new();
        }
    };
    match binding {
        TokenBinding::Pane { .. } => candidates,
        TokenBinding::Agent { agent_id } => calling(state, agent_id, candidates, call).await,
    }
}

fn open(limit: usize) -> TaskFilter {
    TaskFilter {
        states: Some(OPEN_STATES.to_vec()),
        limit,
        ..Default::default()
    }
}

/// The candidates whose session is running `call`.
async fn calling(
    state: &AppState,
    agent_id: &str,
    candidates: Vec<Task>,
    call: &Call<'_>,
) -> Vec<Task> {
    if candidates.is_empty() {
        return candidates;
    }
    let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await else {
        return Vec::new();
    };
    let agent = manager.agent();
    let probes = candidates.iter().map(|task| async {
        let TaskBinding::AgentSession { asid, .. } = &task.binding else {
            return false;
        };
        match agent.get_timeline(asid, PROBE_ITEMS).await {
            Ok(items) => items.iter().any(|item| match &item.part {
                AgentPart::Tool(part) => {
                    part.state == ToolCallStatus::Running
                        && names_tool(&part.name, call.tool)
                        && call.args.iter().all(|(key, value)| {
                            part.input.get(*key).and_then(|v| v.as_str()) == Some(*value)
                        })
                }
                _ => false,
            }),
            Err(_) => false,
        }
    });
    let calling = join_all(probes).await;
    candidates
        .into_iter()
        .zip(calling)
        .filter_map(|(task, calling)| calling.then_some(task))
        .collect()
}

/// Whether a host's name for a tool is `tool`. Hosts prefix MCP tools with
/// the server's name and replace what they do not allow in a name, so
/// OpenCode calls `tasks.delegate` `muqun_tasks_delegate`.
fn names_tool(name: &str, tool: &str) -> bool {
    let plain = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    };
    let (name, tool) = (plain(name), plain(tool));
    name == tool || name.ends_with(&format!("_{tool}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{TimelineItem, TimelineRole};
    use crate::tasks::domain::{now_ms, TaskChange, TaskKind, TaskPrincipal, TaskState};
    use crate::test_support::{fake_session, test_state, FakeAgent};
    use serde_json::json;

    fn open_task(binding: TaskBinding) -> Task {
        let now = now_ms();
        Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "g".into(),
            cwd: None,
            binding,
            state: TaskState::Queued,
            summary: None,
            exit_code: None,
            created_by: TaskPrincipal {
                kind: crate::tasks::domain::PrincipalKind::Device,
                id: "d1".into(),
            },
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Device,
            label: None,
            requester_name: None,
        }
    }

    async fn working(state: &AppState, binding: TaskBinding) -> TaskId {
        let id = state.tasks.insert_for_test(&open_task(binding)).await;
        let to = TaskChange {
            to: TaskState::Working,
            source: "test".into(),
            ..Default::default()
        };
        state.tasks.move_to(&id, to).await.unwrap();
        id
    }

    fn session_of(asid: &str) -> TaskBinding {
        TaskBinding::AgentSession {
            agent_id: "opencode".into(),
            asid: asid.into(),
        }
    }

    fn calling(tool: &str, status: &str, input: serde_json::Value) -> Vec<TimelineItem> {
        let call = serde_json::from_value(json!({
            "id": "call1", "name": tool, "input": input, "state": status, "status": status
        }))
        .unwrap();
        vec![TimelineItem {
            id: "p1".into(),
            message_id: "m1".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Tool(call),
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        }]
    }

    fn agent_token() -> AssistantSession {
        AssistantSession {
            token_id: "tok".into(),
            binding: TokenBinding::Agent {
                agent_id: "opencode".into(),
            },
            label: "opencode".into(),
            expires_ms: None,
        }
    }

    fn pane_token(pane_id: &str) -> AssistantSession {
        AssistantSession {
            token_id: "tok".into(),
            binding: TokenBinding::Pane {
                session_id: "default".into(),
                pane_id: pane_id.into(),
            },
            label: "claude".into(),
            expires_ms: None,
        }
    }

    async fn state_with(timelines: &[(&str, Vec<TimelineItem>)]) -> AppState {
        let state = test_state("admin", Vec::new());
        let mut agent = FakeAgent::new("opencode");
        agent.sessions = timelines
            .iter()
            .map(|(asid, _)| fake_session(asid, 1))
            .collect();
        agent.session_timelines = timelines
            .iter()
            .map(|(asid, items)| (asid.to_string(), items.clone()))
            .collect();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        state
    }

    fn delegate(goal: &str) -> Call<'_> {
        Call {
            tool: "tasks.delegate",
            args: vec![("goal", goal)],
        }
    }

    fn run<'a>(command: &'a str, cwd: &'a str) -> Call<'a> {
        Call {
            tool: "exec.run",
            args: vec![("command", command), ("cwd", cwd)],
        }
    }

    #[tokio::test]
    async fn the_agent_session_running_this_very_call_is_the_parent() {
        let goal = json!({ "goal": "write the tests" });
        let state = state_with(&[
            (
                "ses_parent",
                calling("muqun_tasks_delegate", "running", goal.clone()),
            ),
            (
                "ses_waiting",
                calling("muqun_tasks_wait", "running", json!({})),
            ),
            (
                "ses_done",
                calling("muqun_tasks_delegate", "completed", goal.clone()),
            ),
            // Still streaming its arguments: not yet calling anything.
            (
                "ses_typing",
                calling("muqun_tasks_delegate", "pending", goal),
            ),
        ])
        .await;
        let parent = working(&state, session_of("ses_parent")).await;
        for asid in ["ses_waiting", "ses_done", "ses_typing"] {
            working(&state, session_of(asid)).await;
        }

        let asked = requested(&state, &agent_token(), &delegate("write the tests")).await;
        assert_eq!(asked.parent, Some(parent));
        assert_eq!(asked.origin, TaskOrigin::Agent);
        assert_eq!(asked.by.id, "agent:opencode");
        // Another goal, or another tool, is not what any of them is running.
        let other = requested(&state, &agent_token(), &delegate("something else")).await;
        assert_eq!((other.parent, other.origin), (None, TaskOrigin::Agent));
        let other = requested(&state, &agent_token(), &run("make", "/w")).await;
        assert_eq!(other.parent, None);
    }

    /// A tracked delegation sits in a long `exec.run` while a session the
    /// gateway does not track calls `exec.run` too: the untracked call is not
    /// filed under the tracked task.
    #[tokio::test]
    async fn an_untracked_session_s_call_of_the_same_tool_gets_no_parent() {
        let state = state_with(&[(
            "ses_tracked",
            calling(
                "muqun_exec_run",
                "running",
                json!({ "command": "sleep 40", "cwd": "/w" }),
            ),
        )])
        .await;
        let tracked = working(&state, session_of("ses_tracked")).await;
        let untracked = requested(&state, &agent_token(), &run("cargo test", "/w")).await;
        assert_eq!(untracked.parent, None);
        let same = requested(&state, &agent_token(), &run("sleep 40", "/w")).await;
        assert_eq!(same.parent, Some(tracked), "the positive control");
        let elsewhere = requested(&state, &agent_token(), &run("sleep 40", "/x")).await;
        assert_eq!(elsewhere.parent, None);
    }

    #[tokio::test]
    async fn two_sessions_running_the_same_call_give_no_parent() {
        let input = json!({ "command": "make", "cwd": "/w" });
        let state = state_with(&[
            ("ses_a", calling("muqun_exec_run", "running", input.clone())),
            ("ses_b", calling("muqun_exec_run", "running", input)),
        ])
        .await;
        working(&state, session_of("ses_a")).await;
        working(&state, session_of("ses_b")).await;
        let asked = requested(&state, &agent_token(), &run("make", "/w")).await;
        assert_eq!(asked.parent, None, "ambiguous: filed at the top level");
    }

    #[tokio::test]
    async fn a_pane_token_s_parent_is_the_open_task_in_that_pane() {
        let state = test_state("admin", Vec::new());
        let pane = working(
            &state,
            TaskBinding::Pane {
                session_id: "default".into(),
                pane_id: "%4".into(),
                tab_id: None,
                shell_pid: None,
                agent_id: None,
            },
        )
        .await;
        let inside = requested(&state, &pane_token("%4"), &run("make", "/w")).await;
        assert_eq!(inside.parent, Some(pane));
        assert_eq!(inside.origin, TaskOrigin::Agent);
        let elsewhere = requested(&state, &pane_token("%5"), &run("make", "/w")).await;
        assert_eq!(
            (elsewhere.parent, elsewhere.origin),
            (None, TaskOrigin::Mcp)
        );
    }

    /// `tasks.complete` reaches only the caller's own task: the session
    /// running the call (agent token) or the task in its pane (pane token).
    #[tokio::test]
    async fn only_the_session_working_on_a_task_may_complete_it() {
        let input = json!({ "summary": "all green" });
        let state = state_with(&[
            (
                "ses_mine",
                calling("muqun_tasks_complete", "running", input),
            ),
            ("ses_other", Vec::new()),
        ])
        .await;
        let mine = working(&state, session_of("ses_mine")).await;
        let other = working(&state, session_of("ses_other")).await;
        let complete = Call {
            tool: "tasks.complete",
            args: vec![("summary", "all green")],
        };
        let agent = agent_token();
        assert_eq!(
            own_task(&state, &agent, &complete, None).await,
            Some(mine.clone())
        );
        let with_id = Call {
            tool: "tasks.complete",
            args: vec![("summary", "all green"), ("task_id", &other.0)],
        };
        assert_eq!(
            own_task(&state, &agent, &with_id, Some(&other)).await,
            None,
            "another session's task, named by id"
        );

        let pane = |pane_id: &str| TaskBinding::Pane {
            session_id: "default".into(),
            pane_id: pane_id.into(),
            tab_id: None,
            shell_pid: None,
            agent_id: Some("claude".into()),
        };
        let in_pane = working(&state, pane("%4")).await;
        assert_eq!(
            own_task(&state, &pane_token("%4"), &complete, Some(&in_pane)).await,
            Some(in_pane.clone())
        );
        assert_eq!(
            own_task(&state, &pane_token("%5"), &complete, Some(&in_pane)).await,
            None,
            "another pane"
        );
    }

    #[test]
    fn a_host_s_prefixed_tool_name_still_names_the_tool() {
        for name in [
            "tasks.delegate",
            "muqun_tasks_delegate",
            "mcp__muqun__tasks.delegate",
            "muqun.tasks.delegate",
        ] {
            assert!(names_tool(name, "tasks.delegate"), "{name}");
        }
        assert!(!names_tool("muqun_tasks_wait", "tasks.delegate"));
        assert!(!names_tool("muqun_mytasks_delegate_x", "tasks.delegate"));
        assert!(names_tool("muqun_exec_run", "exec.run"));
    }
}
