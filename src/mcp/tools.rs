//! The MCP tool surface: thin adapters over `FleetService` and `TaskService`.
//! Every result is one JSON text block of at most [`MAX_RESULT_BYTES`];
//! long work returns a handle that `tasks.wait` polls.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::lineage;
use super::tokens::AssistantSession;
use crate::fleet::manifest::{is_self, DescribeError};
use crate::tasks::domain::{clip, TaskBinding, TaskId, TaskPrincipal};
use crate::tasks::run_command::{RunHandle, RunRequest};
use crate::tasks::service::{
    summary_line, task_directory, valid_goal, valid_name, DelegateRequest, Outcome, ReadWhat,
    TaskError,
};
use crate::AppState;

pub(crate) const MAX_RESULT_BYTES: usize = 16 * 1024;
pub(crate) const MAX_WAIT_S: u64 = 45;
pub(crate) const DEFAULT_WAIT_S: u64 = 30;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Room left for `"truncated":true` when a result is clipped.
const CLIP_BUDGET: usize = MAX_RESULT_BYTES - 32;

pub(crate) fn wait_slice(timeout_s: Option<u64>) -> Duration {
    Duration::from_secs(timeout_s.unwrap_or(DEFAULT_WAIT_S).clamp(1, MAX_WAIT_S))
}

/// A JSON tool result. One too large is cut down by [`fit`] and says
/// `"truncated": true`.
pub(crate) fn ok(mut value: Value) -> CallToolResult {
    if fit(&mut value) {
        if let Some(fields) = value.as_object_mut() {
            fields.insert("truncated".into(), Value::Bool(true));
        }
    }
    let text = value.to_string();
    if text.len() > MAX_RESULT_BYTES {
        tracing::warn!("an MCP result of {} bytes did not fit", text.len());
        return fail("internal", "The call failed.");
    }
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// An error result: `{error: <code>, message}`.
pub(crate) fn fail(code: &str, message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        json!({ "error": code, "message": message }).to_string(),
    )])
}

/// Bring `value` within [`CLIP_BUDGET`] bytes of JSON: first drop tail items
/// of its largest array (keeping one), then clip its longest strings,
/// keeping their ends. Each step is one linear pass over the value, never a
/// re-serialisation per cut. Whether anything was cut.
fn fit(value: &mut Value) -> bool {
    let mut index = Index::default();
    let size = index.measure(value, &mut String::new());
    let mut excess = size.saturating_sub(CLIP_BUDGET);
    if excess == 0 {
        return false;
    }
    let largest = index.arrays.iter().max_by_key(|(_, len)| *len);
    if let Some(Value::Array(items)) = largest.and_then(|(at, _)| value.pointer_mut(at)) {
        while excess > 0 && items.len() > 1 {
            let dropped = items.pop().expect("more than one item");
            // The item and the comma before it.
            excess = excess.saturating_sub(dropped.to_string().len() + 1);
        }
    }
    if excess == 0 {
        return true;
    }
    let mut index = Index::default();
    index.measure(value, &mut String::new());
    // Longest first; of equally long strings, the first in the document.
    index
        .strings
        .sort_by_key(|(_, len)| std::cmp::Reverse(*len));
    for (at, len) in index.strings {
        let Some(Value::String(text)) = value.pointer_mut(&at) else {
            continue;
        };
        if excess == 0 || text.len() <= "…".len() {
            break;
        }
        let clipped = clip(text, text.len().saturating_sub(excess + "…".len()));
        excess = excess.saturating_sub(len.saturating_sub(json_len(&clipped)));
        *text = clipped;
    }
    true
}

/// The serialised length of a JSON string holding `text`.
fn json_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len() + 2, |json| json.len())
}

/// Where a value's arrays and strings are (JSON pointers) and how many bytes
/// of JSON each takes.
#[derive(Default)]
struct Index {
    arrays: Vec<(String, usize)>,
    strings: Vec<(String, usize)>,
}

impl Index {
    /// The serialised length of `value` (at pointer `at`), recording its
    /// arrays and strings on the way.
    fn measure(&mut self, value: &Value, at: &mut String) -> usize {
        let nested = |index: &mut Self, at: &mut String, step: &str, item: &Value| {
            let parent = at.len();
            at.push('/');
            at.push_str(&step.replace('~', "~0").replace('/', "~1"));
            let len = index.measure(item, at);
            at.truncate(parent);
            len
        };
        match value {
            Value::String(text) => {
                let len = json_len(text);
                self.strings.push((at.clone(), len));
                len
            }
            Value::Array(items) => {
                let mut len = 2 + items.len().saturating_sub(1);
                for (i, item) in items.iter().enumerate() {
                    len += nested(self, at, &i.to_string(), item);
                }
                self.arrays.push((at.clone(), len));
                len
            }
            Value::Object(fields) => {
                let mut len = 2 + fields.len().saturating_sub(1);
                for (key, item) in fields {
                    len += json_len(key) + 1 + nested(self, at, key, item);
                }
                len
            }
            scalar => scalar.to_string().len(),
        }
    }
}

fn invalid(message: &str) -> CallToolResult {
    fail("invalid_arguments", message)
}

/// Generic answers; the details go to the log.
fn task_error(error: TaskError) -> CallToolResult {
    match error {
        // Someone else's task is not found either, so ids cannot be probed.
        TaskError::NotFound | TaskError::NotYours => {
            fail("task_not_found", "No task with that task_id.")
        }
        TaskError::UnknownMachine { known, .. } => {
            let known: Vec<String> = std::iter::once("self".to_owned()).chain(known).collect();
            fail(
                "machine_unknown",
                &format!("Unknown machine. Known: {}.", known.join(", ")),
            )
        }
        TaskError::MachineUnreachable(name) => fail(
            "machine_unreachable",
            &format!("{name} has not answered in the last 2 minutes; nothing was started there."),
        ),
        TaskError::DelegationsDisabled(name) => fail(
            "delegations_disabled",
            &format!("{name} does not take work from other computers (its owner switched it off)."),
        ),
        TaskError::DelegationLoop(name) => fail(
            "delegation_loop",
            &format!(
                "Your task was handed to this computer by another one, so it cannot hand work on to {name}: one hop only. Do it here, or finish with tasks.complete (outcome blocked) and say what needs {name}."
            ),
        ),
        TaskError::PeerUnauthorized(name) => fail(
            "unauthorized",
            &format!(
                "{name} no longer accepts this computer; link again from your phone."
            ),
        ),
        // An argument the computer there refused is the same mistake as here.
        TaskError::PeerRefused { machine, code }
            if matches!(
                code.as_str(),
                "invalid_request" | "unknown_model" | "unknown_variant" | "terminal_unknown"
            ) =>
        {
            invalid(&format!("{machine} refused the arguments ({code})."))
        }
        // The code is the peer's own, already bounded to `[a-z0-9_]{1,64}`.
        TaskError::PeerRefused { machine, code } => {
            fail(&code, &format!("{machine} refused the task ({code})."))
        }
        TaskError::AgentUnavailable(_) => fail(
            "agent_unavailable",
            "That agent is not available on this computer.",
        ),
        TaskError::UnknownModel(_) => invalid(
            "Unknown model for that agent. Use a model id from its catalog, or <provider_id>/<model_id>.",
        ),
        TaskError::UnknownVariant { wanted, offered } => invalid(
            &crate::tasks::service::unknown_variant_message(Default::default(), &wanted, &offered),
        ),
        TaskError::NoTimeline => invalid("Only a delegated task has a timeline."),
        TaskError::InvalidSummary => invalid("summary must be non-empty and at most 32 KiB"),
        TaskError::ChildrenOpen(which) => fail("children_open", &which),
        TaskError::NoTerminal => fail(
            "no_terminal",
            "No terminal session is reachable on this computer.",
        ),
        TaskError::TerminalUnknown(id) => invalid(&format!(
            "No terminal session '{id}' on this computer; leave terminal out to use the first that answers."
        )),
        error => {
            tracing::warn!("MCP task call failed: {error}");
            fail("internal", "The call failed.")
        }
    }
}

fn reply<T: Serialize>(result: Result<T, TaskError>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(value) => ok(json!(value)),
        Err(error) => task_error(error),
    })
}

/// What `exec.run` answers: the handle, the output so far, and the pane.
fn command_result(handle: RunHandle) -> Value {
    let pane = match &handle.task.binding {
        TaskBinding::Pane {
            session_id,
            pane_id,
            ..
        } => json!({ "session_id": session_id, "pane_id": pane_id }),
        _ => Value::Null,
    };
    json!({
        "task_id": handle.task.id.0,
        "state": handle.task.state,
        "exit_code": handle.exit_code,
        "stdout_tail": handle.stdout_tail,
        "duration_ms": handle.duration_ms,
        "pane": pane,
    })
}

/// An absolute, existing directory, canonicalized.
fn directory(cwd: &str) -> Result<PathBuf, CallToolResult> {
    task_directory(cwd).map_err(|_| invalid("cwd must be an existing absolute directory"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct DescribeArgs {
    /// The computer to describe: "self" (default), or a linked computer's name (from `peers`) or machineId (from fleet.list).
    #[serde(default)]
    pub machine: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExecRunArgs {
    /// Absolute working directory on this computer.
    pub cwd: String,
    /// Shell command, run as `sh -c <command>`.
    pub command: String,
    /// Seconds to wait before returning a task handle (1..=45, default 30).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

impl ExecRunArgs {
    fn request(self) -> Result<RunRequest, CallToolResult> {
        if self.command.trim().is_empty() || self.command.len() > MAX_COMMAND_BYTES {
            return Err(invalid("command must be non-empty and at most 16 KiB"));
        }
        Ok(RunRequest {
            cwd: directory(&self.cwd)?,
            command: self.command,
            timeout: wait_slice(self.timeout_s),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct DelegateArgs {
    /// What the agent should do, in plain language.
    pub goal: String,
    /// Absolute project directory on this computer (any existing one; for a terminal agent, where its pane opens).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Agent to use: a structured agent, e.g. "opencode", or a terminal agent from the agent catalog, e.g. "codex" or "claude" (started in a new terminal pane; model and variant are passed to its launch, mode does not apply). Default: the primary agent.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Agent mode, e.g. "build" or "plan".
    #[serde(default)]
    pub mode: Option<String>,
    /// Model id for the agent, e.g. "claude-haiku-5-5" (or "<provider_id>/<model_id>"). Default: the agent's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Model variant (reasoning effort) from the model's catalog entry, e.g. "high". Applied only together with `model`; ignored otherwise.
    #[serde(default)]
    pub variant: Option<String>,
    /// The computer to run on: "self" (default), or the name of a linked computer from fleet.describe (e.g. "mini"); cwd is then a directory on that computer.
    #[serde(default)]
    pub machine: Option<String>,
    /// For a terminal agent: the terminal session to start it in, by its id on that computer (e.g. "default" for tmux, or a Herdr session's id). Default: the first that answers.
    #[serde(default)]
    pub terminal: Option<String>,
}

impl DelegateArgs {
    /// `machine_id`/`label`: this computer's, so `cwd` is checked here only
    /// when the task runs here; another computer checks its own (`~` too).
    fn request(
        self,
        machine_id: &str,
        label: &str,
        peers: &[String],
    ) -> Result<DelegateRequest, CallToolResult> {
        if !valid_goal(&self.goal) {
            return Err(invalid("goal must be non-empty and at most 32 KiB"));
        }
        let names = [
            &self.agent_id,
            &self.mode,
            &self.model,
            &self.variant,
            &self.terminal,
        ];
        if !names.iter().filter_map(|n| n.as_deref()).all(valid_name) {
            return Err(invalid(
                "agent_id, mode, model, variant and terminal must be non-empty and at most 256 bytes",
            ));
        }
        let here = is_self(self.machine.as_deref(), machine_id, label, peers);
        let cwd = match self.cwd {
            Some(cwd) if here => Some(directory(&cwd)?.to_string_lossy().into_owned()),
            Some(cwd) if !cwd.is_empty() && cwd.len() <= 4096 => Some(cwd),
            Some(_) => return Err(invalid("cwd must be a directory path")),
            None => None,
        };
        Ok(DelegateRequest {
            goal: self.goal,
            cwd,
            agent_id: self.agent_id,
            mode: self.mode,
            model: self.model,
            variant: self.variant,
            machine: self.machine,
            label: None,
            mentions: Vec::new(),
            client_ref: None,
            session_id: self.terminal,
            attachments: Vec::new(),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct WaitArgs {
    /// A task_id from tasks.delegate or exec.run.
    pub task_id: String,
    /// Seconds to wait (1..=45, default 30). Call again while it is working.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum What {
    Summary,
    Timeline,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ReadArgs {
    /// A task_id you created. Leave it out to ask whether this session is
    /// working on a Muqun task, and which (its summary lists the computers
    /// the user named, `mentions`).
    #[serde(default)]
    pub task_id: Option<String>,
    /// "summary": the task as it stands. "timeline": its recent agent items.
    #[serde(default = "summary")]
    pub what: What,
    /// Timeline only: items after this cursor (from a previous read).
    #[serde(default)]
    pub cursor: Option<u64>,
}

fn summary() -> What {
    What::Summary
}

impl ReadArgs {
    fn what(&self) -> ReadWhat {
        match self.what {
            What::Summary => ReadWhat::Summary,
            What::Timeline => ReadWhat::Timeline,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CancelArgs {
    /// A task_id you created.
    pub task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CompleteArgs {
    /// The task you are working on; default: the one this session was given.
    #[serde(default)]
    pub task_id: Option<String>,
    /// What was done -- or, blocked, why you stopped -- in one line (cut to
    /// 240 characters).
    pub summary: String,
    /// `done` (the default) when the task is finished; `blocked` when you
    /// stopped because you need the user: a missing file, an unclear
    /// request, a permission you cannot get. The user answers in this
    /// session and you go on; call tasks.complete again when it is done.
    #[serde(default)]
    pub outcome: Outcome,
}

#[derive(Clone)]
pub(crate) struct MuqunMcp {
    state: AppState,
    /// Who is calling; work is attributed to it.
    session: AssistantSession,
    /// Whether the caller was started for a task ([`started_for_task`]):
    /// its instructions then say so plainly.
    in_task: bool,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl MuqunMcp {
    pub(crate) fn new(state: AppState, session: AssistantSession) -> Self {
        Self {
            state,
            session,
            in_task: false,
            tool_router: Self::tool_router(),
        }
    }

    /// The handler for a caller that [`started_for_task`] says is a task.
    pub(crate) fn in_task(mut self, in_task: bool) -> Self {
        self.in_task = in_task;
        self
    }

    fn by(&self) -> TaskPrincipal {
        self.session.principal()
    }

    #[tool(
        name = "fleet.list",
        description = "List the computers this gateway can hand work to: this one and every linked one, with OS, what each is suitable for, whether it is reachable and how busy it is."
    )]
    async fn fleet_list(&self) -> Result<CallToolResult, ErrorData> {
        let machines = self.state.fleet.list(&self.state).await;
        Ok(ok(json!({ "machines": machines })))
    }

    #[tool(
        name = "fleet.describe",
        description = "What a computer offers: its agents (status, version, modes, model count), terminal backends, toolchains with versions, task counts and what it is suitable for. Without machine, this computer plus a short entry per linked computer (peers)."
    )]
    async fn fleet_describe(
        &self,
        Parameters(args): Parameters<DescribeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let fleet = &self.state.fleet;
        Ok(
            match fleet.describe(&self.state, args.machine.as_deref()).await {
                Ok(manifest) => ok(manifest),
                Err(DescribeError::Unknown(known)) => fail(
                    "machine_unknown",
                    &format!(
                        "Unknown machine. Known: self{}.",
                        known
                            .iter()
                            .map(|name| format!(", {name}"))
                            .collect::<String>()
                    ),
                ),
                Err(DescribeError::Unreachable(name)) => fail(
                    "machine_unreachable",
                    &format!("{name} cannot be reached right now."),
                ),
                Err(DescribeError::NoManifest(name)) => fail(
                    "manifest_unavailable",
                    &format!("{name} answers but did not say what it offers."),
                ),
                Err(DescribeError::Unauthorized(name)) => fail(
                    "unauthorized",
                    &format!("{name} no longer accepts this computer; link again from your phone."),
                ),
            },
        )
    }

    #[tool(
        name = "exec.run",
        description = "Run a shell command in a visible terminal pane on this computer and return its exit code and output tail. Blocks up to timeout_s (max 45); if still running, returns a task_id for tasks.wait."
    )]
    async fn exec_run(
        &self,
        Parameters(args): Parameters<ExecRunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (command, cwd) = (args.command.clone(), args.cwd.clone());
        let request = match args.request() {
            Ok(request) => request,
            Err(refused) => return Ok(refused),
        };
        let call = lineage::Call {
            tool: "exec.run",
            args: vec![("command", &command), ("cwd", &cwd)],
        };
        let by = lineage::requested(&self.state, &self.session, &call).await;
        let run = self.state.tasks.run(&self.state, by, request).await;
        reply(run.map(command_result))
    }

    #[tool(
        name = "tasks.delegate",
        description = "Start a sub-task on an agent, on this computer or (machine) on a linked one: a new session in cwd with your goal as the first prompt (for a terminal agent such as codex or claude, a new terminal pane running it). Returns task_id. If the work will take long, end your turn after delegating; you will be told when it finishes. Use tasks.wait only for short jobs. One hop only: a task another computer handed here cannot hand work to a linked computer (delegation_loop)."
    )]
    async fn tasks_delegate(
        &self,
        Parameters(args): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let goal = args.goal.clone();
        let config = &self.state.config;
        let peers = self.state.peers.names();
        let request = match args.request(&config.server_id, &config.label, &peers) {
            Ok(request) => request,
            Err(refused) => return Ok(refused),
        };
        let call = lineage::Call {
            tool: "tasks.delegate",
            args: vec![("goal", &goal)],
        };
        let by = lineage::requested(&self.state, &self.session, &call).await;
        let caller = lineage::caller_session(&self.state, &self.session, &call).await;
        let view = match self.state.tasks.delegate(&self.state, by, request).await {
            Ok(view) => view,
            Err(error) => return Ok(task_error(error)),
        };
        let id = TaskId(view.task_id.clone());
        if let Some(caller) = caller {
            self.state.tasks.notify(&id, &caller).await;
        }
        if let (Some(parent), Ok(Some(child))) = (
            view.parent_task_id.as_ref(),
            self.state.tasks.task(&id).await,
        ) {
            let parent = TaskId(parent.clone());
            self.state.tasks.waits_on(&parent, &child).await;
        }
        let mut body = json!(view);
        // The create went out and got no answer: whether it started is unknown.
        if let TaskBinding::Peer {
            remote_task_id,
            peer_name,
            ..
        } = &view.binding
        {
            if remote_task_id.is_empty() {
                body["note"] = json!(format!(
                    "Unknown: {peer_name} did not answer in time, so the task may have started there. \
                     Wait on this task_id: it follows that task once {peer_name} answers, or fails if it never started. \
                     Do not delegate it again."
                ));
            }
        }
        Ok(ok(body))
    }

    #[tool(
        name = "tasks.wait",
        description = "Wait up to timeout_s (max 45) for a task to reach done/failed/waiting, or idle_unconfirmed (its agent stopped without completing: read its answer and decide; do not call again while it says idle_unconfirmed). Returns the current state and summary. If the work will take long, end your turn after delegating; you will be told when it finishes. Use tasks.wait only for short jobs."
    )]
    async fn tasks_wait(
        &self,
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (id, slice) = (TaskId(args.task_id), wait_slice(args.timeout_s));
        let tasks = &self.state.tasks;
        let waited = tasks.wait(&self.state, &self.by(), &id, slice).await;
        // What a wait answers is not told again into the caller's session.
        if let Ok(waited) = &waited {
            tasks.told(&id, waited.task.state).await;
        }
        reply(waited)
    }

    #[tool(
        name = "tasks.read",
        description = "Read a task you created: its summary or recent timeline. Without task_id: whether this session itself is working on a Muqun task, and that task, whose summary lists the computers the user named for it (mentions)."
    )]
    async fn tasks_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (state, what) = (&self.state, args.what());
        let Some(task_id) = args.task_id.clone() else {
            let call = lineage::Call {
                tool: "tasks.read",
                args: Vec::new(),
            };
            let own = lineage::own_task(state, &self.session, &call, None).await;
            return Ok(match own {
                Some(id) => match state.tasks.own_view(&id).await {
                    Ok(task) => CallToolResult::structured(json!({
                        "your_task": task,
                        "when_finished": "call tasks.complete with a one-line summary \
                                          (outcome blocked and the reason if you need the user)",
                    })),
                    Err(error) => task_error(error),
                },
                None => CallToolResult::structured(json!({ "your_task": null, "note": NO_TASK })),
            });
        };
        let id = TaskId(task_id);
        match state
            .tasks
            .read(state, &self.by(), &id, what, args.cursor)
            .await
        {
            Err(TaskError::NotYours) => {}
            read => return reply(read),
        }
        // Not one it created: the task this session works on, if it is that.
        let what_text = match args.what {
            What::Summary => "summary",
            What::Timeline => "timeline",
        };
        let call = lineage::Call {
            tool: "tasks.read",
            args: vec![("what", what_text), ("task_id", &id.0)],
        };
        match lineage::own_task(state, &self.session, &call, Some(&id)).await {
            Some(own) => reply(state.tasks.read_own(state, &own, what, args.cursor).await),
            None => Ok(task_error(TaskError::NotFound)),
        }
    }

    #[tool(
        name = "tasks.complete",
        description = "Call this when the task you were given is finished, with a one-line summary (outcome done, the default) -- or when you stop because you need the user, with outcome blocked and the reason (a missing file, an unclear request, a permission you cannot get). Only the session (or terminal pane) working on the task can complete it; stopping or going idle does not, and a blocked task stays open until you call this again with done. If it answers task_not_found, you are not working on a Muqun task: do not call it again."
    )]
    async fn tasks_complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if !matches!(summary_line(&args.summary), Ok(Some(_))) {
            return Ok(task_error(TaskError::InvalidSummary));
        }
        let mut call = lineage::Call {
            tool: "tasks.complete",
            args: vec![("summary", &args.summary)],
        };
        if let Some(id) = &args.task_id {
            call.args.push(("task_id", id));
        }
        let wanted = args.task_id.clone().map(TaskId);
        let own = lineage::own_task(&self.state, &self.session, &call, wanted.as_ref()).await;
        let Some(id) = own else {
            return Ok(fail(
                "task_not_found",
                "No open task of this session was found: this session is not working on a \
                 Muqun task. Do not call tasks.complete again and print no marker.",
            ));
        };
        reply(
            self.state
                .tasks
                .complete(&id, &args.summary, args.outcome)
                .await,
        )
    }

    #[tool(name = "tasks.cancel", description = "Interrupt a task you created.")]
    async fn tasks_cancel(
        &self,
        Parameters(args): Parameters<CancelArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let task_id = args.task_id.clone();
        let id = TaskId(args.task_id);
        let call = lineage::Call {
            tool: "tasks.cancel",
            args: vec![("task_id", &task_id)],
        };
        // Judged before the cancel ends the call: the session making it.
        let caller = lineage::caller_session(&self.state, &self.session, &call).await;
        let cancelled = self.state.tasks.cancel(&self.state, &self.by(), &id).await;
        // The session that would be told cancelled it itself: not told back.
        // Another session under the same token is not that parent.
        if let (Ok(view), Some(caller)) = (&cancelled, caller) {
            if self.state.tasks.notice_of(&id).await.as_ref() == Some(&caller) {
                self.state.tasks.told(&id, view.state).await;
            }
        }
        reply(cancelled)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MuqunMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "muqun-gateway",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions(self.in_task))
    }
}

/// What the server tells a caller about itself and about finishing a task:
/// plainly to one started for a task (`in_task`), conditionally to anyone
/// else, who may never have been given one.
pub(crate) fn instructions(in_task: bool) -> String {
    let about = "Muqun Gateway on this computer. fleet.describe says what it has and which \
                 other computers are linked to it, exec.run runs short commands, \
                 tasks.delegate hands work to a coding agent -- if the work will take long, \
                 end your turn after delegating; you will be told when it finishes. Use \
                 tasks.wait (at most 45 s) only for short jobs; tasks.delegate(machine) \
                 sends work to a linked computer, and your task's summary (tasks.read without \
                 a task_id) lists the ones the user named (mentions).";
    let finish = "call tasks.complete with a one-line summary; when you stop because you \
                  need the user, call it with outcome \"blocked\" and the reason; if you \
                  cannot call it, end with the line MUQUN_TASK_DONE: <summary> (or \
                  MUQUN_TASK_BLOCKED: <reason>). If tasks.complete answers task_not_found, \
                  this session is not a Muqun task: do not call it again and print no marker.";
    if in_task {
        // First and imperative: a host may show this beside many other
        // instructions, and an agent that just stops leaves its task paused.
        format!(
            "IMPORTANT: Muqun started you to work on one task, given as your first \
             prompt, and the person who sent it sees it as unfinished until you report \
             back. As your last action for that task, after answering, call the muqun \
             tool tasks.complete -- {finish} {about}"
        )
    } else {
        format!(
            "{about} If this session was started as a Muqun task -- tasks.read without a \
             task_id tells you -- then when the whole task is finished, {finish} If \
             tasks.read says this session has no task, ignore the task protocol."
        )
    }
}

/// What `tasks.read` without a task_id answers a session that has no task.
const NO_TASK: &str = "This session has no Muqun task: ignore the task protocol -- do not call \
     tasks.complete and print no marker.";

/// Whether the caller of `/mcp` was started for a task: a pane-bound token
/// whose pane holds an open task. An agent-bound token is shared by every
/// session of its agent's service (task or not), so it never is: those
/// sessions get the conditional wording and ask `tasks.read`.
pub(crate) async fn started_for_task(state: &AppState, session: &AssistantSession) -> bool {
    match &session.binding {
        crate::mcp::tokens::TokenBinding::Pane {
            session_id,
            pane_id,
        } => state.tasks.open_task_in_pane(session_id, pane_id).await,
        crate::mcp::tokens::TokenBinding::Agent { .. } => false,
    }
}

/// `tasks.delegate` as an agent in a pane would call it, for other modules' tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::mcp::tokens::TokenBinding;

    /// The body `tasks.delegate(machine, goal)` answers: `Ok` unless it is an error.
    pub(crate) async fn delegate_as(
        state: &AppState,
        machine: &str,
        goal: &str,
    ) -> Result<Value, Value> {
        let session = AssistantSession {
            token_id: "tok-%1".into(),
            binding: TokenBinding::Pane {
                session_id: "default".into(),
                pane_id: "%1".into(),
            },
            label: "test".into(),
            expires_ms: None,
        };
        let args = DelegateArgs {
            goal: goal.into(),
            cwd: None,
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: Some(machine.into()),
            terminal: None,
        };
        let result = MuqunMcp::new(state.clone(), session)
            .tasks_delegate(Parameters(args))
            .await
            .unwrap();
        let result = serde_json::to_value(result).unwrap();
        let body = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        if result["isError"] == true {
            Err(body)
        } else {
            Ok(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};
    use crate::mcp::tokens::TokenBinding;
    use crate::test_support::{output_state, test_state, FakeAgent, FakeHerdr};

    const ASID: &str = "opencode_new";

    fn session(pane_id: &str) -> AssistantSession {
        AssistantSession {
            token_id: format!("tok-{pane_id}"),
            binding: TokenBinding::Pane {
                session_id: "default".into(),
                pane_id: pane_id.into(),
            },
            label: "test".into(),
            expires_ms: None,
        }
    }

    /// The one JSON text block of `result`, its body, and whether it is an error.
    fn answer(result: Result<CallToolResult, ErrorData>) -> (bool, String, Value) {
        let result = serde_json::to_value(result.unwrap()).unwrap();
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "{result}");
        let text = content[0]["text"].as_str().unwrap().to_owned();
        let body = serde_json::from_str(&text).unwrap();
        (result["isError"] == true, text, body)
    }

    fn assistant_text(seq: u64, text: String) -> TimelineItem {
        TimelineItem {
            id: format!("part-{seq}"),
            message_id: "m1".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text { text },
            seq,
            updated_ms: 0,
            ordinal: seq as _,
            attachments: None,
            image_assets: None,
        }
    }

    /// Attach an OpenCode fake whose new session exists, and delegate a task
    /// to it as `mcp`.
    async fn delegated(agent: FakeAgent, mcp: &MuqunMcp) -> String {
        mcp.state
            .agent_runtime
            .attach_for_test(agent.with_sessions(&[(ASID, 1)]).manager())
            .await;
        let args = DelegateArgs {
            goal: "fix the build".into(),
            cwd: None,
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: None,
            terminal: None,
        };
        let (error, _, body) = answer(mcp.tasks_delegate(Parameters(args)).await);
        assert!(!error, "{body}");
        body["task_id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn exec_run_returns_a_handle_after_timeout() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mcp = MuqunMcp::new(output_state(&herdr), session("%1"));
        let args = ExecRunArgs {
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "sleep 60".into(),
            timeout_s: Some(1),
        };
        let (error, _, body) = answer(mcp.exec_run(Parameters(args)).await);
        assert!(!error, "{body}");
        assert_eq!(body["state"], "working", "{body}");
        assert!(body["task_id"].as_str().unwrap().starts_with("t_"));
        assert_eq!(body["exit_code"], Value::Null);
        assert_eq!(
            body["pane"],
            json!({ "session_id": "default", "pane_id": "w1:p9" })
        );
    }

    #[tokio::test]
    async fn an_agent_delegates_to_a_terminal_agent_and_cancels_it() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        herdr.set_panes(json!([
            { "pane_id": "w1:p9", "workspace_id": "w1", "tab_id": "w1:t9" }
        ]));
        let mut state = output_state(&herdr);
        state
            .config
            .agent_commands
            .insert("claude".into(), "sh".into());
        let mcp = MuqunMcp::new(state, session("%1"));
        let args = DelegateArgs {
            goal: "review the diff".into(),
            cwd: None,
            agent_id: Some("claude".into()),
            mode: None,
            model: None,
            variant: None,
            machine: None,
            terminal: None,
        };
        let (error, _, task) = answer(mcp.tasks_delegate(Parameters(args)).await);
        assert!(!error, "{task}");
        assert_eq!(task["state"], "dispatching", "{task}");
        assert_eq!(task["binding"]["kind"], "pane");
        assert_eq!(task["binding"]["pane_id"], "w1:p9");
        assert_eq!(task["binding"]["agent_id"], "claude");

        let args = CancelArgs {
            task_id: task["task_id"].as_str().unwrap().into(),
        };
        let (error, _, cancelled) = answer(mcp.tasks_cancel(Parameters(args)).await);
        assert!(!error, "{cancelled}");
        assert_eq!(cancelled["state"], "cancelled");
        assert!(
            mcp.state
                .tasks
                .pending_notices(0, 10)
                .await
                .unwrap()
                .is_empty(),
            "the caller's own cancel is not told back to it"
        );
        let key = crate::terminal::shortcuts::interrupt_key(Some("claude"), None);
        assert!(
            herdr.calls.lock().unwrap().iter().any(|call| {
                call["method"] == "pane.send_keys"
                    && call["params"]["pane_id"] == "w1:p9"
                    && call["params"]["keys"][0] == key.as_str()
            }),
            "the pane agent was interrupted with {key}"
        );

        // A terminal session this computer does not have: refused, as an
        // argument error, and nothing is made.
        let args = DelegateArgs {
            goal: "review the diff".into(),
            cwd: None,
            agent_id: Some("claude".into()),
            mode: None,
            model: None,
            variant: None,
            machine: None,
            terminal: Some("nope".into()),
        };
        let (error, text, _) = answer(mcp.tasks_delegate(Parameters(args)).await);
        assert!(error, "{text}");
        assert!(text.contains("No terminal session 'nope'"), "{text}");
    }

    #[tokio::test]
    async fn the_timeline_of_a_command_is_an_invalid_argument() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mcp = MuqunMcp::new(output_state(&herdr), session("%1"));
        let args = ExecRunArgs {
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "sleep 60".into(),
            timeout_s: Some(1),
        };
        let (_, _, run) = answer(mcp.exec_run(Parameters(args)).await);
        let args = ReadArgs {
            task_id: Some(run["task_id"].as_str().unwrap().into()),
            what: What::Timeline,
            cursor: None,
        };
        let (error, _, body) = answer(mcp.tasks_read(Parameters(args)).await);
        assert!(error);
        assert_eq!(body["error"], "invalid_arguments", "{body}");
    }

    #[tokio::test]
    async fn tasks_read_of_someone_elses_task_says_not_found() {
        let owner = MuqunMcp::new(test_state("admin", Vec::new()), session("%1"));
        let id = delegated(FakeAgent::new("opencode"), &owner).await;
        let other = MuqunMcp::new(owner.state.clone(), session("%2"));
        let args = ReadArgs {
            task_id: Some(id),
            what: What::Summary,
            cursor: None,
        };
        let (error, _, body) = answer(other.tasks_read(Parameters(args)).await);
        assert!(error);
        assert_eq!(body["error"], "task_not_found", "{body}");
        assert!(body["message"].is_string());
    }

    #[tokio::test]
    async fn a_session_without_a_task_is_told_to_ignore_the_protocol() {
        // No task in this pane: tasks.read says so plainly, and
        // tasks.complete answers not found without asking for a marker.
        let mcp = MuqunMcp::new(test_state("admin", Vec::new()), session("%9"));
        let args = ReadArgs {
            task_id: None,
            what: What::Summary,
            cursor: None,
        };
        let (error, _, body) = answer(mcp.tasks_read(Parameters(args)).await);
        assert!(!error, "{body}");
        assert!(
            body["your_task"].is_null() && body["note"] == NO_TASK,
            "{body}"
        );
        let args = CompleteArgs {
            summary: "done".into(),
            outcome: Outcome::Done,
            task_id: None,
        };
        let (error, _, body) = answer(mcp.tasks_complete(Parameters(args)).await);
        assert!(error);
        assert_eq!(body["error"], "task_not_found", "{body}");
        assert!(!body["message"]
            .as_str()
            .unwrap()
            .contains("MUQUN_TASK_DONE:"));
    }

    /// A device's delegation whose OpenCode session is mid-`tasks.delegate`:
    /// what that call starts is filed under the device's task.
    #[tokio::test]
    async fn a_delegation_from_a_tracked_agent_session_names_its_parent() {
        let state = test_state("admin", Vec::new());
        let call = serde_json::from_value(json!({
            "id": "c1", "name": "muqun_tasks_delegate", "input": { "goal": "sub goal" },
            "state": "running", "status": "running"
        }))
        .unwrap();
        let mut item = assistant_text(1, String::new());
        item.part = AgentPart::Tool(call);
        let mut agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        agent.session_timelines.insert(ASID.into(), vec![item]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let agent_token = AssistantSession {
            token_id: "tok".into(),
            binding: TokenBinding::Agent {
                agent_id: "opencode".into(),
            },
            label: "opencode".into(),
            expires_ms: None,
        };
        let device = crate::tasks::domain::Requested {
            by: crate::tasks::domain::TaskPrincipal {
                kind: crate::tasks::domain::PrincipalKind::Device,
                id: "d1".into(),
            },
            origin: crate::tasks::domain::TaskOrigin::Device,
            parent: None,
            name: Some("phone".into()),
            server_id: None,
        };
        let goal = DelegateArgs {
            goal: "parent goal".into(),
            cwd: None,
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: None,
            terminal: None,
        }
        .request("srv-coding", "coding", &[])
        .unwrap();
        let parent = state.tasks.delegate(&state, device, goal).await.unwrap();

        let mcp = MuqunMcp::new(state, agent_token);
        let args = DelegateArgs {
            goal: "sub goal".into(),
            cwd: None,
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: None,
            terminal: None,
        };
        let (error, _, body) = answer(mcp.tasks_delegate(Parameters(args)).await);
        assert!(!error, "{body}");
        assert_eq!(body["parent_task_id"], parent.task_id, "{body}");
        assert_eq!(body["origin"], "agent");
        assert_eq!(body["requested_by"], Value::Null);
    }

    /// An agent learns the linked computers from fleet.describe and
    /// fleet.list, answered at once from what this gateway last heard (a
    /// peer not checked lately is `null` until a check in the background
    /// answers), and, from its own task, which of them the user named.
    #[tokio::test]
    async fn an_agent_sees_the_linked_computers_and_the_ones_the_user_named() {
        use crate::fleet::peers::Peer;
        use crate::tasks::domain::{PrincipalKind, Task, TaskKind, TaskOrigin, TaskState};

        let herdr = FakeHerdr::start(vec!["$ "], None);
        let state = output_state(&herdr);
        let now = crate::tasks::domain::now_ms();
        let manifest = json!({
            "machineId": "srv-mini", "label": "mini", "os": "macos", "suitableFor": ["ios-build"],
            "tasks": { "active": 2, "waiting": 0 },
            "agents": [{ "id": "opencode", "kind": "opencode", "status": "connected", "modes": [], "modelCount": 3 }],
        });
        // old answers a ping but has no manifest to give (an older gateway);
        // slow takes connections and never answers.
        let old = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_url = format!("http://{}", old.local_addr().unwrap());
        let router = axum::Router::new()
            .route(
                "/api/tasks",
                axum::routing::get(|| async { axum::Json(json!({ "tasks": [] })) }),
            )
            .route(
                "/api/capabilities",
                axum::routing::get(|| async { axum::Json(json!({ "capabilities": [] })) }),
            );
        tokio::spawn(async move { axum::serve(old, router).await });
        let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_url = format!("http://{}", slow.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = slow.accept().await {
                held.push(socket);
            }
        });
        // nas never answered: nothing is known of it yet; pinged, 127.0.0.1:9 refuses at once.
        let refused = "http://127.0.0.1:9".to_owned();
        for (id, name, seen, url) in [
            ("srv-mini", "mini", Some(now), &refused),
            ("srv-nas", "nas", None, &refused),
            ("srv-old", "old", None, &old_url),
            ("srv-slow", "slow", None, &slow_url),
        ] {
            state
                .peers
                .store_for_test(Peer {
                    peer_id: format!("peer-{name}"),
                    server_id: id.into(),
                    name: name.into(),
                    url: url.clone(),
                    token: "t".into(),
                    device_id: "d".into(),
                    transport_key: None,
                    added_ms: 0,
                    last_seen_ms: seen,
                    capabilities: vec!["fleet_peers".into()],
                    manifest: seen.map(|_| manifest.clone()),
                    unauthorized: false,
                })
                .await;
        }
        let mcp = MuqunMcp::new(state.clone(), session("%1"));

        let describe = |machine: Option<&str>| {
            let machine = machine.map(str::to_owned);
            let mcp = mcp.clone();
            async move {
                answer(
                    mcp.fleet_describe(Parameters(DescribeArgs { machine }))
                        .await,
                )
            }
        };
        let (error, _, body) = describe(None).await;
        assert!(!error, "{body}");
        let peers: Vec<_> = body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|peer| {
                (
                    peer["name"].clone(),
                    peer["reachable"].clone(),
                    peer["suitableFor"].clone(),
                )
            })
            .collect();
        assert_eq!(
            peers,
            [
                (json!("mini"), json!(true), json!(["ios-build"])),
                (json!("nas"), Value::Null, json!([])),
                (json!("old"), Value::Null, json!([])),
                (json!("slow"), Value::Null, json!([])),
            ]
        );
        assert_eq!(body["peers"][0]["agents"][0]["id"], "opencode");

        for (machine, expected) in [
            ("MINI", Ok("srv-mini")),
            ("srv-mini", Ok("srv-mini")),
            ("nas", Err("machine_unreachable")),
            ("printer", Err("machine_unknown")),
            ("old", Err("manifest_unavailable")),
        ] {
            let (error, _, body) = describe(Some(machine)).await;
            match expected {
                Ok(id) => assert_eq!(
                    (error, body["machineId"].as_str()),
                    (false, Some(id)),
                    "{machine}: {body}"
                ),
                Err(code) => assert_eq!(
                    (error, body["error"].as_str()),
                    (true, Some(code)),
                    "{machine}: {body}"
                ),
            }
        }

        // slow's check runs in the background (3 s): it holds up nothing.
        let started = std::time::Instant::now();
        let (_, _, listed) = answer(mcp.fleet_list().await);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let machines: Vec<_> = listed["machines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["label"].clone(),
                    m["online"].clone(),
                    m["tasksActive"].clone(),
                )
            })
            .collect();
        assert_eq!(
            machines[1..],
            [
                (json!("mini"), json!(true), json!(2)),
                (json!("nas"), json!(false), json!(0)),
                (json!("old"), json!(true), json!(0)),
                (json!("slow"), Value::Null, json!(0)),
            ]
        );

        // The phone opened a task naming mini; the agent in its pane reads it
        // without knowing its id. Another pane's agent finds no task of its own.
        let task = Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "fix the build".into(),
            cwd: None,
            binding: TaskBinding::Pane {
                session_id: "default".into(),
                pane_id: "%1".into(),
                tab_id: None,
                shell_pid: None,
                agent_id: Some("claude".into()),
                agent_job: None,
                model: None,
                variant: None,
            },
            state: TaskState::Working,
            summary: None,
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Device,
                id: "d1".into(),
            },
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Device,
            label: None,
            requester_name: Some("phone".into()),
            session_title: None,
            mentions: vec!["mini".into()],
            client_ref: None,
            requester_server_id: None,
        };
        let id = state.tasks.insert_for_test(&task).await;
        for (pane, task_id, expected) in [
            ("%1", None, Ok(json!(["mini"]))),
            ("%1", Some(id.0.clone()), Ok(json!(["mini"]))),
            ("%2", Some(id.0.clone()), Err("task_not_found")),
        ] {
            let mcp = MuqunMcp::new(state.clone(), session(pane));
            let args = ReadArgs {
                task_id: task_id.clone(),
                what: What::Summary,
                cursor: None,
            };
            let (error, _, body) = answer(mcp.tasks_read(Parameters(args)).await);
            match expected {
                // Without a task_id the answer is `{your_task, when_finished}`.
                Ok(mentions) => assert_eq!(
                    (
                        error,
                        match task_id {
                            None => &body["your_task"]["mentions"],
                            Some(_) => &body["mentions"],
                        }
                    ),
                    (false, &mentions),
                    "{pane} {task_id:?}: {body}"
                ),
                Err(code) => assert_eq!(
                    (error, body["error"].as_str()),
                    (true, Some(code)),
                    "{pane}: {body}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn output_is_clipped_at_16_kib() {
        let mcp = MuqunMcp::new(test_state("admin", Vec::new()), session("%1"));
        // 40 KiB of one assistant message, in 2 KiB parts.
        let timeline = (1..=20)
            .map(|seq| assistant_text(seq, format!("{seq}:{}", "x".repeat(2048))))
            .collect();
        let id = delegated(FakeAgent::new("opencode").with_timeline(timeline), &mcp).await;
        let args = ReadArgs {
            task_id: Some(id),
            what: What::Timeline,
            cursor: None,
        };
        let (error, text, body) = answer(mcp.tasks_read(Parameters(args)).await);
        assert!(!error, "{body}");
        assert_eq!(body["truncated"], true);
        assert!(text.len() <= MAX_RESULT_BYTES, "{} bytes", text.len());
        assert_eq!(body["cursor"], 20);
        // The newest items are the ones kept.
        let items = body["items"].as_array().unwrap();
        assert!(items.len() < 20);
        assert!(items.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with("…"));
    }

    /// A manifest with thousands of models (OpenCode's catalog) still answers.
    #[tokio::test]
    async fn fleet_describe_answers_under_16_kib_with_a_3000_model_catalog() {
        let mut agent = FakeAgent::new("opencode");
        agent.catalog.models = (0..3000)
            .map(|i| {
                let model = json!({ "id": format!("model-{i}"), "name": format!("Model {i}"), "provider_id": "p" });
                serde_json::from_value(model).unwrap()
            })
            .collect();
        agent.catalog.modes = ["build", "plan"]
            .iter()
            .map(|id| serde_json::from_value(json!({ "id": id, "name": id })).unwrap())
            .collect();
        let mut state = test_state("admin", Vec::new());
        state.agent_runtime = crate::agents::AgentRuntime::with_configs(
            crate::agents::runtime::OpencodeConfig {
                enabled: true,
                autostart: false,
                binary: None,
            },
            Default::default(),
            Default::default(),
            None,
        );
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let mcp = MuqunMcp::new(state, session("%1"));
        let full = mcp.state.fleet.manifest(&mcp.state).await;
        let models: usize = full.agents.iter().map(|a| a.models.len()).sum();
        assert_eq!(models, 3000, "the fake catalog reaches the manifest");
        assert!(json!(full).to_string().len() > MAX_RESULT_BYTES);

        let args = DescribeArgs { machine: None };
        let (error, text, body) = answer(mcp.fleet_describe(Parameters(args)).await);
        assert!(!error, "{body}");
        assert!(text.len() < MAX_RESULT_BYTES, "{} bytes", text.len());
        let opencode = body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == "opencode")
            .unwrap();
        assert_eq!(opencode["modelCount"], 3000, "{opencode}");
        assert_eq!(opencode["modes"], json!(["build", "plan"]));
        assert!(opencode.get("models").is_none());
    }

    #[test]
    fn waits_and_delegations_are_checked_before_anything_runs() {
        for (asked, slice) in [(None, 30), (Some(600), 45), (Some(0), 1)] {
            assert_eq!(wait_slice(asked), Duration::from_secs(slice), "{asked:?}");
        }
        let long = "v".repeat(257);
        // (arguments, the variant the request carries; `None` when refused)
        for (args, variant) in [
            (
                json!({ "goal": "g", "model": "openai/gpt-6", "variant": "high", "later": true }),
                Some(Some("high")),
            ),
            (json!({ "goal": "g" }), Some(None)),
            (json!({ "goal": "g", "model": "m", "variant": "" }), None),
            (json!({ "goal": "g", "model": "m", "variant": "  " }), None),
            (json!({ "goal": "g", "model": "m", "variant": long }), None),
            (json!({ "goal": "g", "agent_id": "" }), None),
            (json!({ "goal": "g", "mode": long }), None),
            // A directory is checked on the computer that runs the task.
            (json!({ "goal": "g", "cwd": "~/src" }), None),
            (
                json!({ "goal": "g", "cwd": "~/src", "machine": "Coding" }),
                None,
            ),
            (
                json!({ "goal": "g", "cwd": "~/src", "machine": "mini" }),
                Some(None),
            ),
        ] {
            let parsed: DelegateArgs = serde_json::from_value(args.clone()).unwrap();
            let request = parsed.request("srv-coding", "coding", &[]).ok();
            assert_eq!(
                request.map(|request| request.variant),
                variant.map(|variant| variant.map(str::to_owned)),
                "{args}"
            );
        }
    }

    #[test]
    fn a_large_array_loses_its_tail_before_strings_are_clipped() {
        let ids: Vec<String> = (0..3000).map(|i| format!("model-{i}")).collect();
        let (error, text, body) = answer(Ok(ok(json!({ "note": "keep me", "ids": ids }))));
        assert!(!error);
        assert!(text.len() <= MAX_RESULT_BYTES);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["note"], "keep me");
        let kept = body["ids"].as_array().unwrap();
        assert!(kept.len() > 1000 && kept.len() < 3000, "{}", kept.len());
        assert_eq!(kept[0], "model-0");
    }

    #[test]
    fn a_long_string_keeps_its_end_and_is_marked() {
        let long = json!({ "stdout_tail": format!("{}end", "y".repeat(40_000)) });
        let (error, text, body) = answer(Ok(ok(long)));
        assert!(!error);
        assert!(text.len() <= MAX_RESULT_BYTES);
        assert_eq!(body["truncated"], true);
        assert!(body["stdout_tail"].as_str().unwrap().ends_with("yend"));
        let (_, _, small) = answer(Ok(ok(json!({ "a": 1 }))));
        assert_eq!(small, json!({ "a": 1 }));
    }

    #[test]
    fn tool_names_keep_their_dots_in_tools_list() {
        let mut names: Vec<String> = MuqunMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "exec.run",
                "fleet.describe",
                "fleet.list",
                "tasks.cancel",
                "tasks.complete",
                "tasks.delegate",
                "tasks.read",
                "tasks.wait"
            ]
        );
    }

    /// Another session under the same agent token cancelling a child is not
    /// its parent: the parent session is still told.
    #[tokio::test]
    async fn a_cancel_from_another_session_is_still_told_to_the_parent() {
        use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};
        use crate::tasks::domain::{NoticeTarget, Task, TaskKind, TaskOrigin, TaskState};
        let state = test_state("admin", Vec::new());
        let child = Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "g".into(),
            cwd: None,
            binding: TaskBinding::None,
            state: TaskState::Working,
            summary: None,
            exit_code: None,
            created_by: crate::tasks::domain::TaskPrincipal {
                kind: crate::tasks::domain::PrincipalKind::Assistant,
                id: TokenBinding::Agent {
                    agent_id: "opencode".into(),
                }
                .owner_key(),
            },
            created_ms: crate::tasks::domain::now_ms(),
            updated_ms: crate::tasks::domain::now_ms(),
            seq: 0,
            parent: None,
            origin: TaskOrigin::Agent,
            label: None,
            requester_name: None,
            session_title: None,
            mentions: Vec::new(),
            client_ref: None,
            requester_server_id: None,
        };
        let id = state.tasks.insert_for_test(&child).await;
        let running: crate::agents::domain::ToolCall = serde_json::from_value(serde_json::json!({
            "id": "c1", "name": "muqun_tasks_cancel", "input": { "task_id": id.0 },
            "state": "running", "status": "running"
        }))
        .unwrap();
        let mut agent =
            FakeAgent::new("opencode").with_sessions(&[("ses_parent", 1), ("ses_other", 1)]);
        agent.session_timelines.insert(
            "ses_other".into(),
            vec![TimelineItem {
                id: "p1".into(),
                message_id: "m1".into(),
                role: TimelineRole::Assistant,
                part: AgentPart::Tool(running),
                seq: 1,
                updated_ms: 0,
                ordinal: 0,
                attachments: None,
                image_assets: None,
            }],
        );
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let busy = crate::agents::domain::AgentDomainEvent::StatusChanged {
            asid: crate::agents::domain::AgentSessionId("ses_other".into()),
            status: crate::agents::domain::AgentSessionStatus::Busy,
            error: None,
            seq: 1,
        };
        state.tasks.follow(&state, &busy).await;
        let parent = NoticeTarget::Session {
            agent_id: "opencode".into(),
            asid: "ses_parent".into(),
        };
        state.tasks.notify(&id, &parent).await;
        let session = AssistantSession {
            token_id: "tok".into(),
            binding: TokenBinding::Agent {
                agent_id: "opencode".into(),
            },
            label: "opencode".into(),
            expires_ms: None,
        };
        let mcp = MuqunMcp::new(state.clone(), session);
        let (error, _, cancelled) = answer(
            mcp.tasks_cancel(Parameters(CancelArgs {
                task_id: id.0.clone(),
            }))
            .await,
        );
        assert!(!error, "{cancelled}");
        assert_eq!(
            state.tasks.pending_notices(0, 10).await.unwrap().len(),
            1,
            "the parent session is still told"
        );
    }
}
