# Agent API

The gateway's agent surface: what the mobile app calls, what comes back, and
what arrives on the event stream. The engine behind it is OpenCode v2.0.1.

Every field name here is the one the code serializes — snake_case, as the
domain types spell it — with two deliberate exceptions, both marked below:
`metadata` and `content` on a tool call are OpenCode's own payloads, forwarded
verbatim, so their keys stay camelCase.

## Conventions

- **Auth.** Every route requires a paired device (`Authorization: Bearer
  <device token>`); a missing or malformed header is `401` and an unpaired or
  revoked device is `403`. This holds in every transport mode — a gateway with
  `transport_encryption: disabled` drops the envelope, not the token.
- **Envelope.** A successful JSON body is wrapped:
  `{"schema_version": "...", "capabilities": {...}, "data": <payload>}`. The
  payload column below describes `data`.
- **Errors.** `{"error": {"code": "...", "message": "..."}}`. The codes used
  here are `agent_unavailable` (503, no engine attached),
  `agent_engine_error` (502, OpenCode refused), `session_not_found` (404),
  `resync_required` (410), and the `invalid_*` family (400).
- **`asid`** is the OpenCode session id (`ses_…`). The gateway does not mint
  ids of its own.
- **Legacy paths.** Everything under `/api/agent-*` also exists under
  `/api/sessions/{session_id}/agent-*` for the routes that had that form before;
  `session_id` is parsed and ignored. Routes added for v2 parity are on the
  global paths only.

---

## Session lifecycle

### `GET /api/agent-sessions`

| Query | Meaning |
|---|---|
| `directory` | Scope to a workspace directory. |
| `parent_id` | List the children of one session. |
| `roots` | `true` lists top-level sessions only — no subagent sessions. |
| `limit`, `order`, `search`, `cursor` | Passed through to OpenCode. `order` is `asc` or `desc`. |

Returns `[AgentSessionInfo]`.

A subagent run creates a real session whose `parent_id` is the caller's. Without
`roots=true` those are mixed into the list.

### `POST /api/agent-sessions`

```json
{ "directory": "/abs/path", "model": {"provider_id": "…", "model_id": "…", "variant": "…"}, "agent": "build" }
```

All three are optional. **Omitting `model` is the correct way to get the user's
configured default** — the gateway no longer substitutes one. `directory` must
be absolute and must exist. Returns `AgentSessionInfo`.

### `GET /api/agent-sessions/{asid}`

Returns `AgentSessionSnapshot`:

```json
{
  "info": { …AgentSessionInfo… },
  "timeline": [ …TimelineItem… ],
  "permissions": [ …PermissionRequest… ],
  "forms": [ …FormRequest… ],
  "inbox": [ …Session.Inbox.Info verbatim… ],
  "seq": 42
}
```

`permissions` and `forms` are re-read from OpenCode on a cold snapshot, so a
prompt raised while the event stream was down is not lost. `timeline` is ordered
by `(message_id, ordinal)`.

### `GET /api/agent-sessions/{asid}/children`

Same response as the list route, filtered to this session's children. Accepts
the same `limit` / `order` / `search` / `cursor`.

### `DELETE /api/agent-sessions/{asid}`

`{"deleted": true}`. OpenCode removes the session's children too, and announces
each one on the stream.

### `POST /api/agent-sessions/{asid}/rename`

`{"title": "…"}` → `{"renamed": true, "title": "…"}`.

Auto-titling happens on its own on the first turn of an untitled session and
arrives as `agent.session.updated`; there is no way to ask for it.

### `POST /api/agent-sessions/{asid}/view`

`{"idle": 1789641018610}` (optional; defaults to now) → `{"viewed": <ms>}`.

Marks the session read. Unread is `info.time_idle > info.time_viewed`.

### `GET /api/agent-sessions/{asid}/export`

`?sanitize=true|false` (default `true`) → the exported transcript
(`{info, messages}`), for a share sheet. There is no session sharing in v2.

---

## Prompting and control

### `POST /api/agent-sessions/{asid}/prompt`

```json
{ "text": "…", "attachments": ["/abs/path or file:// or https:// or data:"], "delivery": "steer" }
```

`delivery` is `steer` (run at the next step boundary) or `queue` (wait behind
what is already queued). Returns `{"submitted": true}`.

There is no model or agent field: **model and agent are session state in v2**.
Switch first, then prompt.

An attachment the app uploaded is an absolute path into the gateway's upload
directory — see [Attachments](#attachments) for how one gets there, and for the
permission rule the gateway puts on the session so the agent can open it without
an approval prompt.

### `POST /api/agent-sessions/{asid}/model`

Accepts either shape:

```json
{ "model": { "provider_id": "opencode", "model_id": "union-alpha", "variant": "thinking" } }
```
```json
{ "provider_id": "opencode", "model_id": "union-alpha" }
```

Returns `{"switched": true}`. `variant` is optional and omitted, never null.

### `POST /api/agent-sessions/{asid}/agent`

`{"agent": "build"}` (a bare `"build"` also works) → `{"ok": true}`. Agent ids
are lowercase; a display name such as `"Build"` is rejected by OpenCode.

### `POST /api/agent-sessions/{asid}/interrupt` · `/abort`

No body → `{"interrupted": true}`. Status goes to `interrupted`.

### `POST /api/agent-sessions/{asid}/command`

```json
{ "name": "review", "arguments": "branch", "delivery": "steer" }
```

A slash command from the catalog's `commands`. A leading `/` on `name` is
stripped; `arguments` fills `$ARGUMENTS`. Returns `{"submitted": true}`.

### `POST /api/agent-sessions/{asid}/skill`

```json
{ "skill": "docs", "resume": true }
```

`skill` is a `skills[].id` from the catalog. `resume` is optional: left out,
OpenCode decides whether the agent loop picks up where the skill left it, which
is what the slash menu wants; `false` appends the skill and leaves the session
idle. Returns `{"status": "ok"}` — OpenCode answers `204`, and what the user
sees is the timeline row below, not this reply. An unknown id is `404` from
OpenCode and comes back as `agent_engine_error`.

The activation is a `skill` message. It reaches the timeline live, from
`session.skill.activated`, as an `AgentPart::Skill` row under
`agent.timeline.upsert` — 2.0.1 has no `session.message.*` family, so that one
event is the whole announcement. The row is addressed `{message_id}:p0` with the
same `message_id` a refetch gives it, so the streamed row and the read-back row
are one row.

A slash menu lists the skills with `slash: true` and leaves the rest to the
agent; see the catalog below.

### `POST /api/agent-sessions/{asid}/background`

No body → `{"backgrounded": true}`. Detaches the foreground tools blocking the
agent loop — a long `shell` is the usual one. They keep running; their cards are
marked `background: true` and the shells stay readable through
`/api/agent-shells`.

### `POST /api/agent-sessions/{asid}/wait`

No body → `{"idle": true}` once the agent loop is idle. For scripted flows, not
for the UI.

### `POST /api/agent-sessions/{asid}/revert` · `/revert/clear`

- `POST …/revert` `{"message_id": "msg_…"}` → `{"status": "ok", "reverted_to": "msg_…"}`.
  Stages and commits a rollback to that message.
- `POST …/revert/clear` → `{"cleared": true}`. Cancels a staged rollback — this
  is redo.

While a rollback is staged, `info.revert` is set.

---

## Attachments

A phone cannot hand a local file to an agent that runs on the gateway's host, so
the file is uploaded first and then referred to by path. Both routes are on the
gateway proper rather than under `/api/agent-*`; both require a paired device.

### `POST /api/uploads`

`multipart/form-data` with one `file` field. The type is decided by sniffing the
content, never by the filename: images (png, jpeg, gif, webp, heic), PDF, the
Office and OpenDocument containers, and low-control UTF-8 text are accepted, and
everything else — executables and scripts first — is refused with `415`. A body
over 25 MiB is `413`. The stored name is generated by the gateway (a UUID plus
the extension the content earned), so nothing the client sent ever becomes a
path.

This route answers a bare JSON body, not the content envelope:

```json
{
  "path": "/home/ryu/.local/share/muqun-gateway/uploads/6f1c….webp",
  "url": "/api/uploads/6f1c….webp",
  "name": "screenshot.png",
  "size": 48213,
  "mime": "image/webp"
}
```

The two references are for two different readers, and both are always present:

- **`path`** is the host path. It goes in the prompt's `attachments`, and the
  agent opens it off this machine.
- **`url`** is the same file over the API. It is what the app draws in the
  transcript — the timeline item carries the host path, which the phone has no
  way to open.

`name` is the client's own filename, sanitised, for the label only. Uploads are
deleted 48 hours after they are written.

### `GET /api/uploads/{file_name}`

Streams one stored upload back. `file_name` is the last segment of `url` — the
generated stored name, and only that: a separator, a traversal, a leading dot, a
symlink, an expired file and an unknown name all answer the same

```json
{ "error": { "code": "upload_not_found", "message": "no such upload" } }
```

with `404`, so a caller cannot map the host by asking. The content type is
sniffed from the bytes again on every read, in the same order the upload used,
so the stored extension never decides on its own. The response carries
`cache-control: private, no-store, max-age=0` — an upload is one device's own
file and must not sit in an intermediary's cache — and the body is streamed
rather than buffered.

### The upload directory permission

The upload directory is not inside the session's working directory, so the first
tool that opens an attachment used to raise an `external_directory` permission
request — an approval prompt for the file the user had just attached themselves.

The gateway now settles that ahead of time. On session create, and again before
any prompt that carries attachments, it reads the session's ruleset from
`GET /api/session/{id}` (`Session.Info.permissions`) and, if the allowance is not
already there, appends

```json
{ "action": "external_directory", "resource": "<uploads dir>/*", "effect": "allow" }
```

and sends the whole ruleset back with `PUT /api/session/{id}/permission/rules`.
That `PUT` replaces the ruleset, which is why what is already on the session is
read first and preserved in order — OpenCode evaluates session rules last and
lets the last match win, so this is an addition, not a replacement of anything
the owner set up. The grant names that one directory and nothing wider; every
other path still asks. It is done once per session per attached engine, logged
at `debug`, and a failure is not fatal: without the rule the agent simply asks
again, which is the behaviour this replaces.

---

## Compaction and context

### `POST /api/agent-sessions/{asid}/compact`

`{"delivery": "steer"}` (optional) → `{"requested": true, "item": <inbox item>}`.

The request is admitted to the inbox and runs at the next step boundary.
Progress arrives as `agent.compaction.changed`, and the finished boundary lands
in the timeline as an `AgentPart::Compaction` row.

### `GET /api/agent-sessions/{asid}/context`

```json
{ "messages": 12, "tokens": { "input": 20801, "output": 41, "reasoning": 134, "cache": { "read": 0, "write": 0 } } }
```

Everything still in the model's context, i.e. after the last compaction.
`tokens` is OpenCode's own `TokenUsage.Info` (camelCase-free, but nested `cache`)
taken from the last assistant message, or `null`.

---

## Inbox (queued and steered work)

| Route | Body | Response |
|---|---|---|
| `GET /api/agent-sessions/{asid}/inbox` | — | `{"items": [ …Session.Inbox.Info… ]}` |
| `DELETE /api/agent-sessions/{asid}/inbox/{inbox_id}` | — | `{"cancelled": true}` |
| `POST /api/agent-sessions/{asid}/inbox/{inbox_id}/steer` | — | `{"delivery": "steer", "inbox_id": "…"}` |
| `POST /api/agent-sessions/{asid}/inbox/{inbox_id}/queue` | — | `{"delivery": "queue", "inbox_id": "…"}` |

Items are OpenCode's verbatim, i.e. camelCase:
`{id, sessionID, timeCreated, type: "user"|"synthetic"|"compaction"|"move", payload, delivery}`.

---

## Permissions and forms

### `POST /api/agent-sessions/{asid}/permissions/{req_id}/reply`

```json
{ "decision": "allow" | "allow_always" | "deny", "message": "why not" }
```

`once` / `always` / `reject` are accepted as aliases for the three decisions.
`message` is optional and is forwarded with the reply. Returns
`{"replied": true}`. A `deny` rejects every pending request in the session,
which is OpenCode's behaviour, not the gateway's.

### `POST /api/agent-sessions/{asid}/forms/{form_id}/reply`

`{"answers": { "<key>": <value>, … }}` → `{"replied": true}`.

Forms are v2's question mechanism; there is no separate question event.

---

## Files, diff, catalog, projects

### `GET /api/agent-files` · `GET /api/agent-sessions/{asid}/files`

`?query=&limit=&directory=` → `[{"path": "…", "name": "…", "kind": "file"|"directory"}]`.
An empty `query` lists the directory. A failed search is now an error, not an
empty list.

### `GET /api/agent-sessions/{asid}/vcs/diff` (also `/vcs-diff`)

`?mode=working|branch|committed` (default `working`) →
`[{"path": "…", "patch": "…", "additions": 0, "deletions": 0}]`.

Scoped to the session's own directory. `mode` is required by OpenCode; omitting
it is why this used to come back empty.

### `GET /api/agent-catalog`

`?directory=` · ETag + `304`. `data`:

```json
{
  "models":   [ { "id", "name", "provider_id", "family?", "limit?", "variants?": [{"id", "reasoning_effort?"}], "cost?", "enabled", "status?" } ],
  "agents":   [ { "id", "name", "description?", "mode?", "color?", "hidden" } ],
  "mcp":      [ { "name", "status", "error?" } ],
  "skills":   [ { "id", "name", "description", "slash", "autoinvoke" } ],
  "providers":[ { "id", "name", "activation?": "auto"|"enabled"|"disabled",
                  "models": [ { "id", "name", "enabled", "variants": [...], "limit?", "status?" } ] } ],
  "commands": [ { "name", "description?", "agent?", "template?" } ],
  "defaults": { "model?": {"provider_id", "model_id", "variant?"}, "agent?": "build" }
}
```

`skills[].slash` and `skills[].autoinvoke` are `Skill.Info`'s own optional
flags, and both default to `false` when the payload leaves them out — which most
skills do. **A slash menu lists only `slash: true`**; the rest exist for the
agent to reach for, and `autoinvoke: true` says it may do so unasked. Activate
one with [`POST …/skill`](#post-apiagent-sessionsasidskill).

A picker should hide `agents[].hidden` and `mode == "subagent"` entries.
A provider with `activation: "disabled"` and a model with `enabled: false` are
carried through rather than filtered — grey them out and say OpenCode on the
host needs configuring. The gateway proxies no credential, integration or OAuth
route: sign-in is done on the host.

### `GET /api/agent-projects`

ETag + `304`. `[{"id", "canonical", "name", "vcs?", "sandboxes": []}]`.

### `GET /api/agent-directories`

`?prefix=&query=` → `[{"name", "path"}]`, at most 50. Local filesystem only.

---

## Background shells

| Route | Query | Response |
|---|---|---|
| `GET /api/agent-shells` | `directory` | `[Shell.Info]` (verbatim) |
| `GET /api/agent-shells/{shell_id}` | — | `Shell.Info` |
| `GET /api/agent-shells/{shell_id}/output` | `cursor`, `limit` | `{output, cursor, size, truncated}` |
| `DELETE /api/agent-shells/{shell_id}` | — | `{"killed": true}` |

`Shell.Info` is OpenCode's own, camelCase:
`{id, status: "running"|"exited"|"timeout"|"killed", command, cwd, shell, file, pid?, exit?, metadata, time}`.

---

## Engine status

### `GET /api/agent-engine`

```json
{ "available": true, "origin": "adopted" | "spawned" | "none",
  "url": "http://127.0.0.1:49374", "version": "2.0.1",
  "stream_connected": true, "autostart": true }
```

The one agent route that answers `200` when no engine is attached — it exists to
explain why the others are returning 503. The gateway re-discovers OpenCode
whenever the health probe fails or its registration moves, and (unless
`opencode.autostart` is `false` in `config.json`) starts
`opencode serve --service` when it cannot find one.

---

## Streaming

### `GET /api/agent-sessions/{asid}/stream`

`text/event-stream`, unencrypted, filtered to one session. First frame is
`event: connected` with `{"asid": "…"}`; keep-alive every 15 s. Subscription
survives OpenCode restarting underneath it.

The main session stream (`GET /api/sessions/{id}/stream`) carries the same
events for every session, encrypted, unfiltered.

### `GET /api/agent-sessions/{asid}/events`

`?after=<seq>` → `[AgentDomainEvent]` from the ring buffer, or **`410
resync_required`** when the requested point has fallen out of it. A client that
has never synced (`after=0`) is told to resync too, rather than being handed a
tail it cannot place.

### `GET /api/agent-sessions/{asid}/timeline`

`?after=<seq>` → `{"items": [...], "status": "busy", "resync": false, "latest_seq": 42}`.

---

## Domain events

The SSE `event:` name and the payload's own `type` are the same string.

### `agent.session.updated`

```json
{ "type": "agent.session.updated", "asid": "ses_1", "seq": 12, "info": { …AgentSessionInfo… } }
```

Sent on create, rename (including auto-title), model or agent switch, usage
update, and deletion. Fields the event did not mention keep their previous
value; a real title is never replaced by an empty one.

### `agent.status.changed`

```json
{ "type": "agent.status.changed", "asid": "ses_1", "status": "failed",
  "error": { "name": "unknown", "message": "Agent not found: \"Build\"", "status": 500 }, "seq": 3 }
```

`status` is `busy | idle | failed | interrupted | retry | unknown`. `error` is
present only with `failed` (and a scheduled retry that carried one).

Busy and idle come from `session.execution.started|succeeded|failed|interrupted`.
A step ending with `finish: "tool-calls"` is **not** the end of a turn and does
not produce an idle.

### `agent.timeline.upsert`

```json
{ "type": "agent.timeline.upsert", "asid": "ses_1", "seq": 14, "items": [ …TimelineItem… ] }
```

Rows are addressed by `id` — upsert, do not append. Ids are stable across the
streaming path and a refetch:

| Row | id |
|---|---|
| assistant text | `{message_id}:t{ordinal}` |
| reasoning | `{message_id}:r{ordinal}` |
| tool call | `{message_id}:tool:{tool_call_id}` |
| everything else | `{message_id}:p{ordinal}` |
| a detached shell | `shell_{shell_id}` |

### `agent.timeline.removed`

```json
{ "type": "agent.timeline.removed", "asid": "ses_1", "ids": ["msg_1:t0"], "seq": 15 }
```

### `agent.permission.pending` / `agent.permission.resolved`

```json
{ "type": "agent.permission.pending", "asid": "ses_1", "seq": 16, "request": { …PermissionRequest… } }
{ "type": "agent.permission.resolved", "asid": "ses_1", "request_id": "per_1", "seq": 17 }
```

### `agent.form.pending` / `agent.form.resolved`

```json
{ "type": "agent.form.pending", "asid": "ses_1", "seq": 18, "request": { …FormRequest… } }
{ "type": "agent.form.resolved", "asid": "ses_1", "form_id": "frm_1", "seq": 19 }
```

### `agent.compaction.changed`

```json
{ "type": "agent.compaction.changed", "session_id": "ses_1",
  "status": "started" | "running" | "completed" | "failed",
  "reason": "auto" | "manual", "delta": "## Objective", "seq": 20 }
```

`delta` is the summary text streaming in, on `running`. When it completes, the
boundary also appears in the timeline as an `AgentPart::Compaction` row.

> This event and `agent.inbox.changed` name the session `session_id`; the older
> events call the same field `asid`. Both are accepted on the way in.

### `agent.inbox.changed`

```json
{ "type": "agent.inbox.changed", "session_id": "ses_1", "seq": 21,
  "items": [ { "id": "msg_1", "sessionID": "ses_1", "type": "user", "payload": {...}, "delivery": "queue" } ] }
```

The whole queue every time, so there is no diff to reconcile.

### `agent.resync`

```json
{ "type": "agent.resync", "asid": "ses_1", "reason": "event_backlog_overflow" }
```

Re-fetch the snapshot. An empty `asid` means every session — the gateway's own
event backlog overflowed.

---

## Types

### `AgentSessionInfo`

```json
{
  "asid": "ses_1",
  "backend_session_id": "ses_1",
  "title": "Tool availability and directory file count request",
  "agent": "build",
  "model": { "provider_id": "opencode", "model_id": "union-alpha", "variant": "default" },
  "status": "busy",
  "directory": "/home/ryu/Work/muqun/app",
  "cost": 0.1,
  "tokens": { "input": 1, "output": 2, "reasoning": 3, "cache_read": 4, "cache_write": 5 },
  "limit": { "context": 200000 },
  "parent_id": "ses_0",
  "project_id": "68c2…",
  "outcome": "succeeded",
  "error": { "name": "unknown", "message": "…", "status": 500 },
  "revert": { "message_id": "msg_1", "part_id": null, "snapshot": null, "files": null },
  "fork": { "session_id": "ses_x", "boundary_type": "before", "message_id": "msg_2" },
  "time_idle": 1789641018610,
  "time_viewed": 1789641000000,
  "deleted": false,
  "updated_ms": 1789641018610
}
```

Only `asid`, `backend_session_id`, `title`, `status` and `updated_ms` are always
present; every other field is omitted when unset. `deleted` is omitted unless
true. `model` is `null`/absent when OpenCode has not said — the gateway does not
invent one.

### `TimelineItem`

```json
{
  "id": "msg_1:tool:call_1",
  "message_id": "msg_1",
  "role": "user" | "assistant" | "system",
  "ordinal": 2,
  "seq": 7,
  "updated_ms": 1789641018610,
  "attachments": ["file:///tmp/screen.png"],
  "part": { "type": "…", … }
}
```

`ordinal` is the part's position within its message — exact once read back from
OpenCode, and the event's own ordinal while streaming. Sort by
`(message_id, ordinal)`; message ids sort by creation.

### `AgentPart`

Tagged by `type`.

**`text`** — `{ "type": "text", "text": "…" }`

**`reasoning`** — `{ "type": "reasoning", "text": "…", "duration_ms": 5 }`

**`tool`**

```json
{
  "type": "tool",
  "id": "call_675c18e7",
  "name": "glob",
  "title": "**/*",
  "input": { "pattern": "**/*", "path": "." },
  "output": "…/sample.txt",
  "content": [ { "type": "text", "text": "…" } ],
  "metadata": { "count": 1, "truncated": false },
  "state": "completed",
  "status": "completed",
  "error": { "name": "…", "message": "…" },
  "child_session_id": "ses_CHILD",
  "background": true,
  "truncated": true,
  "time": { "created": 1, "ran": 2, "completed": 3 }
}
```

- `state` is `pending | streaming | running | completed | failed`. `status`
  carries the same value under the name the previous release used; `error` is
  accepted as an input alias for `failed`.
  - `pending` — the name is known (`session.tool.input.started`) but not the input.
  - `streaming` — the input is still arriving; OpenCode has it as a partial JSON string.
  - `running` — dispatched.
- `title` is derived by the gateway from the name and input (OpenCode has no
  title field); absent for a tool it does not recognise.
- `output` is the text content joined into one string, kept for compatibility.
  `content` is `Tool.Content[]` **verbatim** — text items and
  `{type: "file", uri, mime, name?}` items for images and PDFs.
- `metadata` is the tool's own, **verbatim and camelCase**. It carries
  `files: FileDiff.Info[]` for `edit` (ready-to-render unified diffs),
  `sessionID` and `status` for `subagent`, `exit` and `status` for `shell`, and
  `truncated` almost everywhere.
- `child_session_id` is `metadata.sessionID` lifted out: the subagent's session,
  available from the first progress event, i.e. while it is still running.
- `background` is set when the tool was detached by `POST .../background`.
  OpenCode 2.0.1 has no flag of its own for this — `Shell.Info.metadata` and
  the tool state's `metadata` are free-form objects and never carried a
  `background` key in a full live capture — so the gateway sets it, and reads
  `metadata.background` in case a later version starts sending one.
- `truncated` means the result the user is reading is clipped, either by
  OpenCode's `tool_output` limits or by the gateway's own 64 KiB cap.
- The subagent tool is named `subagent`; `task` is accepted as an alias. It is a
  tool row, not a todo list. Only `todowrite`/`todo`/`tasks` become `todo`.

**`compaction`**

```json
{ "type": "compaction", "status": "running" | "completed" | "failed",
  "reason": "auto" | "manual", "summary": "…", "recent": "…",
  "tokens": {...}, "cost": 0.5, "error": {...} }
```

**`skill`** — `{ "type": "skill", "skill": "…", "name": "…", "text": "…" }`

**`shell`**

```json
{ "type": "shell", "shell_id": "sh_1", "command": "sleep 60",
  "status": "running" | "exited" | "timeout" | "killed",
  "exit": 0.0, "output": "…", "truncated": false }
```

**`model_switched`** — `{ "type": "model_switched", "model": {…}, "previous": {…} }`

**`agent_switched`** — `{ "type": "agent_switched", "agent": "plan", "previous": "build" }`

**`synthetic`** / **`system`** — `{ "type": "synthetic", "text": "…", "description": "…" }`

**`location_switched`** — `{ "type": "location_switched", "directory": "/a", "previous": "/b" }`

**`todo`** — `{ "type": "todo", "items": [ { "text": "…", "done": true } ] }`

**`diff`** — `{ "type": "diff", "file": "…", "diff": "…" }`

**`approval`** — `{ "type": "approval", "request": { …PermissionRequest… } }`

**`form`** — `{ "type": "form", "request": { …FormRequest… } }`

**`status`** — `{ "type": "status", "text": "…" }`

### `PermissionRequest`

```json
{
  "id": "per_1",
  "asid": "ses_1",
  "action": "external_directory",
  "resources": ["/etc/hosts"],
  "save": ["/etc/*"],
  "prompt": "external_directory: /etc/hosts",
  "tool": "tool",
  "source_message_id": "msg_1",
  "source_tool_call_id": "call_1",
  "metadata": { "why": "probe" },
  "message": "…",
  "options": [
    { "index": 0, "label": "Allow Once",   "decision": "allow" },
    { "index": 1, "label": "Always Allow", "decision": "allow_always" },
    { "index": 2, "label": "Reject",       "decision": "deny" }
  ]
}
```

`save` is what an "always" would whitelist project-wide — show it next to the
option. `source_tool_call_id` matches a tool row's `part.id`, so the prompt can
be attached to the exact card.

### `FormRequest`

```json
{ "id": "frm_1", "asid": "ses_1", "title": "…", "fields": [ … ] }
```

Fields are tagged by `type`; all of them carry `key`, `title`, `description?`,
`required` and `when`.

```json
{ "type": "string", "key": "tag", "title": "Tag", "required": true,
  "when": [ { "key": "env", "op": "eq" | "neq", "value": "prod" } ],
  "placeholder": "…", "default": "…", "options": [ {"value","label","description?"} ],
  "format": "email"|"uri"|"date"|"date-time",
  "min_length": 2, "max_length": 40, "pattern": "^v", "custom": true }

{ "type": "number", "key": "…", "min": 0, "max": 10, "default": 1, "when": [...] }
{ "type": "boolean", "key": "…", "default": false, "when": [...] }
{ "type": "multiselect", "key": "…", "options": [...], "default": ["a"], "when": [...] }
{ "type": "external", "key": "…", "url": "https://…", "when": [...] }
{ "type": "unknown", "key": "…", "title": "…", "raw_type": "…", "when": [...] }
```

A field is hidden unless every `when` condition holds against the current
answers. `custom` means a value outside `options` is allowed.

---

## Limits

The mirror is a cache in front of OpenCode, not a store; anything it drops can
be refetched, and a client that has fallen behind gets `resync_required`.

| Bound | Value |
|---|---|
| Timeline rows per session | 1500 (oldest dropped) |
| Tool output kept per call | 64 KiB, then cut and `truncated: true` |
| Event log per session | 2000 events or 4 MiB |
| Sessions in memory | 200, and anything untouched for 6 h |
| SSE reassembly buffer | 8 MiB per frame |

Sessions are also evicted on `session.deleted`.

---

## Things that do not exist in v2, so the gateway does not offer them

- **Session sharing.** Removed upstream. Use `GET …/export`.
- **A model/agent field on a prompt.** Model and agent are session state.
- **`session.status` / `session.idle` events.** Status comes from
  `session.execution.*`; the gateway accepts the others if a later version
  starts sending them.
- **A title endpoint.** Auto-titling is server-side; `rename` is the only write.
- **Credential, integration or provider-auth routes.** The user configures
  OpenCode on the host.
