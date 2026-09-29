# T3 Code wire protocol, as the gateway speaks it

What the `t3` agent adapter (`src/agents/adapters/t3/`) relies on, observed
against a real `t3 serve` **0.0.42** (Linux x64, `@t3code/t3-linux-x64`) with
the Claude Code provider (`claudeAgent`, model `claude-haiku-4-5`), on
2026-09-29. Frame captures are in `src/agents/adapters/t3/fixtures/`
(tokens, tickets, e-mail addresses and the probe's home directory redacted).

Everything below was seen on the wire unless a line is marked
**unverified**, in which case it comes from the contracts in
`packages/contracts/src/` of the T3 source (MIT) and has not been exercised.

`orchestrationProtocolVersion`: the descriptor of 0.0.42 **omits** the field,
which the contract defines as protocol **1** (`ORCHESTRATION_PROTOCOL_VERSION`
in `environment.ts`). The adapter treats absent as 1 and logs a newer value.

## 1. Transport summary

| Concern | Answer |
|---|---|
| Discovery | `GET /.well-known/t3/environment`, unauthenticated, JSON descriptor |
| Bootstrap credential | one-time pairing token (`t3 pair`, or `#token=` fragment of a pairing link) |
| Long-lived credential | bearer access token from `POST /oauth/token` |
| Socket auth | five-minute ticket from `POST /api/auth/websocket-ticket`, in the query string `?wsTicket=` |
| RPC transport | WebSocket `GET /ws`, text frames, JSON, Effect RPC envelopes |
| Read model | HTTP `GET /api/orchestration/shell`, `GET /api/orchestration/threads/:id` (bearer) |
| Commands | `orchestration.dispatchCommand` over the socket (also `POST /api/orchestration/dispatch` over HTTP, unused) |
| Streams | `orchestration.subscribeShell`, `orchestration.subscribeThread` |
| Keepalive | client `{"_tag":"Ping"}` every 5 s, server `{"_tag":"Pong"}` |

## 2. Auth flow

### 2.1 Descriptor

```
GET /.well-known/t3/environment
200 {
  "environmentId": "9cf7bafb-…", "label": "osk",
  "platform": {"os":"linux","arch":"x64","machine":"desktop"},
  "serverVersion": "0.0.42",
  "capabilities": { "connectionProbe": true, "attachmentUploads": true,
    "fileAttachments": {"maxUploadBytes": 52428800}, "threadSettlement": true,
    "threadSnooze": true, "threadPinning": true, "pullRequests": true, … }
}
```

Fixture: `fixtures/environment.json`. Anything that answers this with a
decodable descriptor is a T3 server; 502/503/504 or a decode failure is not.

### 2.2 Pairing credential -> bearer

`t3 pair --base-dir <dir> --ttl 20m --label <label>` prints
`Pairing URL: http://host:port/pair#token=<credential>` and `Token:
<credential>`; the credential is the token. It is exchanged once (RFC 8693
token exchange, form-encoded):

```
POST /oauth/token
content-type: application/x-www-form-urlencoded

grant_type=urn:ietf:params:oauth:grant-type:token-exchange
&subject_token=<pairing credential>
&subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap
&requested_token_type=urn:ietf:params:oauth:token-type:access_token
&client_label=muqun-gateway&client_device_type=bot

200 { "access_token": "<bearer>", "issued_token_type": "…:access_token",
      "token_type": "Bearer", "expires_in": <seconds>, "scope": "orchestration:read orchestration:operate terminal:operate review:write relay:read" }
```

The pairing credential is consumed. The bearer is what the gateway must
persist (`t3-credential.json` in the state directory, written by
`platform/store.rs`); it carries the standard client scopes
(`orchestration:read`, `orchestration:operate`, `terminal:operate`,
`review:write`, `relay:read`). `GET /api/auth/session` with the bearer returns
`{authenticated, auth:{policy,…}, scopes, sessionMethod, expiresAt}` and is a
cheap way to learn the bearer was revoked. A 401 anywhere means "pair again".

Optional `scope=` narrows the grant; it can never widen it. DPoP is
supported by the server but not used here.

### 2.3 Ticket -> socket

```
POST /api/auth/websocket-ticket
authorization: Bearer <bearer>

200 { "ticket": "<opaque>", "expiresAt": "2026-09-29T08:35:13.504Z" }   (5 minutes)
```

Then `GET ws://host:port/ws?wsTicket=<ticket>` (WebSocket upgrade). The
server records optional client identity from other query parameters; the
adapter sends none. A ticket is minted for every (re)connect. The bearer
never travels in a socket URL.

## 3. Effect RPC framing

Every WebSocket text frame is one JSON object, or a JSON array of objects
(`RpcSerialization.layerJson`: `Array.isArray(decoded) ? decoded :
[decoded]`). The adapter accepts both and sends single objects. Binary frames
are not used.

### 3.1 Client -> server

```json
{"_tag":"Request","id":0,"tag":"server.getConfig","payload":{},"headers":[]}
{"_tag":"Ack","requestId":0}
{"_tag":"Interrupt","requestId":0,"interruptors":[]}
{"_tag":"Ping"}
{"_tag":"Eof"}
```

- `id` is a client-chosen request id; the reference client uses a counter
  starting at 0 (a JSON number). Strings are also accepted.
- `tag` is the RPC name (section 4). `payload` is the RPC's input, plain JSON.
- `headers` is `[[name, value], …]`; the server prepends the HTTP upgrade
  headers to it. The adapter always sends `[]`.
- Optional `traceId`, `spanId`, `sampled` propagate a span; unused.
- **`Ack` is not optional for streams**: after each `Chunk` the server parks
  the stream on a latch until the client acks that request id. A client that
  does not ack receives exactly one chunk.
- `Interrupt` cancels a request or ends a stream. The server answers with an
  `Exit` whose cause is `[{"_tag":"Interrupt","fiberId":…}]` (observed).
- `Eof` says the client is done; the server ends the connection once its
  in-flight requests finish. Observed close code afterwards: 1005 (no status).

### 3.2 Server -> client

```json
{"_tag":"Chunk","requestId":3,"values":[{"kind":"event","event":{…}}]}
{"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":{…}}}
{"_tag":"Exit","requestId":3,"exit":{"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":1558}]}}
{"_tag":"Exit","requestId":7,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"OrchestrationDispatchCommandError","message":"…","cause":{…}}}]}}
{"_tag":"Exit","requestId":9,"exit":{"_tag":"Failure","cause":[{"_tag":"Die","defect":"Unknown request tag: no.such.rpc"}]}}
{"_tag":"Pong"}
{"_tag":"Defect","defect":…}
```

- A unary RPC gets one `Exit`. A streaming RPC gets `Chunk`s (each with one
  or more `values`) and finally an `Exit`: `Success` with `value` void when
  the server completed the stream, `Failure` with an `Interrupt` cause after a
  client `Interrupt`.
- `exit.cause` is an Effect `Cause` encoded as an array of entries:
  `Fail{error}` for the RPC's declared error (a tagged error object with
  `_tag` and `message`, plus whatever fields the class declares),
  `Die{defect}` for an unhandled defect (observed: unknown RPC tag, and a
  payload that fails schema validation: `"Missing key\n  at [\"commandId\"]"`),
  `Interrupt{fiberId?}`.
- `Defect` (no request id) is a connection-level failure; the adapter drops
  the socket and reconnects.
- `Pong` answers `Ping`. The reference client pings every 5 seconds and
  treats a ping without a pong by the next tick as a dead socket; the
  adapter does the same. The server also answers WebSocket-level pings.

Decoding and the correlation rules are implemented and unit-tested in
`rpc.rs` against `fixtures/frames.jsonl`.

## 4. RPCs used

| Tag | Kind | Input | Output |
|---|---|---|---|
| `server.probe` | unary | `{}` | `{}` |
| `server.getConfig` | unary | `{}` | `ServerConfig` (section 6) |
| `orchestration.dispatchCommand` | unary | a command (section 5) | `{"sequence": n}` |
| `orchestration.getTurnDiff` | unary | `{threadId, fromTurnCount, toTurnCount, ignoreWhitespace?}` | `{threadId, fromTurnCount, toTurnCount, diff}` |
| `orchestration.getFullThreadDiff` | unary | `{threadId, toTurnCount, ignoreWhitespace?}` | same |
| `orchestration.searchThreads` | unary | `{query (2..200 chars), limit? (1..50)}` | `{matches:[{threadId, projectId, source, snippet, messageCreatedAt}]}` |
| `orchestration.subscribeShell` | stream | `{afterSequence?, requestCompletionMarker?}` | shell items (section 7.1) |
| `orchestration.subscribeThread` | stream | `{threadId, reasoningMessages?, afterSequence?, requestCompletionMarker?, turnLimit?}` | thread items (section 7.2) |

Every orchestration RPC requires `orchestration:read` or
`orchestration:operate`; a missing scope fails with
`EnvironmentAuthorizationError{message, requiredScope}` (**unverified**, the
probe held all scopes).

Diff queries fail with `OrchestrationGetTurnDiffError` /
`OrchestrationGetFullThreadDiffError` when the range exceeds the checkpoints
taken so far (observed message: `Checkpoint unavailable for thread … turn 1:
Turn diff range exceeds current turn count: requested 1, current 0.`). The
`diff` is one unified diff for the whole range (`diff --git a/… b/…` blocks);
empty when nothing changed.

## 5. Commands (`orchestration.dispatchCommand`)

All commands carry `type` and a client-minted `commandId` (UUID). Ids the
client mints (`projectId`, `threadId`, `messageId`, `commandId`) are UUIDs by
convention; the schema only requires trimmed non-empty strings. Timestamps
are ISO 8601 UTC with milliseconds (`2026-09-29T08:30:26.601Z`). The reply
`{"sequence": n}` is the event-log position the command committed at; it
means the intent was recorded, not that the provider finished.

```json
{"type":"project.create","commandId":"…","projectId":"…","title":"t3proj","workspaceRoot":"/abs/path","createdAt":"…"}
{"type":"thread.create","commandId":"…","threadId":"…","projectId":"…","title":"…",
 "modelSelection":{"instanceId":"claudeAgent","model":"claude-haiku-4-5"},
 "runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"createdAt":"…"}
{"type":"thread.turn.start","commandId":"…","threadId":"…",
 "message":{"messageId":"…","role":"user","text":"…","attachments":[]},
 "modelSelection":{…}?, "runtimeMode":"full-access","interactionMode":"default","createdAt":"…"}
{"type":"thread.turn.interrupt","commandId":"…","threadId":"…","turnId"?:"…","createdAt":"…"}
{"type":"thread.approval.respond","commandId":"…","threadId":"…","requestId":"…","decision":"accept","createdAt":"…"}
{"type":"thread.user-input.respond","commandId":"…","threadId":"…","requestId":"…","answers":{"<questionId>":…},"createdAt":"…"}
{"type":"thread.checkpoint.revert","commandId":"…","threadId":"…","turnCount":0,"createdAt":"…"}
{"type":"thread.delete","commandId":"…","threadId":"…"}
{"type":"thread.meta.update","commandId":"…","threadId":"…","title"?:"…","modelSelection"?:{…}}
{"type":"thread.runtime-mode.set","commandId":"…","threadId":"…","runtimeMode":"approval-required","createdAt":"…"}
{"type":"thread.interaction-mode.set","commandId":"…","threadId":"…","interactionMode":"plan","createdAt":"…"}
```

- `modelSelection`: `{instanceId, model, options?}` where `instanceId` is a
  provider instance slug (`codex`, `claudeAgent`, `opencode`, …) and
  `options` is `[{id, value}]` (older servers also accept an object; the
  legacy key `provider` is promoted to `instanceId`). The adapter carries a
  domain `ModelRef.variant` as `{id:"effort", value}`.
- `runtimeMode`: `full-access` | `approval-required` | `auto-accept-edits` |
  `auto`. For the Claude provider these map to SDK permission modes
  (`bypassPermissions`, default, `acceptEdits`, …). In `full-access` the
  probe's `echo` and file write ran without any approval; in
  `approval-required` a file write raised `approval.requested` and a plain
  `echo` did **not** (Claude's own policy allowed it).
- `interactionMode`: `default` | `plan`.
- `decision`: `accept` | `acceptForSession` | `acceptAlways` | `decline` |
  `cancel`. Only `accept` was exercised.
- Approval and interrupt commands on a thread with nothing pending are
  **accepted** (a sequence is returned) and change nothing; the failure mode
  for an unknown thread is `OrchestrationDispatchCommandError` with
  `"…Thread '<id>' does not exist for command '<type>'."`.
- `thread.checkpoint.revert` with `turnCount: 0` restores the files and the
  provider conversation to before the first turn: the thread's messages,
  activities and checkpoints were gone from later snapshots. The server
  confirms with a `thread.reverted{turnCount}` event about a second later.
- Attachments: the `thread.turn.start` message accepts `attachments` (image
  and file references, uploads via `attachments.createUploadUrl`);
  **unverified**, the adapter sends `[]` and reports attachments as
  unsupported.

Other commands exist (`thread.archive`, `thread.settle`, `thread.snooze`,
`thread.pin`, pull-request linking, `thread.session.stop`,
`thread.conversation.revert`) and are not used.

## 6. `server.getConfig`

`{environment, auth, cwd, keybindings, issues, providers, availableEditors,
settings, …}`. Only `providers` matters here (fixture
`fixtures/server_config.json`, trimmed):

```json
{"instanceId":"claudeAgent","driver":"claudeAgent","enabled":true,"installed":true,
 "version":"2.1.284","status":"ready","auth":{"status":"authenticated","type":"Claude Max","label":"…","email":"…"},
 "models":[{"slug":"claude-haiku-4-5","name":"Claude Haiku 4.5","isCustom":false,"isDefault":false,"capabilities":{…}}, …],
 "slashCommands":[{"name":"…","description":"…"}], "skills":[{"name":"…","description":"…","path":"…"}], …}
```

Six instances were reported on the probe host: `codex` and `claudeAgent`
ready, `cursor`, `grok`, `opencode`, `antigravity` disabled/not installed
(OpenCode's own `opencode` binary on `PATH` was not detected by T3 0.0.42).
`status` is `ready | warning | error | disabled`; `auth.status` is
`authenticated | unauthenticated | unknown`.

`subscribeServerConfig` streams the same as a snapshot plus
`providerStatuses` updates; not used.

## 7. Streams

### 7.1 `orchestration.subscribeShell`

Items, in order: `{"kind":"snapshot","snapshot":{snapshotSequence, projects:[ProjectShell], threads:[ThreadShell], updatedAt}}`
(or a replay of events after `afterSequence`), then
`{"kind":"synchronized"}` when `requestCompletionMarker` was true, then live
items: `{"kind":"project-upserted","sequence","project"}`,
`{"kind":"project-removed","sequence","projectId"}`,
`{"kind":"thread-upserted","sequence","thread"}`,
`{"kind":"thread-removed","sequence","threadId"}`. Every thread change
(status, title, model, pending flags, `latestTurn`) arrives as a full
`thread-upserted` row. Fixture: `fixtures/shell_stream_items.json`.

`ThreadShell` (also what `GET /api/orchestration/shell` returns per thread):

```json
{"id","projectId","title","modelSelection":{…},"runtimeMode","interactionMode",
 "branch":null,"worktreePath":null,"pullRequests":[],"branchPullRequest":null,
 "latestTurn":{"turnId","state":"running|interrupted|completed|error","requestedAt","startedAt","completedAt","assistantMessageId"}|null,
 "createdAt","updatedAt","archivedAt":null,"settledOverride":null,"settledAt":null,"unsettledAt":null,
 "snoozedUntil":null,"snoozedAt":null,"pinnedAt":null,"pinOrderKey":null,"activeOrderKey":null,
 "titleRegeneration":null,"titleState":null,
 "session":{"threadId","status":"idle|starting|running|ready|interrupted|stopped|error","providerName","providerInstanceId","runtimeMode","activeTurnId","lastError","updatedAt"}|null,
 "latestUserMessageAt","hasPendingApprovals":false,"hasPendingUserInput":false,"hasActionableProposedPlan":false,
 "backgroundLiveness":null,"planProgress":null}
```

`ProjectShell`: `{id, title, workspaceRoot, repositoryIdentity, defaultModelSelection, defaultThreadEnvMode, autoPull, faviconPath, projectIcon, scripts, createdAt, updatedAt}`.

### 7.2 `orchestration.subscribeThread`

Items: `{"kind":"snapshot","snapshot":{snapshotSequence, thread: Thread, page?}}`
(the full thread: `ThreadShell` fields plus `messages`, `proposedPlans`,
`activities`, `checkpoints`, `deletedAt`), `{"kind":"synchronized"}`, then
`{"kind":"event","event": OrchestrationEvent}`. Fixture:
`fixtures/thread_stream_items.json` (a full approval turn).

`OrchestrationEvent` envelope: `{sequence, eventId, aggregateKind:"thread",
aggregateId:<threadId>, occurredAt, commandId, causationEventId,
correlationId, metadata:{}, type, payload}`.

Events observed, in the order of one turn:

1. `thread.message-sent` `{threadId, messageId, role:"user", text, turnId:null, streaming:false, createdAt, updatedAt}` -- the prompt.
2. `thread.session-set` `{threadId, session:{status:"starting"→"running", activeTurnId, …}}` -- several, as the provider session comes up.
3. `thread.activity-appended` `{threadId, activity}` for tool and approval traffic (section 8).
4. `thread.message-sent` with `role:"assistant"` (or `"reasoning"` when `reasoningMessages:true`), `streaming:true`, `text` = a **delta** to append; `messageId` is `assistant:<uuid>`. Reasoning deltas were not produced by Haiku (**unverified** shape, same schema).
5. `thread.message-sent` `role:"assistant", streaming:false, text:""` -- closes the message. The projector rule (`projector.ts`): streaming text appends; non-streaming non-empty text replaces; empty closes.
6. `thread.session-set` `{session:{status:"ready", activeTurnId:null}}`.
7. `thread.turn-diff-completed` `{threadId, turnId, checkpointTurnCount, checkpointRef:"refs/t3/checkpoints/<b64 threadId>/turn/<n>", status:"ready", files:[{path, kind, additions, deletions}], assistantMessageId, completedAt}`.
8. `thread.activity-appended` with `activity.kind:"checkpoint.captured"`.

Also observed: `thread.reverted {threadId, turnCount}` after a revert. Not
observed: `thread.turn-interrupt-requested` (**unverified** -- every probe
interrupt landed after the sub-two-second turn had completed; the shell row
kept `latestTurn.state:"completed"`), `thread.deleted`,
`thread.meta-updated`, `thread.runtime-mode-set`,
`thread.interaction-mode-set` on the thread stream (the meta commands were
accepted, but the probe thread had no open subscription at the time), and
`thread.user-input-response-requested`.

Sequences: every item that is an event carries `sequence`; snapshots carry
`snapshotSequence`. The adapter remembers the last sequence per stream and
resubscribes with `afterSequence` after a reconnect; the server replays or,
when it cannot, sends a fresh snapshot.

### 7.3 `GET /api/orchestration/threads/:id`

Same `Thread` as the stream snapshot, wrapped `{snapshotSequence, thread,
page?}`. `?reasoningMessages=true` opts in to reasoning roles;
`?turnLimit=N` windows to the last N user turns and adds
`page:{beforeCursor, hasMore, snapshotSequence, threadSequence}`
(`?beforeCursor=` pages older turns). Unknown id: `404
{"_tag":"EnvironmentResourceNotFoundError","code":"not_found","reason":"thread_not_found","traceId"}`.
Fixture: `fixtures/thread_detail.json`.

## 8. Activities

`OrchestrationThreadActivity`: `{id, tone:"info|tool|approval|error", kind,
summary, payload, turnId, sequence?, createdAt}`. Kinds and payloads
observed:

| kind | payload |
|---|---|
| `tool.started` | `{itemType:"command_execution"|"file_change"|…, toolCallId, status:"inProgress", title, detail:"Bash: …", data:{toolName, command?, …}}` |
| `tool.updated` | same, plus `data.rawOutput:{content}` as output arrives |
| `tool.completed` | same, `status:"completed"|"failed"` |
| `approval.requested` | `{requestId, requestKind:"command|file-read|file-change|mcp-elicitation|permission", requestType:"file_change_approval"|…, detail:"Write: {…}", options?:[{decision,label,warning?}], appName?}` |
| `approval.resolved` | `{requestId, requestKind, requestType, decision:"accept"}` |
| `context-window.updated` | token accounting (ignored) |
| `checkpoint.captured` | `{turnCount, status}` |
| `checkpoint.revert.failed` | `{turnCount, detail}`, `tone:"error"`; seen when the workspace is not a git repository (no checkpoints): the `thread.checkpoint.revert` command was still accepted with a sequence |

From the contracts, not observed (**unverified**): `tool.denied
{toolName, toolUseId?, detail?}`, `user-input.requested {requestId,
questions:[{id, header, question, options:[{label, description?}],
allowCustomAnswer?, multiSelect?}], responseMode?}`, `user-input.resolved
{requestId, answers}`, `turn.plan.updated {plan:[{step, status}],
explanation?}`, `runtime.error {message}`, `runtime.warning {message,
detail?}`, `task.*`, `context-compaction`. The mapper handles all of them
from the schema shapes.

Fixtures: `fixtures/activity_approval_requested.json`,
`fixtures/activity_tool_completed.json`.

## 9. What the adapter does with it

- session id = thread id; `create_session` finds the project by
  `workspaceRoot` or dispatches `project.create`, then `thread.create`.
- `send_prompt` -> `thread.turn.start`; `interrupt` -> `thread.turn.interrupt`;
  `reply_permission` -> `thread.approval.respond` (Allow -> `accept`,
  AllowAlways -> `acceptAlways` if offered else `acceptForSession`, Deny ->
  `decline`); `reply_form` -> `thread.user-input.respond`; `revert_session`
  -> `thread.checkpoint.revert` (message id = a turn count, or `turn:<id>`
  resolved through `checkpoints`); `switch_model` -> `thread.meta.update`;
  `rename_session` -> `thread.meta.update`; `delete_session` -> `thread.delete`.
- `get_vcs_diff` (`working`/`branch`) -> `getFullThreadDiff` up to the latest
  `checkpointTurnCount`, split per `diff --git` block.
- `get_catalog` -> providers and models from `server.getConfig`; modes are
  empty (T3 has no persona; `runtimeMode` is a gateway setting).
- Reads (`list_projects`, `list_sessions`, `get_session`, `get_timeline`,
  pending approvals/forms) come from the HTTP snapshots. The shell stream and
  one thread stream per watched thread feed the manager
  (`agents/manager/t3.rs`), which folds them into the domain events. A
  thread is watched when a client opens, creates or reads it, when a list or
  the shell stream shows it running, and unwatched when it is deleted; at
  most 32 threads are watched, least recently used first out.
- Timeline `message_id`s are `<13-digit creation ms>:<T3 id>` (activities
  file under `turn:<turnId>`): the contract orders a timeline by
  `(message_id, ordinal)`, and T3's own ids (UUIDs, `assistant:<uuid>`) do
  not sort by creation. A multi-activity row (a tool card, the plan) keeps
  the time of its first activity. `revert_session` strips the prefix and
  reverts to the turn count before the turn the message belongs to.
