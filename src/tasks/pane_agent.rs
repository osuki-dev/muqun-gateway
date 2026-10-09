//! Delegations to a terminal agent (Codex, Claude Code, ...): the goal is the
//! first prompt of an agent started in a new pane, exactly as the spawn route
//! starts one (`launch_agent`), and the task follows that pane agent's state
//! as the terminal backend reports it.
//!
//! An idle agent is not a finished one -- a terminal agent pauses (thinking,
//! a tool, a long build) and comes back -- so only an explicit signal is
//! `done`: the goal is given with [`completion_request`] (on the agent's
//! command line where it takes one, else typed), and the agent
//! printing a [`DONE_MARKER`] line (or calling `tasks.complete` from the
//! pane's own token) is that signal. Session delegations follow the same
//! rule (`service::turn_end`).
//!
//! | The pane | The task |
//! |---|---|
//! | a `MUQUN_TASK_DONE: <summary>` line below the echoed request, the agent not busy | `done`, with that summary |
//! | agent starting / working | `working` (from `dispatching`, or back from `waiting` or `idle_unconfirmed`) |
//! | agent blocked on a menu (approval, question) | `waiting` |
//! | agent idle on two polls, after it was seen working (or its state sequence moved), or for [`UNCONFIRMED_AFTER`] | `idle_unconfirmed` |
//! | no agent in the pane (two polls running; for a `dispatching` task, only after [`START_GRACE`]) | `failed`: the agent exited |
//! | the pane is gone | `failed`: the pane closed |
//!
//! The marker is looked for in the pane's recent output (not only its screen)
//! on every poll, so the first poll after a gateway restart settles a task
//! whose agent finished meanwhile. A task that is `idle_unconfirmed` is polled
//! every [`PAUSED_POLL`] instead of every [`POLL`], backing off to
//! [`MAX_PAUSED_POLL`] while nothing changes ([`paused_poll`]).

use std::time::Duration;

use serde_json::Value;

use super::domain::{one_line, Task, TaskBinding, TaskChange, TaskId, TaskState};
use super::service::SUMMARY_CHARS;
use super::{may_touch, same_pane};
use crate::agents::tasks as agent_kinds;
use crate::backend::{AgentStatus, OutputFormat, OutputSource, PaneId, ReadPane};
use crate::{
    infer_tmux_agent_status, is_working_line, parts, read_pane_visible_text, terminal_backend,
};
use crate::{AppState, SessionConfig};

/// How often a followed pane is read.
const POLL: Duration = Duration::from_secs(2);
/// How often it is read while its task is `idle_unconfirmed`: an agent that
/// never prints the marker must not cost a poll every two seconds forever.
const PAUSED_POLL: Duration = Duration::from_secs(12);
/// The longest wait between reads of a paused task's pane that stays as it
/// is: a forgotten task costs a few backend calls a minute, and a card on
/// the phone is never more than a minute behind its pane.
const MAX_PAUSED_POLL: Duration = Duration::from_secs(60);
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
/// How long a `dispatching` task's pane may show no agent: the agent is
/// still being started (its start timeout) and given its goal.
const START_GRACE: Duration =
    Duration::from_millis(agent_kinds::DEFAULT_AGENT_START_TIMEOUT_MS + 15_000);
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

/// The failure of a goal typed into an agent's input box but never sent.
pub(crate) const NOT_ACCEPTED: &str = "the goal was not accepted by the agent";
/// How long a typed goal may sit in the input box before Enter is pressed
/// once more, and again after that, and how often the pane is read meanwhile.
const ACCEPT_WAIT: Duration = Duration::from_secs(8);
const ACCEPT_POLL: Duration = Duration::from_secs(1);

/// Whether `prompt` still sits unsent in the agent's input box on `screen`:
/// the box is what lies between the last two rules drawn across the pane
/// (Claude Code's), and it holds the prompt's start or a collapsed paste.
/// A screen with no such box says nothing, so it is never pending.
pub(crate) fn goal_pending(screen: &str, prompt: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let rule = |line: &&str| {
        let line = line.trim();
        line.chars().count() >= 10 && line.chars().all(|c| c == '─')
    };
    let rules: Vec<usize> = (0..lines.len()).filter(|&i| rule(&lines[i])).collect();
    let [.., top, bottom] = rules[..] else {
        return false;
    };
    let start: String = prompt.trim().chars().take(24).collect();
    lines[top + 1..bottom]
        .iter()
        .any(|line| line.contains("[Pasted text") || (!start.is_empty() && line.contains(&start)))
}

/// Wait until the goal typed into pane `pane_id` has left the agent's input
/// box, pressing Enter once more if it has not after [`ACCEPT_WAIT`].
/// Whether it was sent; a pane that cannot be read is taken as sent.
pub(crate) async fn goal_accepted(session: &SessionConfig, pane_id: &str, prompt: &str) -> bool {
    for round in 0..2 {
        let deadline = tokio::time::Instant::now() + ACCEPT_WAIT;
        loop {
            tokio::time::sleep(ACCEPT_POLL).await;
            match read_pane_visible_text(session, pane_id).await {
                Ok(screen) if goal_pending(&screen, prompt) => {}
                _ => return true,
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        if round == 0 {
            let keys = ["Enter".to_owned()];
            if let Err(error) = terminal_backend(session)
                .send_keys(&PaneId::new(pane_id), &keys)
                .await
            {
                tracing::debug!(%error, "terminal task: Enter not sent again");
            }
        }
    }
    false
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
        // Not started yet, not exited.
        (Seen::NoAgent, None) if task == TaskState::Dispatching && elapsed < START_GRACE => {
            return Verdict::Stay
        }
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
        // An agent coming up has not taken its goal yet.
        AgentStatus::Starting if task == TaskState::Dispatching => Verdict::Stay,
        AgentStatus::Starting | AgentStatus::Working => {
            progress.busy = true;
            match task {
                TaskState::Dispatching | TaskState::Waiting | TaskState::IdleUnconfirmed => {
                    Verdict::Working
                }
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
        // Idle while its goal is still being typed: not a pause.
        AgentStatus::Idle | AgentStatus::Completed
            if task == TaskState::Dispatching && elapsed < START_GRACE =>
        {
            Verdict::Stay
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

/// A pane agent's hint (paused) or progress (working), as one line: its last
/// prose block when `agent` has a transcript dictionary, else the screen's
/// last meaningful line -- never decoration (see [`meaningful`]).
pub(crate) fn screen_summary(agent: Option<&str>, visible: &str) -> Option<String> {
    let prose = parts::dictionary_for(agent).and_then(|dictionary| {
        parts::normalize_json(visible, Some(dictionary))
            .iter()
            .rev()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("markdown").and_then(Value::as_str))
            .find_map(|text| {
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.trim_start().starts_with(['>', '$']))
                    .filter_map(meaningful)
                    .collect();
                (!lines.is_empty()).then(|| lines.join(" "))
            })
    });
    let text = prose.or_else(|| last_line(visible))?;
    Some(one_line(&text, SUMMARY_CHARS))
}

/// Banners, tips and the footer hints agents draw around their input, not
/// their work. Specific phrases, not words: prose may well mention `ctrl+c`
/// or auto mode.
const CHROME: [&str; 12] = [
    "mise upgrade",
    "? for shortcuts",
    "esc to interrupt",
    "shift+tab to cycle",
    "bypass permissions on",
    "\u{23f5}\u{23f5}",
    "\u{23f8} plan mode",
    "Ask Codex to do anything",
    "% context left",
    "\u{23ce} send",
    ">_ OpenAI Codex",
    "/model to change",
];

/// Banner and tip lines, by how they start once their decoration is cut.
const CHROME_START: [&str; 2] = ["Update available", "Tip:"];

/// `line`'s text when it says something: not box drawing or a rule, not
/// the agent's input prompt (`❯`, `›`), not a spinner, banner or footer
/// hint, and with at least three letters or digits. A box's right edge is
/// cut off.
fn meaningful(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with(['❯', '›']) || CHROME.iter().any(|c| trimmed.contains(c)) {
        return None;
    }
    let text = trimmed
        .trim_start_matches(|c: char| !c.is_alphanumeric() && !"([{\"'`/~.>$".contains(c))
        .trim_end_matches(|c: char| c == '│' || c.is_whitespace());
    if CHROME_START.iter().any(|start| text.starts_with(start)) {
        return None;
    }
    let words = text.chars().filter(|c| c.is_alphanumeric()).count();
    let drawing = text
        .chars()
        .filter(|c| ('\u{2500}'..='\u{259f}').contains(c))
        .count();
    (words >= 3 && drawing <= words).then_some(text)
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
        .find_map(meaningful)
        .map(str::to_owned)
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
async fn observe(at: &Bound<'_>, rows: &mut Option<usize>) -> Option<Observed> {
    let (seen, screen) = observe_agent(at, rows).await?;
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

/// Claude Code's tool and message rows (`⏺`) on `screen`.
fn claude_rows(screen: &str) -> usize {
    screen
        .lines()
        .filter(|line| line.trim_start().starts_with('⏺'))
        .count()
}

/// The pane's agent and screen. `rows` carries Claude Code's `⏺` row count
/// from the poll before: a Claude Code agent reported working counts as
/// working only with its `esc to interrupt` footer on screen or a row added
/// since then -- text typed into its input box is not work.
async fn observe_agent(at: &Bound<'_>, rows: &mut Option<usize>) -> Option<(Seen, Option<String>)> {
    let backend = terminal_backend(at.session);
    let panes = backend.list_panes().await.ok()?;
    // A pane whose shell pid cannot be told now is still followed.
    if same_pane(&panes, at.pane_id, at.shell_pid) == Some(false) {
        return Some((Seen::Gone, None));
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
    let claude = parts::dictionary_for(agent.kind.as_deref().or(at.agent_id))
        .is_some_and(|dictionary| dictionary.id == "claude");
    if let (true, Some(text)) = (claude, &screen) {
        let now = claude_rows(text);
        let fresh = rows.is_some_and(|before| now > before);
        *rows = Some(now);
        if status == AgentStatus::Working && !fresh && !text.lines().any(is_working_line) {
            status = AgentStatus::Idle;
        }
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
    let mut rows: Option<usize> = None;
    // The task's seq and the pane as the last poll left them, and how many
    // polls in a row have found a paused task just so.
    let mut last: Option<(u64, Option<Seen>)> = None;
    let mut quiet = 0;
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
        let (mut now, mut seq, mut seen) = (task.state, task.seq, None);
        if let Some(observed) = observe(&at, &mut rows).await {
            seen = Some(observed.seen);
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
                state.tasks.note_progress(state, id, &line).await;
            }
            if let Some(change) = change_for(verdict, at.agent_id, screen.as_deref()) {
                match state.tasks.move_to(id, change).await {
                    Ok(moved) => (now, seq) = (moved.state, moved.seq),
                    Err(error) => {
                        tracing::debug!(task = %id.0, %error, "pane follower: move refused")
                    }
                }
            }
            last_screen = screen;
        }
        let unchanged = last == Some((seq, seen));
        last = Some((seq, seen));
        let (next, pause) = paused_poll(quiet, now, unchanged);
        quiet = next;
        tokio::time::sleep(pause).await;
    }
}

/// The quiet count and the wait before the next read, given `quiet` polls
/// in a row that found a paused (`idle_unconfirmed`) task unchanged before
/// this one, the task's state `now`, and whether this poll found it
/// `unchanged`. A paused task waits [`PAUSED_POLL`], doubling with each
/// unchanged poll up to [`MAX_PAUSED_POLL`]; any change -- the task moved,
/// the agent's state or state sequence -- starts again from the shortest.
/// Any other state is read every [`POLL`].
fn paused_poll(quiet: u32, now: TaskState, unchanged: bool) -> (u32, Duration) {
    if now != TaskState::IdleUnconfirmed {
        return (0, POLL);
    }
    let quiet = if unchanged { quiet + 1 } else { 0 };
    let pause = PAUSED_POLL
        .saturating_mul(1 << quiet.min(8))
        .min(MAX_PAUSED_POLL);
    (quiet, pause)
}

/// Send the pane agent its interrupt key (best effort), only while the pane
/// is still the task's ([`may_touch`]).
pub(crate) async fn interrupt(state: &AppState, task: &Task) {
    let Some(at) = bound(state, task) else {
        return;
    };
    let backend = terminal_backend(at.session);
    let pane_id = PaneId::new(at.pane_id);
    if !may_touch(backend.as_ref(), &pane_id, at.shell_pid).await {
        tracing::warn!(task = %task.id.0, "not interrupting: the pane is no longer the task's");
        return;
    }
    let key = crate::terminal::shortcuts::interrupt_key(at.agent_id, None);
    if let Err(error) = backend.send_keys(&pane_id, &[key]).await {
        tracing::warn!(task = %task.id.0, %error, "could not interrupt the pane agent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paused_task_is_read_less_often_while_nothing_changes() {
        use TaskState::{IdleUnconfirmed as Paused, Working};
        let s = Duration::from_secs;
        // Successive polls of one follower: (task state, unchanged since the
        // last poll) and the wait before the next read.
        let mut quiet = 0;
        for (step, (now, unchanged, wait)) in [
            (Paused, false, s(12)),
            (Paused, true, s(24)),
            (Paused, true, s(48)),
            (Paused, true, s(60)),
            (Paused, true, s(60)),
            // A change starts the curve again.
            (Paused, false, s(12)),
            (Paused, true, s(24)),
            // Not paused: the ordinary poll, and the count is gone.
            (Working, true, POLL),
            (Paused, true, s(24)),
        ]
        .into_iter()
        .enumerate()
        {
            let (next, pause) = paused_poll(quiet, now, unchanged);
            assert_eq!(pause, wait, "poll {step}");
            quiet = next;
        }
    }

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
                "an agent still being started has not exited",
                vec![
                    (T::Dispatching, Seen::NoAgent, None, soon),
                    (T::Dispatching, Seen::NoAgent, None, soon),
                ],
                Verdict::Stay,
            ),
            (
                "an agent coming up and idle until its goal is typed is not paused",
                vec![
                    (T::Dispatching, agent(Starting, None), None, soon),
                    (T::Dispatching, agent(Idle, None), None, soon),
                    (T::Dispatching, agent(Idle, None), None, soon),
                ],
                Verdict::Stay,
            ),
            (
                "an agent at work moves its dispatching task to working",
                vec![(T::Dispatching, agent(Working, None), None, soon)],
                Verdict::Working,
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

        // tmux status off the screen: Codex keeps its input prompt drawn
        // below a live working line.
        let prompt = "\n\u{203a} Ask Codex to do anything\n\n  gpt-6 high · ~/w\n";
        for (screen, status) in [
            (
                format!("• Working (25s • esc to interrupt){prompt}"),
                AgentStatus::Working,
            ),
            (
                format!("• Waiting for background terminal (cargo test){prompt}"),
                AgentStatus::Working,
            ),
            (format!("• Fixed the build.{prompt}"), AgentStatus::Idle),
        ] {
            assert_eq!(
                infer_tmux_agent_status(Some("codex"), &screen),
                status,
                "{screen}"
            );
        }
        // Claude Code: a goal typed but not sent is not work; its footer is.
        let rule = "─".repeat(40);
        let unsent = format!(
            "⏺ Ready.\n{rule}\n❯ [Pasted text #1 +3 lines]\n{rule}\n  ⏵⏵ bypass permissions on\n"
        );
        let working = format!("✶ Thinking… (3s · esc to interrupt)\n{rule}\n❯ \n{rule}\n");
        assert_eq!(
            infer_tmux_agent_status(Some("claude"), &unsent),
            AgentStatus::Idle
        );
        assert_eq!(
            infer_tmux_agent_status(Some("claude"), &working),
            AgentStatus::Working
        );
        assert!(goal_pending(&unsent, "fix the build"));
        assert!(!goal_pending(&working, "fix the build"), "an empty box");
        assert!(
            !goal_pending("$ fix the build\n", "fix the build"),
            "no box: cannot tell"
        );
        // The hint and progress skip rules, the prompt, banners, tips and
        // footer hints, but not prose that names a key or a mode.
        for (screen, hint) in [
            (unsent.as_str(), Some("Ready.")),
            (
                "│ Update available! 0.160.0 -> 0.161.0 │\n│ Run mise upgrade codex │\n",
                None,
            ),
            ("  ⎿  Tip: Use /memory to edit memory\n", None),
            (
                "│ >_ OpenAI Codex (v0.161.0) │\n│ model: gpt-6 high  /model to change │\n",
                None,
            ),
            ("  ? for shortcuts    100% context left\n", None),
            ("  ⏎ send   ⌃J newline   ⌃C quit\n", None),
            ("  ⏸ plan mode on (shift+tab to cycle)\n", None),
            (
                "│ the fix is in parse.rs  │\n",
                Some("the fix is in parse.rs"),
            ),
            (
                "• Press ctrl+c to stop the server; auto mode and accept edits stay on.\n",
                Some("Press ctrl+c to stop the server; auto mode and accept edits stay on."),
            ),
            ("Pro Tip: run it twice\n", Some("Pro Tip: run it twice")),
            ("$ cargo test\n", Some("$ cargo test")),
            ("> make check\n› \n", Some("> make check")),
        ] {
            assert_eq!(
                screen_summary(Some("some-agent"), screen).as_deref(),
                hint,
                "{screen}"
            );
        }
    }
}
