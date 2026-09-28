//! Multiplexer session agent HTTP routes: task orchestration, agent dispatch,
//! prompt delivery, approval responses, and agent event history.

use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::agent_events;
use super::approvals;
use super::tasks;
use crate::{
    api_error, approval_data, backend_agent_list, backend_api_error, content_envelope,
    find_session, interrupt_pane, is_scannable_root, pane_agent_and_root,
    platform::i18n,
    read_pane_approval, read_pane_visible_text, require_device, send_pane_keys,
    session_asset_roots,
    terminal::{backend, native, shortcuts},
    terminal_backend, validate_agent_args, validate_text, ApiResult, AppState, BackendCreateTab,
    BackendCreateWorkspace, BackendError, BackendPaneId, BackendSplitDirection, BackendSplitPane,
    BackendStartAgent, BackendWorkspaceId, BackendWorktreeRequest, Pane, SessionConfig,
    MAX_WORKSPACE_LABEL_CHARS,
};

/// How long a freshly spawned agent is given to become one Herdr can name, and
/// how often the first prompt is offered to it meanwhile. Twelve attempts at
/// three quarters of a second is nine seconds -- longer than any of the four
/// agents takes to draw its first prompt on this hardware, and short enough
/// that a spawn still answers inside a phone's patience.
const SPAWN_PROMPT_ATTEMPTS: u32 = 12;
const SPAWN_PROMPT_INTERVAL: Duration = Duration::from_millis(750);
/// Long enough for an agent TUI to finish handling a pasted prompt, short
/// enough that the submit still feels like part of the same action.
const SUBMIT_KEYPRESS_DELAY: Duration = Duration::from_millis(150);
/// How often the pane is re-read while waiting for it to stop redrawing. A
/// prompt that names an image file makes Claude Code read and encode that file
/// and then rewrite its own input line into `[Image #N]`, and an Enter that
/// lands inside that window is swallowed outright. The window was measured at
/// under a second for one small image and at over three seconds for three large
/// ones off a cold page cache, so no fixed delay can cover it. A still screen is
/// only ever a hint about when to try: the staging arrives in bursts with quiet
/// gaps longer than this interval, so a pane can look still and not be.
const SUBMIT_SETTLE_INTERVAL: Duration = Duration::from_millis(150);
/// A beat between an Enter that went nowhere and the next one, so a burst of
/// staging work has room to finish instead of being sampled at the poll rate.
const SUBMIT_RETRY_INTERVAL: Duration = Duration::from_millis(450);
/// How long Herdr is watched for the agent to react to an Enter before that
/// Enter is written off. A real submission showed up in about half a second on
/// a live pane.
const SUBMIT_VERIFY_WINDOW: Duration = Duration::from_millis(700);
/// How often the agent's state is asked for inside that window.
const SUBMIT_VERIFY_INTERVAL: Duration = Duration::from_millis(150);
/// How long such a pane is given to react before it is read back.
const SUBMIT_VERIFY_DELAY: Duration = Duration::from_millis(300);
/// Wall-clock ceiling on the whole settle-send-verify sequence, so a pane that
/// never stops redrawing cannot leave a background task polling Herdr forever.
const SUBMIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How many Enters one prompt is worth when Herdr can say whether the agent
/// took the last one. Every attempt after the first is fired at a pane that has
/// demonstrably not accepted its predecessor, so none of them can be the stray
/// keystroke that answers a permission menu, and the budget can afford to cover
/// a slow staging window.
pub(crate) const SUBMIT_MAX_ATTEMPTS: u32 = 6;
/// The same budget for a pane Herdr lists no agent for. There the only evidence
/// is that the screen moved, and the screen moves on its own, so a submit that
/// cannot be checked properly keeps pressing Enter as few times as possible.
pub(crate) const SUBMIT_BLIND_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTaskBody {
    /// Absolute path of the repo to work in. Has to be one this session already
    /// has open, or inside one.
    pub(crate) repo_path: String,
    /// Branch to work on. Present means "give this task its own checkout";
    /// absent means "work in the repo as it is".
    pub(crate) branch_name: Option<String>,
    /// Herdr agent kind, as listed by `GET /api/agents/catalog`.
    pub(crate) agent: String,
    /// First thing to say to the agent, once it is up and interactive.
    pub(crate) prompt: Option<String>,
    pub(crate) workspace_label: Option<String>,
    /// Extra arguments for the agent's own command line.
    pub(crate) agent_args: Option<Vec<String>>,
    /// How long to wait for the agent to become interactive.
    pub(crate) startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SpawnBody {
    /// A Herdr agent kind, or a profile `agents.json` names.
    pub(crate) agent: String,
    /// Where the agent runs. Held to the same fence as a task's `repo_path`:
    /// a directory this session already works in, and nothing else. Absent
    /// means wherever Herdr puts a new tab.
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Put the agent beside what is already in this tab instead of in a tab of
    /// its own.
    #[serde(default)]
    pub(crate) tab_id: Option<String>,
    /// Typed and submitted once the agent is up.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AgentSendBody {
    pub(crate) text: String,
}

/// How a client answers a pending approval: by option number, or by what the
/// answer means. `fingerprint` is optimistic concurrency -- send back the one
/// the approval was read with and a menu that changed underneath rejects the
/// answer instead of taking it.
#[derive(Deserialize)]
pub(crate) struct AnswerApprovalBody {
    #[serde(default)]
    pub(crate) option: Option<u32>,
    #[serde(default)]
    pub(crate) decision: Option<String>,
    #[serde(default)]
    pub(crate) fingerprint: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentEventsQuery {
    /// The highest `seq` the client already has. Absent means "everything you
    /// still hold", which is what a phone opening a server cold asks for.
    #[serde(default)]
    pub(crate) since: Option<u64>,
}

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/agents/catalog", get(agents_catalog))
        .route(
            "/api/sessions/{session_id}/agent-events",
            get(session_agent_events),
        )
        .route("/api/sessions/{session_id}/tasks", post(create_task))
        .route("/api/sessions/{session_id}/spawn", post(spawn_agent))
        .route("/api/sessions/{session_id}/agents/spawn", post(spawn_agent))
        .route(
            "/api/sessions/{session_id}/agents/{pane_id}/interrupt",
            post(interrupt_pane),
        )
        .route("/api/sessions/{session_id}/agents", get(agents))
        .route("/api/sessions/{session_id}/agents/{target}", get(agent))
        .route(
            "/api/sessions/{session_id}/agents/{target}/focus",
            post(focus_agent),
        )
        .route(
            "/api/sessions/{session_id}/agents/{target}/send",
            post(send_agent),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/approval",
            get(pane_approval).post(answer_pane_approval),
        )
}

/// What the phone can start a task with: every kind the gateway knows, and
/// whether its executable is actually on this machine's `PATH`.
///
/// Not session-scoped: which binaries are installed is a property of the host,
/// not of a Herdr session, and the picker is drawn before a session is chosen.
pub(crate) async fn agents_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let agents = tasks::agent_catalog(&state.config.agent_commands);
    Ok(Json(json!({
        "agents": agents,
        "default_startup_timeout_ms": tasks::DEFAULT_AGENT_START_TIMEOUT_MS
    })))
}

/// What the agents in this session did recently, oldest first.
///
/// For the app's "while you were away" digest: a phone that was off the network
/// for an hour has, at best, a stack of notifications it cannot order and, at
/// worst, none at all. This answers with the transitions themselves, so the
/// digest is built from what happened rather than from what was delivered.
///
/// Memory only and bounded, so `missed` is a real answer rather than an
/// embarrassment: it says the ring rolled past the caller's `since`, and a
/// client that knows its digest is partial can say so instead of implying a
/// complete account.
pub(crate) async fn session_agent_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session_id)?;
    let log = state.agent_events.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent_events_lock_failed",
            "failed to read recent agent activity",
        )
    })?;
    let events = log.since(&session_id, query.since);
    Ok(Json(json!({
        "session_id": session_id,
        "since": query.since,
        "events": events.iter().map(agent_events::AgentEvent::to_json).collect::<Vec<_>>(),
        // What to send as `since` next time, whether or not this answer was
        // empty, so a client polling an idle session does not walk backwards.
        "next_since": log.latest_seq(&session_id),
        "missed": log.missed(&session_id, query.since),
        "capacity": agent_events::RING_CAPACITY,
    })))
}

/// Start a new piece of work: optionally a fresh checkout, a workspace to hold
/// it, an agent running in it, and the first prompt already typed.
///
/// # Why the answer can be a 207
///
/// This is four things happening in a row on someone else's machine, requested
/// from a phone. "It failed" tells the user nothing about whether they now have
/// a checkout, and the wrong guess makes them either abandon real work or
/// create a second copy of it. So every step is recorded, and a run that got
/// part of the way answers 207 with the same body a full success would have,
/// plus the failed step. Nothing was created at all is still a plain error.
///
/// # What gets rolled back, and what does not
///
/// Only a checkout this request created, and only while it is still useless --
/// that is, when the workspace to work in could not be made. Once there is a
/// pane sitting in the new checkout, the user has something they can use, and
/// deleting a fresh branch because `claude` happened not to be installed would
/// destroy more than it tidies. A checkout that was already there is never
/// touched.
pub(crate) async fn create_task(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateTaskBody>,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    if !tasks::is_known_agent_kind(&body.agent, &state.config.agent_commands) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
    }
    if let Some(prompt) = body.prompt.as_deref() {
        validate_text(prompt)?;
    }
    if let Some(args) = body.agent_args.as_deref() {
        validate_agent_args(args)?;
    }
    if let Some(label) = body.workspace_label.as_deref() {
        if label.chars().count() > MAX_WORKSPACE_LABEL_CHARS || label.chars().any(char::is_control)
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_label",
                "workspace_label must be at most 120 printable characters",
            ));
        }
    }
    let timeout_ms = match body.startup_timeout_ms {
        None => tasks::DEFAULT_AGENT_START_TIMEOUT_MS,
        Some(value)
            if (tasks::MIN_AGENT_START_TIMEOUT_MS..=tasks::MAX_AGENT_START_TIMEOUT_MS)
                .contains(&value) =>
        {
            value
        }
        Some(_) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_timeout",
                "startup_timeout_ms must be between 3001 and 300000",
            ))
        }
    };
    if let Some(branch) = body.branch_name.as_deref() {
        tasks::validate_branch_name(branch).map_err(|err| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_branch_name",
                &err.message(i18n::current()),
            )
        })?;
    }

    // The fence. A path the session does not already have is not a path the
    // phone gets to run git in, whatever it claims about itself.
    let roots = task_repo_roots(&state, &session).await;
    let repo_path = tasks::resolve_repo_path(&body.repo_path, &roots).ok_or_else(|| {
        api_error(
            StatusCode::FORBIDDEN,
            "repo_not_allowed",
            "repo_path must be a directory inside a workspace this session has open",
        )
    })?;
    if body.branch_name.is_some() && !tasks::is_git_checkout(&repo_path) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "not_a_git_checkout",
            "repo_path is not a git checkout, so a branch cannot be made in it",
        ));
    }

    let mut steps = tasks::StepLog::new();
    let label = body
        .workspace_label
        .clone()
        .or_else(|| body.branch_name.clone());

    let place = match body.branch_name.as_deref() {
        Some(branch) => {
            match prepare_worktree(&session, &repo_path, branch, label.as_deref(), &mut steps).await
            {
                Ok(place) => place,
                Err(err) => return Ok(task_failure(err, &steps)),
            }
        }
        None => match prepare_workspace(&session, &repo_path, label.as_deref(), &mut steps).await {
            Ok(place) => place,
            Err(err) => return Ok(task_failure(err, &steps)),
        },
    };

    // From here on the user has somewhere to work, so nothing else is fatal and
    // nothing else is rolled back.
    let mut payload = json!({
        "workspace_id": place.workspace_id,
        "pane_id": place.pane_id,
        "worktree_path": place.worktree_path,
        "branch": body.branch_name,
        "agent": body.agent,
        "reused_worktree": place.reused,
        "agent_started": false,
        "prompt_submitted": false
    });

    let mut prompt_target = place.pane_id.clone();
    let agent_command = tasks::agent_command(&body.agent, &state.config.agent_commands);
    match start_backend_agent(
        &session,
        &place.pane_id,
        &body.agent,
        &agent_command,
        body.agent_args.as_deref().unwrap_or_default(),
        timeout_ms,
    )
    .await
    {
        Ok(value) => {
            steps.ok(
                "agent",
                json!({
                    "kind": body.agent,
                    "pane_id": place.pane_id,
                    "argv": value.pointer("/result/argv").cloned()
                }),
            );
            payload["agent_started"] = json!(true);
            if let Some(target) = value.pointer("/result/target").and_then(Value::as_str) {
                prompt_target = target.to_owned();
            }
            payload["agent_instance_id"] = value
                .pointer("/result/instance_id")
                .cloned()
                .unwrap_or(Value::Null);
        }
        Err(err) => {
            steps.failed("agent", err.code(), &err.message());
            return Ok(task_partial(payload, &steps));
        }
    }

    match body.prompt.as_deref() {
        None => steps.skipped("prompt", "no prompt was given"),
        Some(prompt) => {
            // An agent that has just been launched is not a named agent yet --
            // it is a program still deciding what it is, and Herdr only learns
            // its name once it has drawn its first prompt. Waiting for that is
            // the difference between a task that carries its instruction and
            // one that lands on an empty prompt: the first attempt used to
            // arrive before the agent existed and the whole spawn was reported
            // as a failure, though the pane and the agent were both up.
            let mut submitted = Err(HerdrCallError::Unavailable(
                "the agent never became ready".to_owned(),
            ));
            for attempt in 0..SPAWN_PROMPT_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(SPAWN_PROMPT_INTERVAL).await;
                }
                submitted = submit_agent_prompt(&session, &prompt_target, prompt).await;
                if !submitted.as_ref().is_err_and(|err| err.can_retry_prompt()) {
                    break;
                }
            }
            match submitted {
                Ok(_) => {
                    schedule_submit_keypress(session.clone(), place.pane_id.clone());
                    steps.ok("prompt", json!({ "bytes": prompt.len() }));
                    payload["prompt_submitted"] = json!(true);
                }
                Err(err) => steps.failed("prompt", err.code(), &err.message()),
            }
        }
    }

    Ok(task_partial(payload, &steps))
}

/// Start an agent, from the phone, without describing a repository.
///
/// Task dispatch is the heavyweight door: it takes a repo, cuts a branch, makes
/// a checkout, and is the right thing when the work is new. This is the other
/// one -- "run codex here" -- which is what someone reaching for their phone in
/// a queue actually wants, and which used to take three calls and a knowledge
/// of which pane to split.
///
/// # What is checked before anything is created
///
/// The agent has to be one this gateway offers, and `cwd` has to be a directory
/// the session already works in -- the same fence `repo_path` is under, for the
/// same reason: a phone does not get to name a directory on the host and have
/// something run in it.
///
/// # Why the answer can be a 207
///
/// The same reason task dispatch's can. Once the pane exists the user has
/// somewhere to type, and "the agent did not come up" must not read as "nothing
/// happened" -- they would spawn a second one.
pub(crate) async fn spawn_agent(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SpawnBody>,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    if !tasks::is_known_agent_kind(&body.agent, &state.config.agent_commands)
        && !shortcuts::is_known_agent(&body.agent)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "unknown_agent",
            "agent is not one this gateway offers; see GET /api/agents/catalog",
        ));
    }
    if let Some(prompt) = body.prompt.as_deref() {
        validate_text(prompt)?;
    }

    let cwd = match body.cwd.as_deref() {
        None => None,
        Some(raw) => {
            let roots = task_repo_roots(&state, &session).await;
            let path = tasks::resolve_repo_path(raw, &roots).ok_or_else(|| {
                api_error(
                    StatusCode::FORBIDDEN,
                    "cwd_not_allowed",
                    "cwd must be a directory inside a workspace this session has open",
                )
            })?;
            Some(path.to_string_lossy().into_owned())
        }
    };

    let mut steps = tasks::StepLog::new();
    let place = spawn_place(&session, body.tab_id.as_deref(), cwd.as_deref(), &mut steps).await?;

    let mut payload = json!({
        "session_id": session_id,
        "pane_id": place.pane_id,
        "tab_id": place.tab_id,
        "agent": body.agent,
        "cwd": cwd,
        "agent_started": false,
        "prompt_submitted": false,
    });

    let mut prompt_target = place.pane_id.clone();
    let agent_command = tasks::agent_command(&body.agent, &state.config.agent_commands);
    match start_backend_agent(
        &session,
        &place.pane_id,
        &body.agent,
        &agent_command,
        &[],
        tasks::DEFAULT_AGENT_START_TIMEOUT_MS,
    )
    .await
    {
        Ok(value) => {
            steps.ok(
                "agent",
                json!({
                    "kind": body.agent,
                    "pane_id": place.pane_id,
                    "argv": value.pointer("/result/argv").cloned()
                }),
            );
            payload["agent_started"] = json!(true);
            if let Some(target) = value.pointer("/result/target").and_then(Value::as_str) {
                prompt_target = target.to_owned();
            }
            payload["agent_instance_id"] = value
                .pointer("/result/instance_id")
                .cloned()
                .unwrap_or(Value::Null);
        }
        Err(err) => {
            steps.failed("agent", err.code(), &err.message());
            // The pane is real and the user can type in it, so this is a 207
            // rather than an error that implies nothing was created.
            return Ok(task_partial(payload, &steps));
        }
    }

    match body.prompt.as_deref() {
        None => steps.skipped("prompt", "no prompt was given"),
        Some(prompt) => {
            // An agent that has just been launched is not a named agent yet --
            // it is a program still deciding what it is, and Herdr only learns
            // its name once it has drawn its first prompt. Waiting for that is
            // the difference between a task that carries its instruction and
            // one that lands on an empty prompt: the first attempt used to
            // arrive before the agent existed and the whole spawn was reported
            // as a failure, though the pane and the agent were both up.
            let mut submitted = Err(HerdrCallError::Unavailable(
                "the agent never became ready".to_owned(),
            ));
            for attempt in 0..SPAWN_PROMPT_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(SPAWN_PROMPT_INTERVAL).await;
                }
                submitted = submit_agent_prompt(&session, &prompt_target, prompt).await;
                if !submitted.as_ref().is_err_and(|err| err.can_retry_prompt()) {
                    break;
                }
            }
            match submitted {
                Ok(_) => {
                    schedule_submit_keypress(session.clone(), place.pane_id.clone());
                    steps.ok("prompt", json!({ "bytes": prompt.len() }));
                    payload["prompt_submitted"] = json!(true);
                }
                Err(err) => steps.failed("prompt", err.code(), &err.message()),
            }
        }
    }

    Ok(task_partial(payload, &steps))
}

pub(crate) async fn agents(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    Ok(Json(
        backend_agent_list(session)
            .await
            .map_err(backend_api_error)?,
    ))
}

pub(crate) async fn agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let agent = terminal_backend(session)
        .get_agent(&target)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::agent_get(agent)))
}

pub(crate) async fn focus_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    terminal_backend(session)
        .focus_agent(&target)
        .await
        .map_err(backend_api_error)?;
    Ok(Json(backend::compat::command_ok("agent_focused")))
}

pub(crate) async fn send_agent(
    State(state): State<AppState>,
    Path((session_id, target)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AgentSendBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    validate_text(&body.text)?;
    let session = find_session(&state.config, &session_id)?;
    let result = submit_agent_prompt(session, &target, &body.text)
        .await
        .map_err(|err| err.into_api_error("agent.prompt"))?;
    schedule_submit_keypress(session.clone(), target);
    Ok(Json(result))
}

/// Whether the pane is blocked on a permission menu, and what it is asking.
///
/// Deliberately its own endpoint rather than a part. `docs/content-model.md`
/// keeps the part set closed and gives approvals a part type only in v2; until
/// then this carries the same information without spending the closed set on
/// it, and without a client having to poll the transcript to learn that the
/// agent is waiting.
pub(crate) async fn pane_approval(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    // An agent that reports its approvals is believed over the screen: a menu
    // read off a terminal can only ever be the last frame drawn, and a protocol
    // says outright whether it is still waiting and on which request.
    let (agent, root) = pane_agent_and_root(&session, &pane_id).await;
    if let Some(adapter) = native::adapter_for(agent.as_deref()) {
        // Only when an endpoint is configured: an adapter with nothing behind
        // it must leave the pane on the drawn-menu path rather than answering
        // "idle" for a pane that is in fact blocked.
        if native::endpoint(adapter).is_some() {
            let pending = native::pending(adapter, root.as_deref(), i18n::current()).await;
            return Ok(Json(content_envelope(native_approval_data(
                &session_id,
                &pane_id,
                agent.as_deref(),
                pending.as_ref(),
            ))));
        }
    }

    let (agent, approval) = read_pane_approval(&session, &pane_id).await?;
    Ok(Json(content_envelope(approval_data(
        &session_id,
        &pane_id,
        agent.as_deref(),
        approval.as_ref(),
        "menu",
    ))))
}

/// Answer the menu the pane is blocked on.
///
/// The answer is named by option number or by decision (`allow`,
/// `allow_always`, `deny`), never by keystroke: which keys move an agent's
/// cursor is exactly the detail a client should not have to know, and having to
/// know it is what makes raw `send-keys` the fallback path rather than the one
/// a client reaches for.
pub(crate) async fn answer_pane_approval(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<AnswerApprovalBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();

    // An agent that reports its approvals is answered through its protocol
    // rather than through its keyboard. Naming the request by id is what makes
    // that strictly safer: there is no cursor to walk, and a request that was
    // resolved between the read and the answer is refused by the agent instead
    // of being answered blind by whatever the menu was redrawn into.
    if let Some(answered) = answer_native_approval(&session, &session_id, &pane_id, &body).await? {
        return Ok(Json(answered));
    }

    let (agent, pending) = read_pane_approval(&session, &pane_id).await?;
    let Some(pending) = pending else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_not_pending",
            "the pane is not waiting on an approval",
        ));
    };
    // The phone may be acting on a notification from a minute ago. A
    // fingerprint mismatch means the agent has moved on to a different
    // question, and answering that one blind is exactly what must not happen.
    if let Some(expected) = body.fingerprint.as_deref() {
        if expected != pending.fingerprint {
            return Err(api_error(
                StatusCode::CONFLICT,
                "approval_changed",
                "the pane is waiting on a different approval",
            ));
        }
    }

    let index = match (body.option, body.decision.as_deref()) {
        (Some(index), _) => index,
        (None, Some(name)) => {
            let decision = match name {
                "allow" => approvals::Decision::Allow,
                "allow_always" => approvals::Decision::AllowAlways,
                "deny" => approvals::Decision::Deny,
                _ => {
                    return Err(api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_decision",
                        "decision must be allow, allow_always, or deny",
                    ))
                }
            };
            pending
                .option_for(decision)
                .map(|option| option.index)
                .ok_or_else(|| {
                    api_error(
                        StatusCode::CONFLICT,
                        "decision_unavailable",
                        "this approval offers no option with that meaning",
                    )
                })?
        }
        (None, None) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_answer",
                "answer with an option number or a decision",
            ))
        }
    };
    let answered = pending
        .option(index)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_option",
                "this approval has no option with that number",
            )
        })?
        .clone();
    let keys = pending.keys_for(index).ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_option",
            "this approval has no option with that number",
        )
    })?;

    send_pane_keys(&session, &pane_id, &keys).await?;

    // Some menus act on the digit; some want it confirmed afterwards. From out
    // here the two look identical, so the confirm is deferred and only sent
    // when the same menu is demonstrably still standing -- the lesson from
    // following a pasted prompt with Enter, where the two raced.
    let mut sent = keys;
    tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
    let mut after = read_pane_approval(&session, &pane_id)
        .await
        .map(|(_, approval)| approval)
        .unwrap_or_default();
    if after
        .as_ref()
        .is_some_and(|approval| approval.fingerprint == pending.fingerprint)
    {
        send_pane_keys(&session, &pane_id, &["Enter".to_string()]).await?;
        sent.push("Enter".into());
        tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
        after = read_pane_approval(&session, &pane_id)
            .await
            .map(|(_, approval)| approval)
            .unwrap_or_default();
    }
    let resolved = after
        .as_ref()
        .is_none_or(|approval| approval.fingerprint != pending.fingerprint);

    let mut data = approval_data(
        &session_id,
        &pane_id,
        agent.as_deref(),
        after.as_ref(),
        "menu",
    );
    if let Some(object) = data.as_object_mut() {
        object.insert("resolved".into(), json!(resolved));
        object.insert("sent_keys".into(), json!(sent));
        object.insert(
            "answered".into(),
            json!({
                "fingerprint": pending.fingerprint,
                "index": answered.index,
                "decision": answered.decision.as_str(),
            }),
        );
    }
    Ok(Json(content_envelope(data)))
}

/// Where a spawned agent will run.
pub(crate) struct SpawnPlace {
    pub(crate) pane_id: String,
    pub(crate) tab_id: Option<String>,
}

/// Make somewhere for the agent to run: a split of the named tab, or a tab of
/// its own.
///
/// Splitting is what "run another agent on this" means -- the second agent
/// lands beside the first, in view, rather than in a tab the user has to go
/// find. A tab id naming nothing is refused before anything is created, because
/// the alternative is silently spawning somewhere the caller did not ask for.
async fn spawn_place(
    session: &SessionConfig,
    tab_id: Option<&str>,
    cwd: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let Some(tab_id) = tab_id else {
        return spawn_in_new_tab(session, cwd, None, steps).await;
    };
    spawn_beside(session, tab_id, cwd, steps).await
}

/// A task that asked for no particular tab, and the fallback for one whose tab
/// would not split.
async fn spawn_in_new_tab(
    session: &SessionConfig,
    cwd: Option<&str>,
    workspace_id: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let backend = terminal_backend(session);
    let tab = backend
        .create_tab(&BackendCreateTab {
            workspace_id: workspace_id.map(BackendWorkspaceId::new),
            cwd: cwd.map(PathBuf::from),
            label: None,
            focus: false,
        })
        .await
        .map_err(backend_api_error)?;
    let pane_id = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .map(|pane| pane.id.as_str().to_owned())
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "backend_malformed_response",
                "terminal backend did not return the created pane",
            )
        })?;
    let tab_id = Some(tab.id.as_str().to_owned());
    steps.ok("pane", json!({ "pane_id": pane_id, "tab_id": tab_id }));
    Ok(SpawnPlace { pane_id, tab_id })
}

/// A task placed beside what the reader was already looking at.
async fn spawn_beside(
    session: &SessionConfig,
    tab_id: &str,
    cwd: Option<&str>,
    steps: &mut tasks::StepLog,
) -> ApiResult<SpawnPlace> {
    let host = pane_in_tab(session, tab_id).await.ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "tab_not_found",
            "that tab has no pane to split",
        )
    })?;
    // Repeated downward splits can leave an assistant only five rows tall:
    // even its own transcript no longer renders the response there. Keep a
    // normal 24-row terminal for both halves, or use a background tab in the
    // same workspace. Neither path changes the user's current focus.
    if !can_split_agent_pane(host.viewport_rows) {
        steps.skipped(
            "split",
            "not enough terminal rows; using a tab in the same workspace",
        );
        return spawn_in_new_tab(session, cwd, Some(host.workspace_id.as_str()), steps).await;
    }
    // A split that Ghostty refuses -- a tab already carrying as many panes as
    // its layout will hold, which is the ordinary state of a tab someone works
    // in -- must not lose the task. The tab was a preference, not the request:
    // the request was "start this agent". So a refusal falls back to a tab of
    // its own, recorded as such rather than passed off as the split that was
    // asked for.
    let split = terminal_backend(session)
        .split_pane(&BackendSplitPane {
            pane_id: host.id.clone(),
            direction: BackendSplitDirection::Down,
            ratio: None,
            cwd: cwd.map(PathBuf::from),
            env: None,
        })
        .await;
    let pane = match split {
        Ok(pane) => pane,
        Err(err) => {
            steps.skipped(
                "split",
                &format!("{err} -- starting in a tab of its own instead"),
            );
            return spawn_in_new_tab(session, cwd, Some(host.workspace_id.as_str()), steps).await;
        }
    };
    let pane_id = pane.id.as_str().to_owned();
    steps.ok(
        "pane",
        json!({ "pane_id": pane_id, "tab_id": tab_id, "split_from": host.id.as_str() }),
    );
    Ok(SpawnPlace {
        pane_id,
        tab_id: Some(tab_id.to_owned()),
    })
}

/// A pane to split in the named tab, preferring the one that has focus.
pub(crate) fn can_split_agent_pane(viewport_rows: Option<u32>) -> bool {
    viewport_rows.is_none_or(|rows| rows >= 48)
}

async fn pane_in_tab(session: &SessionConfig, tab_id: &str) -> Option<Pane> {
    terminal_backend(session)
        .list_panes()
        .await
        .ok()?
        .into_iter()
        .filter(|pane| pane.tab_id.as_str() == tab_id)
        .max_by_key(|pane| pane.focused)
}

/// Where the task will be worked on, however it got there.
pub(crate) struct TaskPlace {
    pub(crate) workspace_id: String,
    pub(crate) pane_id: String,
    pub(crate) worktree_path: Option<String>,
    pub(crate) reused: bool,
}

/// The no-branch case: a workspace on the repo as it stands.
async fn prepare_workspace(
    session: &SessionConfig,
    repo_path: &FsPath,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    steps.skipped("worktree", "no branch_name was given");
    let backend = terminal_backend(session);
    let workspace = match backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: Some(repo_path.to_owned()),
            label: label.map(str::to_owned),
            focus: false,
        })
        .await
    {
        Ok(workspace) => workspace,
        Err(err) => {
            let err = HerdrCallError::Unavailable(err.to_string());
            steps.failed("workspace", err.code(), &err.message());
            return Err(err);
        }
    };
    let pane = backend
        .list_panes()
        .await
        .map_err(|err| HerdrCallError::Unavailable(err.to_string()))?
        .into_iter()
        .find(|pane| pane.workspace_id == workspace.id)
        .ok_or_else(|| HerdrCallError::malformed("workspace.create"))?;
    let place = TaskPlace {
        workspace_id: workspace.id.as_str().to_owned(),
        pane_id: pane.id.as_str().to_owned(),
        worktree_path: None,
        reused: false,
    };
    steps.ok(
        "workspace",
        json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
    );
    Ok(place)
}

/// The branch case.
///
/// Herdr's `worktree.create` makes the checkout *and* the workspace and pane in
/// one response, so there is no moment where a checkout exists with nothing
/// attached to it. Before creating anything it asks what checkouts the repo
/// already has: a phone that retried after losing its answer gets the existing
/// one back rather than a git error about the branch being checked out
/// elsewhere. A Herdr too old for these methods falls back to running git here.
async fn prepare_worktree(
    session: &SessionConfig,
    repo_path: &FsPath,
    branch: &str,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    let backend = terminal_backend(session);
    let request = BackendWorktreeRequest {
        cwd: repo_path.to_owned(),
        branch: branch.to_owned(),
        label: label.map(str::to_owned),
        focus: false,
    };
    let existing = backend
        .list_worktrees(&request.cwd)
        .await
        .ok()
        .and_then(|worktrees| {
            worktrees.into_iter().find(|worktree| {
                worktree
                    .branch
                    .as_deref()
                    .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name))
                    == Some(branch)
            })
        });

    if let Some(worktree) = existing {
        match backend.open_worktree(&request).await {
            Ok(placement) => {
                let path = worktree.path.to_string_lossy().into_owned();
                let place = TaskPlace {
                    workspace_id: placement.workspace_id.as_str().to_owned(),
                    pane_id: placement.pane_id.as_str().to_owned(),
                    worktree_path: Some(path.clone()),
                    reused: true,
                };
                steps.ok(
                    "worktree",
                    json!({ "path": path, "branch": branch, "reused": true }),
                );
                steps.ok(
                    "workspace",
                    json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
                );
                return Ok(place);
            }
            // Reuse is an optimisation, not a contract. If opening the existing
            // checkout fails, fall through and let the create path report a
            // real error rather than masking it with this one.
            Err(err) => eprintln!("task: worktree open for {branch} failed: {err}"),
        }
    }

    match backend.create_worktree(&request).await {
        Ok(placement) => {
            let path = placement
                .path
                .map(|path| path.to_string_lossy().into_owned());
            let place = TaskPlace {
                workspace_id: placement.workspace_id.as_str().to_owned(),
                pane_id: placement.pane_id.as_str().to_owned(),
                worktree_path: path.clone(),
                reused: false,
            };
            steps.ok(
                "worktree",
                json!({ "path": path, "branch": branch, "reused": false }),
            );
            steps.ok(
                "workspace",
                json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
            );
            Ok(place)
        }
        Err(BackendError::Unsupported(_)) => {
            prepare_worktree_with_git(session, repo_path, branch, label, steps).await
        }
        Err(err) => {
            let err = backend_call_error("worktree.create", err);
            steps.failed("worktree", err.code(), &err.message());
            Err(err)
        }
    }
}

/// The fallback for a Herdr without `worktree.*`: run git here, then ask for a
/// workspace on the result. This is the only path that can leave a checkout
/// with nothing attached, so it is the only one that rolls back.
async fn prepare_worktree_with_git(
    session: &SessionConfig,
    repo_path: &FsPath,
    branch: &str,
    label: Option<&str>,
    steps: &mut tasks::StepLog,
) -> Result<TaskPlace, HerdrCallError> {
    let repo = repo_path.to_owned();
    let branch_name = branch.to_owned();
    let target = tasks::default_worktree_path(&repo, &branch_name).ok_or_else(|| {
        let err = HerdrCallError::Unavailable("repo_path has no parent directory".into());
        steps.failed("worktree", err.code(), &err.message());
        err
    })?;

    let outcome = {
        let repo = repo.clone();
        let branch_name = branch_name.clone();
        let target = target.clone();
        tokio::task::spawn_blocking(move || tasks::git_worktree_add(&repo, &target, &branch_name))
            .await
    };
    let added = match outcome {
        Ok(Ok(added)) => added,
        Ok(Err(err)) => {
            let err = HerdrCallError::Unavailable(err.to_string());
            steps.failed("worktree", "worktree_create_failed", &err.message());
            return Err(err);
        }
        Err(err) => {
            let err = HerdrCallError::Unavailable(format!("git task panicked: {err}"));
            steps.failed("worktree", "worktree_create_failed", &err.message());
            return Err(err);
        }
    };
    let path = added.path.to_string_lossy().into_owned();
    steps.ok(
        "worktree",
        json!({ "path": path, "branch": branch, "reused": !added.created }),
    );

    let backend = terminal_backend(session);
    let created = match backend
        .create_workspace(&BackendCreateWorkspace {
            cwd: Some(PathBuf::from(&path)),
            label: label.map(str::to_owned),
            focus: false,
        })
        .await
    {
        Err(err) => Err(HerdrCallError::Unavailable(err.to_string())),
        Ok(workspace) => backend
            .list_panes()
            .await
            .map_err(|err| HerdrCallError::Unavailable(err.to_string()))?
            .into_iter()
            .find(|pane| pane.workspace_id == workspace.id)
            .map(|pane| TaskPlace {
                workspace_id: workspace.id.as_str().to_owned(),
                pane_id: pane.id.as_str().to_owned(),
                worktree_path: Some(path.clone()),
                reused: !added.created,
            })
            .ok_or_else(|| HerdrCallError::malformed("workspace.create")),
    };

    match created {
        Ok(place) => {
            steps.ok(
                "workspace",
                json!({ "workspace_id": place.workspace_id, "pane_id": place.pane_id }),
            );
            Ok(place)
        }
        Err(err) => {
            steps.failed("workspace", err.code(), &err.message());
            // A checkout with no workspace is the one genuinely useless state,
            // and only ours to undo when this request is what made it.
            if added.created {
                let repo = repo.clone();
                let target = added.path.clone();
                let removed =
                    tokio::task::spawn_blocking(move || tasks::git_worktree_remove(&repo, &target))
                        .await;
                match removed {
                    Ok(Ok(())) => steps.rolled_back("worktree", json!({ "path": path })),
                    Ok(Err(remove_err)) => {
                        steps.failed("rollback", "rollback_failed", &remove_err.to_string())
                    }
                    Err(join_err) => {
                        steps.failed("rollback", "rollback_failed", &join_err.to_string())
                    }
                }
            } else {
                steps.skipped("rollback", "the checkout was already there");
            }
            Err(err)
        }
    }
}

/// Nothing usable was created: an ordinary error, with the steps attached so
/// the client can still see how far it got.
pub(crate) fn task_failure(err: HerdrCallError, steps: &tasks::StepLog) -> Response {
    // Herdr refusing is the user's request being wrong -- a path it does not
    // recognise, a branch already checked out somewhere it will not touch.
    // Anything else is the gateway or the socket failing, which is not.
    let status = match err {
        HerdrCallError::Herdr { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({
            "error": { "code": err.code(), "message": err.message() },
            "steps": steps.value()
        })),
    )
        .into_response()
}

/// Something usable was created. 207 when a later step failed, so a client can
/// tell "your agent is running" from "your checkout is waiting for you".
pub(crate) fn task_partial(mut payload: Value, steps: &tasks::StepLog) -> Response {
    payload["steps"] = steps.value();
    let status = if steps.has_failure() {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };
    (status, Json(payload)).into_response()
}

/// Directories a task is allowed to start in.
///
/// Herdr does not report "repos this session has" as such, so this is assembled
/// from what it does report: the repo root and checkout path of every workspace
/// that is a git checkout, plus the working directory of every pane. The repo
/// root matters because a pane is usually somewhere inside the repo rather than
/// at its top, and branching from the top is the normal request.
async fn task_repo_roots(state: &AppState, session: &SessionConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !is_scannable_root(&path) {
            return;
        }
        let Ok(canonical) = std::fs::canonicalize(&path) else {
            return;
        };
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    };

    match terminal_backend(session).list_workspaces().await {
        Ok(workspaces) => {
            for workspace in workspaces {
                if let Some(path) = workspace.repo_root {
                    push(path);
                }
                if let Some(path) = workspace.checkout_path {
                    push(path);
                }
            }
        }
        Err(err) => eprintln!("task roots: workspace list failed: {err}"),
    }

    for root in session_asset_roots(state, session, None).await {
        push(root.path);
    }
    roots
}

/// Send a prompt to an agent and make sure it is actually submitted.
///
/// Shared by `POST .../agents/{target}/send` and by task dispatch, because the
/// second half of it -- the separate Enter -- is not an optional flourish, and a
/// second copy of it would drift.
pub(crate) async fn submit_agent_prompt(
    session: &SessionConfig,
    target: &str,
    text: &str,
) -> Result<Value, HerdrCallError> {
    terminal_backend(session)
        .prompt_agent(target, text)
        .await
        .map(|_| backend::compat::command_ok("agent_prompted"))
        .map_err(|err| match err {
            BackendError::Refused { code, message } => HerdrCallError::Herdr {
                method: "agent.prompt".to_owned(),
                error: json!({ "code": code, "message": message }),
            },
            other => HerdrCallError::Unavailable(other.to_string()),
        })
}

async fn start_backend_agent(
    session: &SessionConfig,
    pane_id: &str,
    kind: &str,
    command: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<Value, HerdrCallError> {
    let started = terminal_backend(session)
        .start_agent(&BackendStartAgent {
            pane_id: BackendPaneId::new(pane_id),
            kind: kind.to_owned(),
            command: command.to_owned(),
            executable: tasks::find_on_path(command),
            args: args.to_vec(),
            timeout_ms,
        })
        .await
        .map_err(|err| backend_call_error("agent.start", err))?;
    Ok(
        json!({ "result": { "argv": started.argv, "instance_id": started.instance_id, "target": started.target } }),
    )
}

/// Belt and braces for a paste-vs-keypress race in agent TUIs: when the prompt
/// text and its newline arrive in one PTY write, Claude Code's input treats the
/// newline as pasted content and leaves the prompt sitting in its input box
/// unsubmitted (reproduced on camera, muqun card #571). A short beat later, a
/// separate Enter keystroke submits it. When the prompt DID submit, the input
/// box is empty and an Enter there is a no-op, so this is idempotent.
/// Best-effort by design: a pane that vanished between the two calls must not
/// turn a delivered prompt into an error. An agent target is its pane id, which
/// is what the key press needs.
///
/// The beat cannot be a fixed one, because a prompt that names an image file
/// keeps the agent busy staging it for as long as reading and encoding that
/// file takes, and every Enter sent before that finishes is discarded. So the
/// keystroke is sent at a pane that has stopped redrawing and is then checked
/// against Herdr's own reading of the agent, the same shape the approvals
/// confirm uses.
pub(crate) fn schedule_submit_keypress(session: SessionConfig, pane_id: String) {
    tokio::spawn(async move {
        // Herdr 0.9 owns paste + delayed Enter. Another Enter can answer a
        // subsequent dialog, so the legacy workaround must not run there.
        if !terminal_backend(&session)
            .needs_submit_keypress()
            .await
            .unwrap_or(false)
        {
            return;
        }
        submit_keypress(&session, &pane_id).await;
    });
}

/// Send the Enter that submits a prompt, and keep sending it until the agent
/// shows that one landed.
///
/// The screen cannot answer "did it submit". Staging an image repaints the pane
/// constantly without submitting anything, so "the text changed" says yes to a
/// prompt still sitting in the input box; measured against a live pane, that
/// false positive is what stopped the retry from ever running. What does
/// discriminate is Herdr's `state_change_seq`: it does not move at all for the
/// whole staging window, and a real submission advances it as the agent leaves
/// idle. So the settled screen only decides *when* to press Enter, and the
/// sequence decides whether it worked.
pub(crate) async fn submit_keypress(session: &SessionConfig, pane_id: &str) {
    tokio::time::sleep(SUBMIT_KEYPRESS_DELAY).await;
    let deadline = Instant::now() + SUBMIT_SETTLE_TIMEOUT;
    // A send is addressed to a pane, and a pane need not be running an agent
    // Herdr knows about, so the sequence is what this hopes for rather than what
    // it requires.
    let baseline = agent_state_change_seq(session, pane_id).await;
    if baseline.is_none() {
        eprintln!(
            "agent submit for pane {pane_id}: the terminal backend lists no agent state for it, \
             falling back to watching the screen"
        );
    }
    let attempts = if baseline.is_some() {
        SUBMIT_MAX_ATTEMPTS
    } else {
        SUBMIT_BLIND_MAX_ATTEMPTS
    };
    let mut previous: Option<String> = None;
    for attempt in 0..attempts {
        let Some(settled) = settled_pane_text(session, pane_id, &mut previous, deadline).await
        else {
            eprintln!("agent submit for pane {pane_id} gave up: the pane never settled");
            return;
        };
        let sent = terminal_backend(session)
            .send_keys(&BackendPaneId::new(pane_id), &["Enter".to_owned()])
            .await;
        if let Err(err) = sent {
            eprintln!(
                "agent submit for pane {pane_id} failed to send Enter: {}",
                err
            );
            return;
        }
        match baseline {
            Some(baseline) => {
                if agent_state_advanced(session, pane_id, baseline, deadline).await {
                    return;
                }
            }
            None => {
                tokio::time::sleep(SUBMIT_VERIFY_DELAY).await;
                let Ok(after) = read_pane_visible_text(session, pane_id).await else {
                    eprintln!(
                        "agent submit for pane {pane_id} gave up: the pane could not be verified"
                    );
                    return;
                };
                // All this says is that something moved. It is the weakest of
                // the two answers, which is why the blind budget is small.
                if after != settled {
                    return;
                }
                previous = Some(after);
            }
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(SUBMIT_RETRY_INTERVAL).await;
        }
    }
    eprintln!(
        "agent submit for pane {pane_id} gave up: the agent did not take the Enter \
         in {attempts} attempts"
    );
}

/// The backend's count of how many times this pane's agent has changed state.
///
/// `None` means the backend lists no agent for the pane, which a send to a plain
/// shell pane legitimately is, or that it could not be asked.
async fn agent_state_change_seq(session: &SessionConfig, pane_id: &str) -> Option<u64> {
    let value = match backend_agent_list(session).await {
        Ok(value) => value,
        Err(err) => {
            eprintln!("terminal backend agent list failed: {err}");
            return None;
        }
    };
    value
        .pointer("/result/agents")?
        .as_array()?
        .iter()
        .find(|agent| agent.get("pane_id").and_then(Value::as_str) == Some(pane_id))?
        .get("state_change_seq")?
        .as_u64()
}

/// Watch the agent for a beat and say whether it moved off the sequence it was
/// on before the Enter, which is the one reading that means the prompt went in.
async fn agent_state_advanced(
    session: &SessionConfig,
    pane_id: &str,
    baseline: u64,
    deadline: Instant,
) -> bool {
    let until = Instant::now() + SUBMIT_VERIFY_WINDOW;
    loop {
        tokio::time::sleep(SUBMIT_VERIFY_INTERVAL).await;
        if agent_state_change_seq(session, pane_id)
            .await
            .is_some_and(|seq| seq > baseline)
        {
            return true;
        }
        let now = Instant::now();
        if now >= until || now >= deadline {
            return false;
        }
    }
}

/// Read the pane until two consecutive reads come back identical, and hand back
/// that text. `previous` carries the last reading across attempts so a pane that
/// was already still is not waited on twice.
///
/// `None` means the pane was still moving at the deadline, or could not be read
/// at all -- either way there is nothing safe to press Enter against.
async fn settled_pane_text(
    session: &SessionConfig,
    pane_id: &str,
    previous: &mut Option<String>,
    deadline: Instant,
) -> Option<String> {
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        // The read failure is already logged one level down.
        let text = read_pane_visible_text(session, pane_id).await.ok()?;
        if previous.as_deref() == Some(text.as_str()) {
            return Some(text);
        }
        *previous = Some(text);
        tokio::time::sleep(SUBMIT_SETTLE_INTERVAL).await;
    }
}

/// A Herdr call that separates "the socket did not answer" from "Herdr said no",
/// which the plain [`call_session_method`] deliberately does not: it hands the
/// whole envelope, error and all, straight to the client. Orchestrating several
/// calls needs to know which one refused and why.
#[derive(Debug)]
pub(crate) enum HerdrCallError {
    Unavailable(String),
    Herdr {
        method: String,
        error: Value,
    },
    /// Herdr answered successfully but not with the shape the schema promises,
    /// which is a bug somewhere rather than a user error.
    Malformed(String),
}

impl HerdrCallError {
    /// Only an explicit pre-submission refusal is safe to repeat. A lost
    /// socket response can follow a successful write; repeating it duplicates
    /// the user's instruction. Blocked dialogs must never be auto-answered.
    pub(crate) fn can_retry_prompt(&self) -> bool {
        matches!(self, Self::Herdr { error, .. }
            if error.get("code").and_then(Value::as_str) == Some("agent_not_found"))
    }

    pub(crate) fn malformed(method: &str) -> Self {
        Self::Malformed(method.to_owned())
    }

    pub(crate) fn code(&self) -> &str {
        match self {
            Self::Unavailable(_) => "herdr_unavailable",
            Self::Herdr { error, .. } => error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("herdr_error"),
            Self::Malformed(_) => "invalid_herdr_response",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unavailable(detail) => detail.clone(),
            Self::Herdr { method, error } => {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Herdr refused the request");
                format!("{method}: {message}")
            }
            Self::Malformed(method) => {
                format!("{method} did not answer with the expected fields")
            }
        }
    }

    pub(crate) fn into_api_error(self, method: &str) -> (StatusCode, Json<Value>) {
        let status = match self {
            Self::Herdr { .. } => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        let code = self.code().to_owned();
        let message = self.message();
        eprintln!("Herdr request {method} failed: {message}");
        api_error(status, &code, &message)
    }
}

pub(crate) fn backend_call_error(method: &str, error: BackendError) -> HerdrCallError {
    match error {
        BackendError::Refused { code, message } => HerdrCallError::Herdr {
            method: method.to_owned(),
            error: json!({ "code": code, "message": message }),
        },
        BackendError::InvalidResponse(_) => HerdrCallError::malformed(method),
        other @ (BackendError::Unavailable | BackendError::Unsupported(_)) => {
            HerdrCallError::Unavailable(other.to_string())
        }
        other @ BackendError::InvalidTarget(_) => HerdrCallError::Unavailable(other.to_string()),
    }
}

/// The same payload for a pane whose agent reports its approvals.
///
/// One shape, two sources: `approval` carries the request the client answers,
/// and only the fields a protocol can honestly fill in. There is no cursor, so
/// no option is `selected`; the identity is the agent's own request id rather
/// than a fingerprint of the drawn text, because the agent guarantees it.
pub(crate) fn native_approval_data(
    session_id: &str,
    pane_id: &str,
    agent: Option<&str>,
    pending: Option<&native::NativeApproval>,
) -> Value {
    json!({
        "session_id": session_id,
        "pane_id": pane_id,
        "state": if pending.is_some() { "pending" } else { "idle" },
        "approval": pending.map(|pending| {
            let request = &pending.request;
            json!({
                "approval_id": request.id,
                "fingerprint": request.id,
                "prompt": request.prompt,
                "tool": request.tool,
                "context": request.context,
                "options": request
                    .options
                    .iter()
                    .map(|option| json!({
                        "index": option.index,
                        "label": option.label,
                        "decision": option.decision,
                    }))
                    .collect::<Vec<Value>>(),
            })
        }),
        "pane": {
            "pane_id": pane_id,
            "agent": agent,
            "approvals": "protocol",
        },
    })
}

/// Answer a pane's approval through the agent's own protocol, when it has one.
///
/// `Ok(None)` means this pane has no native source -- no adapter, no configured
/// endpoint, or nothing pending there -- and the caller falls through to the
/// drawn-menu path. Nothing here can make a pane unanswerable.
async fn answer_native_approval(
    session: &SessionConfig,
    session_id: &str,
    pane_id: &str,
    body: &AnswerApprovalBody,
) -> ApiResult<Option<Value>> {
    let (agent, root) = pane_agent_and_root(session, pane_id).await;
    let Some(adapter) = native::adapter_for(agent.as_deref()) else {
        return Ok(None);
    };
    if native::endpoint(adapter).is_none() {
        return Ok(None);
    }
    // Past this point the protocol is the authority, and a pane it says is not
    // waiting is not answered by keystroke either: the menu still on the screen
    // is the last frame of one that has already been resolved.
    let Some(pending) = native::pending(adapter, root.as_deref(), i18n::current()).await else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_not_pending",
            "the pane is not waiting on an approval",
        ));
    };

    // A protocol names its answers, so an option number is read off the part
    // the client was shown rather than off a cursor the agent moved.
    let decision = match (body.decision.as_deref(), body.option) {
        (Some(name), _) => decision_named(name)?,
        (None, Some(index)) => pending
            .request
            .options
            .iter()
            .find(|option| option.index == index)
            .map(|option| decision_named(option.decision))
            .transpose()?
            .ok_or_else(|| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_option",
                    "this approval has no option with that number",
                )
            })?,
        (None, None) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_answer",
                "answer with an option number or a decision",
            ))
        }
    };
    let accepted = native::answer(&pending, decision).await.unwrap_or(false);
    if !accepted {
        return Err(api_error(
            StatusCode::CONFLICT,
            "approval_changed",
            "the agent no longer has that request pending",
        ));
    }

    let mut data = approval_data(session_id, pane_id, agent.as_deref(), None, "protocol");
    if let Some(object) = data.as_object_mut() {
        object.insert("resolved".into(), json!(true));
        // No keystrokes were sent, and saying so is how a client can tell the
        // two paths apart without the wire naming either agent's protocol.
        object.insert("sent_keys".into(), json!([] as [String; 0]));
        object.insert(
            "answered".into(),
            json!({
                "approval_id": pending.request.id,
                "decision": decision.as_str(),
            }),
        );
    }
    Ok(Some(content_envelope(data)))
}

/// The decision a client named, or a 400.
fn decision_named(name: &str) -> ApiResult<approvals::Decision> {
    match name {
        "allow" => Ok(approvals::Decision::Allow),
        "allow_always" => Ok(approvals::Decision::AllowAlways),
        "deny" => Ok(approvals::Decision::Deny),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_decision",
            "decision must be allow, allow_always, or deny",
        )),
    }
}
