//! Delegations to a terminal agent (Codex, Claude Code, ...): the goal is the
//! first prompt of an agent started in a new pane, exactly as the spawn route
//! starts one (`launch_agent`), and the task follows that pane agent's state
//! as the terminal backend reports it.
//!
//! An idle agent is not a finished one -- a terminal agent pauses (thinking,
//! a tool, a long build) and comes back -- so only an explicit signal is
//! `done`: the goal is given with [`completion_instruction`] (out of band
//! where the agent takes it -- Claude Code's appended system prompt, with
//! one typed retry if it exits at once that way -- else after the goal,
//! [`completion_request`];
//! the goal on the agent's command line where it takes one, else typed),
//! and the agent
//! printing a [`DONE_MARKER`] line (or calling `tasks.complete` from the
//! pane's own token) is that signal. An agent that stops because it needs
//! the user says so the same way: a [`BLOCKED_MARKER`] line, or
//! `tasks.complete` with outcome `blocked`. Session delegations follow the
//! same rules (`service::turn_end`, `TaskService::settle`).
//!
//! | The pane | The task |
//! |---|---|
//! | a `MUQUN_TASK_DONE: <summary>` below the echoed request (at a line's start or after the agent's words on it), the agent not busy | `done`, with that summary |
//! | a new `MUQUN_TASK_BLOCKED: <reason>` line below the echoed request, the agent not busy | `waiting` (*blocked*), with that reason (replacing an earlier one) |
//! | *blocked*: agent idle, or still busy before it was seen idle on two polls | stays `waiting` |
//! | *blocked*: agent starting / working after it was seen idle on two polls (the user answered) | `working` |
//! | agent starting / working | `working` (from `dispatching`, or back from `waiting` or `idle_unconfirmed`) |
//! | agent blocked on a menu (approval, question) | `waiting`, with the screen's last meaningful line ([`screen_summary`]) |
//! | agent idle on two polls, after it was seen working (or its state sequence moved), or for [`UNCONFIRMED_AFTER`] | `idle_unconfirmed` |
//! | no agent in the pane (two polls running; for a `dispatching` task, only after [`START_GRACE`]) | `failed`: the agent exited |
//! | the pane is gone | `failed`: the pane closed |
//!
//! The marker is looked for in the pane's recent output (not only its screen)
//! on every poll, so the first poll after a gateway restart settles a task
//! whose agent finished meanwhile. A task is *blocked* while it is `waiting`
//! because its agent said so -- the marker, or a `tasks.complete` call with
//! outcome `blocked` (read off the task's last event when someone else moved
//! it) -- and stops being blocked once it moves on. A blocked line already
//! acted on does not block again: an answered task that pauses with the
//! same old line still on screen is `idle_unconfirmed`. "That line" is an
//! occurrence, not a text: the same reason printed once more blocks again
//! ([`Marker::Blocked`] counts them), and a restarted follower takes the
//! reasons in the task's `marker_blocked` events as read ([`Progress::seed`]). A task that is
//! `idle_unconfirmed` or blocked is polled every [`PAUSED_POLL`] instead of
//! every [`POLL`], backing off to [`MAX_PAUSED_POLL`] while nothing changes
//! ([`paused_poll`]).

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
/// A task title's length, from the goal.
const TITLE_CHARS: usize = 60;
/// The shortest a title cut back to a word boundary may be: a goal whose
/// first word is a long path is cut hard instead (the app clips the same).
const MIN_TITLE_CUT: usize = 30;
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
/// The line it prints when it stopped because it needs the user.
pub(crate) const BLOCKED_MARKER: &str = "MUQUN_TASK_BLOCKED:";
pub(crate) const SUMMARY_PLACEHOLDER: &str = "<one-line summary>";
pub(crate) const REASON_PLACEHOLDER: &str = "<one-line reason>";
/// Words of the typed request that can only come from its echo; a marker
/// counts only below the last line holding one. Several, since a narrow
/// pane wraps the echo anywhere.
const ECHO_WORDS: [&str; 6] = [
    "exactly in this form",
    "<one-line",
    "one-line summary>",
    "summary>",
    "one-line reason>",
    "reason>",
];
const SOURCE: &str = "pane_follower";
/// The source of a move to `waiting` on a blocked marker (pane or session).
pub(crate) const MARKER_BLOCKED: &str = "marker_blocked";

/// Whether `agent_id` is a terminal agent kind the agent catalog lists.
pub(crate) fn is_terminal_agent(state: &AppState, agent_id: &str) -> bool {
    agent_kinds::is_known_agent_kind(agent_id, &state.config.agent_commands)
}

/// Whether the catalog finds `agent_id`'s executable on `PATH`.
pub(crate) fn is_available(state: &AppState, agent_id: &str) -> bool {
    let command = agent_kinds::agent_command(agent_id, &state.config.agent_commands);
    agent_kinds::find_on_path(&command).is_some()
}

/// What every delegation to an agent (a terminal agent's pane or an agent
/// session) asks besides its goal: to say how it ended -- the
/// `tasks.complete` tool, or else a marker line. It is plumbing, never shown
/// to the reader: it goes out of band where the agent takes that (a
/// synthetic OpenCode message; Claude Code's appended system prompt, unless
/// its configured launch arguments append one of their own) and is cut from
/// the served transcript otherwise ([`super::transcript`]). A Claude Code
/// seen to start with `--append-system-prompt-file` and gone again within
/// [`SETTLE`] -- its pane back at a shell with no Claude Code process left
/// under it ([`exited_to_shell`]) -- is started once more with the
/// instruction typed after its goal, rather than failing the task; a start
/// that times out is never retried. Codex and other terminal agents get it
/// typed: a terminal shows what was typed. It opens with
/// [`REQUEST_OPENING`].
pub(crate) fn completion_instruction() -> String {
    format!(
        "{REQUEST_OPENING}call the muqun tool tasks.complete with a \
         one-line summary, or with outcome \"blocked\" and the reason if you stopped because \
         you need the user (a missing file, an unclear request, a permission you cannot get); \
         if you cannot call it, print one line exactly in this form: \
         {DONE_MARKER} {SUMMARY_PLACEHOLDER} -- or, blocked, {BLOCKED_MARKER} {REASON_PLACEHOLDER}"
    )
}

/// The opening of every completion instruction this gateway has sent.
pub(crate) const REQUEST_OPENING: &str = "When the whole task is finished, ";

/// The goal followed by [`completion_instruction`], for an agent that can
/// only be given one text.
pub(crate) fn completion_request(goal: &str) -> String {
    format!("{goal}\n\n{}", completion_instruction())
}

/// [`completion_instruction`] in a file for an agent that reads it from one
/// (Claude Code's `--append-system-prompt-file`), so the launch line typed
/// into the pane carries a path rather than the instruction. `None` when it
/// cannot be written.
pub(crate) async fn completion_instruction_file() -> Option<std::path::PathBuf> {
    let path = crate::state_dir().ok()?.join("task-instruction.md");
    let text = completion_instruction();
    if tokio::fs::read_to_string(&path).await.ok().as_deref() == Some(text.as_str()) {
        return Some(path);
    }
    // Written beside it and renamed over it, so a Claude Code started by a
    // concurrent delegation never reads half a file.
    let partial = path.with_extension(format!("md.{}", uuid::Uuid::new_v4().simple()));
    let written = async {
        tokio::fs::write(&partial, &text).await?;
        tokio::fs::rename(&partial, &path).await
    };
    if let Err(error) = written.await {
        tracing::debug!(%error, "task instruction file not written");
        let _ = tokio::fs::remove_file(&partial).await;
        return None;
    }
    Some(path)
}

/// How long an agent started with its instruction out of band is watched
/// for an exit: one that refuses the flag exits at once. The watch ends as
/// soon as the agent's prompt is up.
const SETTLE: Duration = Duration::from_secs(3);
const SETTLE_POLL: Duration = Duration::from_millis(200);
/// The shells a pane goes back to when its agent exits.
const SHELLS: [&str; 13] = [
    "sh", "bash", "zsh", "fish", "ksh", "mksh", "dash", "ash", "tcsh", "csh", "nu", "xonsh",
    "elvish",
];

/// Whether the `kind` agent just started in `pane_id` exited again within
/// [`SETTLE`]: the pane no longer runs it, its foreground is a shell, and no
/// `kind` process is left anywhere under the pane's shell (a launcher script
/// can put a shell in the foreground of a running agent). Watched until
/// the agent's prompt is up (Herdr reports its state; on tmux, its input
/// box is drawn), at most [`SETTLE`]. A pane or process table that cannot be
/// read says no: a launch that cannot be shown to have failed is never
/// started twice.
pub(crate) async fn exited_to_shell(session: &SessionConfig, pane_id: &str, kind: &str) -> bool {
    let backend = terminal_backend(session);
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        tokio::time::sleep(SETTLE_POLL).await;
        let Ok(pane) = backend.get_pane(&PaneId::new(pane_id)).await else {
            return false;
        };
        let foreground = pane.foreground_command.as_deref();
        if at_shell(pane.agent.as_deref(), foreground, kind) {
            return match pane.shell_pid {
                Some(shell) => process_table()
                    .await
                    .is_some_and(|table| !runs_under(&table, shell, kind)),
                None => false,
            };
        }
        let ready = pane.agent.as_deref() == Some(kind)
            && (matches!(
                pane.agent_status,
                AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked
            ) || read_pane_visible_text(session, pane_id)
                .await
                .is_ok_and(|screen| input_box(&screen).is_some()));
        if ready || tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

fn at_shell(agent: Option<&str>, foreground: Option<&str>, kind: &str) -> bool {
    let shell = foreground.is_some_and(|command| SHELLS.contains(&command_name(command)));
    agent != Some(kind) && shell
}

/// A command's name: its path's last part, without a login shell's `-`.
fn command_name(command: &str) -> &str {
    let name = command.rsplit('/').next().unwrap_or(command);
    name.trim_start_matches('-')
}

/// `ps -A -o pid=,ppid=,comm=`: every process's pid, parent and command.
async fn process_table() -> Option<String> {
    let out = tokio::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a process named `name` is `root` or below it in `table` (the
/// output of [`process_table`]).
fn runs_under(table: &str, root: u32, name: &str) -> bool {
    let rows: Vec<(u32, u32, &str)> = table
        .lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (ppid, command) = rest.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, ppid.parse().ok()?, command.trim()))
        })
        .collect();
    let mut tree = vec![root];
    let mut at = 0;
    while at < tree.len() {
        let parent = tree[at];
        tree.extend(
            rows.iter()
                .filter(|(pid, ppid, _)| *ppid == parent && !tree.contains(pid))
                .map(|(pid, _, _)| *pid)
                .collect::<Vec<_>>(),
        );
        at += 1;
    }
    rows.iter()
        .any(|(pid, _, command)| tree.contains(pid) && command_name(command) == name)
}

/// How an agent said its task ended, with its one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    Done(String),
    /// `count`: how many lines in the output read as this very marker, so
    /// the same reason printed again (after the user answered) is new.
    Blocked {
        reason: String,
        count: usize,
    },
}

/// The last marker in `output`, done or blocked, read as [`marker_line`]
/// reads one, below the request echoed in the prompt. Code fences are not
/// looked at: a pane's window of output can start inside one.
pub(crate) fn marker(output: &str) -> Option<Marker> {
    let found = marker_lines(output);
    match &found.last()?.2 {
        Marker::Blocked { reason, .. } => {
            let same = |(_, _, m): &&(usize, usize, Marker)| matches!(m, Marker::Blocked { reason: r, .. } if r == reason);
            Some(Marker::Blocked {
                reason: reason.clone(),
                count: found.iter().filter(same).count(),
            })
        }
        done => Some(done.clone()),
    }
}

/// Where in `output` [`marker`] reads the task's end from: the index of
/// the line in `output.lines()` and the byte where the marker starts in it
/// (the marker runs to the line's end). The one span the served transcript
/// leaves out ([`super::transcript`]), so a marker the ledger ignores stays
/// in view.
pub(crate) fn consumed_marker(output: &str) -> Option<(usize, usize)> {
    marker_lines(output)
        .last()
        .map(|(line, start, _)| (*line, *start))
}

/// Every marker that counts in `output`: its line, where it starts on it,
/// and what it says.
fn marker_lines(output: &str) -> Vec<(usize, usize, Marker)> {
    let lines: Vec<&str> = output.lines().collect();
    let below_echo = lines
        .iter()
        .rposition(|line| ECHO_WORDS.iter().any(|word| line.contains(word)))
        .map_or(0, |echo| echo + 1);
    lines
        .iter()
        .enumerate()
        .skip(below_echo)
        .filter_map(|(at, line)| {
            let (start, marker) = marker_line(line)?;
            Some((at, start, marker))
        })
        .collect()
}

/// Whether `line` opens or closes a markdown code fence (for display:
/// detection does not look at fences).
pub(crate) fn is_fence(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("```") || line.starts_with("~~~")
}

/// The first marker on `line` that counts, and the byte it starts at: one
/// at the line's start (after any bullet or markup the agent draws) or
/// right after the agent's own words on it ("Done.MUQUN_TASK_DONE: …"), but
/// not inside a word, nor quoted mid-sentence (after a backtick or quote),
/// and with a line that is not a placeholder or a piece of one. The marker
/// runs to the line's end; what follows it is the summary or reason.
fn marker_line(line: &str) -> Option<(usize, Marker)> {
    let mut found: Vec<(usize, bool)> = [(DONE_MARKER, true), (BLOCKED_MARKER, false)]
        .iter()
        .flat_map(|(token, done)| line.match_indices(token).map(move |(at, _)| (at, *done)))
        .filter(|(at, _)| {
            let before = &line[..*at];
            let at_start = !before.chars().any(char::is_alphanumeric);
            let previous = before.chars().next_back();
            let in_word = previous.is_some_and(|c| c.is_alphanumeric() || c == '_');
            let quoted = previous.is_some_and(|c| matches!(c, '`' | '"' | '\'' | '“' | '‘'));
            at_start || !(in_word || quoted)
        })
        .collect();
    found.sort_unstable();
    found.into_iter().find_map(|(at, done)| {
        let (token, placeholder) = if done {
            (DONE_MARKER, SUMMARY_PLACEHOLDER)
        } else {
            (BLOCKED_MARKER, REASON_PLACEHOLDER)
        };
        let text = line[at + token.len()..].trim();
        let stand_in = text.starts_with('<')
            || placeholder.starts_with(text)
            || text.trim_matches(|c| c == '.' || c == '…').is_empty();
        if stand_in {
            return None;
        }
        let text = one_line(text, SUMMARY_CHARS);
        Some((
            at,
            if done {
                Marker::Done(text)
            } else {
                Marker::Blocked {
                    reason: text,
                    count: 1,
                }
            },
        ))
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
    let Some(inside) = input_box(screen) else {
        return false;
    };
    let start: String = prompt.trim().chars().take(24).collect();
    inside
        .iter()
        .any(|line| line.contains("[Pasted text") || (!start.is_empty() && line.contains(&start)))
}

/// The lines of the agent's input box on `screen`: what lies between the
/// last two rules drawn across the pane (Claude Code's), if there are two.
fn input_box(screen: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = screen.lines().collect();
    let rule = |line: &&str| {
        let line = line.trim();
        line.chars().count() >= 10 && line.chars().all(|c| c == '─')
    };
    let rules: Vec<usize> = (0..lines.len()).filter(|&i| rule(&lines[i])).collect();
    let [.., top, bottom] = rules[..] else {
        return None;
    };
    Some(lines[top + 1..bottom].to_vec())
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

/// A task's title (and a terminal task's tab and pane title): its label,
/// else the first sentence of the name its agent session gave itself, else
/// its goal's first sentence ([`first_sentence`]) -- cut to
/// [`TITLE_CHARS`] with `…`, back to a word when that keeps
/// [`MIN_TITLE_CUT`] characters.
pub(crate) fn title(label: Option<&str>, session_title: Option<&str>, goal: &str) -> String {
    let text = match (label, session_title) {
        (Some(label), _) => label,
        (None, Some(name)) => first_sentence(name),
        (None, None) => first_sentence(goal),
    };
    let text = one_line(text, usize::MAX);
    let line = one_line(&text, TITLE_CHARS);
    let Some(cut) = line.strip_suffix('…') else {
        return line;
    };
    // Cut mid-word unless a space came next: back to the last whole word,
    // when that keeps at least MIN_TITLE_CUT characters; else a hard cut.
    let whole = text[cut.len()..].starts_with(' ');
    match cut.rsplit_once(' ') {
        Some((words, _)) if !whole && words.chars().count() >= MIN_TITLE_CUT => {
            format!("{words}…")
        }
        _ => line,
    }
}

/// `text` up to the first `.` or `?`/`!` followed by a space or the end
/// (`.` itself left out; a `.` inside a word, as in `main.rs`, does not end
/// it, but one running a sentence into the next, as in `done.Next`, does),
/// a `。`/`？`/`！`, or a line end. A text that starts with a stop has
/// its first line instead, so a title is never empty.
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let sentence = sentence_of(text);
    if sentence
        .trim_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation() || c == '…')
        .is_empty()
    {
        return text.lines().next().unwrap_or(text);
    }
    sentence
}

fn sentence_of(text: &str) -> &str {
    let mut chars = text.char_indices().peekable();
    let mut before = ' ';
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|(_, next)| *next);
        let spaced = next.is_none_or(char::is_whitespace);
        // `exists.File`: a sentence run into the next one.
        let run_on = before.is_lowercase() && next.is_some_and(char::is_uppercase);
        before = c;
        let cut = match c {
            '\n' | '。' | '.' if c != '.' || spaced || run_on => Some(at),
            '？' | '！' => Some(at + c.len_utf8()),
            '?' | '!' if spaced => Some(at + c.len_utf8()),
            _ => None,
        };
        if let Some(cut) = cut {
            return &text[..cut];
        }
    }
    text
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
    /// Whether the task is blocked (see the module docs).
    held: Held,
    /// The blocked marker last acted on: its reason, and how many lines
    /// read as it then ([`Marker::Blocked`]).
    blocked_by: Option<(String, usize)>,
}

/// A blocked task's agent, as the follower has seen it since the block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Held {
    #[default]
    No,
    /// Blocked; the agent may still be finishing the turn that said so.
    Reported,
    /// Blocked, and the agent was seen idle since: work now is an answer.
    Idle,
}

impl Progress {
    /// A new follower's view of the blocked marker on screen: one whose
    /// reason the task's history (`acted`, the reasons of its earlier
    /// `marker_blocked` moves) already holds was acted on before a restart.
    pub(crate) fn seed(&mut self, marker: Option<&Marker>, acted: &[String]) {
        if let Some(Marker::Blocked { reason, count }) = marker {
            if acted.contains(reason) {
                self.blocked_by = Some((reason.clone(), *count));
            }
        }
    }

    /// The blocked line a failure may name: one not acted on yet (printed
    /// just before the pane went), or the one the task is still blocked by
    /// -- never an old one the user has answered since.
    fn failure_reason<'a>(&self, marker: Option<&'a Marker>) -> Option<&'a Marker> {
        let Some(Marker::Blocked { reason, count }) = marker else {
            return None;
        };
        let acted = self
            .blocked_by
            .as_ref()
            .is_some_and(|(acted, seen)| acted == reason && seen >= count);
        (!acted || self.held != Held::No).then_some(marker?)
    }

    /// Whether the task is blocked, as its stored history says: kept when
    /// it is, and the follower's own view is kept while it still is.
    pub(crate) fn blocked(&mut self, blocked: bool) {
        self.held = match (blocked, self.held) {
            (false, _) => Held::No,
            (true, Held::No) => Held::Reported,
            (true, held) => held,
        };
    }
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
    /// The agent printed the blocked marker; its reason.
    Blocked(String),
    Failed(&'static str),
}

/// The mapping in the module docs. `marker` is the last marker on screen;
/// `elapsed` is how long the task has been followed.
pub(crate) fn judge(
    task: TaskState,
    seen: Seen,
    marker: Option<&Marker>,
    progress: &mut Progress,
    elapsed: Duration,
) -> Verdict {
    if task.is_terminal() {
        return Verdict::Stay;
    }
    if task != TaskState::Waiting {
        progress.held = Held::No;
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
        (Seen::Agent { status, .. }, Some(Marker::Done(summary))) if !busy(status) => {
            return Verdict::Done(summary.clone())
        }
        (Seen::Agent { status, .. }, Some(Marker::Blocked { reason, count }))
            if !busy(status)
                && progress
                    .blocked_by
                    .as_ref()
                    .is_none_or(|(acted, seen)| acted != reason || count > seen) =>
        {
            progress.blocked_by = Some((reason.clone(), *count));
            progress.held = Held::Idle;
            // While waiting too: a new reason replaces the old one.
            return Verdict::Blocked(reason.clone());
        }
        (Seen::Gone | Seen::NoAgent, Some(Marker::Done(summary))) => {
            return Verdict::Done(summary.clone())
        }
        (Seen::Gone, _) => return Verdict::Failed(PANE_CLOSED),
        // Not started yet, not exited.
        (Seen::NoAgent, _) if task == TaskState::Dispatching && elapsed < START_GRACE => {
            return Verdict::Stay
        }
        (Seen::NoAgent, _) => {
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
    if progress.held != Held::No {
        // Blocked: only work after the agent was seen stopped is an answer.
        return match status {
            AgentStatus::Starting | AgentStatus::Working if progress.held == Held::Idle => {
                progress.held = Held::No;
                progress.busy = true;
                Verdict::Working
            }
            AgentStatus::Idle | AgentStatus::Completed if progress.idle >= IN_A_ROW => {
                progress.held = Held::Idle;
                Verdict::Stay
            }
            _ => Verdict::Stay,
        };
    }
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
    marker: Option<Marker>,
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
            .and_then(|output| marker(&output.text)),
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

/// The change a verdict records. `marker` is the blocked line a failure
/// names ([`Progress::failure_reason`]): a pane that fails right after one
/// still says why the agent stopped.
fn change_for(
    verdict: Verdict,
    agent_id: Option<&str>,
    screen: Option<&str>,
    marker: Option<&Marker>,
) -> Option<TaskChange> {
    let mut source = SOURCE;
    let (to, summary, note) = match verdict {
        Verdict::Stay => return None,
        Verdict::Working => (TaskState::Working, None, None),
        // The reason is also the event's note: a restarted follower reads
        // which blocked lines were acted on from it ([`Progress::seed`]).
        Verdict::Blocked(reason) => {
            source = MARKER_BLOCKED;
            (TaskState::Waiting, Some(reason.clone()), Some(reason))
        }
        Verdict::Waiting => (
            TaskState::Waiting,
            screen.and_then(|text| screen_summary(agent_id, text)),
            None,
        ),
        Verdict::Unconfirmed => (
            TaskState::IdleUnconfirmed,
            screen.and_then(|text| screen_summary(agent_id, text)),
            None,
        ),
        Verdict::Done(summary) => (TaskState::Done, Some(summary), None),
        Verdict::Failed(why) => {
            let line = match marker {
                Some(Marker::Blocked { reason, .. }) => {
                    one_line(&format!("{why}: {reason}"), SUMMARY_CHARS)
                }
                _ => failure_line(why, screen),
            };
            (TaskState::Failed, Some(line), Some(why.to_owned()))
        }
    };
    Some(TaskChange {
        to,
        source: source.into(),
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
    let mut last_marker: Option<Marker> = None;
    // The screen last read: a closed pane's failure still says what was on it.
    let mut last_screen: Option<String> = None;
    let mut rows: Option<usize> = None;
    // The task's seq and the pane as the last poll left them, and how many
    // polls in a row have found a paused task just so.
    let mut last: Option<(u64, Option<Seen>)> = None;
    let mut quiet = 0;
    // The task's seq as the follower last saw or left it: any other means
    // someone else moved it, and whether it is blocked is read again.
    let mut known_seq = None;
    // The blocked reasons acted on before this follower, read once.
    let mut acted: Option<Vec<String>> = None;
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
        if known_seq != Some(task.seq) {
            progress.blocked(state.tasks.blocked_at(&task.id).await.is_some());
        }
        let (mut now, mut seq, mut seen) = (task.state, task.seq, None);
        if let Some(observed) = observe(&at, &mut rows).await {
            seen = Some(observed.seen);
            let marker = match observed.seen {
                Seen::Gone => last_marker.take(),
                _ => observed.marker,
            };
            if acted.is_none() {
                let reasons = state.tasks.marker_reasons(id).await;
                progress.seed(marker.as_ref(), &reasons);
                acted = Some(reasons);
            }
            let verdict = judge(
                task.state,
                observed.seen,
                marker.as_ref(),
                &mut progress,
                started.elapsed(),
            );
            let screen = observed.screen.or_else(|| last_screen.take());
            if let Some(line) = screen
                .as_deref()
                .and_then(|text| screen_summary(at.agent_id, text))
            {
                state.tasks.note_progress(state, id, &line).await;
            }
            let blocked = progress.failure_reason(marker.as_ref());
            let change = change_for(verdict, at.agent_id, screen.as_deref(), blocked);
            last_marker = marker;
            if let Some(change) = change {
                match state.tasks.move_to(id, change).await {
                    Ok(moved) => (now, seq) = (moved.state, moved.seq),
                    Err(error) => {
                        tracing::debug!(task = %id.0, %error, "pane follower: move refused")
                    }
                }
            }
            last_screen = screen;
        }
        known_seq = Some(seq);
        let unchanged = last == Some((seq, seen));
        last = Some((seq, seen));
        let paused = now == TaskState::IdleUnconfirmed || progress.held != Held::No;
        let (next, pause) = paused_poll(quiet, paused, unchanged);
        quiet = next;
        tokio::time::sleep(pause).await;
    }
}

/// The quiet count and the wait before the next read, given `quiet` polls
/// in a row that found a paused (`idle_unconfirmed` or blocked) task
/// unchanged before this one, whether the task is `paused` now, and whether
/// this poll found it `unchanged`. A paused task waits [`PAUSED_POLL`],
/// doubling with each unchanged poll up to [`MAX_PAUSED_POLL`]; any change
/// -- the task moved, the agent's state or state sequence -- starts again
/// from the shortest. Any other task is read every [`POLL`].
fn paused_poll(quiet: u32, paused: bool, unchanged: bool) -> (u32, Duration) {
    if !paused {
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
        let (paused, working) = (true, false);
        let s = Duration::from_secs;
        // Successive polls of one follower: (paused -- idle_unconfirmed or
        // blocked, unchanged since the last poll) and the wait before the
        // next read.
        let mut quiet = 0;
        for (step, (now, unchanged, wait)) in [
            (paused, false, s(12)),
            (paused, true, s(24)),
            (paused, true, s(48)),
            (paused, true, s(60)),
            (paused, true, s(60)),
            // A change starts the curve again.
            (paused, false, s(12)),
            (paused, true, s(24)),
            // Not paused: the ordinary poll, and the count is gone.
            (working, true, POLL),
            (paused, true, s(24)),
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
        let green = Some(Marker::Done("all green".into()));
        let blocked = |count| {
            Some(Marker::Blocked {
                reason: "no such file".into(),
                count,
            })
        };
        let stuck = blocked(1);
        // Each row: the polls one follower sees, in order (task state, pane,
        // last marker on screen, time followed), and the verdict of the last.
        type Poll = (T, Seen, Option<Marker>, Duration);
        let cases: Vec<(&str, Vec<Poll>, Verdict)> = vec![
            (
                "starting is work",
                vec![(T::Working, agent(Starting, None), None, soon)],
                Verdict::Stay,
            ),
            (
                "the marker is done",
                vec![(T::Working, agent(Idle, None), green.clone(), soon)],
                done(),
            ),
            (
                "the marker found after a restart is done, whatever the agent does now",
                vec![(T::IdleUnconfirmed, Seen::NoAgent, green.clone(), soon)],
                done(),
            ),
            (
                "a marker while the agent still works is not the end yet",
                vec![(T::Working, agent(Working, None), green.clone(), soon)],
                Verdict::Stay,
            ),
            (
                "a marker read before the pane closed is done",
                vec![(T::Working, Seen::Gone, green.clone(), soon)],
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
                "the blocked marker waits with its reason",
                vec![(T::Working, agent(Idle, None), stuck.clone(), soon)],
                Verdict::Blocked("no such file".into()),
            ),
            (
                "a blocked task stays waiting while its agent sits idle",
                vec![
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (T::Waiting, agent(Idle, None), stuck.clone(), late),
                    (T::Waiting, agent(Idle, None), stuck.clone(), late),
                ],
                Verdict::Stay,
            ),
            (
                "an answered block works, and its old line does not block again",
                vec![
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (T::Waiting, agent(Working, None), stuck.clone(), soon),
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                ],
                Verdict::Unconfirmed,
            ),
            (
                "a new reason while blocked replaces the old one",
                vec![
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (
                        T::Waiting,
                        agent(Idle, None),
                        Some(Marker::Blocked {
                            reason: "the config is missing too".into(),
                            count: 1,
                        }),
                        soon,
                    ),
                ],
                Verdict::Blocked("the config is missing too".into()),
            ),
            (
                "a blocked task is done on the done marker",
                vec![
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (T::Waiting, agent(Idle, None), green.clone(), soon),
                ],
                done(),
            ),
            (
                "the same reason printed again after an answer blocks again",
                vec![
                    (T::Working, agent(Idle, None), stuck.clone(), soon),
                    (T::Waiting, agent(Working, None), stuck.clone(), soon),
                    (T::Working, agent(Idle, None), blocked(2), soon),
                ],
                Verdict::Blocked("no such file".into()),
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
                verdict = judge(task, seen, marker.as_ref(), &mut progress, elapsed);
            }
            assert_eq!(verdict, expected, "{name}");
        }
        // Blocked by tasks.complete (the task's history says so): the rest
        // of the agent's turn is not an answer; work after it stopped is.
        let mut progress = Progress::default();
        progress.blocked(true);
        let polls = [
            (agent(Working, None), Verdict::Stay),
            (agent(Idle, None), Verdict::Stay),
            (agent(Idle, None), Verdict::Stay),
            (agent(Working, None), Verdict::Working),
        ];
        for (step, (seen, expected)) in polls.into_iter().enumerate() {
            let verdict = judge(T::Waiting, seen, None, &mut progress, late);
            assert_eq!(verdict, expected, "blocked by the tool, poll {step}");
        }
        // After a restart: a blocked line the history already acted on, on a
        // task answered since, does not block it again.
        let mut progress = Progress::default();
        progress.seed(stuck.as_ref(), &["no such file".to_owned()]);
        let verdict = judge(
            T::Working,
            agent(Idle, None),
            stuck.as_ref(),
            &mut progress,
            soon,
        );
        assert_eq!(verdict, Verdict::Stay, "seeded from history");
    }

    #[test]
    fn the_marker_hint_and_failure_lines_are_read_off_the_screen() {
        // The request echoed in the prompt is not the marker; the agent's line is.
        let goal = completion_request("fix the build");
        let echoed = format!("> {}\n", goal.replace('\n', "\n  "));
        assert_eq!(marker(&echoed), None);
        let finished = format!("{echoed}⏺ MUQUN_TASK_DONE: build fixed,   12 tests pass\n\n❯ \n");
        assert_eq!(
            marker(&finished),
            Some(Marker::Done("build fixed, 12 tests pass".into()))
        );
        let blocked = format!("{echoed}⏺ MUQUN_TASK_BLOCKED: notes.md does not exist\n");
        assert_eq!(
            marker(&blocked),
            Some(Marker::Blocked {
                reason: "notes.md does not exist".into(),
                count: 1
            })
        );
        // A marker after the agent's own words on the same line counts; one
        // inside a word does not.
        assert_eq!(
            marker("Hello. How can I help?MUQUN_TASK_DONE: Greeted the user"),
            Some(Marker::Done("Greeted the user".into()))
        );
        for mention in [
            "XMUQUN_TASK_DONE: no",
            "I print `MUQUN_TASK_DONE: x` at the end",
            "the line \"MUQUN_TASK_BLOCKED: why\" is for later",
        ] {
            assert_eq!(marker(mention), None, "{mention}");
        }
        assert_eq!(
            marker("`MUQUN_TASK_DONE: fixed`"),
            Some(Marker::Done("fixed`".into()))
        );
        // A window of output that starts inside a code fence still finds
        // the agent's marker after the fence closes.
        let mid_fence = "  let x = 1;\n```\n\u{23fa} MUQUN_TASK_DONE: built\n";
        assert_eq!(marker(mid_fence), Some(Marker::Done("built".into())));
        // A narrow pane wraps the echo so the marker starts a line, and an
        // agent restating the request writes a placeholder of its own.
        for not_yet in [
            "> When the whole task is finished, print one line exactly in this form:\n  MUQUN_TASK_DONE: <one-line\n  summary>\n",
            "> ... print one line exactly in this\n  form:\n  MUQUN_TASK_DONE: <one-\n  line summary>\n",
            "⏺ I will end with\n  MUQUN_TASK_DONE: <summary>\n",
            "⏺ I will end with\n  MUQUN_TASK_DONE: …\n",
            "> ... -- or, blocked, MUQUN_TASK_BLOCKED:\n  <one-line reason>\n",
            "⏺ Otherwise\n  MUQUN_TASK_BLOCKED: <one-\n",
        ] {
            assert_eq!(marker(not_yet), None, "{not_yet}");
        }

        // An agent started with its instruction out of band that refused it
        // is gone and its pane back at a shell; a slow or running one is not.
        for (agent, foreground, back) in [
            (None, Some("-zsh"), true),
            (None, Some("/usr/bin/bash"), true),
            (Some("claude"), Some("claude"), false),
            (None, Some("node"), false),
            (None, None, false),
        ] {
            assert_eq!(
                at_shell(agent, foreground, "claude"),
                back,
                "{foreground:?}"
            );
        }
        // ... and only once no such process is left under the pane's shell.
        let table = "  1     0 init\n 40     1 -zsh\n 41    40 /usr/local/bin/claude-shim\n \
                     42    41 sh\n 43    42 claude\n 50     1 claude\n";
        for (root, gone) in [
            (40, false),
            (42, false),
            (41, false),
            (50, false),
            (7, true),
        ] {
            assert_eq!(!runs_under(table, root, "claude"), gone, "{root}");
        }

        // A title: the label, the session's name, else the goal's first
        // sentence cut at a word.
        let long = "fix the build ".repeat(9);
        for (label, session, goal, expected) in [
            (Some("build"), Some("Fix CI"), "fix the build", "build"),
            (None, Some("Fix CI"), "fix the build", "Fix CI"),
            (
                None,
                Some("Creating the requested file and verifying it exists.File creation…"),
                "make a file",
                "Creating the requested file and verifying it exists",
            ),
            (None, None, ". fix it", ". fix it"),
            (
                None,
                None,
                "In /very/long/path/to/some/deeply/nested/project/directory/of/the/app run the tests",
                "In /very/long/path/to/some/deeply/nested/project/directory/…",
            ),
            (None, None, "... fix it", "... fix it"),
            (
                None,
                None,
                "Fix src/main.rs. Then run the tests",
                "Fix src/main.rs",
            ),
            (
                None,
                None,
                "Why does it fail? Look at the log",
                "Why does it fail?",
            ),
            (None, None, "整理笔记。然后提交", "整理笔记"),
            (None, None, "first line\nsecond line", "first line"),
            (
                None,
                None,
                long.as_str(),
                "fix the build fix the build fix the build fix the build fix…",
            ),
        ] {
            assert_eq!(title(label, session, goal), expected, "{goal}");
        }

        let claude =
            "⏺ Bash(cargo test)\n  ⎿  ok\n\n⏺ All 12 tests pass.\n\n❯ \n  ? for shortcuts\n";
        assert_eq!(
            screen_summary(Some("claude"), claude).as_deref(),
            Some("All 12 tests pass.")
        );
        // A task waiting on a menu says what the screen asks.
        let menu = "⏺ Bash(cargo clean)\n\n Do you want to proceed?\n";
        let waiting = change_for(Verdict::Waiting, Some("codex"), Some(menu), None).unwrap();
        assert_eq!(waiting.summary.as_deref(), Some("Do you want to proceed?"));
        // A pane that closes right after a blocked line still says why.
        let reason = Marker::Blocked {
            reason: "notes.md does not exist".into(),
            count: 1,
        };
        let mut progress = Progress::default();
        let closed = change_for(
            Verdict::Failed(PANE_CLOSED),
            None,
            Some(menu),
            progress.failure_reason(Some(&reason)),
        );
        assert_eq!(
            closed.unwrap().summary.as_deref(),
            Some("the pane closed: notes.md does not exist")
        );
        // ... but not one acted on and answered since: the task works again.
        progress.blocked_by = Some(("notes.md does not exist".into(), 1));
        assert_eq!(progress.failure_reason(Some(&reason)), None);
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
            ("Tip: Press Enter to submit your prompt\n", None),
            ("⏺ Read file.rs\n", Some("Read file.rs")),
            ("✶\n", None),
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
            (
                "The process exits when I press ctrl+c.\n",
                Some("The process exits when I press ctrl+c."),
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
