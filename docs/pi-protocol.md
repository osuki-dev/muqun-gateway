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
| `enabled` | `true` | Attach when a `pi` binary is found. Session children start on use; discovery and catalogs may run short probes. |
| `binary` | `pi` on `PATH` | A value with a path separator is a file path. |
| `session_dir` | `PI_CODING_AGENT_SESSION_DIR`, else `~/.pi/agent/sessions` | Pi's session-file root. Must match where Pi writes. Discovery/history read existing files; explicit clone/delete operations create or remove files. |
| `approvals` | `ask` | `ask`: `bash`/`edit`/`write` wait for an App approval. `never`: no gate. Any other value asks. |
| `idle_timeout_secs` | `900` | A child with no run and no command for this long is closed. |

Discovery: id/kind `pi`, name `Pi`, endpoint `stdio://pi`, version from
`pi --version`. `disabled` when `enabled` is false, `not_installed` when no
binary is found, `reachable` before the first supervision pass, `connected`
once attached. It ranks last in `AGENT_PREFERENCE`.

`muqun-gateway agent setup pi` prints host setup and configuration guidance.
It does not start Pi, change configuration, install packages, start a login
flow or restart Gateway. Providers and authentication are configured in Pi
on the host; discovery alone does not verify model authentication.

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
- Framing: LF-terminated JSON objects, optional CR dropped, 1 MiB per record
  (a longer one kills the child). Commands carry a string `id`; the matching
  `response` resolves it (60 s timeout). Writes have a 5 s deadline. An
  interrupted write or timed-out request closes the transport, reports an
  unknown outcome and is never automatically replayed. Native diagnostics
  are drained without retaining their contents; upstream errors returned to
  the App use fixed messages.
- Idle: children are touched on every record and command; the reaper (every
  30 s) never closes a child whose session is running (a long silent tool call)
  or whose session file does not exist yet (a new session before its first
  message: the child is all there is of it). Idle children get stdin closed,
  then a kill after 5 s. Manager shutdown closes every child.
- Children use an owned process group on Unix. Cleanup kills remaining tool
  descendants before reaping the leader, so its process-group identity
  cannot be reused during cleanup. Shutdown includes the stdin-lock wait in
  its deadline.
- All children and both event queues share a 16 MiB raw-record byte budget;
  each queue also holds at most 32 records. Exceeding a byte budget or losing
  a child event stops that child. Losing manager events stops every Pi child
  and explicitly resolves pending approvals before requesting a resync.
  Limits also include 8 live children, 64 cached session folders, and 4 MiB
  of conservatively accounted retained payload per folder. A folder overflow
  fails that session. Accounting traverses retained payloads without
  serializing the entire folder on every text delta.
- History reads retain at most 16 MiB per file/tail and 1 MiB per entry;
  headers are bounded at 64 KiB. Discovery scans at most 4,096 session files
  and 1,024 directories. Reads reject non-regular files and final symlinks;
  Unix opens are nonblocking so a FIFO cannot hold session discovery open.
  A history read above its budget returns an explicit error.
- The extension is written to `<state_dir>/pi/` (owner-only directory, atomic
  rename, a symlink at the file is removed, a symlinked directory refused).
  Without a state directory Pi is not attached (discovery `offline`, reason
  "no state directory for the Pi extension"): Pi executes that file, so it
  never goes to a shared temp path.
- The model catalog comes from a throwaway
  `pi --mode rpc --no-session --no-extensions` child
  (`get_available_models`, `get_available_thinking_levels`, `get_state`),
  cached 60 s.
- Temporary catalog and clone probes have separate lifecycle ownership from
  session children. Cancellation terminates a probe, and its handle stays
  registered until shutdown or reaping can await its exit. Gateway shutdown
  waits for admissions and owned children; stopped supervision cannot attach
  another Pi runtime.

## Session inventory and metadata

Session summaries use a separate streaming projection. It retains entry
identity and parent links, titles, model/effort changes and usage counters,
while discarding image data and conversation bodies from the summary. A
valid large image entry therefore does not prevent the whole session list
from loading. Opening that session's history still uses the history budgets
above and may return a size error.

Each summary scan allows 128 MiB of source data, 8 MiB per record, 32,768
projected entries and 16 MiB of accounted retained metadata. An incomplete
scan keeps the session visible with a metadata error and leaves model,
tokens and cost unknown; it does not present partial totals as complete.
At most 64 summaries are cached by file path, precise modification time and
length. An appended model change, rename or message invalidates that file's
cached summary. Live status and authoritative native statistics are applied
after the cached file summary.

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

A newly created session may have no `sessionFile` before its first message.
Tail reads discover the matching native file after it is written and bind
it only to the same child generation. If a persisted entry is observed
before its live echo, its entry identity is retained and reused when that
echo arrives. Identical text in distinct native entries remains distinct.

Live timeline events are emitted from the mirror's stamped upsert result.
Their item revisions match the event sequence and snapshot revisions, so a
follow-up can acknowledge the App's optimistic row after earlier events.
Model changes likewise publish the actual normalized `get_state` selection
to the mirror rather than leaving the toolbar and session snapshot divergent.

A child that exits during a run produces a synthetic
`agent_settled{aborted:true}`, and its open approvals are resolved. When a
write respawns a dead child before its forwarder noticed, the driver publishes
a synthetic `muqun_discard` naming the old child's open approvals and
provisional rows, so the mirror drops them. `agent_settled` resolves any
approval still open. Records carry the child's generation through both
queues. Folding and tail-offset updates reject an older generation, including
records already queued before a replacement. Records for a deleted session
are dropped.

## Extension contract

`muqun-extension.js` (embedded, rewritten at every attach):

- With `MUQUN_MCP_URL` and `MUQUN_MCP_TOKEN` set, registers the MCP server
  `muqun` with a bearer header.
- Unless `MUQUN_PI_APPROVALS=never`, on `tool_call` for `bash`, `edit`, `write`
  (including nested calls) asks
  `ctx.ui.confirm("muqun.permission", JSON.stringify({tool, toolCallId, input}))`
  with no timeout and `{signal: ctx.signal}`, and blocks the call when the
  answer is false. Stop (`abort`) fires that signal: Pi resolves the dialog
  `false` without sending anything, the call is blocked and the run settles
  with `aborted:true`, which drops the card.
- RPC mode reports `ctx.hasUI=true`; its dialogs use the RPC subprotocol.
  Missing UI, a failed bridge, an exception, cancellation or any answer
  other than the boolean `true` blocks the tool. MCP registration failure
  cannot remove the installed gate. No approval expires into an allow.
- A permission payload must identify `bash.command`, `edit.path` or
  `write.path` as a nonempty string. Unknown tools and malformed or empty
  resources are cancelled instead of becoming unscoped permission cards.

On the wire this is
`{"type":"extension_ui_request","id":…,"method":"confirm","title":"muqun.permission","message":"<json>"}`,
answered with `{"type":"extension_ui_response","id":…,"confirmed":true|false}`.
Any other dialog (`select`, `confirm`, `input`, `editor`) is answered
`{"cancelled":true}` at once; fire-and-forget UI methods are ignored. The reply
the gateway sends is also folded, which resolves the pending approval.

## Mapping

| AgentPort | Pi |
|---|---|
| `probe` | healthy while a session child is alive; otherwise `pi --version` succeeds (cached 60 s) |
| `list_projects` | distinct header `cwd`; `name` = last segment; `missing` when the directory is gone |
| `list_sessions` | session files (one bucket with a directory filter), newest first (`order=asc` reverses); title = latest `session_info.name`, else first user text (80 chars), else `New session`; model from the last `model_change`/`thinking_level_change`; status `Busy` while the live child runs, else `Idle`; `search` matches the title; a parent filter other than top-level matches nothing |
| `get_session` | the same, one file; a fresh session without a file from live state |
| `create_session(dir, model, _)` | new child in `dir`; `set_model` (+ `set_thinking_level` for a variant); `get_state` |
| `send_prompt` | attachments as host paths in the text (`ByPath`); idle → `prompt`; running → `steer` for delivery `steer`, else `prompt` with `streamingBehavior:"followUp"`; failures are returned without replay |
| `interrupt` | `abort`; no child means no run. Pi may continue existing native queued messages after abort; the Gateway preserves those messages rather than clearing another client's queue |
| `switch_model` | `set_model {provider, modelId}`; `variant` → `set_thinking_level {level}`; read `get_state` afterward and retain the actual normalized model/level, including before the first session-file write |
| `reply_permission` | `extension_ui_response {id, confirmed}`; accepts only one reply while that generation's request is pending; persistent permission and reason text are refused |
| `get_pending_permissions` | the driver's live pending map |
| `get_timeline(limit)` | active branch of the file (follow `parentId` from the last entry) mapped, merged with live rows not yet in the file, last `limit` |
| `prepare_timeline` / `prepare_earlier_timeline` | file-backed row-id cursor; snapshot and older-page boundaries move only when the mirror accepts the candidate. Snapshot refresh preserves an older accepted boundary. A lost anchor or concurrent stale commit reports `HistoryCursorLost`; empty means the actual active-branch start |
| `rename_session` | `set_session_name` (child started if needed) |
| `delete_session` | close the child, remove the file (only under the session root) |
| `compact_session` | `compact` sent in the background (it answers only when done); progress via `compaction_start`/`compaction_end`; returns `{"started":true}` |
| `get_catalog` | model probe child, see above; thinking levels become variants of reasoning models. Commands come from a live child in the requested directory, or an isolated extension-free probe there. Cold command results are cached per directory for 30 s, at most 64 directories; a matching live child is always queried directly. The cold probe lists only the templates/skills it observes; extension commands become available with a live session |
| `run_command` / `execute_command` | validate against that session's current `get_commands`, then submit `/name arguments` through `prompt`, including when busy. Built-in TUI-only commands are not offered |
| `fork_session` | whole-session native `clone` runs against an owner-only private source snapshot, with output kept private until the probe exits. The new header points to the original source, and a complete new file is published atomically into the source bucket without replacing an existing file. The source file and source child are preserved. Forking at a message boundary remains unsupported |
| `get_pending_forms` | empty |
| `switch_mode`, `revert_session`, `find_files`, `get_vcs_diff`, `reply_form` | `Unsupported` |

The isolated clone probe uses `--no-extensions`; user
`session_before_fork` handlers do not participate in this copy operation.

Live record → change: `agent_start` Busy; `agent_settled` Idle or
Interrupted; `auto_retry_start` Retry; `message_update` text/thinking deltas
and `toolcall_end` rows; `tool_execution_start/end` tool state; `message_end`
the authoritative rows of the message; `compaction_start/end` compaction
status; `session_info_changed` title; usage from `message_update.usage` and
`message_end.message.usage`. Message usage accumulates per response without
double-counting streaming snapshots, seeded from persisted history. Live
session reads and settlement refresh `get_session_stats`, whose authoritative
totals also include tool and compaction usage. After those native totals are
available, partial-message counters cannot overwrite them. `context_usage`
comes only from Pi's `contextUsage`; a post-compaction unknown stays unknown.

Features: streaming, reasoning effort, model selection, compaction,
attachments by path, session rename/delete/fork, slash commands and older
timeline pages; tool approvals when `approvals` asks. No modes, separate
skills catalog, revert, inbox, worktrees or session-list pagination. Native
`fork(entryId)`, `get_tree` and arbitrary branch navigation exist in Pi but
are not exposed by this adapter's current App contract.
