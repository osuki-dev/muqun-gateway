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
  `saved_permission_not_found` (404), `workspace_missing` (404),
  `resync_required` (410), and the `invalid_*` family (400).
- **A folder that is gone.** Every route that names a workspace directory —
  `vcs/diff`, the worktree routes, `agent-catalog?directory=`, the file search,
  and a session move's target — checks the directory is still on the host
  before proxying. If it is not, the answer is **`404 workspace_missing`**, and
  it carries the path as its own field so the app can offer to forget the
  session rather than read it out of a sentence:

  ```json
  { "error": { "code": "workspace_missing",
               "message": "The workspace folder is gone: /tmp/muqun-c10/repo",
               "directory": "/tmp/muqun-c10/repo" } }
  ```

  This used to be a `502 agent_engine_error` whose message was empty: OpenCode
  answers a deleted directory with a bare HTTP 500 and no body, and the gateway
  relayed it verbatim. It told the user their agent had broken when their
  folder had simply been deleted. A 502 is now never blank either — an upstream
  failure with no body reads `OpenCode answered 500 to GET /api/vcs/diff`, so
  there is always a status and a route to go on.
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

### `POST /api/agent-sessions/{asid}/revert` and its two halves

A rollback is two steps in OpenCode: a boundary is **staged**, which changes
nothing on disk and can still be withdrawn, and then **committed**, which
deletes the boundary message and everything after it and puts the files back.
The app stages, shows the user what would go, and commits or clears.

- `POST …/revert/stage`

  ```json
  { "message_id": "msg_1", "files": true }
  ```

  → the staged boundary, `Session.Revert` in the gateway's own snake_case:

  ```json
  { "revert": { "message_id": "msg_1", "part_id": null, "snapshot": null,
                "files": [ { "file": "src/a.ts", "patch": "@@ …", "additions": 3,
                             "deletions": 1, "status": "modified" } ] } }
  ```

  `files` is optional: `true` asks OpenCode to work out the file changes the
  rollback would undo and return them as `FileDiff.Info[]`, which is what the
  confirmation draws. Leave it out and OpenCode decides; `files` then comes back
  `null` or empty. Staging again with another `message_id` moves the boundary.
  An unknown message is `404` from OpenCode, and staging while the session is
  running is `409` — both arrive as `agent_engine_error`.

- `POST …/revert/commit` → `{"committed": true}`. Applies what is staged.
  OpenCode answers `204`. With nothing staged this is a no-op, not an error.

- `POST …/revert/clear` → `{"cleared": true}`. Withdraws the staging — redo.

- `POST …/revert` `{"message_id": "msg_…"}` → `{"status": "ok", "reverted_to": "msg_…"}`.
  The original one-shot: stage with `files: true` and commit, in one call. It is
  unchanged and still supported; new work should use the two steps, because a
  rollback that cannot be previewed cannot be confirmed.

While a rollback is staged, `info.revert` is set — on the snapshot as well as on
the stream, and the mirror is updated from the event, so a refetch taken at any
point agrees with what was streamed. Every step is announced as
[`agent.revert.changed`](#agentrevertchanged).

---

## Worktrees

A worktree is a second checkout of the same repository that OpenCode manages
for you, so an agent can work on a branch without disturbing the one you have
open. The gateway proxies OpenCode's inventory and adds nothing of its own.

`directory` on every route here is the **project** directory — the repository
root whose inventory is being read or changed — and never a worktree's own.
It goes to OpenCode as the deep-object `location[directory]` these endpoints
take; omitted, OpenCode falls back to its own default location.

### `GET /api/agent-worktrees`

`?directory=/abs/repo` → `{"items": [Worktree.Directory]}`:

```json
{ "items": [
  { "directory": "/home/ryu/repo" },
  { "directory": "/home/ryu/.local/share/opencode/worktree/016d5f/probe", "strategy": "git" }
] }
```

The project's own root is in the list, and it is the entry **without a
`strategy`**: it is not a worktree OpenCode created, and `DELETE` refuses it.
Entries OpenCode manages carry `strategy` (`"git"` on 2.0.1).

### `POST /api/agent-worktrees`

```json
{ "directory": "/abs/repo", "name": "probe", "branch": "main" }
```

→ `{"worktree": {"directory": "…/worktree/016d5f/probe"}}` (`Worktree.Info`).

Every field is optional, `directory` included — `{}` is a valid create and
OpenCode names the worktree itself (it picked `sunny-circuit` when asked). The
two that matter:

- **`name`** is the worktree directory's name.
- **`branch`** is an **existing ref to branch from**, not a name to create.
  `{"branch": "probe-branch"}` against a repo without that ref is
  `fatal: invalid reference: probe-branch`, as a `502 agent_engine_error`.

`from` and `strategy` are passed through as `Worktree.CreateInput` defines
them; `from` is a directory rather than a ref (`{"from": "main"}` answers
`Worktree directory unavailable: main`). Fields the caller did not set are left
out of the request entirely, because the input declares
`additionalProperties: false` and an explicit `null` is refused.

Creation is synchronous: the route answers when the worktree exists. The
project's inventory change arrives separately as
[`agent.worktree.changed`](#agentworktreechanged).

### `DELETE /api/agent-worktrees`

```json
{ "directory": "/abs/repo", "worktree": "…/worktree/016d5f/probe", "force": true }
```

→ `{"deleted": true}`. **Two directories, and they are not the same one**:
`directory` is the project, `worktree` is the checkout being removed.

`force` defaults to `false` here. OpenCode's own `Worktree.RemoveInput` makes it
required — omitting it is `Missing key at ["force"]` — so the gateway always
sends it. A worktree with local changes refuses without it, and the refusal
carries `forceRequired: true`.

### `POST /api/agent-worktrees/refresh`

`{"directory": "/abs/repo"}` (optional body) → `{"refreshed": true}`.
Rediscovers worktrees on disk and reconciles the project's inventory, for when
something changed outside OpenCode.

### `POST /api/agent-sessions/{asid}/move`

`{"directory": "/abs/path"}` → the session as it is afterwards
(`AgentSessionInfo`), read back rather than assembled from the request.

This is what a worktree is for: point an existing session at one. The reply is
re-read because a move can change more than the directory — see the scope note.

**There is no scope check, because 2.0.1 expresses no scope rule.** The spec
requires only `directory`; the live service accepts *any* directory that
exists, including one belonging to another project, and the session then joins
that directory's project — `project_id` changes with it. A directory that does
not exist is `400 Directory does not exist: …`, surfaced as
`agent_engine_error`, and that is the whole of the rule. The gateway does not
add one of its own: refusing a move OpenCode allows would be the gateway
inventing policy it was not given. A client that wants to keep a session inside
one project should offer only that project's
[worktree list](#get-apiagent-worktrees) as targets.

The move also arrives on the stream as `agent.session.updated` with the new
`directory` — mapped straight from `session.moved`'s own payload, so it does not
wait on a refetch.

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

### `GET /api/agent-sessions/{asid}/permissions/saved`

What an `allow_always` left behind — `PermissionSaved.Info`, scoped to this
session's project:

```json
{ "items": [ { "id": "psv_1", "project_id": "cb4a45a3…",
               "action": "bash", "resource": "git status" } ] }
```

`action` and `resource` are the pair the permission prompt showed as `save`.
The list is a project's, not a session's — the session names the project, and
the gateway reads `projectID` off it on every call rather than trusting a cached
one. A session whose project OpenCode does not report is `502
agent_engine_error`, not an unscoped list of everything.

### `DELETE /api/agent-sessions/{asid}/permissions/saved/{id}`

`{"deleted": true}` — the agent asks again next time. OpenCode answers `204`.

`id` is checked against this session's project before anything is deleted: a
device holding one session must not reach into another project's list through
it. An id that is not there — including one already deleted — is

```json
{ "error": { "code": "saved_permission_not_found",
             "message": "no such saved permission in this session's project" } }
```

with `404`.

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

```json
{ "files": [ { "path": "src/a.rs", "patch": "@@ …", "additions": 3, "deletions": 1 } ],
  "vcs": "git",
  "reason": null }
```

> **Shape change.** This used to be a bare array. It is now an object, and the
> array is under `files`. The two new fields are why — see below.

Scoped to the session's own directory. `mode` is required by OpenCode; omitting
it is why this used to come back empty.

OpenCode answers `200` with an empty list **both** for a clean repository and
for a directory that is not a repository at all, so on its own the app could
not tell "nothing has changed" from "there is nothing here to change", and
showed the same empty screen for both. So:

- `vcs` is `"git"` when the session's directory is inside a git working tree,
  and `null` when it is not.
- `reason` is `"not_a_repository"` in that second case, and `null` otherwise.

A directory that no longer exists is not this case: that is
[`404 workspace_missing`](#conventions).

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

`?directory=` scopes the catalog **up**, never down: the agents, commands and
skills a project defines are added to the global ones, and a directory never
returns fewer agents than no directory. The gateway enforces that, because
OpenCode's `GET /api/agent` is a snapshot that fills in over roughly a second
for a directory nothing has opened yet — first empty, then the built-ins, then
the user's own agents. A catalog request waits for the scoped list to hold
everything the unscoped one holds (bounded, about 1.5s) rather than handing
over whichever stage it caught. A catalog that still has no agents is answered
`cache-control: private, no-store` and **without an ETag**, so an empty picker
can never be cached.

User-defined agents come from `~/.config/opencode/agents/<name>.md` (global),
`.opencode/agents/<name>.md` (per project, discovered from the directory up to
the project root) and an `agent` block in `opencode.json`. They arrive with
`mode`, `description`, `color` and `hidden: false` like any other.

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
explain why the others are returning 503.

`origin` is how the engine currently attached was obtained: `adopted` for a
service that was already running, `spawned` for one this gateway started,
`none` when nothing is attached.

### Which OpenCode, and who starts it

The gateway keeps an engine attached for its own lifetime. It adopts a healthy
service if one is registered, and otherwise starts
`opencode serve --service` — unless `opencode.autostart` is `false` in
`config.json`.

**The binary is `opencode.binary` from `config.json`, or `opencode` as `PATH`
resolves it. There is no third place.** The gateway does not look inside an
install directory of its own: where OpenCode lives differs per OS and per
install, and a gateway reaching into one would quietly run a different binary
than the owner's shell does. If `PATH` is not the right answer, name the file:

```json
{ "opencode": { "autostart": true, "binary": "/absolute/path/to/opencode" } }
```

A configured path that does not exist is an error naming it, never a quiet fall
back to `PATH`.

**That matters because the two ways of running the gateway have different
`PATH`s.** `muqun-gateway service install` runs it under systemd with the
unit's own environment; `muqun-gateway start` runs it from the shell you typed
in, and inherits that shell's `PATH` — including any version manager earlier on
it. The same machine can resolve `opencode` to two different files depending on
which one you used, so the gateway logs the file it actually resolved, at
`INFO`, on every start and every adopt:

```
INFO no OpenCode service found, starting one binary=/home/you/.opencode/bin/opencode version="opencode v2.0.1"
INFO started an OpenCode service and attached to it url=http://127.0.0.1:49374 version="2.0.1" binary=/home/you/.opencode/bin/opencode
INFO adopted the running OpenCode service url=http://127.0.0.1:49374 version="2.0.1" binary=/home/you/.opencode/bin/opencode
```

For an adopted service the path is read off the running process, because *which
`opencode` am I talking to* is the question after a restart and a bare
`opencode` does not answer it.

**Anything below 2.0 is refused, started or adopted.** v1 is a different API:
attaching to one used to look like success and then fail on every route. The
refusal is one `ERROR` naming the file or URL, the version it reported, and what
to do:

```
ERROR refusing to start /usr/local/bin/opencode: it reports version opencode 1.18.4,
      and this gateway speaks OpenCode 2.x only. Install OpenCode 2, or point the
      gateway at the right one by setting `opencode.binary` to its absolute path
      in config.json
```

A version that cannot be read is allowed through — silence is not evidence of
being old — so only a legible version below 2.0 is refused.

### Losing the engine

The supervisor re-discovers whenever the health probe fails or the registration
moves. It also watches the event stream, and **a dropped stream is acted on
within about a second** rather than at the next health poll: the stream going
down is the engine telling us it has gone, and waiting out the poll interval
left the app silent for as long as it had left to run. A stream that keeps
dropping backs off — immediately the first time, then 1s doubling to 30s — so
flapping cannot spin the supervisor, and a full healthy interval resets it.

---

## Streaming

### `GET /api/agent-sessions/{asid}/stream`

`text/event-stream`, filtered to one session. First frame is `event: connected`
with `{"asid": "…"}`; keep-alive every 15 s. Subscription survives OpenCode
restarting underneath it.

**Sealed when the device's transport is encrypted.** A response that never ends
cannot be authenticated as a whole, so each event is sealed on its own —
exactly as `GET /api/sessions/{id}/events` has always done, and with the same
record shape, so a client that can read one can read the other:

```
event: <ENCRYPTED_SSE_EVENT>
data: {"v":1,"sid":"<stream id>","seq":0,"ciphertext":"…"}
```

The key is derived from the device's transport material, the stream id and the
request nonce; the AAD is the request AAD, the stream id and the sequence
number, and the sequence number is also the nonce — so a record moved or
replayed into another slot never opens. Opened, each record is
`{"event": "<name>", "data": "<the payload as a string>"}`, where `<name>` is
the event name the plaintext stream would have used. A record that cannot be
sealed is **dropped**, never sent in the clear.

A device paired without a transport key — a `transport_encryption: disabled`
deployment — gets the plaintext stream byte for byte, under the event's own
name. Nothing about cleartext changed.

> Until this release the stream was never sealed, on any deployment. A gateway
> configured `transport_encryption: required` still put the device token and
> every agent event on the wire in the clear. A client that opens this with a
> bare `Authorization` header rather than the encrypted-stream path will now
> receive sealed frames it cannot read on such a deployment, and must be
> updated.

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

### `agent.revert.changed`

```json
{ "type": "agent.revert.changed", "asid": "ses_1", "seq": 22, "state": "staged",
  "revert": { "message_id": "msg_1", "part_id": null, "snapshot": null,
              "files": [ { "file": "src/a.ts", "patch": "@@ …", "additions": 3,
                           "deletions": 1, "status": "modified" } ] } }
{ "type": "agent.revert.changed", "asid": "ses_1", "seq": 23, "state": "cleared",
  "revert": null }
{ "type": "agent.revert.changed", "asid": "ses_1", "seq": 24, "state": "committed",
  "revert": null }
```

`state` is `staged | committed | cleared`. `revert` is always present as a key:
the staged boundary on `staged`, and `null` on the other two, where nothing is
staged any more. `info.revert` on the session follows the same values.

The three come from OpenCode's own `session.revert.staged`,
`session.revert.committed` and `session.revert.cleared`, whose payloads on 2.0.1
are — captured live against the running service:

```json
{"type":"session.revert.staged",    "data":{"sessionID":"ses_1","revert":{"messageID":"msg_1","files":[]}}}
{"type":"session.revert.cleared",   "data":{"sessionID":"ses_1"}}
{"type":"session.revert.committed", "data":{"sessionID":"ses_1","to":"msg_1"}}
```

`committed` is also the only notice that rows have gone: OpenCode deletes the
boundary message and everything after it and has no message-removed event, so
the gateway drops those rows from the mirror and sends
[`agent.timeline.removed`](#agenttimelineremoved) for them in the same breath.

### `agent.inbox.changed`

```json
{ "type": "agent.inbox.changed", "session_id": "ses_1", "seq": 21,
  "items": [ { "id": "msg_1", "sessionID": "ses_1", "type": "user", "payload": {...}, "delivery": "queue" } ] }
```

The whole queue every time, so there is no diff to reconcile.

### `agent.worktree.changed`

```json
{ "type": "agent.worktree.changed", "state": "updated",
  "directory": "/home/ryu/repo", "project_id": "016d5ff1…" }
{ "type": "agent.worktree.changed", "state": "resolved",
  "directory": "/home/ryu/repo", "project_id": "016d5ff1…" }
{ "type": "agent.worktree.changed", "state": "ready", "directory": "/home/ryu/repo",
  "name": "probe", "branch": "main" }
{ "type": "agent.worktree.changed", "state": "failed", "directory": "/home/ryu/repo",
  "error": "fatal: invalid reference: nope" }
```

A worktree belongs to a project, not a session, so this event **carries no
`asid`** — which is exactly how it reaches everything: an event with no session
goes to every `GET /api/agent-sessions/{asid}/stream` as well as the
device-wide `GET /api/sessions/{id}/stream`, the same way `agent.resync` does.
There is no `seq`: it is not replayable from a session's ring buffer, and
`GET …/events?after=` will not hand it back.

`state` is:

| `state` | From | Meaning |
|---|---|---|
| `updated` | `worktree.updated` | The project's inventory changed — re-list it. |
| `resolved` | `worktree.resolved` | A location resolved to this worktree directory. |
| `ready` | `worktree.ready`, `workspace.ready` | A worktree was prepared; `name` and `branch` describe it. |
| `failed` | `worktree.failed`, `workspace.failed` | `error` carries the message. |

`directory` is the event's own when it has one (`worktree.resolved`) and
otherwise the project directory off the event envelope's `location`, because
`worktree.updated` names only `projectID`.

**There is no `creating`.** Nothing in 2.0.1 announces a creation starting:
`POST /api/worktree` blocks until the worktree exists, and the inventory change
follows. A client that wants a spinner should show it around its own request.

What 2.0.1 actually emits, measured by driving a full create / list / refresh /
move / remove cycle against the live service while tailing `/api/event`:

```json
{"type":"worktree.resolved","data":{"projectID":"016d5ff1…","directory":"/tmp/muqun-gw-wt","previous":"global"}}
{"type":"worktree.updated","location":{"directory":"/tmp/muqun-gw-wt"},"data":{"projectID":"016d5ff1…"}}
```

`worktree.ready`, `worktree.failed` and the `workspace.*` pair did **not**
appear in any of it. They are in the binary's event registry — with schemas
`{name, branch?}`, `{message}` and, for `workspace.status`, `{workspaceID,
status}` — so the first two are mapped here in case a flow that does emit them
turns up. `workspace.status` is deliberately **not** mapped: it reports a
remote workspace's *connection* state (`connected` / `connecting` /
`disconnected` / `error`), names no directory, and has nothing to say about a
worktree; folding it in would wake a client waiting on its checkout every time
a socket reconnected.

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
  - `streaming` — the input is still arriving; read `input_partial`, not `input`.
  - `running` — dispatched.
- `input_partial` is the card's only content while the arguments are still
  arriving, so the same call a moment earlier reads:

  ```json
  { "type": "tool", "id": "call_1", "name": "shell", "state": "streaming",
    "status": "streaming", "input": null,
    "input_partial": "{\"command\":\"echo hel", "time": { "created": 1 } }
  ```

  It is the tool's arguments as they arrive, the concatenation of every
  `session.tool.input.delta` so far, which is raw text and **not valid JSON
  until it ends**. It is absent at `pending`, absent again the moment a real
  `input` lands, and never present alongside one — `input` is `null` while the
  preview is live, because half an argument list is not an input. It is capped
  at **8 KiB**; a longer argument stops growing the preview and arrives whole
  as `input` a moment later.

  The 2.0.1 payload behind it, from the event schema the binary declares:

  ```json
  {"type":"session.tool.input.started","data":{"sessionID":"ses_1","assistantMessageID":"msg_1","id":"call_1","name":"shell"}}
  {"type":"session.tool.input.delta",  "data":{"sessionID":"ses_1","assistantMessageID":"msg_1","id":"call_1","delta":"{\"command\":\""}}
  {"type":"session.tool.input.ended",  "data":{"sessionID":"ses_1","assistantMessageID":"msg_1","id":"call_1","text":"{\"command\":\"echo hello\"}"}}
  ```

  The chunk is `delta` and the whole of it is `text`; OpenCode's own TUI and
  transcript builder concatenate the one and then replace it with the other.
  Whether a call streams at all is the provider's choice — the `opencode` free
  models send no deltas and jump from `started` to `ended`, so a card that
  never shows a preview is not a fault.
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
option, and what the entry in
[`GET …/permissions/saved`](#get-apiagent-sessionsasidpermissionssaved) is made
of afterwards. `source_tool_call_id` matches a tool row's `part.id`, so the prompt can
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
