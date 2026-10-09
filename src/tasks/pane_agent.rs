//! Delegations to a terminal agent (Codex, Claude Code, ...): the goal is the
//! first prompt of an agent started in a new pane, exactly as the spawn route
//! starts one (`launch_agent`), and the task follows that pane agent's state
//! as the terminal backend reports it.
//!
//! An idle agent is not a finished one -- a terminal agent pauses (thinking,
//! a tool, a long build) and comes back -- so only an explicit signal is
//! `done`: the goal is typed with [`completion_request`], and the agent
//! printing a [`DONE_MARKER`] line (or calling `tasks.complete` from the
//! pane's own token) is that signal. Session delegations follow the same
//! rule (`service::turn_end`).
//!
//! | The pane | The task |
//! |---|---|
//! | a `MUQUN_TASK_DONE: <summary>` line below the echoed request, the agent not busy | `done`, with that summary |
//! | agent starting / working | `working` (back from `waiting` or `idle_unconfirmed`) |
//! | agent blocked on a menu (approval, question) | `waiting` |
//! | agent idle on two polls, after it was seen working (or its state sequence moved), or for [`UNCONFIRMED_AFTER`] | `idle_unconfirmed` |
//! | no agent in the pane (two polls running) | `failed`: the agent exited |
//! | the pane is gone | `failed`: the pane closed |
//!
//! The marker is looked for in the pane's recent output (not only its screen)
//! on every poll, so the first poll after a gateway restart settles a task
//! whose agent finished meanwhile. A task that is `idle_unconfirmed` is polled
//! every [`PAUSED_POLL`] instead of every [`POLL`].

use std::time::Duration;

use serde_json::Value;

use super::domain::{one_line, Task, TaskBinding, TaskChange, TaskId, TaskState};
use super::service::SUMMARY_CHARS;
use crate::agents::tasks as agent_kinds;
use crate::backend::{AgentStatus, OutputFormat, OutputSource, PaneId, ReadPane};
use crate::{infer_tmux_agent_status, parts, read_pane_visible_text, terminal_backend};
use crate::{AppState, SessionConfig};

/// How often a followed pane is read.
const POLL: Duration = Duration::from_secs(2);
/// How often it is read while its task is `idle_unconfirmed`: an agent that
/// never prints the marker must not cost a poll every two seconds forever.
const PAUSED_POLL: Duration = Duration::from_secs(12);
/// Lines of recent output searched for the marker.
const MARKER_LINES: u32 = 200;
/// A tab and pane title's length, from the goal.
const TITLE_CHARS: usize = 40;
/// How long an agent may sit idle, never seen working, before its task is
/// `idle_unconfirmed`: a reply quicker than [`POLL`] on a backend without a
/// state sequence (tmux) is never seen busy.
pub(crate) const UNCONFIRMED_AFTER: Duration = Duration::from_secs(30);
/// Polls in a row without an agent in the pane before it counts as exited,
/// and in a row idle before a pause counts, so one listing taken mid-redraw
/// (or a screen read between two tool calls) does not move the task.
const IN_A_ROW: u32 = 2;
pub(crate) const PANE_CLOSED: &str = "the pane closed";
pub(crate) const AGENT_EXITED: &str = "the agent exited before it finished";
/// The line a terminal agent prints when the delegated goal is finished.
pub(crate) const DONE_MARKER: &str = "MUQUN_TASK_DONE:";
const SUMMARY_PLACEHOLDER: &str = "<one-line summary>";
/// Words of the typed request that can only come from its echo; a marker
/// counts only below the last line holding one. Several, since a narrow
/// pane wraps the echo anywhere.
const ECHO_WORDS: [&str; 4] = [
    "exactly in this form",
    "<one-line",
    "one-line summary>",
    "summary>",
];
const SOURCE: &str = "pane_follower";

/// Whether `agent_id` is a terminal agent kind the agent catalog lists.
pub(crate) fn is_terminal_agent(state: &AppState, agent_id: &str) -> bool {
    agent_kinds::is_known_agent_kind(agent_id, &state.config.agent_commands)
}

/// Whether the catalog finds `agent_id`'s executable on `PATH`.
pub(crate) fn is_available(state: &AppState, agent_id: &str) -> bool {
    let command = agent_kinds::agent_command(agent_id, &state.config.agent_commands);
    agent_kinds::find_on_path(&command).is_some()
}

/// The first prompt of every delegation to an agent (a terminal agent's
/// pane or an agent session): the goal, then the request to say when it is
/// finished -- the `tasks.complete` tool, or else the completion marker.
pub(crate) fn completion_request(goal: &str) -> String {
    format!(
        "{goal}\n\nWhen the whole task is finished, call the muqun tool tasks.complete with a \
         one-line summary; if you cannot call it, print one line exactly in this form: \
         {DONE_MARKER} {SUMMARY_PLACEHOLDER}"
    )
}

/// The summary of the last completion marker line in `output`. A marker
/// counts only at the start of a line (after any bullet the agent draws),
/// below the request echoed in the prompt, and with a summary that is not
/// the placeholder or a piece of it.
pub(crate) fn done_marker(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let below_echo = lines
        .iter()
        .rposition(|line| ECHO_WORDS.iter().any(|word| line.contains(word)))
        .map_or(0, |echo| echo + 1);
    lines[below_echo..].iter().rev().find_map(|line| {
        let line = line.trim_start_matches(|c: char| !c.is_alphanumeric());
        let summary = line.strip_prefix(DONE_MARKER)?.trim();
        let placeholder = summary.starts_with('<')
            || SUMMARY_PLACEHOLDER.starts_with(summary)
            || summary.trim_matches(|c| c == '.' || c == '…').is_empty();
        (!placeholder).then(|| one_line(summary, SUMMARY_CHARS))
    })
}

/// A terminal task's tab and pane title: its label, else the start of its
/// goal.
pub(crate) fn pane_title(label: Option<&str>, goal: &str) -> String {
    one_line(label.unwrap_or(goal), TITLE_CHARS)
}

/// The pane agent as one poll found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    Gone,
    NoAgent,
    Agent {
        status: AgentStatus,
        /// The backend's count of the agent's state changes (Herdr only).
        seq: Option<u64>,
    },
}

/// What a follower remembers between polls.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// The agent was seen working (or blocked) since the task started.
    busy: bool,
    /// The first state sequence seen; `Some(None)` once seen without one.
    baseline: Option<Option<u64>>,
    absent: u32,
    idle: u32,
}

/// Where a poll moves the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Stay,
    Working,
    Waiting,
    Unconfirmed,
    /// The agent printed the marker; its summary.
    Done(String),
    Failed(&'static str),
}

/// The mapping in the module docs. `marker` is the summary of a completion
/// marker on screen; `elapsed` is how long the task has been followed.
pub(crate) fn judge(
    task: TaskState,
    seen: Seen,
    marker: Option<&str>,
    progress: &mut Progress,
    elapsed: Duration,
) -> Verdict {
    if task.is_terminal() {
        return Verdict::Stay;
    }
    // A marker counts once the agent has stopped: one mentioned mid-stream
    // is not the end, and a real one is followed by the idle prompt.
    let busy = |status| {
        matches!(
            status,
            AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
        )
    };
    let (status, seq) = match (seen, marker) {
        (Seen::Agent { status, .. }, Some(summary)) if !busy(status) => {
            return Verdict::Done(summary.to_owned())
        }
        (Seen::Gone | Seen::NoAgent, Some(summary)) => return Verdict::Done(summary.to_owned()),
        (Seen::Gone, None) => return Verdict::Failed(PANE_CLOSED),
        (Seen::NoAgent, None) => {
            progress.absent += 1;
            if progress.absent >= IN_A_ROW {
                return Verdict::Failed(AGENT_EXITED);
            }
            return Verdict::Stay;
        }
        (Seen::Agent { status, seq }, _) => (status, seq),
    };
    progress.absent = 0;
    if busy(status) || status == AgentStatus::Unknown {
        progress.idle = 0;
    } else {
        progress.idle += 1;
    }
    let baseline = *progress.baseline.get_or_insert(seq);
    let moved = matches!((seq, baseline), (Some(now), Some(first)) if now > first);
    match status {
        AgentStatus::Starting | AgentStatus::Working => {
            progress.busy = true;
            match task {
                TaskState::Waiting | TaskState::IdleUnconfirmed => Verdict::Working,
                _ => Verdict::Stay,
            }
        }
        AgentStatus::Blocked => {
            progress.busy = true;
            if task == TaskState::Waiting {
                Verdict::Stay
            } else {
                Verdict::Waiting
            }
        }
        AgentStatus::Idle | AgentStatus::Completed => {
            let paused = progress.busy || moved || elapsed >= UNCONFIRMED_AFTER;
            if paused && progress.idle >= IN_A_ROW && task != TaskState::IdleUnconfirmed {
                Verdict::Unconfirmed
            } else {
                Verdict::Stay
            }
        }
        AgentStatus::Unknown => Verdict::Stay,
    }
}

/// A paused pane agent's hint, as one line: its last prose block when
/// `agent` has a transcript dictionary, else the screen's last line with any
/// letter or digit in it.
pub(crate) fn screen_summary(agent: Option<&str>, visible: &str) -> Option<String> {
    let prose = parts::dictionary_for(agent).and_then(|dictionary| {
        parts::normalize_json(visible, Some(dictionary))
            .iter()
            .rev()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("markdown").and_then(Value::as_str))
            .find(|text| !text.trim().is_empty())
            .map(str::to_owned)
    });
    let text = prose.or_else(|| last_line(visible))?;
    Some(one_line(&text, SUMMARY_CHARS))
}

/// A failed pane task's summary: why, and the screen's last line when the
/// screen could be read.
pub(crate) fn failure_line(why: &str, screen: Option<&str>) -> String {
    match screen.and_then(last_line) {
        Some(line) => one_line(&format!("{why}: {line}"), SUMMARY_CHARS),
        None => why.to_owned(),
    }
}

fn last_line(visible: &str) -> Option<String> {
    visible
        .lines()
        .rev()
        .find(|line| line.chars().any(char::is_alphanumeric))
        .map(|line| line.trim().to_owned())
}

struct Bound<'a> {
    session: &'a SessionConfig,
    pane_id: &'a str,
    shell_pid: Option<u32>,
    agent_id: Option<&'a str>,
}

fn bound<'a>(state: &'a AppState, task: &'a Task) -> Option<Bound<'a>> {
    let TaskBinding::Pane {
        session_id,
        pane_id,
        shell_pid,
        agent_id,
        ..
    } = &task.binding
    else {
        return None;
    };
    let session = state.config.sessions.iter().find(|s| &s.id == session_id)?;
    Some(Bound {
        session,
        pane_id,
        shell_pid: *shell_pid,
        agent_id: agent_id.as_deref(),
    })
}

/// What one poll read: the pane agent, its screen, and the marker's summary
/// from its recent output.
struct Observed {
    seen: Seen,
    screen: Option<String>,
    marker: Option<String>,
}

/// One read of the pane, its agent, its screen and its recent output; `None`
/// when the backend cannot say.
async fn observe(at: &Bound<'_>) -> Option<Observed> {
    let (seen, screen) = observe_agent(at).await?;
    let marker = match seen {
        Seen::Gone => None,
        _ => terminal_backend(at.session)
            .read_pane(&ReadPane {
                pane_id: PaneId::new(at.pane_id),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: MARKER_LINES,
                start: None,
                end: None,
            })
            .await
            .ok()
            .and_then(|output| done_marker(&output.text)),
    };
    Some(Observed {
        seen,
        screen,
        marker,
    })
}

async fn observe_agent(at: &Bound<'_>) -> Option<(Seen, Option<String>)> {
    let backend = terminal_backend(at.session);
    let panes = backend.list_panes().await.ok()?;
    let Some(pane) = panes.iter().find(|pane| pane.id.as_str() == at.pane_id) else {
        return Some((Seen::Gone, None));
    };
    // tmux hands `%N` out again after a restart: another shell is another pane.
    if let (Some(expected), Some(current)) = (at.shell_pid, pane.shell_pid) {
        if expected != current {
            return Some((Seen::Gone, None));
        }
    }
    let agents = backend.list_agents().await.ok()?;
    let screen = read_pane_visible_text(at.session, at.pane_id).await.ok();
    let Some(agent) = agents
        .into_iter()
        .find(|a| a.pane_id.as_str() == at.pane_id)
    else {
        return Some((Seen::NoAgent, screen));
    };
    let mut status = agent.status;
    if let (AgentStatus::Unknown, Some(_), Some(text)) = (status, &agent.kind, &screen) {
        status = infer_tmux_agent_status(agent.kind.as_deref(), text);
    }
    let seen = Seen::Agent {
        status,
        seq: agent.state_change_seq,
    };
    Some((seen, screen))
}

/// The change a verdict records.
fn change_for(
    verdict: Verdict,
    agent_id: Option<&str>,
    screen: Option<&str>,
) -> Option<TaskChange> {
    let (to, summary, note) = match verdict {
        Verdict::Stay => return None,
        Verdict::Working => (TaskState::Working, None, None),
        Verdict::Waiting => (TaskState::Waiting, None, None),
        Verdict::Unconfirmed => (
            TaskState::IdleUnconfirmed,
            screen.and_then(|text| screen_summary(agent_id, text)),
            None,
        ),
        Verdict::Done(summary) => (TaskState::Done, Some(summary), None),
        Verdict::Failed(why) => (
            TaskState::Failed,
            Some(failure_line(why, screen)),
            Some(why.to_owned()),
        ),
    };
    Some(TaskChange {
        to,
        source: SOURCE.into(),
        note,
        summary,
        ..Default::default()
    })
}

/// Follow `task`'s pane agent until the task ends, unless something already
/// does. The driver calls this for every open pane delegation, which is how
/// following resumes after a gateway restart.
pub(crate) fn follow(state: &AppState, task: &Task) {
    if !state.tasks.claim_follow(&task.id) {
        return;
    }
    let state = state.clone();
    let id = task.id.clone();
    tokio::spawn(async move {
        run(&state, &id).await;
        state.tasks.release_follow(&id);
    });
}

async fn run(state: &AppState, id: &TaskId) {
    let started = tokio::time::Instant::now();
    let mut progress = Progress::default();
    // The marker last read: a pane closed right after its agent printed it
    // still finished its task.
    let mut last_marker: Option<String> = None;
    // The screen last read: a closed pane's failure still says what was on it.
    let mut last_screen: Option<String> = None;
    loop {
        let task = match state.tasks.task(id).await {
            Ok(Some(task)) if !task.state.is_terminal() => task,
            Ok(_) => return,
            Err(error) => {
                tracing::debug!(task = %id.0, %error, "pane follower: read failed");
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        let Some(at) = bound(state, &task) else {
            return;
        };
        let mut now = task.state;
        if let Some(observed) = observe(&at).await {
            let marker = match observed.seen {
                Seen::Gone => last_marker.take(),
                _ => observed.marker,
            };
            let verdict = judge(
                task.state,
                observed.seen,
                marker.as_deref(),
                &mut progress,
                started.elapsed(),
            );
            last_marker = marker;
            let screen = observed.screen.or_else(|| last_screen.take());
            if let Some(line) = screen
                .as_deref()
                .and_then(|text| screen_summary(at.agent_id, text))
            {
                state.tasks.note_progress(id, &line).await;
            }
            if let Some(change) = change_for(verdict, at.agent_id, screen.as_deref()) {
                match state.tasks.move_to(id, change).await {
                    Ok(moved) => now = moved.state,
                    Err(error) => {
                        tracing::debug!(task = %id.0, %error, "pane follower: move refused")
                    }
                }
            }
            last_screen = screen;
        }
        let pause = if now == TaskState::IdleUnconfirmed {
            PAUSED_POLL
        } else {
            POLL
        };
        tokio::time::sleep(pause).await;
    }
}

/// Send the pane agent its interrupt key (best effort).
pub(crate) async fn interrupt(state: &AppState, task: &Task) {
    let Some(at) = bound(state, task) else {
        return;
    };
    let key = crate::terminal::shortcuts::interrupt_key(at.agent_id, None);
    if let Err(error) = terminal_backend(at.session)
        .send_keys(&PaneId::new(at.pane_id), &[key])
        .await
    {
        tracing::warn!(task = %task.id.0, %error, "could not interrupt the pane agent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pane_agent_s_state_moves_the_task() {
        use AgentStatus::*;
        use TaskState as T;
        let agent = |status, seq| Seen::Agent { status, seq };
        let soon = Duration::from_secs(1);
        let late = UNCONFIRMED_AFTER;
        let done = || Verdict::Done("all green".into());
        // Each row: the polls one follower sees, in order (task state, pane,
        // marker summary on screen, time followed), and the verdict of the last.
        type Poll = (T, Seen, Option<&'static str>, Duration);
        let cases: Vec<(&str, Vec<Poll>, Verdict)> = vec![
            (
                "starting is work",
                vec![(T::Working, agent(Starting, None), None, soon)],
                Verdict::Stay,
            ),
            (
                "the marker is done",
                vec![(T::Working, agent(Idle, None), Some("all green"), soon)],
                done(),
            ),
            (
                "the marker found after a restart is done, whatever the agent does now",
                vec![(T::IdleUnconfirmed, Seen::NoAgent, Some("all green"), soon)],
                done(),
            ),
            (
                "a marker while the agent still works is not the end yet",
                vec![(T::Working, agent(Working, None), Some("all green"), soon)],
                Verdict::Stay,
            ),
            (
                "a marker read before the pane closed is done",
                vec![(T::Working, Seen::Gone, Some("all green"), soon)],
                done(),
            ),
            (
                "busy then idle once is a read between two tool calls",
                vec![
                    (T::Working, agent(Working, None), None, soon),
                    (T::Working, agent(Idle, None), None, soon),
                ],
                Verdict::Stay,
            ),
            (
                "busy then idle twice is only a pause",
                vec![
                    (T::Working, agent(Working, None), None, soon),
                    (T::Working, agent(Idle, None), None, soon),
                    (T::Working, agent(Idle, None), None, soon),
                ],
                Verdict::Unconfirmed,
            ),
            (
                "idle on a moved sequence is a pause though never seen busy",
                vec![
                    (T::Working, agent(Idle, Some(7)), None, soon),
                    (T::Working, agent(Idle, Some(9)), None, soon),
                ],
                Verdict::Unconfirmed,
            ),
            (
                "idle before taking the goal is nothing yet",
                vec![(T::Working, agent(Idle, Some(7)), None, soon)],
                Verdict::Stay,
            ),
            (
                "idle, never busy, for long is a pause",
                vec![
                    (T::Working, agent(Idle, None), None, late),
                    (T::Working, agent(Idle, None), None, late),
                ],
                Verdict::Unconfirmed,
            ),
            (
                "a pause that stays paused is not moved again",
                vec![(T::IdleUnconfirmed, agent(Idle, None), None, late)],
                Verdict::Stay,
            ),
            (
                "a pause that runs again works",
                vec![(T::IdleUnconfirmed, agent(Working, None), None, late)],
                Verdict::Working,
            ),
            (
                "a menu waits",
                vec![(T::Working, agent(Blocked, None), None, soon)],
                Verdict::Waiting,
            ),
            (
                "an answered menu works again",
                vec![
                    (T::Working, agent(Blocked, None), None, soon),
                    (T::Waiting, agent(Working, None), None, soon),
                ],
                Verdict::Working,
            ),
            (
                "unknown says nothing",
                vec![(T::Working, agent(Unknown, None), None, late)],
                Verdict::Stay,
            ),
            (
                "a closed pane fails",
                vec![(T::Working, Seen::Gone, None, soon)],
                Verdict::Failed(PANE_CLOSED),
            ),
            (
                "one poll without the agent is not an exit",
                vec![(T::Working, Seen::NoAgent, None, soon)],
                Verdict::Stay,
            ),
            (
                "two are",
                vec![
                    (T::Working, Seen::NoAgent, None, soon),
                    (T::Working, Seen::NoAgent, None, soon),
                ],
                Verdict::Failed(AGENT_EXITED),
            ),
            (
                "a finished task stays",
                vec![(T::Done, Seen::Gone, None, soon)],
                Verdict::Stay,
            ),
        ];
        for (name, polls, expected) in cases {
            let mut progress = Progress::default();
            let mut verdict = Verdict::Stay;
            for (task, seen, marker, elapsed) in polls {
                verdict = judge(task, seen, marker, &mut progress, elapsed);
            }
            assert_eq!(verdict, expected, "{name}");
        }
    }

    #[test]
    fn the_marker_hint_and_failure_lines_are_read_off_the_screen() {
        // The request echoed in the prompt is not the marker; the agent's line is.
        let goal = completion_request("fix the build");
        let echoed = format!("> {}\n", goal.replace('\n', "\n  "));
        assert_eq!(done_marker(&echoed), None);
        let finished = format!("{echoed}⏺ MUQUN_TASK_DONE: build fixed,   12 tests pass\n\n❯ \n");
        assert_eq!(
            done_marker(&finished).as_deref(),
            Some("build fixed, 12 tests pass")
        );
        // A narrow pane wraps the echo so the marker starts a line, and an
        // agent restating the request writes a placeholder of its own.
        for not_yet in [
            "> When the whole task is finished, print one line exactly in this form:\n  MUQUN_TASK_DONE: <one-line\n  summary>\n",
            "> ... print one line exactly in this\n  form:\n  MUQUN_TASK_DONE: <one-\n  line summary>\n",
            "⏺ I will end with\n  MUQUN_TASK_DONE: <summary>\n",
            "⏺ I will end with\n  MUQUN_TASK_DONE: …\n",
        ] {
            assert_eq!(done_marker(not_yet), None, "{not_yet}");
        }
        assert_eq!(
            pane_title(None, &"fix the build ".repeat(9))
                .chars()
                .count(),
            40
        );
        assert_eq!(pane_title(Some("build"), "fix the build"), "build");

        let claude =
            "⏺ Bash(cargo test)\n  ⎿  ok\n\n⏺ All 12 tests pass.\n\n❯ \n  ? for shortcuts\n";
        assert_eq!(
            screen_summary(Some("claude"), claude).as_deref(),
            Some("All 12 tests pass.")
        );
        let plain = "build ok\n   done:   3 files  \n\n───\n";
        assert_eq!(
            screen_summary(Some("some-agent"), plain).as_deref(),
            Some("done: 3 files")
        );
        assert_eq!(
            failure_line(AGENT_EXITED, Some("panic: no config\n$ \n")),
            "the agent exited before it finished: panic: no config"
        );
        assert_eq!(failure_line(PANE_CLOSED, None), "the pane closed");
    }
}
