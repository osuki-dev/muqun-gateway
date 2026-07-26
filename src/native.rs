//! Native protocol adapters: a second source for the same parts.
//!
//! A marker dictionary reads structure back off a terminal because that is all
//! some agents leave behind. Two of them leave more: opencode runs an HTTP
//! server beside its TUI, and Codex speaks a JSON-RPC app-server. Where such a
//! protocol answers, reading it is strictly better -- a tool's exit code, a
//! patch, a checklist and a pending permission arrive as data instead of as
//! glyphs a table has to guess at.
//!
//! What does **not** change is the wire. `docs/content-model.md` keeps the part
//! set closed and the envelope versioned, and an adapter is subject to both:
//!
//! - It produces the same [`parts::Part`] values a dictionary produces, through
//!   [`parts::Assembler`], which pins every part to rows of a transcript the
//!   adapter renders. The two invariants -- `fallback_text` is those rows
//!   verbatim, and every non-blank row lands in exactly one part -- therefore
//!   hold for an adapter exactly as they do for a table, and are checked by the
//!   same tests.
//! - It names no agent-specific construct on the wire. The only thing a client
//!   learns is `pane.parts: "native"` beside the existing `"dictionary"` and
//!   `"text"`, which is a value added to an existing enum, not a new concept.
//! - It is never required. Detection is a probe: no endpoint, an endpoint that
//!   does not answer, or a session that cannot be matched to the pane all fall
//!   through to the dictionary. A native adapter can only ever add structure.
//!
//! ## Where the protocol came from
//!
//! Nothing here is remembered. The opencode mapping is written against the
//! OpenAPI document opencode 1.18.0 serves at `GET /doc` on its own server --
//! the same document that generates its clients -- and every field used below
//! was read off a live session of that build. `tests/fixtures/opencode-native-*`
//! are those reads saved verbatim.

use serde_json::Value;

use crate::approvals::Decision;
use crate::parts::{ApprovalChoice, ApprovalRequest, Assembler, Part, TodoItem, ToolStatus};

/// How many result lines a tool block keeps. A native `read` returns a whole
/// file, and a phone wants the head of it plus the honest `truncated` flag that
/// the content model already carries.
const MAX_RESULT_LINES: usize = 40;
/// How much of one result line survives. Same reason.
const MAX_RESULT_LINE_CHARS: usize = 500;
/// The input fields agents' own tools are called with, in the order a one-line
/// display would prefer them. Read off the live tool set of opencode 1.18.0
/// (`bash`, `read`, `edit`, `write`, `grep`, `glob`, `webfetch`), not guessed.
const INPUT_DISPLAY_KEYS: &[&str] = &["command", "filePath", "pattern", "path", "url", "query"];
/// How many messages of a session are read. A pane read is a screenful or two;
/// this is the native equivalent of `PARTS_DEFAULT_LINES`.
pub const DEFAULT_MESSAGE_LIMIT: usize = 40;

/// One agent family's native protocol.
///
/// Keyed the same way the dictionaries and the composer tables are -- a
/// substring of the agent name Herdr reports -- so one pane resolves to at most
/// one adapter and at most one table.
pub struct Adapter {
    /// Stable id on the wire, beside `dictionary` in the pane descriptor.
    pub id: &'static str,
    /// The environment variable naming this agent's server. Absent from the
    /// environment means "no native source on this machine", which is a
    /// supported answer and not an error.
    pub endpoint_env: &'static str,
    /// What the agent's own release calls the protocol, for bug reports.
    pub protocol: &'static str,
}

/// opencode's server API. `opencode serve` (and the server the TUI already runs
/// beside itself, which `opencode attach` connects to) speaks it.
pub const OPENCODE: Adapter = Adapter {
    id: "opencode",
    endpoint_env: "HERDR_GATEWAY_OPENCODE_URL",
    protocol: "opencode-server",
};

const ADAPTERS: &[(&[&str], &Adapter)] = &[(&["opencode", "open-code"], &OPENCODE)];

/// Which native adapter, if any, could read this agent.
///
/// Answering `Some` says only that an adapter exists, never that it reached
/// anything: the endpoint probe is a separate step, and the caller falls back
/// to the dictionary whenever it fails.
pub fn adapter_for(agent: Option<&str>) -> Option<&'static Adapter> {
    let agent = agent?.trim().to_ascii_lowercase();
    if agent.is_empty() {
        return None;
    }
    ADAPTERS
        .iter()
        .find(|(names, _)| names.iter().any(|name| agent.contains(name)))
        .map(|(_, adapter)| *adapter)
}

/// The base URL this adapter's server is at, if the operator named one.
///
/// Configuration rather than discovery on purpose. opencode's server binds an
/// ephemeral port by default and publishes it nowhere the gateway may read, so
/// sniffing for it would mean guessing at ports on the host -- exactly the kind
/// of probing the rest of this gateway refuses to do. Naming the endpoint is
/// one line of config and it is unambiguous.
pub fn endpoint(adapter: &Adapter) -> Option<String> {
    let url = std::env::var(adapter.endpoint_env).ok()?;
    let url = url.trim().trim_end_matches('/');
    (!url.is_empty()).then(|| url.to_owned())
}

/// A native read: the parts, and what the adapter learned about the source.
pub struct NativeRead {
    pub parts: Vec<Value>,
    /// The agent's own session identity, so a client can tell one native read
    /// from the next one without the wire naming an opencode concept.
    pub session: Option<String>,
    /// The protocol version the server reported.
    pub version: Option<String>,
}

// -- opencode --------------------------------------------------------------

/// Turn one opencode session into parts.
///
/// `messages` is the body of `GET /session/{id}/message`: an array of
/// `{ info, parts }`. `pending` is `GET /permission` filtered to this session.
/// Both are taken as `Value` rather than as typed structs because the mapping
/// only ever reads a handful of fields, and a protocol that grows a field must
/// not stop the gateway.
pub fn opencode_normalize(messages: &Value, pending: &[Value]) -> (Vec<Part>, String) {
    let mut out = Assembler::new();
    for message in messages.as_array().into_iter().flatten() {
        let user = message.pointer("/info/role").and_then(Value::as_str) == Some("user");
        for part in message
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            opencode_part(part, user, &mut out);
        }
    }
    for request in pending {
        if let Some(approval) = opencode_approval(request) {
            out.approval(approval);
        }
    }
    let transcript = out.transcript();
    (out.finish(), transcript)
}

/// One opencode message part.
///
/// The parts opencode uses to drive its own renderer -- `step-start`,
/// `step-finish`, `snapshot`, `agent`, `compaction`, `retry` -- carry no text a
/// user would read and are skipped. That is not a hole in the "nothing is
/// dropped" invariant: on this path the source *is* what the adapter renders,
/// and a part with nothing to render never enters it. The dictionary path is
/// unaffected, since a terminal never showed those either.
fn opencode_part(part: &Value, user: bool, out: &mut Assembler) {
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" | "reasoning" => {
            // Synthetic text is context opencode injected into the model's
            // input, not something the agent said; `ignored` is text it has
            // already decided not to show.
            if flag(part, "synthetic") || flag(part, "ignored") {
                return;
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            // Reasoning has no type of its own in the closed set, so it
            // degrades to `text` -- the same answer the dictionaries give a
            // drawn thinking tree.
            if user && kind == "text" {
                out.prompt(text);
            } else {
                out.text(text);
            }
        }
        "tool" => opencode_tool(part, out),
        "file" => {
            let name = part
                .get("filename")
                .and_then(Value::as_str)
                .or_else(|| part.get("url").and_then(Value::as_str))
                .unwrap_or_default();
            out.text(name);
        }
        "patch" => {
            let files: Vec<&str> = part
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            out.diff(None, files.iter().map(|file| (*file).to_owned()).collect());
        }
        "subtask" => {
            let description = part
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.text(description);
        }
        _ => {}
    }
}

/// A `ToolPart`, which is where a native read earns its keep.
///
/// The dictionary reads a tool's outcome off the first line of whatever it
/// printed, because the terminal never carried an exit code. Here the protocol
/// carries one, and it carries the patch an edit produced and the checklist a
/// todo write submitted -- so the same three part types come out typed from
/// data rather than from a majority vote over line shapes.
fn opencode_tool(part: &Value, out: &mut Assembler) {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or_default();
    let state = part.get("state").unwrap_or(&Value::Null);
    let status = state
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let metadata = state.get("metadata").unwrap_or(&Value::Null);

    // opencode's own one-line display of the call. It is the protocol's answer
    // to "what was this called with", so it beats re-rendering the input JSON.
    // A call that has not run yet has no display line, so the fields tools
    // actually take are tried next and the JSON is the last resort -- a block
    // that reads `bash({"command":"…"})` is a worse fallback than the terminal.
    let input = state
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let input = state.get("input")?;
            INPUT_DISPLAY_KEYS
                .iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .map(str::to_owned)
                .or_else(|| Some(compact_input(input)))
        })
        .unwrap_or_default();

    // A checklist submitted through the todo tool is a `todo` part, not a tool
    // block that happens to print boxes.
    if let Some(items) = todo_items(metadata) {
        out.todo(items);
        return;
    }

    let (result, truncated) = match status {
        "completed" => {
            let output = state
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default();
            clamp(output)
        }
        "error" => {
            let error = state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            clamp(error)
        }
        _ => (Vec::new(), false),
    };
    let truncated = truncated || flag(metadata, "truncated");

    let outcome = match status {
        "error" => ToolStatus::Error,
        "pending" | "running" => ToolStatus::Running,
        // A shell tool reports the exit code the terminal threw away.
        _ => match metadata.get("exit").and_then(Value::as_i64) {
            Some(0) | None => ToolStatus::Ok,
            Some(_) => ToolStatus::Error,
        },
    };
    out.tool_block(tool, &input, result, outcome, truncated);

    // The patch an edit produced, split off beside its block -- the same shape
    // the dictionaries produce when a result group turns out to be a diff.
    if let Some((file, patch)) = file_diff(metadata) {
        out.diff(file, patch.lines().map(str::to_owned).collect());
    }
}

/// The checklist a todo tool submitted, when this tool submitted one.
///
/// Read off `metadata.todos` rather than off the tool's name: an agent may call
/// it whatever it likes, and the shape of the payload is what makes it a
/// checklist.
fn todo_items(metadata: &Value) -> Option<Vec<TodoItem>> {
    let todos = metadata.get("todos")?.as_array()?;
    if todos.is_empty() {
        return None;
    }
    Some(
        todos
            .iter()
            .map(|todo| {
                let text = todo
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let done = todo.get("status").and_then(Value::as_str) == Some("completed");
                TodoItem::new(text, done)
            })
            .collect(),
    )
}

/// The unified patch an edit produced, and the file it touched.
fn file_diff(metadata: &Value) -> Option<(Option<String>, String)> {
    let diff = metadata.get("filediff");
    let patch = diff
        .and_then(|diff| diff.get("patch"))
        .or_else(|| metadata.get("diff"))
        .and_then(Value::as_str)?;
    if patch.trim().is_empty() {
        return None;
    }
    let file = diff
        .and_then(|diff| diff.get("file"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((file, patch.trim_end().to_owned()))
}

/// A pending `PermissionRequest` as an `approval` part.
///
/// The three answers are the protocol's own: `POST
/// /session/{id}/permissions/{id}` takes `once`, `always` or `reject`, and
/// nothing else. Their labels are written here rather than quoted from the
/// agent, the same rule the push payloads follow -- an agent's own wording
/// routinely embeds the command it is asking about.
fn opencode_approval(request: &Value) -> Option<ApprovalRequest> {
    let id = request.get("id").and_then(Value::as_str)?.to_owned();
    let action = request
        .get("permission")
        .and_then(Value::as_str)
        .or_else(|| request.get("action").and_then(Value::as_str))
        .unwrap_or("this");
    let resources = request
        .get("patterns")
        .or_else(|| request.get("resources"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let options = [Decision::Allow, Decision::AllowAlways, Decision::Deny]
        .into_iter()
        .enumerate()
        .map(|(position, decision)| {
            let index = position as u32 + 1;
            ApprovalChoice {
                index,
                label: decision.public_label(index),
                decision: decision.as_str(),
            }
        })
        .collect();
    Some(ApprovalRequest {
        id,
        prompt: format!("Allow {action}?"),
        tool: Some(action.to_owned()),
        context: resources,
        options,
    })
}

/// The reply an answer to a native approval turns into, in the protocol's own
/// words. `None` for a decision opencode has no spelling for, which the caller
/// answers with a 400 rather than by guessing.
///
/// The mapping is total in the other direction and that is the point: opencode
/// offers exactly `once`, `always` and `reject`, which is why an `approval`
/// part from this adapter always carries exactly three answers.
pub fn opencode_reply(decision: Decision) -> Option<&'static str> {
    match decision {
        Decision::Allow => Some("once"),
        Decision::AllowAlways => Some("always"),
        Decision::Deny => Some("reject"),
        Decision::Other => None,
    }
}

/// What a pane is blocked on according to its agent's own protocol.
///
/// The drawn-menu detector has to prove a menu is still standing before it
/// answers, because a terminal only ever shows the last frame. A protocol says
/// so directly, and it says which request -- which is why answering natively
/// names an id and never a cursor position.
pub struct NativeApproval {
    pub adapter: &'static Adapter,
    pub base: String,
    pub session: String,
    pub request: ApprovalRequest,
}

/// The pending request for this pane, if the agent reports one.
pub async fn pending(adapter: &Adapter, root: Option<&std::path::Path>) -> Option<NativeApproval> {
    let base = endpoint(adapter)?;
    let root = root?;
    if adapter.id != "opencode" {
        return None;
    }
    let directory = root.to_string_lossy().to_string();
    let sessions: Value = get(client(), &base, "/session", &[("directory", &directory)]).await?;
    let session = newest_session(&sessions)?;
    let pending: Value = get(client(), &base, "/permission", &[("directory", &directory)]).await?;
    let request = pending
        .as_array()?
        .iter()
        .find(|request| request.get("sessionID").and_then(Value::as_str) == Some(&session))
        .and_then(opencode_approval)?;
    Some(NativeApproval {
        adapter: &OPENCODE,
        base,
        session,
        request,
    })
}

/// Answer a pending request through the protocol. `true` when the agent
/// accepted the reply.
pub async fn answer(approval: &NativeApproval, decision: Decision) -> Option<bool> {
    let reply = match approval.adapter.id {
        "opencode" => opencode_reply(decision)?,
        _ => return None,
    };
    let response = client()
        .post(format!(
            "{}/session/{}/permissions/{}",
            approval.base, approval.session, approval.request.id
        ))
        .json(&serde_json::json!({ "response": reply }))
        .send()
        .await
        .ok()?;
    Some(response.status().is_success())
}

/// Result lines, capped in both directions, with whether anything was cut.
fn clamp(text: &str) -> (Vec<String>, bool) {
    let mut truncated = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.trim_end().lines() {
        if lines.len() == MAX_RESULT_LINES {
            truncated = true;
            break;
        }
        if line.chars().count() > MAX_RESULT_LINE_CHARS {
            truncated = true;
            lines.push(line.chars().take(MAX_RESULT_LINE_CHARS).collect());
        } else {
            lines.push(line.to_owned());
        }
    }
    (lines, truncated)
}

/// A tool's input when the protocol offered no display line for it: the JSON,
/// on one line, so the block still says what it was called with.
fn compact_input(input: &Value) -> String {
    let rendered = serde_json::to_string(input).unwrap_or_default();
    if rendered.chars().count() > MAX_RESULT_LINE_CHARS {
        rendered.chars().take(MAX_RESULT_LINE_CHARS).collect()
    } else {
        rendered
    }
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// -- reading a live server -------------------------------------------------

/// A native read is on the critical path of a phone opening a pane, so it gets
/// a hard deadline: a server that is slow is a server that is not there, and
/// the dictionary answer is already in hand.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// The one entry point the endpoint calls: whichever adapter covers this agent,
/// asked about this pane's workspace. `None` at any step -- no adapter, no
/// configured endpoint, no workspace, nothing listening, no session -- means
/// the caller uses the dictionary, which is always available.
pub async fn read(
    adapter: &Adapter,
    root: Option<&std::path::Path>,
    limit: usize,
) -> Option<NativeRead> {
    let base = endpoint(adapter)?;
    let root = root?;
    match adapter.id {
        "opencode" => opencode_read(client(), &base, root, limit).await,
        _ => None,
    }
}

/// Read a pane's transcript out of an opencode server.
///
/// Every step is allowed to fail into `None`: an endpoint that is not up, a
/// build too old to answer `/global/health`, a workspace with no session. The
/// caller falls back to the dictionary, so a native read can add structure and
/// can never take a pane away.
pub async fn opencode_read(
    client: &reqwest::Client,
    base: &str,
    root: &std::path::Path,
    limit: usize,
) -> Option<NativeRead> {
    let health: Value = get(client, base, "/global/health", &[]).await?;
    if health.get("healthy").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let version = health
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // The pane's own working directory is what ties it to a session: opencode
    // scopes its sessions by directory, and the pane's cwd is the same fence
    // the assets and file-search APIs are gated on.
    let directory = root.to_string_lossy().to_string();
    let sessions: Value = get(client, base, "/session", &[("directory", &directory)]).await?;
    let session = newest_session(&sessions)?;

    let messages: Value = get(
        client,
        base,
        &format!("/session/{session}/message"),
        &[("directory", &directory), ("limit", &limit.to_string())],
    )
    .await?;

    // Pending permissions are listed server-wide; only this session's are this
    // pane's business.
    let pending: Vec<Value> = get(client, base, "/permission", &[("directory", &directory)])
        .await
        .and_then(|value: Value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.get("sessionID").and_then(Value::as_str) == Some(&session))
        .collect();

    // The rendered transcript the spans index into is not put on the wire: a
    // client rebuilds it by joining the parts' `fallback_text`, which is the
    // same thing byte for byte. That equality is what the invariant tests
    // assert, so it is a fact about the format rather than a convention.
    let (parts, _) = opencode_normalize(&messages, &pending);
    Some(NativeRead {
        parts: parts.iter().map(Part::to_json).collect(),
        session: Some(session),
        version,
    })
}

/// The session this pane is most likely showing: the one touched last.
fn newest_session(sessions: &Value) -> Option<String> {
    sessions
        .as_array()?
        .iter()
        .filter(|session| session.get("parentID").is_none())
        .max_by_key(|session| {
            session
                .pointer("/time/updated")
                .and_then(Value::as_u64)
                .or_else(|| session.pointer("/time/created").and_then(Value::as_u64))
                .unwrap_or(0)
        })
        .and_then(|session| session.get("id").and_then(Value::as_str))
        .map(str::to_owned)
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Option<Value> {
    let response = client
        .get(format!("{base}{path}"))
        .query(query)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::normalize;
    use serde_json::json;

    /// A real opencode 1.18.0 session, captured off the server this adapter is
    /// written against: a todo write, a shell command, a file read, a file
    /// edit, and the agent's own prose.
    const SESSION: &str = include_str!("../tests/fixtures/opencode-native-session.json");
    /// The same server with a permission pending, captured by giving a session
    /// a `bash -> ask` rule and asking it to run a command.
    const APPROVAL: &str = include_str!("../tests/fixtures/opencode-native-approval.json");
    const SNAPSHOT: &str = include_str!("../tests/fixtures/opencode-native-parts.json");
    /// The terminal capture the marker dictionary reads, for the comparison
    /// that says what a native source buys.
    const TRANSCRIPT: &str = include_str!("../tests/fixtures/opencode-transcript.txt");

    fn read(fixture: &str) -> (Vec<Part>, String) {
        let value: Value = serde_json::from_str(fixture).expect("fixture parses");
        let pending: Vec<Value> = value
            .get("pending_permissions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        opencode_normalize(value.get("messages").unwrap_or(&Value::Null), &pending)
    }

    fn kinds(parts: &[Part]) -> Vec<&'static str> {
        parts.iter().map(Part::kind).collect()
    }

    #[test]
    fn an_agent_resolves_to_an_adapter_by_the_same_substring_rule_as_a_dictionary() {
        assert_eq!(adapter_for(Some("opencode")).unwrap().id, "opencode");
        assert_eq!(adapter_for(Some("herdr:opencode")).unwrap().id, "opencode");
        assert_eq!(adapter_for(Some("open-code")).unwrap().id, "opencode");
        // Claude Code has no protocol at all, and Codex's is not wired yet:
        // both stay on their dictionaries, which is a supported answer.
        assert!(adapter_for(Some("claude")).is_none());
        assert!(adapter_for(Some("codex")).is_none());
        assert!(adapter_for(Some("")).is_none());
        assert!(adapter_for(None).is_none());
    }

    #[test]
    fn a_native_adapter_never_claims_a_pane_the_dictionary_does_not_also_cover() {
        // The adapter is an upgrade to a dictionary, never a replacement for
        // having one: if the endpoint is down the pane still has to normalize.
        for (aliases, _) in ADAPTERS {
            for alias in *aliases {
                assert!(
                    crate::parts::dictionary_for(Some(alias)).is_some(),
                    "{alias} has an adapter and no dictionary to fall back to"
                );
            }
        }
    }

    #[test]
    fn a_live_session_normalizes_into_the_same_closed_part_set() {
        let (parts, _) = read(SESSION);
        let mut seen = kinds(&parts);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, ["diff", "prompt", "text", "todo", "tool-block"]);
    }

    #[test]
    fn a_shell_tool_takes_its_status_from_the_exit_code_the_terminal_threw_away() {
        let (parts, _) = read(SESSION);
        let bash = parts
            .iter()
            .map(Part::to_json)
            .find(|part| part["tool"] == "bash")
            .expect("the session ran a shell command");
        assert_eq!(bash["input"], "wc -l Cargo.toml");
        assert_eq!(bash["result"][0], "      28 Cargo.toml");
        assert_eq!(bash["status"], "ok");

        // The dictionary can only read `Error…` off the first result line; the
        // protocol says so outright.
        let failed = json!({ "messages": [{ "info": { "role": "assistant" }, "parts": [{
            "type": "tool", "tool": "bash",
            "state": { "status": "completed", "title": "false", "output": "",
                       "metadata": { "exit": 1, "output": "" } }
        }] }] });
        let (parts, _) = opencode_normalize(failed.get("messages").unwrap(), &[]);
        assert_eq!(parts[0].to_json()["status"], "error");
    }

    #[test]
    fn an_edit_carries_its_patch_as_a_diff_beside_the_block() {
        let (parts, _) = read(SESSION);
        let json: Vec<Value> = parts.iter().map(Part::to_json).collect();
        let block = json
            .iter()
            .position(|part| part["tool"] == "edit")
            .expect("the session edited a file");
        let diff = &json[block + 1];
        assert_eq!(diff["type"], "diff");
        assert_eq!(diff["file"], "/tmp/native-probe.txt");
        let hunks: Vec<&str> = diff["hunks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(hunks.contains(&"-hello"), "{hunks:?}");
        assert!(hunks.contains(&"+hello world"), "{hunks:?}");
    }

    #[test]
    fn a_todo_write_is_a_checklist_rather_than_a_tool_block() {
        let (parts, _) = read(SESSION);
        let todo = parts
            .iter()
            .map(Part::to_json)
            .find(|part| part["type"] == "todo")
            .expect("the session wrote todos");
        let items = todo["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["text"], "Plan three steps using the todo tool");
        // in_progress is not done. A dictionary reading `☐`/`☑` off the screen
        // cannot see the difference between pending and in flight either, but
        // here the distinction is at least made from data.
        assert_eq!(items[0]["done"], false);
        // The last write of the session ticked the first two off.
        let last = parts
            .iter()
            .map(Part::to_json)
            .rfind(|part| part["type"] == "todo")
            .unwrap();
        assert_eq!(last["items"][0]["done"], true);
    }

    #[test]
    fn the_users_own_message_is_a_prompt_and_the_agents_is_text() {
        let (parts, _) = read(SESSION);
        assert_eq!(parts[0].kind(), "prompt");
        assert!(parts[0].to_json()["text"]
            .as_str()
            .unwrap()
            .starts_with("Use your todo tool"));
        assert!(parts.iter().any(|part| part.kind() == "text"));
    }

    #[test]
    fn a_pending_permission_becomes_an_approval_part_with_the_protocols_own_answers() {
        let (parts, _) = read(APPROVAL);
        let approval = parts
            .iter()
            .map(Part::to_json)
            .find(|part| part["type"] == "approval")
            .expect("the fixture was captured while a permission was pending");
        assert_eq!(approval["approval_id"], "per_f9f28b0d4001QKMLkVCekhlekI");
        assert_eq!(approval["tool"], "bash");
        assert_eq!(approval["prompt"], "Allow bash?");
        assert_eq!(approval["context"][0], "echo native-approval-probe");
        // Exactly the three replies `POST .../permissions/{id}` accepts, in the
        // decision vocabulary the drawn-menu detector already speaks.
        let decisions: Vec<&str> = approval["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|option| option["decision"].as_str())
            .collect();
        assert_eq!(decisions, ["allow", "allow_always", "deny"]);
        assert_eq!(opencode_reply(Decision::Allow), Some("once"));
        assert_eq!(opencode_reply(Decision::AllowAlways), Some("always"));
        assert_eq!(opencode_reply(Decision::Deny), Some("reject"));
        assert_eq!(opencode_reply(Decision::Other), None);
    }

    #[test]
    fn an_approval_part_quotes_no_wording_the_agent_wrote() {
        // The same privacy rule the push payloads follow: the labels are the
        // gateway's, so an agent that embeds a command in its own prompt text
        // cannot leak it through a part a client renders as a button row.
        let (parts, _) = read(APPROVAL);
        let approval = parts
            .iter()
            .map(Part::to_json)
            .find(|part| part["type"] == "approval")
            .unwrap();
        let labels: Vec<&str> = approval["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|option| option["label"].as_str())
            .collect();
        assert_eq!(labels, ["Approve", "Approve and don't ask again", "Deny"]);
    }

    #[test]
    fn a_tool_still_running_is_running_and_a_blocked_one_is_too() {
        let (parts, _) = read(APPROVAL);
        let blocked = parts
            .iter()
            .map(Part::to_json)
            .find(|part| part["type"] == "tool-block")
            .expect("the fixture caught a tool waiting on its permission");
        assert_eq!(blocked["status"], "running");
        assert_eq!(blocked["result"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_call_that_has_not_run_yet_still_says_what_it_was_called_with() {
        // A blocked or in-flight call has no display line, because opencode
        // writes one when the tool runs. Falling straight to the input JSON
        // would make the fallback text worse than the terminal it replaces.
        let value = json!([{ "info": { "role": "assistant" }, "parts": [
            { "type": "tool", "tool": "bash",
              "state": { "status": "running", "input": { "command": "echo hi" } } },
            { "type": "tool", "tool": "read",
              "state": { "status": "pending", "input": { "filePath": "src/main.rs" }, "raw": "" } },
            { "type": "tool", "tool": "mystery",
              "state": { "status": "running", "input": { "shape": "unknown" } } }
        ] }]);
        let (parts, _) = opencode_normalize(&value, &[]);
        let rendered: Vec<Value> = parts.iter().map(Part::to_json).collect();
        assert_eq!(rendered[0]["input"], "echo hi");
        assert_eq!(rendered[1]["input"], "src/main.rs");
        // A tool nobody has a display key for still says something rather than
        // nothing, which is the whole point of the fallback.
        assert_eq!(rendered[2]["input"], "{\"shape\":\"unknown\"}");
        for part in &rendered {
            assert_eq!(part["status"], "running");
        }
    }

    #[test]
    fn a_long_tool_result_is_capped_and_says_so() {
        let long = (1..=200)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let value = json!([{ "info": { "role": "assistant" }, "parts": [{
            "type": "tool", "tool": "read",
            "state": { "status": "completed", "title": "big.txt", "output": long,
                       "metadata": {} }
        }] }]);
        let (parts, _) = opencode_normalize(&value, &[]);
        let block = parts[0].to_json();
        assert_eq!(block["result"].as_array().unwrap().len(), MAX_RESULT_LINES);
        assert_eq!(block["truncated"], true);
    }

    #[test]
    fn the_parts_opencode_draws_its_own_renderer_with_are_not_content() {
        // step-start and friends have no text a user would read. Skipping them
        // is what keeps a native transcript the same length as the pane.
        let value = json!([{ "info": { "role": "assistant" }, "parts": [
            { "type": "step-start" },
            { "type": "step-finish", "reason": "stop" },
            { "type": "snapshot", "snapshot": "abc" },
            { "type": "agent", "name": "build" },
            { "type": "text", "text": "done" }
        ] }]);
        let (parts, _) = opencode_normalize(&value, &[]);
        assert_eq!(kinds(&parts), ["text"]);
    }

    #[test]
    fn injected_context_is_not_the_agent_talking() {
        let value = json!([{ "info": { "role": "user" }, "parts": [
            { "type": "text", "text": "<system>rules</system>", "synthetic": true },
            { "type": "text", "text": "real question" }
        ] }]);
        let (parts, _) = opencode_normalize(&value, &[]);
        assert_eq!(kinds(&parts), ["prompt"]);
        assert_eq!(parts[0].to_json()["text"], "real question");
    }

    // -- the two invariants, on the native path --------------------------

    /// A part's span names the rows it owns of the transcript the adapter
    /// rendered, and its `fallback_text` is those rows byte for byte. Exactly
    /// the assertion `parts.rs` makes about a dictionary read, against exactly
    /// the same [`Part`] type -- which is the point of routing an adapter
    /// through the assembler rather than letting it build parts by hand.
    fn assert_spans(parts: &[Part], transcript: &str) {
        let source: Vec<&str> = transcript.lines().collect();
        let mut previous = None;
        for part in parts {
            let value = part.to_json();
            let start = value["range"]["start"].as_u64().unwrap() as usize;
            let end = value["range"]["end"].as_u64().unwrap() as usize;
            assert!(start <= end);
            assert!(end < source.len(), "span runs past the rendered transcript");
            if let Some(previous) = previous {
                assert!(start > previous, "span runs backwards");
            }
            assert_eq!(
                value["fallback_text"].as_str().unwrap(),
                source[start..=end].join("\n"),
                "fallback_text is not its own rows verbatim"
            );
            previous = Some(end);
        }
    }

    fn assert_covers_every_line(parts: &[Part], transcript: &str) {
        let source: Vec<&str> = transcript
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let covered: Vec<&str> = parts
            .iter()
            .flat_map(|part| part.fallback_text().lines())
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(covered, source, "a native read lost or reordered a line");
    }

    #[test]
    fn a_native_read_holds_both_invariants() {
        for fixture in [SESSION, APPROVAL] {
            let (parts, transcript) = read(fixture);
            assert!(!parts.is_empty());
            assert_spans(&parts, &transcript);
            assert_covers_every_line(&parts, &transcript);
        }
    }

    #[test]
    fn an_empty_session_is_an_empty_read_and_not_an_error() {
        let (parts, transcript) = opencode_normalize(&json!([]), &[]);
        assert!(parts.is_empty());
        assert!(transcript.is_empty());
        // A protocol that grew a shape nobody here knows degrades the same way.
        let (parts, _) = opencode_normalize(&json!("not an array"), &[]);
        assert!(parts.is_empty());
    }

    /// The card's real question: what does reading the protocol buy over
    /// reading the screen? Both sources are the same agent, so the answer has
    /// to be measurable rather than asserted.
    #[test]
    fn the_native_read_types_strictly_more_than_the_dictionary_read() {
        let (native, _) = read(SESSION);
        let dictionary = normalize(TRANSCRIPT, crate::parts::dictionary_for(Some("opencode")));
        let share = |parts: &[Part]| {
            let typed = parts.iter().filter(|part| part.kind() != "text").count();
            (typed, parts.len())
        };
        let (native_typed, native_total) = share(&native);
        let (table_typed, table_total) = share(&dictionary);
        eprintln!(
            "opencode: dictionary typed {table_typed}/{table_total}, native typed \
             {native_typed}/{native_total}"
        );
        // The dictionary cannot produce a diff or a todo from opencode's TUI at
        // all -- opencode prints neither in a shape a marker table can read --
        // and the native read produces both.
        assert!(!kinds(&dictionary).contains(&"diff"));
        assert!(!kinds(&dictionary).contains(&"todo"));
        assert!(kinds(&native).contains(&"diff"));
        assert!(kinds(&native).contains(&"todo"));
        assert!(
            native_typed * 4 >= native_total,
            "an adapter must buy its keep"
        );
    }

    /// Pinned per opencode release, the same discipline the dictionaries are
    /// under: a protocol that moves a field has to show up as a reviewable diff
    /// rather than as a quiet loss of structure. Re-pin with
    /// `UPDATE_PART_SNAPSHOTS=1 cargo test`.
    #[test]
    fn the_live_session_normalizes_to_its_pinned_snapshot() {
        let (parts, _) = read(SESSION);
        let actual =
            serde_json::to_string_pretty(&parts.iter().map(Part::to_json).collect::<Vec<_>>())
                .unwrap();
        if std::env::var_os("UPDATE_PART_SNAPSHOTS").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/opencode-native-parts.json");
            std::fs::write(&path, format!("{actual}\n")).unwrap();
            return;
        }
        assert_eq!(
            actual.trim(),
            SNAPSHOT.trim(),
            "the opencode mapping drifted"
        );
    }

    /// The endpoint is configuration, and an absent one is the normal case.
    #[test]
    fn an_endpoint_is_read_from_the_environment_and_may_simply_not_be_there() {
        assert_eq!(OPENCODE.endpoint_env, "HERDR_GATEWAY_OPENCODE_URL");
        // Every adapter's variable has to be its own, or one server would
        // answer for another agent's pane.
        let mut vars: Vec<&str> = ADAPTERS
            .iter()
            .map(|(_, adapter)| adapter.endpoint_env)
            .collect();
        let count = vars.len();
        vars.sort_unstable();
        vars.dedup();
        assert_eq!(vars.len(), count);
    }
}
