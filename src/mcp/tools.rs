//! The MCP tool surface: thin adapters over `FleetService` and `TaskService`.
//! Every result is one JSON text block of at most [`MAX_RESULT_BYTES`];
//! long work returns a handle that `tasks.wait` polls.

use std::path::{Path, PathBuf};
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
use crate::fleet::manifest::FleetService;
use crate::tasks::domain::{clip, TaskBinding, TaskId, TaskPrincipal};
use crate::tasks::run_command::{RunHandle, RunRequest};
use crate::tasks::service::{DelegateRequest, ReadWhat, TaskError};
use crate::AppState;

pub(crate) const MAX_RESULT_BYTES: usize = 16 * 1024;
pub(crate) const MAX_WAIT_S: u64 = 45;
pub(crate) const DEFAULT_WAIT_S: u64 = 30;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_GOAL_BYTES: usize = 32 * 1024;
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
        TaskError::UnknownMachine(_) => fail(
            "machine_unknown",
            "Unknown machine. Only this computer is available.",
        ),
        TaskError::AgentUnavailable(_) => fail(
            "agent_unavailable",
            "That agent is not available on this computer.",
        ),
        TaskError::UnknownModel(_) => invalid(
            "Unknown model for that agent. Use a model id from its catalog, or <provider_id>/<model_id>.",
        ),
        TaskError::UnknownVariant(_) => invalid(
            "variant is not one of that model's variants.",
        ),
        TaskError::NoTimeline => invalid("Only a delegated task has a timeline."),
        TaskError::NoTerminal => fail(
            "no_terminal",
            "No terminal session is reachable on this computer.",
        ),
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
    let not_a_directory = || invalid("cwd must be an existing absolute directory");
    if !Path::new(cwd).is_absolute() {
        return Err(not_a_directory());
    }
    let canonical = Path::new(cwd)
        .canonicalize()
        .map_err(|_| not_a_directory())?;
    canonical
        .is_dir()
        .then_some(canonical)
        .ok_or_else(not_a_directory)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct DescribeArgs {
    /// The computer to describe: "self" (default) or its machineId.
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
    /// Absolute project directory on this computer.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Structured agent to use, e.g. "opencode". Default: the primary agent.
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
    /// The computer to run on: "self" (default) or its machineId.
    #[serde(default)]
    pub machine: Option<String>,
}

impl DelegateArgs {
    fn request(self) -> Result<DelegateRequest, CallToolResult> {
        if self.goal.trim().is_empty() || self.goal.len() > MAX_GOAL_BYTES {
            return Err(invalid("goal must be non-empty and at most 32 KiB"));
        }
        let cwd = match self.cwd {
            Some(cwd) => Some(directory(&cwd)?.to_string_lossy().into_owned()),
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
    /// A task_id you created.
    pub task_id: String,
    /// "summary": the task as it stands. "timeline": its recent agent items.
    pub what: What,
    /// Timeline only: items after this cursor (from a previous read).
    #[serde(default)]
    pub cursor: Option<u64>,
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

#[derive(Clone)]
pub(crate) struct MuqunMcp {
    state: AppState,
    /// Who is calling; work is attributed to it.
    session: AssistantSession,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl MuqunMcp {
    pub(crate) fn new(state: AppState, session: AssistantSession) -> Self {
        Self {
            state,
            session,
            tool_router: Self::tool_router(),
        }
    }

    fn by(&self) -> TaskPrincipal {
        self.session.principal()
    }

    #[tool(
        name = "fleet.list",
        description = "List the computers this gateway knows (this one only for now): alias, OS, what each is suitable for, how busy it is."
    )]
    async fn fleet_list(&self) -> Result<CallToolResult, ErrorData> {
        let manifest = self.state.fleet.manifest(&self.state).await;
        Ok(ok(
            json!({ "machines": [FleetService::summary(&manifest)] }),
        ))
    }

    #[tool(
        name = "fleet.describe",
        description = "What a computer offers: its agents (status, version, modes, model count), terminal backends, toolchains with versions, task counts and what it is suitable for."
    )]
    async fn fleet_describe(
        &self,
        Parameters(args): Parameters<DescribeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let fleet = &self.state.fleet;
        let manifest = fleet.describe(&self.state, args.machine.as_deref()).await;
        let unknown = TaskError::UnknownMachine(args.machine.unwrap_or_default());
        reply(manifest.ok_or(unknown))
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
        description = "Start a sub-task on an agent on this computer: a new session in cwd with your goal as the first prompt. Returns task_id; poll with tasks.wait."
    )]
    async fn tasks_delegate(
        &self,
        Parameters(args): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let goal = args.goal.clone();
        let request = match args.request() {
            Ok(request) => request,
            Err(refused) => return Ok(refused),
        };
        let call = lineage::Call {
            tool: "tasks.delegate",
            args: vec![("goal", &goal)],
        };
        let by = lineage::requested(&self.state, &self.session, &call).await;
        reply(self.state.tasks.delegate(&self.state, by, request).await)
    }

    #[tool(
        name = "tasks.wait",
        description = "Wait up to timeout_s (max 45) for a task to reach done/failed/waiting. Returns the current state and summary; call again while it says working."
    )]
    async fn tasks_wait(
        &self,
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (id, slice) = (TaskId(args.task_id), wait_slice(args.timeout_s));
        let tasks = &self.state.tasks;
        reply(tasks.wait(&self.state, &self.by(), &id, slice).await)
    }

    #[tool(
        name = "tasks.read",
        description = "Read a task you created: its summary or recent timeline."
    )]
    async fn tasks_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let what = args.what();
        let id = TaskId(args.task_id);
        let tasks = &self.state.tasks;
        reply(
            tasks
                .read(&self.state, &self.by(), &id, what, args.cursor)
                .await,
        )
    }

    #[tool(name = "tasks.cancel", description = "Interrupt a task you created.")]
    async fn tasks_cancel(
        &self,
        Parameters(args): Parameters<CancelArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = TaskId(args.task_id);
        reply(self.state.tasks.cancel(&self.state, &self.by(), &id).await)
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
            .with_instructions(
                "Muqun Gateway on this computer. fleet.describe says what it has, exec.run \
                 runs short commands, tasks.delegate + tasks.wait hand work to a coding agent. \
                 Waits are capped at 45 s; call tasks.wait again while it says working.",
            )
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
            task_id: run["task_id"].as_str().unwrap().into(),
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
            task_id: id,
            what: What::Summary,
            cursor: None,
        };
        let (error, _, body) = answer(other.tasks_read(Parameters(args)).await);
        assert!(error);
        assert_eq!(body["error"], "task_not_found", "{body}");
        assert!(body["message"].is_string());
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
        };
        let goal = DelegateArgs {
            goal: "parent goal".into(),
            cwd: None,
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: None,
        }
        .request()
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
        };
        let (error, _, body) = answer(mcp.tasks_delegate(Parameters(args)).await);
        assert!(!error, "{body}");
        assert_eq!(body["parent_task_id"], parent.task_id, "{body}");
        assert_eq!(body["origin"], "agent");
        assert_eq!(body["requested_by"], Value::Null);
    }

    #[tokio::test]
    async fn fleet_describe_of_mac_is_machine_unknown() {
        let mcp = MuqunMcp::new(test_state("admin", Vec::new()), session("%1"));
        let args = DescribeArgs {
            machine: Some("mac".into()),
        };
        let (error, _, body) = answer(mcp.fleet_describe(Parameters(args)).await);
        assert!(error);
        assert_eq!(body["error"], "machine_unknown", "{body}");
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
            task_id: id,
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
    fn waits_are_capped_at_45_seconds() {
        assert_eq!(wait_slice(None), Duration::from_secs(30));
        assert_eq!(wait_slice(Some(600)), Duration::from_secs(45));
        assert_eq!(wait_slice(Some(0)), Duration::from_secs(1));
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
                "tasks.delegate",
                "tasks.read",
                "tasks.wait"
            ]
        );
    }

    #[test]
    fn delegate_args_carry_an_optional_variant_and_tolerate_unknown_fields() {
        let args: DelegateArgs = serde_json::from_value(json!({
            "goal": "g", "model": "openai/gpt-6", "variant": "high", "later": true
        }))
        .unwrap();
        let request = args.request().unwrap();
        assert_eq!(request.variant.as_deref(), Some("high"));
        let none: DelegateArgs = serde_json::from_value(json!({ "goal": "g" })).unwrap();
        assert_eq!(none.request().unwrap().variant, None);
    }
}
