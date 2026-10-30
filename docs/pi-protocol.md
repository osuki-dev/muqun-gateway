# Pi RPC adapter

The `pi` agent drives a locally installed [Pi](https://pi.dev) coding agent
through its RPC mode. Verified against Pi **1.4.0**; the sources are the docs
shipped with the install (`docs/rpc.md`, `rpc-commands.md`, `json.md`,
`rpc-extension-ui.md`, `session-format.md`, `extensions.md`). No App change is
required: the App renders the agent from discovery.

## Configuration

`config.json` block `pi` (all keys optional; the block is omitted while it is
the default):

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Attach when a `pi` binary is found. Nothing runs until a session is used. |
| `binary` | `pi` on `PATH` | A value with a path separator is a file path. |
| `session_dir` | `PI_CODING_AGENT_SESSION_DIR`, else `~/.pi/agent/sessions` | Read-only root of Pi's session files. Must match where Pi writes. |
| `approvals` | `ask` | `ask`: `bash`/`edit`/`write` wait for an App approval. `never`: no gate. Any other value asks. |
| `idle_timeout_secs` | `900` | A child with no run and no command for this long is closed. |

Discovery: id/kind `pi`, name `Pi`, endpoint `stdio://pi`, version from
`pi --version`. `disabled` when `enabled` is false, `not_installed` when no
binary is found, `reachable` before the first supervision pass, `connected`
once attached. It ranks last in `AGENT_PREFERENCE`.

## Process model

- Session id = the session header `id` (a UUID). Session files are
  `<root>/--<cwd>--/<timestamp>_<id>.jsonl`, or flat in the root when Pi's
  session directory was overridden. Listings, titles, models and history are
  read from the files without a child.
- A child is started lazily for the first write to a session (prompt, model
  switch, rename, compact) or by `create_session`:
  `pi --mode rpc --session <file> -e <state_dir>/pi/muqun-extension.js`, with
  `current_dir` = the header `cwd`. A new session is
  `pi --mode rpc -e <ext>` in the requested directory, then `get_state` for
  `sessionId`/`sessionFile`. Pi writes the file only with the first message, so
  until then the session is answered from the driver's live state.
- Child environment: `MUQUN_MCP_URL` and `MUQUN_MCP_TOKEN` (a runtime token
  minted per attach, agent id `pi`) when MCP is on, and `MUQUN_PI_APPROVALS`.
- Framing: LF-terminated JSON objects, optional CR dropped, 16 MiB per record
  (a longer one kills the child). Commands carry a string `id`; the matching
  `response` resolves it (60 s timeout). Stderr is logged, never parsed.
- Idle: children are touched on every record and command; the reaper (every
  30 s) never closes a child whose session is running (a long silent tool call)
  or whose session file does not exist yet (a new session before its first
  message: the child is all there is of it). Idle children get stdin closed,
  then a kill after 5 s. Manager shutdown closes every child.
- The extension is written to `<state_dir>/pi/` (owner-only directory, atomic
  rename, a symlink at the file is removed, a symlinked directory refused).
  Without a state directory Pi is not attached (discovery `offline`, reason
  "no state directory for the Pi extension"): Pi executes that file, so it
  never goes to a shared temp path.
- The model catalog comes from a throwaway
  `pi --mode rpc --no-session --no-extensions` child
  (`get_available_models`, `get_available_thinking_levels`, `get_state`),
  cached 60 s.

## Live events and re-keying

The driver subscribes to every child and forwards each record, tagged with the
session, to the manager (`agents/manager/pi.rs`), which runs it through the
driver's per-session `LiveFolder` and applies the result to the mirror.

Pi 1.4.0 sends no entry id for a live message (`entry_appended` is emitted only
for extension entries). Live rows are keyed `live-<n>[:<contentIndex>]`; after
every `message_end`, `tool_execution_start`, `extension_ui_request`,
`turn_end` and `agent_settled` the driver reads the session file from the
last byte it saw (Pi prints `message_end` before it appends the entry, so the
later records catch a lost race before a tool run or an approval wait) and feeds each new `message` entry back as a synthetic
`{"type":"entry_appended","entry":…}`. The folder matches it on role and
message timestamp, removes the provisional rows and upserts them under the
entry id, so a later file read produces the same rows. Row `message_id` is
`<message timestamp ms, 20 digits>:<entry id>` so the mirror's ordering
follows creation time.

A child that exits during a run produces a synthetic
`agent_settled{aborted:true}`, and its open approvals are resolved. When a
write respawns a dead child before its forwarder noticed, the driver publishes
a synthetic `muqun_discard` naming the old child's open approvals and
provisional rows, so the mirror drops them. `agent_settled` resolves any
approval still open. Records for a session with no live state (one just
deleted) are dropped.

## Extension contract

`muqun-extension.js` (embedded, rewritten at every attach):

- With `MUQUN_MCP_URL` and `MUQUN_MCP_TOKEN` set, registers the MCP server
  `muqun` with a bearer header.
- Unless `MUQUN_PI_APPROVALS=never`, on `tool_call` for `bash`, `edit`, `write`
  (top-level calls) asks
  `ctx.ui.confirm("muqun.permission", JSON.stringify({tool, toolCallId, input}))`
  with no timeout and `{signal: ctx.signal}`, and blocks the call when the
  answer is false. Stop (`abort`) fires that signal: Pi resolves the dialog
  `false` without sending anything, the call is blocked and the run settles
  with `aborted:true`, which drops the card.

On the wire this is
`{"type":"extension_ui_request","id":…,"method":"confirm","title":"muqun.permission","message":"<json>"}`,
answered with `{"type":"extension_ui_response","id":…,"confirmed":true|false}`.
Any other dialog (`select`, `confirm`, `input`, `editor`) is answered
`{"cancelled":true}` at once; fire-and-forget UI methods are ignored. The reply
the gateway sends is also folded, which resolves the pending approval.

## Mapping

| AgentPort | Pi |
|---|---|
| `probe` | healthy while any child is alive; otherwise `pi --version` succeeds (cached 60 s) |
| `list_projects` | distinct header `cwd`; `name` = last segment; `missing` when the directory is gone |
| `list_sessions` | session files (one bucket with a directory filter), newest first (`order=asc` reverses); title = latest `session_info.name`, else first user text (80 chars), else `New session`; model from the last `model_change`/`thinking_level_change`; status `Busy` while the live child runs, else `Idle`; `search` matches the title; a parent filter other than top-level matches nothing |
| `get_session` | the same, one file; a fresh session without a file from live state |
| `create_session(dir, model, _)` | new child in `dir`; `set_model` (+ `set_thinking_level` for a variant); `get_state` |
| `send_prompt` | attachments as host paths in the text (`ByPath`); idle → `prompt`; running → `steer` for delivery `steer`, else `prompt` with `streamingBehavior:"followUp"`; a "streaming" refusal of an idle-looking session is re-sent once with `followUp` (the refusal precedes acceptance) |
| `interrupt` | `abort`; no child means no run |
| `switch_model` | `set_model {provider, modelId}`; `variant` → `set_thinking_level {level}` |
| `reply_permission` | `extension_ui_response {id, confirmed}`; `AllowAlways` acts as `Allow`; a reason text is refused |
| `get_pending_permissions` | the driver's live pending map |
| `get_timeline(limit)` | active branch of the file (follow `parentId` from the last entry) mapped, merged with live rows not yet in the file, last `limit` |
| `rename_session` | `set_session_name` (child started if needed) |
| `delete_session` | close the child, remove the file (only under the session root) |
| `compact_session` | `compact` sent in the background (it answers only when done); progress via `compaction_start`/`compaction_end`; returns `{"started":true}` |
| `get_catalog` | probe child, see above; thinking levels become variants of reasoning models |
| `get_pending_forms` | empty |
| `switch_mode`, `revert_session`, `fork_session`, `find_files`, `get_vcs_diff`, `reply_form` | `Unsupported` |

Live record → change: `agent_start` Busy; `agent_settled` Idle or
Interrupted; `auto_retry_start` Retry; `message_update` text/thinking deltas
and `toolcall_end` rows; `tool_execution_start/end` tool state; `message_end`
the authoritative rows of the message; `compaction_start/end` compaction
status; `session_info_changed` title; usage from `message_update.usage` and
`message_end.message.usage`.

Features: streaming, reasoning effort, model selection, compaction,
attachments by path, session rename and delete; tool approvals when
`approvals` asks. No modes, skills, slash commands, revert, fork, inbox,
worktrees or pagination.
