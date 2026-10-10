//! Telling a parent how its delegation ended, so it need not poll.
//!
//! `tasks.delegate` records where it was asked from ([`NoticeTarget`]): the
//! agent service's session, or the pane a terminal agent runs in. When the
//! child reaches a reported state ([`REPORTED_STATES`]) and that session is
//! idle -- its turn over -- one plain line goes into it, as a user message
//! would: `mini finished: <summary>`. A parent still in its turn (inside a
//! `tasks.wait`, say) is sent nothing; the wait answers it, and a state a
//! wait returned is not sent again. Each state is told once, recorded on
//! the child in the store, so a restart does not tell it twice.

use std::time::Duration;

use super::domain::{now_ms, NoticeTarget, Task, TaskBinding, TaskState, REPORTED_STATES};
use super::events::TaskDomainEvent;
use crate::agents::domain::{AgentDomainEvent, AgentSessionStatus};
use crate::AppState;

/// How far back an untold state is still worth telling.
const LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);
/// Untold states looked at per pass.
const BATCH: usize = 20;
/// A pass also runs this often: a pane agent going idle sends no event.
const SWEEP: Duration = Duration::from_secs(30);
/// The longest result a line carries.
const MAX_RESULT_CHARS: usize = 4000;

/// Tell parents for the life of the gateway: on every child update to a
/// reported state, every session going idle, and every [`SWEEP`].
pub(crate) fn spawn(state: AppState) {
    let mut tasks = state.task_events.subscribe();
    let mut agents = state.agent_runtime.subscribe_events();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let mut sweep = tokio::time::interval(SWEEP);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = sweep.tick() => {
                    state.tasks.republish_waiting_parents().await;
                }
                event = tasks.recv() => match event {
                    Ok(TaskDomainEvent::Updated { task, .. }) if REPORTED_STATES.contains(&task.state) => {}
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                event = agents.recv() => match event {
                    Ok(AgentDomainEvent::StatusChanged { status: AgentSessionStatus::Idle, .. }) => {}
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
            }
            deliver(&state).await;
        }
    });
}

/// One pass: tell every idle parent its child's untold state. How many were told.
pub(crate) async fn deliver(state: &AppState) -> usize {
    let since = now_ms().saturating_sub(LOOKBACK.as_millis() as u64);
    let pending = match state.tasks.pending_notices(since, BATCH).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::debug!(%error, "notices: pending lookup failed");
            return 0;
        }
    };
    let mut told = 0;
    for (child, target) in pending {
        match ready(state, &target).await {
            Ready::NotYet => continue,
            // The record is the Home row and the parent's own summary.
            Ready::Never(why) => {
                tracing::info!(task = %child.id.0, why, "notices: the parent is not told");
                state.tasks.told(&child.id, child.state).await;
                continue;
            }
            Ready::Now => {}
        }
        let line = message(state, &child).await;
        match send(state, &target, &line).await {
            Ok(()) => {
                state.tasks.told(&child.id, child.state).await;
                told += 1;
            }
            Err(error) => tracing::debug!(task = %child.id.0, %error, "notices: not told yet"),
        }
    }
    told
}

/// Whether a target can be told now.
enum Ready {
    Now,
    /// Its turn is not over yet.
    NotYet,
    /// It never will be: a pane whose agent is gone (typing there would
    /// reach a shell).
    Never(&'static str),
}

/// A session not running a turn; a pane whose own delegation's follower saw
/// its agent stop, with that agent still alive in it.
async fn ready(state: &AppState, target: &NoticeTarget) -> Ready {
    match target {
        NoticeTarget::Session { agent_id, asid } => {
            if state.tasks.is_busy(asid) {
                return Ready::NotYet;
            }
            let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await else {
                return Ready::NotYet;
            };
            let idle = manager
                .sessions()
                .get_session(asid)
                .await
                .is_ok_and(|session| {
                    !matches!(
                        session.status,
                        AgentSessionStatus::Busy | AgentSessionStatus::Retry
                    )
                });
            if idle {
                Ready::Now
            } else {
                Ready::NotYet
            }
        }
        NoticeTarget::Pane {
            session_id,
            pane_id,
        } => {
            let Some((stopped, kind)) = state.tasks.pane_task(session_id, pane_id).await else {
                return Ready::Never("no delegation runs in that pane");
            };
            if !stopped {
                return Ready::NotYet;
            }
            let Some(kind) = kind else {
                return Ready::Never("the pane runs no known agent");
            };
            let Some(session) = terminal_session(state, session_id) else {
                return Ready::Never("the terminal session is gone");
            };
            if super::pane_agent::agent_lives_in(session, pane_id, &kind).await {
                Ready::Now
            } else {
                Ready::Never("the pane's agent has exited")
            }
        }
    }
}

fn terminal_session<'a>(
    state: &'a AppState,
    session_id: &str,
) -> Option<&'a crate::platform::config::SessionConfig> {
    state
        .config
        .sessions
        .iter()
        .find(|session| session.id == session_id)
}

async fn send(state: &AppState, target: &NoticeTarget, line: &str) -> anyhow::Result<()> {
    match target {
        NoticeTarget::Session { agent_id, asid } => {
            let manager = state
                .agent_runtime
                .manager_for_agent(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("{agent_id} is not attached"))?;
            manager
                .prompts()
                .send_prompt(
                    &crate::agents::domain::AgentSessionId(asid.clone()),
                    line,
                    &[],
                    None,
                )
                .await?;
        }
        NoticeTarget::Pane {
            session_id,
            pane_id,
        } => {
            let session = terminal_session(state, session_id)
                .ok_or_else(|| anyhow::anyhow!("no terminal session {session_id}"))?;
            // As the goal is sent: the agent's own prompt call (a bracketed
            // paste on tmux), then Enter once the paste has settled.
            crate::agents::session_routes::submit_agent_prompt(session, pane_id, line)
                .await
                .map_err(|error| anyhow::anyhow!(error.message()))?;
            crate::agents::session_routes::schedule_submit_keypress(
                session.clone(),
                pane_id.clone(),
            );
        }
    }
    Ok(())
}

/// `text` as one line of plain prose: escape sequences (CSI, OSC and other
/// `ESC` forms) and control characters gone, line breaks and tabs turned
/// into single spaces, at most `max` characters.
pub(crate) fn one_plain_line(text: &str, max: usize) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: up to BEL or ST.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' | '\n' | '\t' => plain.push(' '),
            c if c.is_control() => {}
            c => plain.push(c),
        }
    }
    plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// The line a parent is told: who, how it ended, and what it said.
pub(crate) async fn message(state: &AppState, child: &Task) -> String {
    let who = match &child.binding {
        TaskBinding::Peer { peer_name, .. } => one_plain_line(peer_name, 80),
        _ => format!(
            "The task \"{}\"",
            one_plain_line(
                &super::pane_agent::title(
                    child.label.as_deref(),
                    child.session_title.as_deref(),
                    &child.goal
                ),
                120
            )
        ),
    };
    // The line starts with `who`: never a slash command, a shell escape or
    // a vim-style command.
    let who = who.trim_start_matches(['/', '!', ':', ' ']);
    let who = if who.is_empty() { "A sub-task" } else { who };
    // A session delegation's last message in full; else the summary line.
    let answer = match &child.binding {
        TaskBinding::AgentSession { asid, .. }
            if matches!(child.state, TaskState::Done | TaskState::IdleUnconfirmed) =>
        {
            super::service::full_answer(state, asid, child.created_ms).await
        }
        _ => None,
    };
    let result = one_plain_line(
        &answer.or_else(|| child.summary.clone()).unwrap_or_default(),
        MAX_RESULT_CHARS,
    );
    let (how, fallback) = match child.state {
        TaskState::Done => ("finished", "it is done."),
        TaskState::Waiting => ("needs an answer", "it is waiting for you."),
        TaskState::IdleUnconfirmed => (
            "stopped without saying it is finished",
            "it left no message.",
        ),
        TaskState::Cancelled => ("could not finish", "it was cancelled."),
        _ => ("could not finish", "it failed."),
    };
    let result = if result.is_empty() { fallback } else { &result };
    format!("{who} {how}: {result}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::AgentSessionId;
    use crate::tasks::domain::{PrincipalKind, TaskId, TaskKind, TaskOrigin, TaskPrincipal};
    use crate::test_support::{test_device, test_state, FakeAgent};

    fn child(state: TaskState, summary: Option<&str>) -> Task {
        let now = now_ms();
        Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "build it".into(),
            cwd: None,
            binding: TaskBinding::Peer {
                peer_id: "peer-mini".into(),
                peer_server_id: "srv-mini".into(),
                remote_task_id: "t_r".into(),
                peer_name: "mini".into(),
            },
            state,
            summary: summary.map(Into::into),
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Assistant,
                id: "a".into(),
            },
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Agent,
            label: None,
            requester_name: None,
            session_title: None,
            mentions: Vec::new(),
            client_ref: None,
            requester_server_id: None,
        }
    }

    /// A child reaching each reported state, its parent session idle or in
    /// its turn: what the parent is told, once.
    #[tokio::test]
    async fn a_parent_is_told_once_and_only_when_its_turn_is_over() {
        let parent = NoticeTarget::Session {
            agent_id: "opencode".into(),
            asid: "ses_parent".into(),
        };
        for (state, summary, parent_busy, told) in [
            (
                TaskState::Done,
                Some("built"),
                false,
                Some("mini finished: built"),
            ),
            (
                TaskState::Cancelled,
                None,
                false,
                Some("mini could not finish: it was cancelled."),
            ),
            (
                TaskState::Waiting,
                Some("which branch?"),
                false,
                Some("mini needs an answer: which branch?"),
            ),
            // Mid-turn (inside a tasks.wait): nothing is sent; the wait answers it.
            (TaskState::Done, Some("built"), true, None),
        ] {
            let app = test_state("admin", vec![test_device("d1", "token")]);
            let agent = FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1)]);
            let prompts = agent.prompts.clone();
            app.agent_runtime.attach_for_test(agent.manager()).await;
            if parent_busy {
                app.tasks
                    .follow(
                        &app,
                        &AgentDomainEvent::StatusChanged {
                            asid: AgentSessionId("ses_parent".into()),
                            status: AgentSessionStatus::Busy,
                            error: None,
                            seq: 1,
                        },
                    )
                    .await;
            }
            let id = app.tasks.insert_for_test(&child(state, summary)).await;
            app.tasks.notify(&id, &parent).await;
            // A second pass -- or a restart, which reads the same record --
            // tells nothing more.
            deliver(&app).await;
            deliver(&app).await;
            let sent = prompts.lock().unwrap().clone();
            assert_eq!(
                sent,
                told.map(str::to_owned).into_iter().collect::<Vec<_>>(),
                "{state:?}"
            );
            if parent_busy {
                // The wait that answered it marks it told.
                app.tasks.told(&id, state).await;
                assert!(app.tasks.pending_notices(0, 10).await.unwrap().is_empty());
            }
        }
    }

    /// While a child works, its waiting parent's progress says so; once
    /// the child's end is told, the line goes.
    #[tokio::test]
    async fn a_waiting_parent_shows_whom_it_waits_for_until_it_is_told() {
        let app = test_state("admin", vec![test_device("d1", "token")]);
        let agent = FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1)]);
        app.agent_runtime.attach_for_test(agent.manager()).await;
        let parent = app
            .tasks
            .insert_for_test(&Task {
                binding: TaskBinding::AgentSession {
                    agent_id: "opencode".into(),
                    asid: "ses_parent".into(),
                },
                ..child(TaskState::IdleUnconfirmed, None)
            })
            .await;
        let id = app
            .tasks
            .insert_for_test(&Task {
                parent: Some(parent.clone()),
                ..child(TaskState::Working, None)
            })
            .await;
        let progress = |app: AppState, id: TaskId| async move {
            let task = app.tasks.task(&id).await.unwrap().unwrap();
            app.tasks.present(&task).await.progress
        };
        let working = app.tasks.task(&id).await.unwrap().unwrap();
        app.tasks.waits_on(&parent, &working).await;
        assert_eq!(
            progress(app.clone(), parent.clone()).await.as_deref(),
            Some("Waiting for mini · under a minute")
        );
        app.tasks.told(&id, TaskState::Done).await;
        assert_eq!(progress(app.clone(), parent).await, None);
    }

    /// Text the child wrote reaches the parent as one plain line that
    /// starts with who is speaking; a pane whose agent is gone is never
    /// typed into, and that delivery is given up, not retried.
    #[tokio::test]
    async fn what_a_parent_is_told_is_one_plain_line_and_never_reaches_a_shell() {
        let app = test_state("admin", vec![test_device("d1", "token")]);
        let agent = FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1)]);
        let prompts = agent.prompts.clone();
        app.agent_runtime.attach_for_test(agent.manager()).await;
        let hostile = Task {
            binding: TaskBinding::Peer {
                peer_id: "peer-x".into(),
                peer_server_id: "srv-x".into(),
                remote_task_id: "t_r".into(),
                peer_name: "/evil".into(),
            },
            ..child(
                TaskState::Done,
                Some("ok\r\n/exit\u{1b}[2J\u{1b}]0;title\u{7}\ttouch pwned\u{9b}"),
            )
        };
        let id = app.tasks.insert_for_test(&hostile).await;
        let parent = NoticeTarget::Session {
            agent_id: "opencode".into(),
            asid: "ses_parent".into(),
        };
        app.tasks.notify(&id, &parent).await;
        deliver(&app).await;
        assert_eq!(
            prompts.lock().unwrap().clone(),
            ["evil finished: ok /exit touch pwned"]
        );

        // A pane delegation whose agent cannot be found in its pane.
        let pane = app
            .tasks
            .insert_for_test(&Task {
                binding: TaskBinding::Pane {
                    session_id: "default".into(),
                    pane_id: "%9".into(),
                    tab_id: None,
                    shell_pid: None,
                    agent_id: Some("claude".into()),
                },
                ..child(TaskState::IdleUnconfirmed, None)
            })
            .await;
        let id = app
            .tasks
            .insert_for_test(&Task {
                parent: Some(pane),
                ..child(TaskState::Done, Some("built"))
            })
            .await;
        let target = NoticeTarget::Pane {
            session_id: "default".into(),
            pane_id: "%9".into(),
        };
        app.tasks.notify(&id, &target).await;
        assert_eq!(deliver(&app).await, 0);
        assert!(
            app.tasks.pending_notices(0, 10).await.unwrap().is_empty(),
            "given up, not retried"
        );
    }
}
