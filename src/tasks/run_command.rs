//! `exec.run`: a shell command typed into a new, visible terminal tab, with
//! its exit code read back from a marker line the same typed line prints.
//!
//! The command runs in the user's own terminal, where they can watch it and
//! take over. On exit 0 the tab closes; otherwise it stays so the user can
//! look at what went wrong.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use tokio::time::Instant;

use super::domain::{
    now_ms, Requested, Task, TaskBinding, TaskChange, TaskId, TaskKind, TaskState,
};
use super::may_touch;
use super::ports::TaskStore;
use crate::backend::{
    CreateTab, OutputFormat, OutputSource, PaneId, ReadPane, SendTextMode, TabId, TerminalBackend,
};
use crate::platform::config::SessionConfig;
use crate::terminal::exit_marker::{command_line, find_exit_after_command, output_after_command};
use crate::{terminal_backend, AppState};

/// The longest a call blocks; the caller polls again after that.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(45);
/// The last bytes of output a caller sees.
pub(crate) const TAIL_BYTES: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const READ_LINES: u32 = 400;
const LABEL_CHARS: usize = 24;
const SOURCE: &str = "run_command";
/// The error `start` gives when no configured session's backend answers.
pub(crate) const NO_TERMINAL: &str = "no reachable terminal session";
/// The error when the task's pane closed or now runs another shell.
const PANE_GONE: &str = "the task's pane is gone";

pub(crate) struct RunRequest {
    pub cwd: PathBuf,
    pub command: String,
    /// At most [`MAX_WAIT`].
    pub timeout: Duration,
}

pub(crate) struct RunHandle {
    pub task: Task,
    /// `None` while the command is still running.
    pub exit_code: Option<i32>,
    /// What the command printed so far, the last [`TAIL_BYTES`].
    pub stdout_tail: String,
    pub duration_ms: u64,
}

/// Open a tab in `request.cwd` on the first reachable session, type the
/// command there, and wait up to `request.timeout` for it to finish.
pub(crate) async fn start(
    state: &AppState,
    store: &dyn TaskStore,
    by: Requested,
    request: RunRequest,
) -> anyhow::Result<RunHandle> {
    launch(state, store, by, request, TaskId::new()).await
}

async fn launch(
    state: &AppState,
    store: &dyn TaskStore,
    by: Requested,
    request: RunRequest,
    id: TaskId,
) -> anyhow::Result<RunHandle> {
    let deadline = Instant::now() + request.timeout.min(MAX_WAIT);
    let (session, backend) = reachable_session(state)
        .await
        .ok_or_else(|| anyhow!(NO_TERMINAL))?;
    let label: String = request.command.chars().take(LABEL_CHARS).collect();
    let tab = backend
        .create_tab(&CreateTab {
            workspace_id: None,
            cwd: Some(request.cwd.clone()),
            label: Some(format!("muqun · {label}")),
            focus: false,
            env: None,
        })
        .await
        .context("create the command tab")?;
    // The new pane and its shell pid, which binds the task to this shell
    // rather than to an id tmux may hand out again after a restart.
    let listed = backend.list_panes().await;
    let tab_id = tab.id.clone();
    let found = match (tab.active_pane_id.clone(), listed) {
        (Some(pane_id), Ok(panes)) => {
            let shell_pid = panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .and_then(|pane| pane.shell_pid);
            Ok((pane_id, shell_pid))
        }
        (Some(pane_id), Err(error)) => {
            tracing::warn!("exec.run: no shell pid for the new pane: {error}");
            Ok((pane_id, None))
        }
        (None, listed) => listed.context("list panes").and_then(|panes| {
            panes
                .into_iter()
                .find(|pane| pane.tab_id == tab.id)
                .map(|pane| (pane.id, pane.shell_pid))
                .ok_or_else(|| anyhow!("the new tab has no pane"))
        }),
    };
    let (pane_id, shell_pid) = match found {
        Ok(found) => found,
        Err(error) => {
            close_unused(backend.as_ref(), &tab_id).await;
            return Err(error);
        }
    };
    let now = now_ms();
    let recorded = async {
        let task = store
            .insert(&Task {
                id,
                kind: TaskKind::Command,
                goal: request.command.clone(),
                cwd: Some(request.cwd.to_string_lossy().into_owned()),
                binding: TaskBinding::Pane {
                    session_id: session.id.clone(),
                    pane_id: pane_id.as_str().to_owned(),
                    tab_id: Some(tab.id.as_str().to_owned()),
                    shell_pid,
                    agent_id: None,
                },
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: by.by,
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: by.parent,
                origin: by.origin,
                label: None,
                requester_name: by.name,
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: None,
            })
            .await?;
        store
            .transition(&task.id, change(TaskState::Dispatching))
            .await
    };
    let task = match recorded.await {
        Ok(task) => task,
        Err(error) => {
            close_unused(backend.as_ref(), &tab_id).await;
            return Err(error);
        }
    };
    let line = command_line(&request.command, &task.id.0);
    let typed = async {
        backend
            .send_text(&pane_id, &line, SendTextMode::Paste)
            .await?;
        backend.send_keys(&pane_id, &["Enter".to_owned()]).await
    };
    if let Err(error) = typed.await {
        let why = format!("the command could not be typed: {error:#}");
        let failed = TaskChange {
            summary: Some(super::domain::one_line(&why, super::service::SUMMARY_CHARS)),
            note: Some(why),
            ..change(TaskState::Failed)
        };
        store.transition(&task.id, failed).await?;
        return Err(anyhow!(error).context("type the command"));
    }
    let task = store
        .transition(&task.id, change(TaskState::Working))
        .await?;
    poll(state, store, &task, deadline).await
}

/// Close a tab nothing was typed into, so a failed start leaves none behind.
async fn close_unused(backend: &dyn TerminalBackend, tab_id: &TabId) {
    if let Err(error) = backend.close_tab(tab_id).await {
        tracing::warn!("exec.run: could not close the unused tab: {error}");
    }
}

/// Read the task's pane until its exit marker shows or `deadline` passes.
/// On exit the task moves to Done (0) or Failed (anything else) and an exit-0
/// tab is closed. Past the deadline the task is left Working.
pub(crate) async fn poll(
    state: &AppState,
    store: &dyn TaskStore,
    task: &Task,
    deadline: Instant,
) -> anyhow::Result<RunHandle> {
    let duration_ms = || now_ms().saturating_sub(task.created_ms);
    if task.state.is_terminal() {
        return Ok(RunHandle {
            task: task.clone(),
            exit_code: task.exit_code,
            stdout_tail: String::new(),
            duration_ms: task.updated_ms.saturating_sub(task.created_ms),
        });
    }
    let (backend, pane_id, tab_id) = pane_of(state, task).await?;
    let read = ReadPane {
        pane_id: pane_id.clone(),
        source: OutputSource::RecentUnwrapped,
        format: OutputFormat::Text,
        lines: READ_LINES,
        start: None,
        end: None,
    };
    loop {
        let text = backend
            .read_pane(&read)
            .await
            .context("read the pane")?
            .text;
        let stdout_tail = tail(output_after_command(&text, &task.id.0));
        if let Some(code) = find_exit_after_command(&text, &task.id.0) {
            let finished = TaskChange {
                exit_code: Some(code),
                // Kept whatever the code: an exit-0 tab closes now, and any
                // other pane may be closed later, taking the output with it.
                summary: Some(format!("exit {code}\n{stdout_tail}")),
                ..change(if code == 0 {
                    TaskState::Done
                } else {
                    TaskState::Failed
                })
            };
            let task = match store.transition(&task.id, finished).await {
                Ok(task) => task,
                // Cancelled meanwhile: that is the answer.
                Err(error) => store.get(&task.id).await?.ok_or(error)?,
            };
            if code == 0 {
                if let Some(tab_id) = &tab_id {
                    if let Err(error) = backend.close_tab(tab_id).await {
                        tracing::warn!("exec.run: could not close the finished tab: {error}");
                    }
                }
            }
            return Ok(RunHandle {
                task,
                exit_code: Some(code),
                stdout_tail,
                duration_ms: duration_ms(),
            });
        }
        if Instant::now() + POLL_INTERVAL > deadline {
            return Ok(RunHandle {
                task: task.clone(),
                exit_code: None,
                stdout_tail,
                duration_ms: duration_ms(),
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Interrupt the command: Ctrl-C into its pane, best effort -- a pane that
/// is gone, or no longer the task's, is left alone, and the caller records
/// the cancel either way.
pub(crate) async fn interrupt(state: &AppState, task: &Task) {
    let sent = match pane_of(state, task).await {
        Ok((backend, pane_id, _)) => backend
            .send_keys(&pane_id, &["ctrl+c".to_owned()])
            .await
            .map_err(anyhow::Error::from),
        Err(error) => Err(error),
    };
    if let Err(error) = sent {
        tracing::warn!(task = %task.id.0, "exec.run: could not interrupt the command: {error:#}");
    }
}

pub(crate) async fn reachable_session(
    state: &AppState,
) -> Option<(SessionConfig, Box<dyn TerminalBackend>)> {
    for session in &state.config.sessions {
        let backend = terminal_backend(session);
        if matches!(backend.probe_reachable().await, Ok(true)) {
            return Some((session.clone(), backend));
        }
    }
    None
}

/// The task's backend, pane and tab. A pane bound with a shell pid must
/// still run that shell: after a tmux restart the same id can name a
/// stranger's pane, which must not be read or interrupted.
async fn pane_of(
    state: &AppState,
    task: &Task,
) -> anyhow::Result<(Box<dyn TerminalBackend>, PaneId, Option<TabId>)> {
    let TaskBinding::Pane {
        session_id,
        pane_id,
        tab_id,
        shell_pid,
        ..
    } = &task.binding
    else {
        return Err(anyhow!("not a command task"));
    };
    let session = state
        .config
        .sessions
        .iter()
        .find(|session| &session.id == session_id)
        .ok_or_else(|| anyhow!("the task's terminal session is gone"))?;
    let backend = terminal_backend(session);
    let pane_id = PaneId::new(pane_id);
    if !may_touch(backend.as_ref(), &pane_id, *shell_pid).await {
        return Err(anyhow!(PANE_GONE));
    }
    Ok((backend, pane_id, tab_id.as_deref().map(TabId::new)))
}

fn change(to: TaskState) -> TaskChange {
    TaskChange {
        to,
        source: SOURCE.into(),
        ..Default::default()
    }
}

/// The last [`TAIL_BYTES`] of `text`, cut on a char boundary.
fn tail(text: &str) -> String {
    let mut start = text.len().saturating_sub(TAIL_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::domain::{PrincipalKind, TaskPrincipal};
    use crate::tasks::store_sqlite::SqliteTaskStore;
    use crate::test_support::{output_state, FakeHerdr};
    use serde_json::{json, Value};

    const ID: &str = "t_runtest";

    fn principal() -> Requested {
        Requested::mcp(TaskPrincipal {
            kind: PrincipalKind::Assistant,
            id: "pane:default/w1:p1".into(),
        })
    }

    fn request(command: &str, timeout: Duration) -> RunRequest {
        RunRequest {
            cwd: std::env::temp_dir(),
            command: command.into(),
            timeout,
        }
    }

    /// Prompt, then the echoed typed line, then output and the marker -- what
    /// a pane shows across three reads.
    fn screens(command: &str, output: &str, code: i32) -> Vec<String> {
        let echo = format!("$ {}", command_line(command, ID));
        vec![
            "$ ".into(),
            echo.clone(),
            format!("{echo}\n{output}\u{1f}muqun-exit:{ID}:{code}\u{1f}\n$ "),
        ]
    }

    fn methods(herdr: &FakeHerdr, method: &str) -> Vec<Value> {
        herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["method"] == method)
            .cloned()
            .collect()
    }

    async fn run(
        screens: Vec<String>,
        command: &str,
        timeout: Duration,
    ) -> (FakeHerdr, SqliteTaskStore, RunHandle) {
        let herdr = FakeHerdr::start(screens.iter().map(String::as_str).collect(), None);
        let state = output_state(&herdr);
        let store = SqliteTaskStore::new(state.db.clone());
        let handle = launch(
            &state,
            &store,
            principal(),
            request(command, timeout),
            TaskId(ID.into()),
        )
        .await
        .unwrap();
        (herdr, store, handle)
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code_and_keeps_its_pane() {
        let command = "echo hi; exit 3";
        let (herdr, store, handle) = run(
            screens(command, "hi\n", 3),
            command,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(handle.exit_code, Some(3));
        assert_eq!(handle.stdout_tail, "hi\n");
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.exit_code, Some(3));
        assert_eq!(task.summary.as_deref(), Some("exit 3\nhi\n"));
        assert_eq!(task.kind, TaskKind::Command);
        assert_eq!(task.goal, command);
        assert_eq!(handle.task, task);
        assert!(
            methods(&herdr, "tab.close").is_empty(),
            "a failure keeps its pane"
        );

        let create = &methods(&herdr, "tab.create")[0]["params"];
        assert_eq!(create["label"], "muqun · echo hi; exit 3");
        assert_eq!(create["focus"], false);
        assert_eq!(create["cwd"], json!(std::env::temp_dir()));
        let typed = &methods(&herdr, "pane.send_text")[0]["params"];
        assert_eq!(
            typed["text"],
            format!(
                "sh -c 'sh -c \"$1\"; printf \"\\037muqun-exit:%s:%d\\037\\n\" \"$2\" \"$?\"' sh 'echo hi; exit 3' {ID}"
            )
        );
        assert_eq!(herdr.enters().len(), 1);
        let path: Vec<TaskState> = store
            .events(&task.id)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.to)
            .collect();
        assert_eq!(
            path,
            [
                TaskState::Queued,
                TaskState::Dispatching,
                TaskState::Working,
                TaskState::Failed
            ]
        );
    }

    #[tokio::test]
    async fn a_successful_command_closes_its_tab() {
        let (herdr, store, handle) =
            run(screens("true", "", 0), "true", Duration::from_secs(10)).await;
        assert_eq!(handle.exit_code, Some(0));
        assert_eq!(handle.stdout_tail, "");
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        assert_eq!(task.state, TaskState::Done);
        let closed = methods(&herdr, "tab.close");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0]["params"]["tab_id"], "w1:t9");
    }

    #[tokio::test]
    async fn past_the_deadline_without_a_marker_the_task_is_still_working() {
        let echo = format!("$ {}\n", command_line("sleep 60", ID));
        let (herdr, store, handle) = run(
            vec!["$ ".into(), echo],
            "sleep 60",
            Duration::from_millis(1200),
        )
        .await;
        assert_eq!(handle.exit_code, None);
        assert_eq!(handle.task.state, TaskState::Working);
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        assert_eq!(task.state, TaskState::Working);
        assert!(methods(&herdr, "tab.close").is_empty());
        // This backend could not tell the shell pid: bound by id only, as
        // stored rows from before the pid existed are.
        assert!(matches!(
            task.binding,
            TaskBinding::Pane {
                shell_pid: None,
                agent_id: None,
                ..
            }
        ));

        interrupt(&output_state(&herdr), &task).await;
        let keys = methods(&herdr, "pane.send_keys");
        assert_eq!(keys.last().unwrap()["params"]["keys"], json!(["ctrl+c"]));
    }

    #[tokio::test]
    async fn a_marker_printed_before_the_typed_line_does_not_finish_the_task() {
        let echo = format!("$ {}", command_line("sleep 60", ID));
        let forged = format!("\u{1f}muqun-exit:{ID}:0\u{1f}\n{echo}\n");
        let (_herdr, _store, handle) =
            run(vec![forged], "sleep 60", Duration::from_millis(600)).await;
        assert_eq!(handle.exit_code, None);
        assert_eq!(handle.task.state, TaskState::Working);
    }

    /// A long output scrolls the typed line out of the read window; the run
    /// still finishes on its marker.
    #[tokio::test]
    async fn a_long_output_that_scrolled_the_typed_line_away_still_finishes() {
        let long = (0..500).map(|i| format!("line {i}\n")).collect::<String>();
        let window = long.lines().skip(120).collect::<Vec<_>>().join("\n");
        let screen = format!("{window}\n\u{1f}muqun-exit:{ID}:2\u{1f}\n$ ");
        let (herdr, store, handle) =
            run(vec![screen], "seq 500; exit 2", Duration::from_secs(5)).await;
        assert_eq!(handle.exit_code, Some(2));
        assert!(handle.stdout_tail.ends_with("line 498\nline 499\n"));
        assert!(handle.stdout_tail.len() <= TAIL_BYTES);
        assert!(!handle.stdout_tail.contains("muqun-exit"));
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.exit_code, Some(2));
        assert!(methods(&herdr, "tab.close").is_empty());
    }

    fn new_pane(shell_pid: u32) -> Value {
        json!([{ "pane_id": "w1:p9", "workspace_id": "w1", "tab_id": "w1:t9", "shell_pid": shell_pid }])
    }

    /// tmux hands `%N` out again after a server restart: a pane with the
    /// task's id but another shell is not the task's pane.
    #[tokio::test]
    async fn a_pane_now_running_another_shell_is_gone() {
        let echo = format!("$ {}\n", command_line("sleep 60", ID));
        let herdr = FakeHerdr::start(vec!["$ ", &echo], None);
        herdr.set_panes(new_pane(100));
        let state = output_state(&herdr);
        let store = SqliteTaskStore::new(state.db.clone());
        let handle = launch(
            &state,
            &store,
            principal(),
            request("sleep 60", Duration::from_millis(600)),
            TaskId(ID.into()),
        )
        .await
        .unwrap();
        assert_eq!(handle.task.state, TaskState::Working);
        assert!(matches!(
            handle.task.binding,
            TaskBinding::Pane {
                shell_pid: Some(100),
                agent_id: None,
                ..
            }
        ));

        herdr.set_panes(new_pane(200));
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        let Err(error) = poll(&state, &store, &task, Instant::now()).await else {
            panic!("a reused pane must not be read");
        };
        assert_eq!(error.to_string(), "the task's pane is gone");
        let reads = methods(&herdr, "pane.read").len();
        interrupt(&state, &task).await;
        assert!(methods(&herdr, "pane.send_keys")
            .iter()
            .all(|call| call["params"]["keys"] != json!(["ctrl+c"])));
        assert_eq!(methods(&herdr, "pane.read").len(), reads, "nothing read");
        let task = store.get(&TaskId(ID.into())).await.unwrap().unwrap();
        assert_eq!(task.state, TaskState::Working);
    }

    #[test]
    fn the_tail_keeps_the_last_bytes_on_a_char_boundary() {
        assert_eq!(tail("abc"), "abc");
        let long = format!("é{}", "a".repeat(TAIL_BYTES - 1));
        let cut = tail(&long);
        assert!(cut.len() <= TAIL_BYTES && cut.chars().all(|c| c == 'a'));
    }
}
