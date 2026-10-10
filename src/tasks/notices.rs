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
use super::pane_agent::{Front, PaneJob};
use crate::agents::domain::{AgentDomainEvent, AgentSessionStatus};
use crate::AppState;

/// How far back an untold state is still worth telling.
const LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);
/// Untold states looked at per pass, newest first: one a parent cannot take
/// yet never keeps a newer one waiting, and one it never will is resolved
/// at once.
const BATCH: usize = 50;
/// A pass also runs this often: a pane agent going idle sends no event.
const SWEEP: Duration = Duration::from_secs(30);
/// How long a pane whose agent is not in front (suspended with Ctrl-Z, say)
/// is tried again before its result is given up: it may come back with `fg`.
const IN_FRONT_WITHIN: Duration = Duration::from_secs(60 * 60);

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
        let readiness = match parent_ended(state, &child).await {
            Some(why) => Ready::Never(why),
            None => ready(state, &child, &target).await,
        };
        match readiness {
            Ready::NotYet => continue,
            // The record is the Home row and the parent's own summary.
            Ready::Never(why) => {
                tracing::info!(task = %child.id.0, why, "notices: the parent is not told");
                state.tasks.told_task(&child).await;
                continue;
            }
            Ready::Now => {}
        }
        let line = message(state, &child).await;
        match send(state, &target, &line).await {
            Ok(Sent::Delivered) => {
                state.tasks.told_task(&child).await;
                told += 1;
            }
            // Not in front right now: tried again on a later pass, for a while.
            Ok(Sent::Withdrawn(why)) if recent(&child) => {
                tracing::info!(task = %child.id.0, why, "notices: the parent is told later");
            }
            Ok(Sent::Withdrawn(why)) => {
                tracing::info!(task = %child.id.0, why, "notices: the parent is not told");
                state.tasks.told_task(&child).await;
            }
            Err(error) => tracing::debug!(task = %child.id.0, %error, "notices: not told yet"),
        }
    }
    told
}

/// How a send ended.
enum Sent {
    Delivered,
    /// The pane's foreground stopped being its agent before Enter: the
    /// pasted line was cleared, never submitted.
    Withdrawn(&'static str),
}

/// Why a child's parent task will never take its result: it ended (the
/// owner cancelled it, say) -- telling it would start a turn nobody wants.
async fn parent_ended(state: &AppState, child: &Task) -> Option<&'static str> {
    let parent = child.parent.as_ref()?;
    let parent = state.tasks.task(parent).await.ok()??;
    parent
        .state
        .is_terminal()
        .then_some("the parent task has ended")
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
/// Whether `child`'s untold move is recent enough to wait for its pane's
/// agent to come back to the front.
fn recent(child: &Task) -> bool {
    now_ms().saturating_sub(child.updated_ms) < IN_FRONT_WITHIN.as_millis() as u64
}

async fn ready(state: &AppState, child: &Task, target: &NoticeTarget) -> Ready {
    match target {
        NoticeTarget::Session { agent_id, asid } => {
            if state.tasks.is_busy(asid) {
                return Ready::NotYet;
            }
            let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await else {
                return Ready::NotYet;
            };
            match manager.sessions().get_session(asid).await {
                Ok(session)
                    if !matches!(
                        session.status,
                        AgentSessionStatus::Busy | AgentSessionStatus::Retry
                    ) =>
                {
                    Ready::Now
                }
                Err(error) if super::driver::session_gone(&error) => {
                    Ready::Never("the parent session is gone")
                }
                _ => Ready::NotYet,
            }
        }
        NoticeTarget::Pane {
            session_id,
            pane_id,
        } => {
            let Some((pane_state, job)) = state.tasks.pane_task(session_id, pane_id).await else {
                return Ready::Never("no delegation runs in that pane");
            };
            match pane_state {
                TaskState::IdleUnconfirmed | TaskState::Done => {}
                TaskState::Cancelled | TaskState::Failed => {
                    return Ready::Never("the pane's delegation has ended")
                }
                _ => return Ready::NotYet,
            }
            let Some(job) = job else {
                return Ready::Never("no launched agent job is recorded for that pane");
            };
            if terminal_session(state, session_id).is_none() {
                return Ready::Never("the terminal session is gone");
            }
            match super::pane_agent::job_in_front(&job).await {
                Front::Agent => Ready::Now,
                Front::Gone => Ready::Never("the launched agent job is gone"),
                Front::Other if recent(child) => Ready::NotYet,
                Front::Other => {
                    Ready::Never("the launched agent job has not been in front for an hour")
                }
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

async fn send(state: &AppState, target: &NoticeTarget, line: &str) -> anyhow::Result<Sent> {
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
            let job = state
                .tasks
                .pane_task(session_id, pane_id)
                .await
                .and_then(|(_, job)| job)
                .ok_or_else(|| anyhow::anyhow!("no launched agent job in that pane"))?;
            return send_to_pane(session, pane_id, job, &without_file_mentions(line)).await;
        }
    }
    Ok(Sent::Delivered)
}

/// Type `line` into the `kind` agent in `pane_id` ([`type_line`]).
async fn send_to_pane(
    session: &crate::platform::config::SessionConfig,
    pane_id: &str,
    job: PaneJob,
    line: &str,
) -> anyhow::Result<Sent> {
    let typist = PaneTypist {
        session,
        pane: crate::terminal::backend::PaneId::new(pane_id),
        job,
        line,
    };
    type_line(&typist).await
}

/// What [`type_line`] does to a pane, one step at a time.
trait Typist {
    /// Whether the agent holds the terminal now.
    fn in_front(&self) -> BoxFuture<'_, bool>;
    /// The agent's own prompt call: a bracketed paste on tmux.
    fn paste(&self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Whether an Enter must follow the paste (Herdr's `agent.prompt`
    /// submits by itself).
    fn needs_enter(&self) -> BoxFuture<'_, bool>;
    fn enter(&self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Ctrl-U: the pasted line off the input again.
    fn clear(&self) -> BoxFuture<'_, ()>;
    /// Whether the line still sits in the agent's input box.
    fn still_typed(&self) -> BoxFuture<'_, bool>;
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Paste the line once the agent holds the terminal, and look again before
/// each Enter: once something else has it, the pasted line is cleared and
/// Enter is never pressed.
async fn type_line(typist: &impl Typist) -> anyhow::Result<Sent> {
    const NOT_IN_FRONT: &str = "the pane's agent is not in its foreground";
    if !typist.in_front().await {
        return Ok(Sent::Withdrawn(NOT_IN_FRONT));
    }
    typist.paste().await?;
    if !typist.needs_enter().await {
        return Ok(Sent::Delivered);
    }
    for _ in 0..2 {
        if !typist.in_front().await {
            typist.clear().await;
            return Ok(Sent::Withdrawn(NOT_IN_FRONT));
        }
        typist.enter().await?;
        if !typist.still_typed().await {
            break;
        }
    }
    Ok(Sent::Delivered)
}

struct PaneTypist<'a> {
    session: &'a crate::platform::config::SessionConfig,
    pane: crate::terminal::backend::PaneId,
    job: PaneJob,
    line: &'a str,
}

impl Typist for PaneTypist<'_> {
    fn in_front(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { super::pane_agent::job_in_front(&self.job).await == Front::Agent })
    }
    fn paste(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            crate::agents::session_routes::submit_agent_prompt(
                self.session,
                self.pane.as_str(),
                self.line,
            )
            .await
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!(error.message()))
        })
    }
    fn needs_enter(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let needs = crate::terminal_backend(self.session)
                .needs_submit_keypress()
                .await
                .unwrap_or(true);
            // Let the paste settle before the first look and Enter.
            tokio::time::sleep(Duration::from_millis(500)).await;
            needs
        })
    }
    fn enter(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            crate::terminal_backend(self.session)
                .send_keys(&self.pane, &["Enter".to_owned()])
                .await?;
            Ok(())
        })
    }
    fn clear(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = crate::terminal_backend(self.session)
                .send_keys(&self.pane, &["ctrl+u".to_owned()])
                .await;
        })
    }
    fn still_typed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let typed = crate::read_pane_visible_text(self.session, self.pane.as_str())
                .await
                .is_ok_and(|screen| super::pane_agent::goal_pending(&screen, self.line));
            if typed {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            typed
        })
    }
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

/// `line` for an agent's typed input box: any word that starts with `@`
/// could attach a file there (`@Makefile` as much as `@src/main.rs`), so its
/// `@` becomes `＠`. An `@` inside a word (an email) is left alone, and the
/// session API path never gets this: it attaches nothing.
fn without_file_mentions(line: &str) -> String {
    line.split(' ')
        .map(|word| match word.strip_prefix('@') {
            Some(rest) => format!("＠{rest}"),
            _ => word.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
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

    /// A parent that ended (the owner cancelled it) or a session that is gone
    /// is never told -- and the row is resolved, not left to block newer ones.
    #[tokio::test]
    async fn an_ended_parent_or_a_gone_session_is_not_told_and_does_not_wait() {
        let app = test_state("admin", vec![test_device("d1", "token")]);
        let agent = FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1)]);
        let prompts = agent.prompts.clone();
        app.agent_runtime.attach_for_test(agent.manager()).await;
        let cancelled = app
            .tasks
            .insert_for_test(&child(TaskState::Cancelled, None))
            .await;
        for (parent, asid) in [(Some(cancelled), "ses_parent"), (None, "ses_gone")] {
            let id = app
                .tasks
                .insert_for_test(&Task {
                    parent,
                    ..child(TaskState::Done, Some("built"))
                })
                .await;
            let target = NoticeTarget::Session {
                agent_id: "opencode".into(),
                asid: asid.into(),
            };
            app.tasks.notify(&id, &target).await;
        }
        deliver(&app).await;
        assert!(prompts.lock().unwrap().is_empty());
        assert!(
            app.tasks.pending_notices(0, 10).await.unwrap().is_empty(),
            "resolved"
        );
    }

    /// A second question after the first was answered is told too: what was
    /// told is the child's move, not just its state.
    #[tokio::test]
    async fn a_second_question_is_told_again() {
        let app = test_state("admin", vec![test_device("d1", "token")]);
        let agent = FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1)]);
        let prompts = agent.prompts.clone();
        app.agent_runtime.attach_for_test(agent.manager()).await;
        let id = app
            .tasks
            .insert_for_test(&child(TaskState::Waiting, Some("which branch?")))
            .await;
        let parent = NoticeTarget::Session {
            agent_id: "opencode".into(),
            asid: "ses_parent".into(),
        };
        app.tasks.notify(&id, &parent).await;
        deliver(&app).await;
        for (to, summary) in [
            (TaskState::Working, None),
            (TaskState::Waiting, Some("which remote?")),
        ] {
            let change = crate::tasks::domain::TaskChange {
                to,
                source: "test".into(),
                summary: summary.map(Into::into),
                ..Default::default()
            };
            app.tasks.move_to(&id, change).await.unwrap();
        }
        deliver(&app).await;
        assert_eq!(
            prompts.lock().unwrap().clone(),
            [
                "mini needs an answer: which branch?",
                "mini needs an answer: which remote?"
            ]
        );
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
        let done = crate::tasks::domain::TaskChange {
            to: TaskState::Done,
            source: "test".into(),
            ..Default::default()
        };
        app.tasks.move_to(&id, done).await.unwrap();
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

        // A pane delegation with no launched job recorded (an older binding,
        // or a launch whose job was not found).
        let pane = app
            .tasks
            .insert_for_test(&Task {
                binding: TaskBinding::Pane {
                    session_id: "default".into(),
                    pane_id: "%9".into(),
                    tab_id: None,
                    shell_pid: None,
                    agent_id: Some("claude".into()),
                    agent_job: None,
                },
                ..child(TaskState::IdleUnconfirmed, None)
            })
            .await;
        let target = NoticeTarget::Pane {
            session_id: "default".into(),
            pane_id: "%9".into(),
        };
        // Nothing typed, ever: never a guess at what holds the pane, so it is
        // given up at once, recent or not.
        for (age_ms, pending) in [(0, 0), (2 * 60 * 60 * 1000, 0)] {
            let id = app
                .tasks
                .insert_for_test(&Task {
                    parent: Some(pane.clone()),
                    updated_ms: now_ms() - age_ms,
                    ..child(TaskState::Done, Some("built"))
                })
                .await;
            app.tasks.notify(&id, &target).await;
            assert_eq!(deliver(&app).await, 0);
            let left = app.tasks.pending_notices(0, 10).await.unwrap();
            assert_eq!(
                left.iter().filter(|(task, _)| task.id == id).count(),
                pending,
                "{age_ms}"
            );
        }
    }

    /// The agent left the front between the paste and Enter: the pasted line
    /// is cleared and Enter is never pressed.
    #[tokio::test]
    async fn a_line_pasted_before_the_agent_left_is_cleared_not_submitted() {
        use std::sync::Mutex;
        struct Scripted {
            in_front: Mutex<Vec<bool>>,
            steps: Mutex<Vec<&'static str>>,
        }
        impl Typist for Scripted {
            fn in_front(&self) -> BoxFuture<'_, bool> {
                let next = self.in_front.lock().unwrap().remove(0);
                Box::pin(async move { next })
            }
            fn paste(&self) -> BoxFuture<'_, anyhow::Result<()>> {
                self.steps.lock().unwrap().push("paste");
                Box::pin(async { Ok(()) })
            }
            fn needs_enter(&self) -> BoxFuture<'_, bool> {
                Box::pin(async { true })
            }
            fn enter(&self) -> BoxFuture<'_, anyhow::Result<()>> {
                self.steps.lock().unwrap().push("enter");
                Box::pin(async { Ok(()) })
            }
            fn clear(&self) -> BoxFuture<'_, ()> {
                self.steps.lock().unwrap().push("ctrl-u");
                Box::pin(async {})
            }
            fn still_typed(&self) -> BoxFuture<'_, bool> {
                Box::pin(async { false })
            }
        }
        for (in_front, steps, delivered) in [
            (vec![true, true], vec!["paste", "enter"], true),
            (vec![true, false], vec!["paste", "ctrl-u"], false),
            (vec![false], vec![], false),
        ] {
            let typist = Scripted {
                in_front: Mutex::new(in_front),
                steps: Mutex::new(Vec::new()),
            };
            let sent = type_line(&typist).await.unwrap();
            assert_eq!(matches!(sent, Sent::Delivered), delivered);
            assert_eq!(*typist.steps.lock().unwrap(), steps);
        }
    }

    #[test]
    fn a_word_initial_at_is_defused_for_a_typed_pane() {
        assert_eq!(
            without_file_mentions(
                "mini finished: see @src/main.rs and @.env, ask @alice or bob@x.org"
            ),
            "mini finished: see ＠src/main.rs and ＠.env, ask ＠alice or bob@x.org"
        );
    }
}
