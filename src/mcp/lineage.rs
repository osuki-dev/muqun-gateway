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
use crate::agents::domain::{AgentPart, AgentSessionId, TimelineItem, ToolCallStatus};
use crate::agents::manager::AgentManager;
use crate::agents::ports::mirror::SessionMirrorPort;
use crate::tasks::domain::{
    NoticeTarget, Requested, Task, TaskBinding, TaskFilter, TaskId, TaskKind, TaskOrigin,
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
        server_id: None,
    }
}

/// The session making `call`, where its result is told (`tasks::notices`):
/// a pane token's pane; for an agent token, the one session of that agent
/// running this very call among those running a turn now -- task or not,
/// since a person's own session delegates too. `None` when none or several
/// fit: the caller then learns the result from `tasks.wait` only.
pub(crate) async fn caller_session(
    state: &AppState,
    session: &AssistantSession,
    call: &Call<'_>,
) -> Option<NoticeTarget> {
    let agent_id = match &session.binding {
        TokenBinding::Pane {
            session_id,
            pane_id,
        } => {
            return Some(NoticeTarget::Pane {
                session_id: session_id.clone(),
                pane_id: pane_id.clone(),
            })
        }
        TokenBinding::Agent { agent_id } => agent_id,
    };
    let manager = state.agent_runtime.manager_for_agent(agent_id).await?;
    let busy = state.tasks.busy_sessions();
    let probes = busy.iter().map(|asid| async {
        session_items(&manager, asid)
            .await
            .iter()
            .any(|item| runs(item, call))
    });
    let running = join_all(probes).await;
    let mut fits = busy
        .into_iter()
        .zip(running)
        .filter_map(|(asid, running)| running.then_some(asid));
    match (fits.next(), fits.next()) {
        (Some(asid), None) => Some(NoticeTarget::Session {
            agent_id: agent_id.clone(),
            asid,
        }),
        _ => None,
    }
}

/// Whether `item` is `call` running right now.
fn runs(item: &TimelineItem, call: &Call<'_>) -> bool {
    match &item.part {
        AgentPart::Tool(part) => {
            part.state == ToolCallStatus::Running
                && names_tool(&part.name, call.tool)
                && call.args.iter().all(|(key, value)| {
                    part.input.get(*key).and_then(|v| v.as_str()) == Some(*value)
                })
        }
        _ => false,
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
    let probes = candidates.iter().map(|task| async {
        let TaskBinding::AgentSession { asid, .. } = &task.binding else {
            return false;
        };
        session_items(&manager, asid)
            .await
            .iter()
            .any(|item| runs(item, call))
    });
    let calling = join_all(probes).await;
    candidates
        .into_iter()
        .zip(calling)
        .filter_map(|(task, calling)| calling.then_some(task))
        .collect()
}

/// A session's latest timeline items as both sources hold them: the agent's
/// own read, then the gateway's mirror of its stream. DeepSeek and T3 show a
/// call in the stream before their page has it, so a call running right now
/// may be in either.
async fn session_items(manager: &AgentManager, asid: &str) -> Vec<TimelineItem> {
    let mut items = manager
        .agent()
        .get_timeline(asid, PROBE_ITEMS)
        .await
        .unwrap_or_default();
    if let Some(snapshot) = manager
        .mirror()
        .get_snapshot(&AgentSessionId(asid.to_owned()))
        .await
    {
        let streamed = snapshot.timeline;
        items.extend(
            streamed[streamed.len().saturating_sub(PROBE_ITEMS)..]
                .iter()
                .cloned(),
        );
    }
    items
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
            session_title: None,
            mentions: Vec::new(),
            client_ref: None,
            requester_server_id: None,
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

    /// For an agent token, the parent is the one tracked session running
    /// this very call (tool and arguments) right now; anything less certain
    /// is no parent.
    #[tokio::test]
    async fn the_agent_session_running_this_very_call_is_the_parent() {
        let goal = json!({ "goal": "write the tests" });
        let sleep = json!({ "command": "sleep 40", "cwd": "/w" });
        let make = json!({ "command": "make", "cwd": "/w" });
        let several = || {
            vec![
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
                    calling("muqun_tasks_delegate", "pending", goal.clone()),
                ),
            ]
        };
        let one_in_sleep = || {
            vec![(
                "ses_parent",
                calling("muqun_exec_run", "running", sleep.clone()),
            )]
        };
        let rows = [
            (
                "the session running the call",
                several(),
                delegate("write the tests"),
                true,
            ),
            ("another goal", several(), delegate("something else"), false),
            ("another tool", several(), run("make", "/w"), false),
            // An untracked session calling exec.run meanwhile is not the
            // tracked one waiting in its own long exec.run.
            (
                "another command",
                one_in_sleep(),
                run("cargo test", "/w"),
                false,
            ),
            (
                "the same command",
                one_in_sleep(),
                run("sleep 40", "/w"),
                true,
            ),
            (
                "the same command elsewhere",
                one_in_sleep(),
                run("sleep 40", "/x"),
                false,
            ),
            (
                "two sessions running the same call",
                vec![
                    ("ses_a", calling("muqun_exec_run", "running", make.clone())),
                    ("ses_b", calling("muqun_exec_run", "running", make.clone())),
                ],
                run("make", "/w"),
                false,
            ),
        ];
        for (name, sessions, call, has_parent) in rows {
            let state = state_with(&sessions).await;
            let mut parent = None;
            for (asid, _) in &sessions {
                let id = working(&state, session_of(asid)).await;
                if *asid == "ses_parent" {
                    parent = Some(id);
                }
            }
            let asked = requested(&state, &agent_token(), &call).await;
            let expected = if has_parent { parent } else { None };
            assert_eq!(asked.parent, expected, "{name}");
            assert_eq!(asked.origin, TaskOrigin::Agent, "{name}");
            assert_eq!(asked.by.id, "agent:opencode");
            // The same session, task or not, is where its result is told.
            for (asid, _) in &sessions {
                let busy = crate::agents::domain::AgentDomainEvent::StatusChanged {
                    asid: crate::agents::domain::AgentSessionId(asid.to_string()),
                    status: crate::agents::domain::AgentSessionStatus::Busy,
                    error: None,
                    seq: 1,
                };
                state.tasks.follow(&state, &busy).await;
            }
            let told = caller_session(&state, &agent_token(), &call).await;
            let expected = has_parent.then(|| NoticeTarget::Session {
                agent_id: "opencode".into(),
                asid: "ses_parent".into(),
            });
            assert_eq!(told, expected, "{name}");
        }
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
                agent_job: None,
                model: None,
                variant: None,
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
            ("ses_streamed", Vec::new()),
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
        // DeepSeek and T3: the running call is only in the streamed mirror.
        let streamed = working(&state, session_of("ses_streamed")).await;
        let manager = state
            .agent_runtime
            .manager_for_agent("opencode")
            .await
            .unwrap();
        let input = json!({ "summary": "done here", "task_id": streamed.0 });
        manager
            .mirror()
            .upsert_timeline_items(
                &crate::agents::domain::AgentSessionId("ses_streamed".into()),
                calling("muqun_tasks_complete", "running", input),
            )
            .await;
        let from_stream = Call {
            tool: "tasks.complete",
            args: vec![("summary", "done here"), ("task_id", &streamed.0)],
        };
        assert_eq!(
            own_task(&state, &agent, &from_stream, Some(&streamed)).await,
            Some(streamed.clone())
        );

        let pane = |pane_id: &str| TaskBinding::Pane {
            session_id: "default".into(),
            pane_id: pane_id.into(),
            tab_id: None,
            shell_pid: None,
            agent_id: Some("claude".into()),
            agent_job: None,
            model: None,
            variant: None,
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
