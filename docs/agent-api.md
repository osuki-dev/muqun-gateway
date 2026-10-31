# Agent API

The gateway's agent surface: what the mobile app calls, what comes back, and
what arrives on the event stream. The agents behind it are OpenCode, DeepSeek,
T3 Code and Codex; where this document describes OpenCode's behaviour, it is
OpenCode's.

Optional account rate windows use the provider-neutral
[account limits contract](agent-account-limits.md), separately from session
context/token usage. Gate the existing context-panel section on
`agent_account_limits_v1` and the selected agent's `features.accountLimits`.

Every field name here is the one the code serializes — snake_case, as the
domain types spell it — with two deliberate exceptions, both marked below:
`metadata` and `content` on a tool call are OpenCode's own payloads, forwarded
verbatim, so their keys stay camelCase.

## Conventions

Terminal `/agents` and `/snapshot` responses advertise the optional
`terminal_pending_user_input` capability when they can relay Herdr's native
`pending_user_input` boolean. An exact `true` means an ordinary native question
is pending; it can coexist with `working`, `idle`, or a focused input dialog.
An approval overlay or temporarily unknown screen preserves previously known
pending attention without permitting `agent.prompt`; it is independent of
permission approval. Older Herdr versions omit the fact and are represented as
unknown (`null`); this must not be confused with an observed `false`. Clients
must not infer a question from `blocked`, titles, or versions.
The fact identifies pending attention, not a question form or request ID.
Sending an ordinary follow-up does not constitute answering the native question.
The notification watcher remembers this boolean per opaque agent instance and
raises `agent.input_required` on a newly observed pending boundary. Continued
work-state changes or entering/leaving native answer/approval controls while
pending do not repeat the reminder. Only an explicit false or disappearing
instance clears the reminder boundary. Startup seeds
already pending instances without replaying their old reminders. No request ID
is available, so replacing one pending question with another without a false
observation cannot be distinguished or announced separately.

Native agent rows also relay optional `shell_pid`, observed with the agent, for
correlating an existing pane task with its shell incarnation. Missing or invalid
PIDs remain unknown. This field is not a substitute for opaque `instance_id`:
another agent can start in the same shell, and the operating system can reuse PIDs.

- **Auth.** Every route requires a paired device (`Authorization: Bearer
  <device token>`); a missing or malformed header is `401` and an unpaired or
  revoked device is `403`. This holds in every transport mode — a gateway with
  `transport_encryption: disabled` drops the envelope, not the token.
- **Envelope.** A successful JSON body is wrapped:
  `{"schema_version": "...", "capabilities": {...}, "data": <payload>}`. The
  payload column below describes `data`.
- **Instance generation.** The gateway mints one opaque string, `generation`,
  when the process starts and never changes it while the process lives.
  Everything it keeps only in memory — above all the per-pane scrollback it
  stitches into terminal reads — is valid within one generation. It appears as
  top-level `generation` on `GET /health`, `GET /api/meta`, `GET /api/discovery`
  and `GET /api/capabilities`; as `generation` in the `hello` frame of
  `GET /api/ws`; and on every pane read and streamed output frame (see
  `docs/content-model.md`, "Instance generation"). Compare it for equality
  only. When it changes, drop the terminal rows held from before and read
  again instead of merging the new reads under them. A gateway that predates
  the field sends none; treat absence as "unknown" and keep the old behaviour.
- **Errors.** `{"error": {"code": "...", "message": "..."}}`. The codes used
  here are `agent_unavailable` (503, no agent attached),
  `agent_error` (502, OpenCode refused), `session_not_found` (404),
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

  This used to be a `502 agent_error` whose message was empty: OpenCode
  answers a deleted directory with a bare HTTP 500 and no body, and the gateway
  relayed it verbatim. It told the user their agent had broken when their
  folder had simply been deleted. A 502 is now never blank either — an upstream
  failure with no body reads `OpenCode answered 500 to GET /api/vcs/diff`, so
  there is always a status and a route to go on.
- **Directories may start with `~`.** Every agent route that takes a
  directory — `POST /api/agent-sessions`, `?directory=` on the session list,
  `agent-catalog`, the file search, `agent-shells` and the worktree routes, a
  session move's target, and a task's `repo_path` / spawn's `cwd` — accepts
  `~` and `~/…` for the gateway user's home as well as an absolute path. The
  gateway expands it before any check and, when the folder exists, sends and
  answers the canonical absolute path, so `~/Work/x` and `/home/u/Work/x` are
  one project. `~user` is not expanded and fails as any non-absolute path does
  (`400 invalid_directory` on create); a `~/…` that does not exist is the
  route's usual not-found answer (`400 directory_not_found` on create,
  `404 workspace_missing` with the expanded path elsewhere).
- **Compression.** Responses are gzipped (or brotli'd) when the client asks
  with `Accept-Encoding` and the body is over 512 bytes. `text/event-stream`
  and already-compressed content such as an uploaded image are never touched.
  Every answer carries `Vary: accept-encoding`; a validated one carries
  `Vary: accept-encoding, accept-language`.

  Compression happens **inside** the encrypted transport, on the plaintext,
  because ciphertext does not compress.

- **Compression inside the envelope.** An encrypted client is reading a base64
  body whose real headers are sealed with it, so it cannot use
  `Content-Encoding` the ordinary way. It opts in by request header instead:

  ```
  X-Muqun-Envelope-Accept: gzip
  ```

  and the sealed payload then carries a **top-level `content_encoding` field,
  beside `headers`** — not a line inside the header map, which is where a
  `content-encoding` would tell the client to inflate a body it has already
  inflated:

  ```json
  { "status": 200, "headers": { … }, "body": "<base64>", "content_encoding": "gzip" }
  ```

  `gzip` is the only value ever sent, and it is sent only to a client that
  asked for it by name — a request without the header is answered exactly as
  it was before this existed, so an older app is unaffected. Absent means the
  body is the response body.

- **`asid`** is the owning agent's native session/thread id (for example,
  OpenCode's `ses_…` or a Codex thread id). The gateway does not mint ids of its own.
- **Legacy paths.** Everything under `/api/agent-*` also exists under
  `/api/sessions/{session_id}/agent-*` for the routes that had that form before;
  `session_id` is parsed and ignored. Routes added for v2 parity are on the
  global paths only.

---

## Agents

The gateway can have more than one agent attached at once. Today the ids are
`opencode`, `deepseek`, `t3` (T3 Code) and `codex`. OpenCode is preferred: when a
request names no agent, the **primary** answers — the first attached of
`opencode`, `deepseek`, `t3`, `codex`, in that order. The primary is only that internal
preference order; the contract names no "active" agent. Inside an agent, a
persona such as OpenCode's `build` or `plan` is a **mode**; T3 Code and Codex have none,
so their `modes[]` are empty.

Discovery's App picker order is separate from implicit routing: an enabled
Codex with status `connected` or `reachable` is listed first, followed by
OpenCode. Other providers retain their relative order. Codex in any unavailable
state is not promoted. This presentation preference must not override the
App's explicit saved provider selection. Without a selection, clients should
choose the first usable entry rather than an unavailable row.

### Feature gate

`/health` and `/api/capabilities` list **`multi_agent`** in their flat
`capabilities` array. An App should offer an agent picker only when it is
present; a gateway without it has one agent and ignores every `agent_id`
parameter below.

### Discovery payload

Agent discovery is on `GET /api/discovery` and `GET /api/capabilities`
(identical body), at `planes.agents`, which is the only key for this plane.
**This payload is camelCase**, unlike the rest of this document, and is *not*
wrapped in the envelope: the top level is `{"ok", "gatewayVersion",
"apiVersion", "apiMajor", "platform", "serverId", "label", "planes",
"transports", "capabilities"}`; `transports` is described under "WebSocket
events" below. Both routes require a paired device (of either kind) or the
admin token, like `/health`: a caller with no credential is `401`, a wrong one
`403`. Only the owner's app device gets the agents' `endpoint` and `version`;
a linked gateway and the admin token get the document with those removed.
Nothing in the document is served unauthenticated -- it names the computer's
id, label, version and configuration.

`GET /health` and `GET /api/meta` retain the same plane structure, but their
agent plane is a local runtime snapshot: they do not probe upstream providers
or read model catalogs. Attached providers report their current readiness;
an enabled provider without an attachment reports `unknown`, not an inferred
offline or unsupported verdict. Their `models` and `modes` arrays are empty.
Use the explicit discovery routes above for complete provider features and
catalogs. Terminal liveness, compatibility and transport facts remain live.

```json
{
  "supported": true,
  "agents": [
    {
      "id": "opencode",
      "name": "OpenCode",
      "kind": "opencode",
      "status": "connected",
      "enabled": true,
      "endpoint": "http://127.0.0.1:49374",
      "version": "2.0.1",
      "models": [
        { "id": "union-alpha", "name": "Union Alpha", "providerId": "opencode",
          "supportsReasoning": true, "reasoningEffortTiers": ["low", "high"] }
      ],
      "modes": [ { "id": "build", "name": "build", "description": "…" } ],
      "features": {
        "streaming": true, "reasoningEffort": true, "modelSelection": true,
        "toolApprovals": true, "worktrees": true, "revert": true, "stagedRevert": true,
        "inbox": true, "modes": true, "skills": true, "slashCommands": true, "compaction": true,
        "backgroundShells": true, "attachments": true, "attachmentsByPath": false
      }
    }
  ],
  "features": { "multiAgent": true, "catalogAggregation": true, "sessionRouting": true }
}
```

- `agents[]` has one entry per known agent, attached or not. `id` is the value
  to send as `agent_id` everywhere below; `kind` equals `id` today, and equals
  the `agent_id` on that agent's sessions.
- `status` values are snake_case: `connected` (attached and answering),
  `reachable` (an endpoint answers but it is not attached), `offline`,
  `disabled`, `not_installed`, `unconfigured`, `unsupported` (the agent
  answers but speaks a protocol this gateway cannot drive, including unknown T3
  descriptor protocols or a Codex version outside its pinned baseline).
  `connected` and `reachable` can
  be selected: `reachable` means the gateway will attach on first use. The
  rest cannot. `muqun-gateway agent` reads this list from the running
  gateway's `/api/capabilities` (over loopback with the admin token, which
  sees versions and endpoints like the owner's App; a linked gateway does
  not) and prints the next step for each. Only when no gateway answers does
  it probe the same list from the shell; that probe never attaches or signs
  in, so it shows `reachable` where the gateway may say `connected`, and
  Codex, whose sign-in only the gateway checks, as `unknown` rather than
  `unconfigured`. The local probe still checks a held T3 bearer and reports
  `unconfigured` when the server refuses it.

  People read six words, the same in the App, `agent` and `manage`; the wire
  `status` is unchanged and `agent --json` still prints it:

  | Word | wire `status` |
  |---|---|
  | ready | `connected`, `reachable` |
  | needs setup | `unconfigured` |
  | not running | `offline` |
  | not installed | `not_installed` |
  | needs update | `unsupported` |
  | off | `disabled`, or `enabled: false` |
  | unknown (not a state, nothing checked it) | `unknown` |

  | `status` | Next step `muqun-gateway agent` prints |
  |---|---|
  | `reachable` | none |
  | `connected` | none |
  | `unconfigured` (t3: no bearer, or a refused one) | `muqun-gateway agent setup t3` |
  | `unconfigured` (codex: authentication missing) | configure the existing Codex CLI login on the host; `muqun-gateway agent setup codex` explains setup without starting it |
  | `disabled` | `muqun-gateway agent setup t3` / `muqun-gateway agent setup deepseek`; for OpenCode or Codex, set the corresponding `enabled` key; check `agent_runtime.enabled` for the master switch |
  | `offline` | the agent's own start command: `opencode service start`, or "open T3 Code, or run `t3 service install`" |
  | `offline` (deepseek) | Gateway starts or joins DeepSeek Harness; run `muqun-gateway agent setup deepseek` for the manual command (`dsh`, otherwise `bunx`, otherwise `npx --yes`, still `web --no-open`, host and port from `deepseek.endpoint` or `http://127.0.0.1:3080`) |
  | `offline` (codex) | check the host's Codex configuration; failed prompts are never automatically retried |
  | `not_installed` | install OpenCode 2 (or set `opencode.enabled` to false) / T3 Code / Codex 0.162.1 (or set `codex.enabled` to false) |
  | `not_installed` (deepseek) | install `dsh`, or have `bunx` or `npx` on PATH; a missing runner does not stop the gateway, and status does not download or start the harness |
  | `unsupported` | check the protocol/version baseline and Gateway compatibility |
- `reason` is a one-line, human-readable explanation, present only when the
  status alone does not say why (for example, an unsupported T3 descriptor
  protocol or unchecked Codex readiness). Show it, don't
  parse it.
- `signInFailedAtMs` (capability `agent_signin_state`) is present on an ACP
  agent (FX, Qoder CLI, and every other agent the shared ACP adapter drives)
  whose last `session/new` or `session/load` was refused with ACP's
  `auth_required` (-32000). Discovery then reports it `unconfigured` with a
  `reason` saying it was not signed in the last time a session was opened, so
  the App guides the user instead of hitting the same refusal again. The
  gateway does not start the agent to re-check. It is cleared by the next
  open that succeeds (an existing session counts) or by
  `POST /api/agents/{kind}/signin-check` (owner App or admin token; answers
  `{"kind", "cleared"}`, 404 when the agent is not attached), which an App's
  "check again" calls before retrying. The memory is per process: a gateway
  restart forgets it.
- `endpoint` and `version` are omitted when unknown or redacted. `models[]` and
  `modes[]` are that agent's own catalog summary; `reasoningEffortTiers` is
  omitted when empty; `modes[].description` when unset.
- `ledgerDelivery` (per agent) lists the deliveries `POST /api/tasks` can
  perform for that `agent_id`: `"session"` (a session adapter of that kind
  is attached) and/or `"pane"` (a terminal launcher is known). See
  `docs/mcp.md`, capability `task_ledger_delivery`.
- `visibility` and `picker` (per agent, capability `agent_visibility`) say
  where the App's new-session picker lists the agent. `visibility` is
  `"hidden"` when the owner hid it (config.json `agent_visibility`, keyed by
  agent kind) and `"auto"` otherwise; treat any other value as `"auto"`.
  `picker` is computed by the gateway, so the App keeps no per-agent rules:
  `"more"` when the agent is hidden, `enabled: false`/`disabled`, or
  `not_installed`; `"default"` for everything else (ready, needs setup, not
  running, needs update). A hidden agent is still probed, attached and listed
  here with its real `status` and `features`, and its existing sessions stay
  readable and usable; only the new-session list changes. Visibility applies
  only to session agents in this array; terminal-pane agents
  (`GET /api/agents/catalog`) are unaffected.

  `PUT /api/agents/{kind}/visibility` with `{"visibility": "auto"|"hidden"}`
  changes it for every device. The owner's App or the admin token only (a
  linked gateway gets 403); any other value is 400 `invalid_visibility`, a
  kind outside 1-64 of `[A-Za-z0-9_-]` is 400 `invalid_agent`. It answers
  `{"kind", "visibility"}`. `"hidden"` writes the kind into config.json's
  `agent_visibility`; `"auto"` removes it. Discovery reads the file on every
  request, so the change (and one made with `muqun-gateway agent hide|show`
  or `manage`) applies on the next discovery without a restart.

  Compatibility: an older App ignores both fields and lists every agent as
  before (hiding is therefore never done by setting `enabled: false` or by
  dropping the agent from this array, which would break its existing
  sessions there). A newer App talking to an older gateway sees neither
  field nor the capability: it should derive `picker` from the same rules
  with every agent `auto`, and not offer hide/show.
- `features` (per agent) says what the agent can do, so the App can hide
  controls rather than call and get `501 feature_unsupported`. The keys are
  camelCase (there is no snake_case spelling of them in this payload; the
  `reasoning_effort` field on prompt bodies and catalog variants is a different
  thing):

| Flag | Meaning |
|---|---|
| `streaming` | Live text and tool deltas arrive on the session stream. |
| `reasoningEffort` | Reasoning-effort selection is supported; use the catalog's actual model `variants[]` to determine available choices. |
| `modelSelection` | The agent lists selectable models; optional scope/persistence fields distinguish next-turn choices from durable session state. |
| `toolApprovals` | The agent raises permission requests the App answers. |
| `worktrees` | The `/api/agent-worktrees` routes work against this agent. |
| `revert` | File-restoring revert is supported. |
| `stagedRevert` | Revert is two-step (`POST …/revert/stage`, then commit). When false, staging answers `501 feature_unsupported`; one-step revert is available only if `revert` is true. |
| `inbox` | Queued and steered prompts (`/inbox`, `delivery`) work. |
| `modes` | The agent has modes (personas or presets) to pick; `mode` on create and `POST …/mode` apply. `modes[]` and the catalog's `modes` are meaningful only when true. |
| `skills` | The catalog `skills[]` list is the agent's and skill activation works. |
| `slashCommands` | The catalog `commands[]` list is runnable (`POST /api/agent-sessions/{asid}/command`). |
| `compaction` | `POST /api/agent-sessions/{asid}/compact` works. |
| `backgroundShells` | The `/api/agent-shells` routes list, read and kill the agent's background shells. |
| `attachments` | A prompt may carry `attachments`; when false the agent answers `501 feature_unsupported`. |
| `attachmentsByPath` | The agent has no native attachment API: the gateway lists the attachments as host paths at the end of the prompt text and the agent opens them with its own tools. Only uploads are accepted (see [`POST …/prompt`](#post-apiagent-sessionsasidprompt)). |
| `timelineEarlier` | `POST …/timeline/earlier` pages older timeline rows after the selected snapshot. Requires global `agent_timeline_earlier_v1` too; absent or false means unsupported. |

  The App renders agent-specific UI from these flags alone, never from the
  agent's `kind`. The base flags above are always present (booleans, never omitted).

  Flags reflect the agent's last probe (cached for about ten seconds).
  Unknown extra fields may appear; ignore them. Optional fields may be booleans
  or metadata, not necessarily booleans. Codex declares `sessionPagination`,
  `sessionRename`, `sessionFork`, `sessionDelete` and `forms` true,
  `mcpElicitation` false, `modelSelectionScope: "next_turn"`,
  `modelSelectionPersistence: "gateway_until_turn_start"`, and
  `protocolBaseline: "rust-v0.162.1"`. T3 declares
  `orchestrationProtocols: [1, 2]`. Gate controls on capabilities, not agent names;
  these additive declarations do not require an agent-name-specific App change.

  What each agent reports today:

  | Flag | `opencode` | `deepseek` | `t3` | `codex` |
  |---|---|---|---|---|
  | `streaming` | yes | yes | yes | yes |
  | `reasoningEffort` | when a model supports reasoning or effort tiers | when a model supports reasoning or effort tiers | no | yes |
  | `modelSelection` | when it lists models | when it lists models | yes | yes (next turn) |
  | `toolApprovals` | yes | yes | yes | yes |
  | `worktrees` | yes | no | no | no |
  | `revert` | yes | no | only known protocol 1 | no |
  | `stagedRevert` | yes | no | no | no |
  | `inbox` | yes | no | no | yes (Gateway queue) |
  | `modes` | yes | yes (presets) | no | no |
  | `skills` | yes | no | no | no |
  | `slashCommands` | yes | no | no | no |
  | `compaction` | yes | no | no | yes (owned native idle sessions) |
  | `backgroundShells` | yes | no | no | no |
  | `attachments` | yes | yes | yes | yes |
  | `attachmentsByPath` | no | yes | yes | yes |
  | `sessionPagination` | yes | no | no | yes (forward only) |
  | `timelineEarlier` | yes | yes | no | yes |

  `t3` is `name: "T3 Code"`; its `version` is the T3 server version, its
  `models[]` are every model of the providers T3 has ready (Claude Code,
  Codex, …). While not attached it is `reachable` when the server answers and
  the gateway holds a credential (a bearer or an unspent pairing token) it
  will attach with, and `unconfigured` when the server answers but the
  gateway holds no credential for it. The descriptor automatically selects
  protocol 1 or 2; omission means 1. Unknown or malformed values are unsupported,
  not interpreted as V1. V2 uses the pinned nightly-source baseline in the
  [T3 protocol reference](t3-protocol.md).
  `revert` is false for V2 and unknown protocols. A protocol-1 T3 revert rolls back
  whole turns: reverting to any row of a turn removes that turn and
  everything after it (`agent.revert.changed` `committed`, then the timeline
  is re-read); there is no staged state.
- `features` (plane level): `multiAgent` (more than one agent may be
  attached), `catalogAggregation` (the unfiltered catalog is merged across
  attached agents), `sessionRouting` (per-session routes resolve to the
  owning agent by themselves).

### `agent_id` on every session

Every `AgentSessionInfo` carries `agent_id`: the id of the agent that owns the
session. It is on list, get (`data.info`), create, the `move` reply, and on
every `agent.session.updated` event. Per-session routes
(`/api/agent-sessions/{asid}/…`) need no `agent_id`: the gateway remembers
which agent owns each session it has listed or created, and asks the attached
agents about one it has not seen.

### Selecting an agent

| Where | How |
|---|---|
| `POST /api/agent-sessions` | `agent_id` in the JSON body. Absent, the primary. A model's `provider_id` never selects the agent. |
| `GET /api/agent-sessions` (and `/children`) | `?agent_id=<id>` lists that agent only. Absent, **every attached agent is merged**, newest `updated_ms` first. An agent that errors is left out of the merged list (and logged) rather than failing it; if every agent fails, or the one you named does, the answer is `502 agent_error`. |
| `GET /api/agent-catalog` | `?agent_id=<id>` returns that agent's catalog. Absent, the merged catalog of all attached agents. |
| `GET /api/agent-projects` | `?agent_id=<id>`. Absent, the primary. |
| `GET /api/agent-status` | `?agent_id=<id>`. Absent, the primary. |

`GET /api/agent-directories` reads the local filesystem, not an agent, and
takes no `agent_id`.

Errors, for every route above that takes `agent_id`:

| Status | `error.code` | When |
|---|---|---|
| `400` | `invalid_agent` | The id is not one this gateway knows (`opencode`, `deepseek`, `t3`, `codex`). |
| `503` | `agent_unavailable` | The id is known but not attached; the message names it (`Agent 'deepseek' is not available`). |

### Compatibility

There are no aliases. An App that never sends `agent_id` gets the primary for
create, projects and status, and sees `agent_id` as one extra string on
session objects. Its unfiltered session list and catalog already span every
attached agent (the `asid`s work on every per-session route). To show one agent
at a time, an App sends `?agent_id=`, and only when `multi_agent` is
advertised: an older gateway ignores the parameter.

---

### Terminal agents launched for remote dispatch

`POST /api/sessions/{id}/tasks` and `POST /api/sessions/{id}/spawn` start a
CLI agent (Codex, Claude Code, ...) in a terminal pane. Whoever asked is on a
phone and cannot answer an approval or a "trust this folder?" prompt in that
pane, so these agents start with their own bypass flags by default.

These terminal routes are unchanged by the Codex service adapter. An explicit
task `agent_id: codex` still uses a pane, even when the private service adapter
is connected. Service sessions use `/api/agent-sessions`; their permissions are
not governed by terminal dispatch bypass flags or folder-trust edits.

| Kind | Default launch arguments |
|---|---|
| `codex` | `--dangerously-bypass-approvals-and-sandbox -c projects={"<dir>"={trust_level="trusted"}, ...}` -- the task directory, its git checkout and, for a linked worktree, the main checkout, each only when strictly inside the user's home; left out with `"agent_trust_folders": false`. The path goes in the TOML value, not the key: Codex's `-c` splits keys on every `.`. The value replaces Codex's whole `projects` table for that run. |
| `claude` | `--dangerously-skip-permissions` |
| any other | none |

Claude Code and Codex started this way also get the muqun MCP server on their
launch line (`--mcp-config …`, `-c mcp_servers.muqun.…`) unless
`mcp.inject_into_agents` is off; see [docs/mcp.md](mcp.md#host-setup). Their
configuration files are not written for this (folder trust, below, is a
separate write).

`config.json` changes this per kind: a non-empty list replaces the defaults,
an empty list turns them off. A task's own `agent_args` come after them.

Claude Code keeps folder trust in its own config, not in a flag. Before
`claude` starts in a directory, the gateway sets
`projects["<dir>"].hasTrustDialogAccepted = true` in
`$CLAUDE_CONFIG_DIR/.claude.json` (`CLAUDE_CONFIG_DIR` from `agent_env`, else
the gateway's environment) or `~/.claude.json`. Only that key is written,
other keys keep their values and order, and only for a directory strictly
inside the user's home, named without `..`. It is on by default, as the
owner's choice for agents started from their phone; `"agent_trust_folders":
false` in `config.json` leaves that file alone and drops Codex's `-c projects=…`.

```json
{
  "agent_launch_args": { "codex": [], "claude": ["--permission-mode", "acceptEdits"] },
  "agent_env": { "HTTPS_PROXY": "http://127.0.0.1:7890", "HTTP_PROXY": "http://127.0.0.1:7890" }
}
```

`agent_env` is for what the owner's interactive shell sets but the gateway's
own environment (started by systemd or launchd) lacks -- typically a proxy,
without which the agent fails with "Connection refused". The variables are set
on the pane the agent starts in: tmux `-e KEY=VALUE` on `new-session` (tmux
3.2+) and on `new-window` / `split-window` (3.0+), Herdr's `env` on
`tab.create`, `workspace.create` and `pane.split`. When the backend cannot set
a pane's environment -- an older tmux, or a worktree pane that Herdr's
`worktree.create` / `worktree.open` made -- the variables go on the launch
instead: tmux, which types the launch line itself, prefixes it with
`env 'KEY=VALUE' ...` (so they also show in that pane's scrollback). Herdr's
`agent.start` takes no environment, so there the task's `steps` record `env`
as skipped. Names must be `[A-Za-z_][A-Za-z0-9_]*`; others are ignored with a
warning naming the key. Values are never logged, but they are not secret from
the host: `tmux -e KEY=VALUE` and the `env` prefix put them on a command line,
which any local user can read in the process list while it runs. Put nothing
in `agent_env` that must stay hidden from other accounts on the machine.

## Session lifecycle

### `GET /api/agent-sessions`

| Query | Meaning |
|---|---|
| `directory` | Scope to a workspace directory. |
| `parent_id` | List the children of one session. |
| `roots` | `true` lists top-level sessions only — no subagent sessions. |
| `limit`, `order`, `search`, `cursor` | Passed through to OpenCode. `order` is `asc` or `desc`. `limit` defaults to **50 per agent**. A `cursor` requires an explicit nonempty `agent_id`; unscoped cursors are rejected before any provider fanout. |
| `agent_id` | Only that agent's sessions; absent merges every attached agent. See [Agents](#agents). |

Returns `[AgentSessionInfo]`.

This legacy array remains sorted newest `updated_ms` first, including a selected
agent list. Aggregation and partial-provider-failure behavior are unchanged. It
does not expose continuation cursors; use the additive page route below instead.

A subagent run creates a real session whose `parent_id` is the caller's. Without
`roots=true` those are mixed into the list.

### Runtime provenance and explicit refresh

The additive capability **`agent_session_runtime_v1`** permits optional `runtime`
on session info, including snapshots, lists, creation and `agent.session.updated`.
Currently Codex supplies it; absence preserves the existing provider contract.

```json
{"source":"terminal","interaction":"terminal","status":"working","transcript":"persisted",
 "terminal":{"session_id":"local","pane_id":"w1:p1","instance_id":"opaque"}}
```

`source` is `native`, `terminal`, or `unobserved`. `interaction` is `native`,
`terminal`, or `unavailable`; it is independent of status. A native source is
observed through an accepted subscription to the connected Codex runtime (private
or shared daemon). A terminal source is a unique
current local Herdr observation matching the Codex conversation identity and
provider exactly, with a complete opaque observed binding. No conversation
identity is decoded by the App. It revalidates all three terminal binding fields
against the existing authenticated session agent listing before navigating.
Navigation does not send, focus, answer, or approve anything. A binding may
remain stable when the same terminal resumes the same conversation; it does not
claim a new process incarnation. This is sufficient for conversation-scoped
reads and navigation, while actual process-targeting assignments require their
existing stronger launch evidence.

Terminal `status` is the observed `idle`, `working`, `blocked`, `starting`,
`completed`, or `unknown`. Working/starting map to ordinary session `busy`, idle
to `idle`, and the remaining terminal statuses to `unknown`. Blocked does not
fabricate a native permission or question. Changed observations use the existing
session-update event, polled at one-second intervals with a bounded backend read;
these metadata events do not replace the transcript.

`unobserved` keeps session status unknown. With complete inventory and no exact
terminal binding it carries `interaction: "native", reason: "not_loaded"`:
only an explicit user mutation may resume saved history. Incomplete inventory or
ambiguous bindings carry `interaction: "unavailable"`, with reason
`terminal_unavailable` or `ambiguous_terminal`. Opening, polling and refreshing
never resume a thread. Native prompt, interrupt, model-switch and delete controls
on an externally bound thread return `409 agent_terminal_interaction_required`
with `error.runtime`; mutation guards reread the binding without using cached
observations. Upstream Codex's per-thread writer lock remains the final authority.

An explicitly opened same-daemon loaded conversation can rejoin that runtime and
replay its pending requests. That accepted subscription takes precedence over a
terminal observation. A persisted history read cannot establish that ownership
or recreate its callbacks.

Optional `runtime.transcript` is `live`, `persisted`, or `snapshot`. Native
subscribed events are `live`. A `persisted` observation follows completed durable
items and reported token counts for the exact terminal conversation, through
existing typed timeline/session events. It does not provide token deltas, pending
tool starts, or actionable approvals. The App may apply live/persisted item updates
using its existing scroll policy; neither mode requests a history replacement or
forces the viewport to move. `snapshot` remains explicit-refresh-only.

Persisted readers use the path selected by native `thread/read`, confined beneath
the initialized Codex home's `sessions` directory, with exact session-header
identity and no symlink traversal. At most eight readers are retained, with four
bounded initial preparations. Initial sampling is limited to the last 1 MiB;
incremental reads are 256 KiB per poll with a 4 MiB record limit. Durable IDs trigger
at most four turn-scoped native item-page reads and 2 MiB of replies per batch,
retaining Codex's redaction and process-completion projection. Unchanged files do
not trigger item-page reads. There are no directory scans or new file endpoints.
Native-selected path changes, inode replacement, truncation, header/anchor changes,
rollback, binding loss or exhausted observation budgets freeze the reader until an
explicit refresh; retained transcript rows are not replaced automatically. Usage
is unknown when the bounded initial sample contains no complete token-count record.

The capability **`agent_session_refresh_v1`** adds `?refresh=true` to the existing
GET session snapshot endpoint (global and legacy aliases). This is an explicit
provider reread, bounded to the normal 100-row tail, four concurrent refreshes
per provider, and 30 seconds. Normal snapshots remain cached. A concurrent newer
mirror event wins: refresh fails instead of rolling it back. Successful refresh
invalidates older replay cursors, without broadcasting replacement history.
The App preserves its viewport and only replaces history when the reader
explicitly requests refresh; reconnects and metadata updates do not do so.
Existing device authentication, peer exclusion, filtering, image handling and
ETag behavior apply to both forms of the endpoint.

### `GET /api/agent-sessions/page`

The global capability **`agent_session_pages_v1`** declares that this endpoint
exists. It does not promise every provider supports pagination: check
`planes.agents.agents[].features.sessionPagination` for the selected provider.
Only `true` declares support (currently OpenCode and Codex); absent or `false` means
unsupported.

| Query | Meaning |
|---|---|
| `agent_id` | **Required**, one explicit nonempty provider ID, maximum 256 UTF-8 bytes. Never aggregates. |
| `directory` | Optional nonempty absolute or home-relative workspace path, maximum 4096 UTF-8 bytes. Uses the list's trimming, `~` expansion and canonicalization when the directory exists. |
| `parent_id` | Optional nonempty parent ID, maximum 256 UTF-8 bytes; literal `null` selects roots. An explicit value takes precedence over `roots`. |
| `roots` | Optional boolean; `true` selects roots (`parent_id=null`) when no parent ID is given. |
| `limit` | Integer **1–100**, default **50**. |
| `order` | Only **`asc`** or **`desc`**, default **`desc`**. |
| `search` | Optional provider search filter, maximum 4096 UTF-8 bytes. |
| `cursor` | Optional nonempty opaque provider cursor, maximum **16384 UTF-8 bytes**. URL-encode without changing its bytes. |

Control characters, empty scope values, invalid limits/order and malformed typed
parameters are rejected with `400` before any upstream request. An unknown agent
is `400 invalid_agent`; a known unattached agent is `503 agent_unavailable`.
An adapter without pagination returns `501 feature_unsupported`, not an empty
page. The selected provider's failure uses the existing `502 agent_error` mapping.

Authentication, encrypted transport policy, the versioned content envelope and
private ETag/`If-None-Match` behavior match the legacy list. The nested `data`
is intentional, preserving the OpenCode page contract:

```json
{
  "schema_version": "1.5.0",
  "capabilities": { "parts": true, "assets": true, "image_upload": true, "composer": true, "message_image_assets": true, "session_descendants": true },
  "data": {
    "data": [{ "asid": "ses_1", "agent_id": "opencode", "title": "Example", "status": "idle", "updated_ms": 1 }],
    "cursor": { "next": "opaque-provider-cursor", "previous": null }
  }
}
```

Both cursor keys are always present and explicitly **`null`** when unavailable.
The Gateway preserves provider order and both cursors, applies its normal
agent ownership and read-marker overlays, and records session routes without
sorting, refetching, aggregating or following another page automatically.

Codex pagination is forward-only: `cursor.next` continues in the requested
order and `cursor.previous` is always `null`. Do not infer reverse navigation
from its pagination flag or change the common order/cursor scope rules.

Cursors are upstream values, **not locally cryptographically bound** by Gateway.
Clients must reuse a cursor only on this selected-provider endpoint with the same
agent, normalized directory, parent/roots filter, search, order and page size.
**Restart pagination if any of those change**; never broadcast a provider cursor
to every agent. Existing Apps still request fifty sessions and cannot
automatically page. App adoption is deferred; the legacy array routes remain.

### `POST /api/agent-sessions`

```json
{ "directory": "/abs/path", "model": {"provider_id": "…", "model_id": "…", "variant": "…"}, "mode": "build", "agent_id": "opencode" }
```

All four are optional; `agent_id` picks the agent (see [Agents](#agents)) and
`mode` the session's mode. **Omitting `model` is the correct way to get the user's
configured default** — the gateway no longer substitutes one. `directory` must
be absolute or `~`-rooted and must exist; the session records it canonical. Returns `AgentSessionInfo`.

### `POST /api/agents/{agent_id}/sessions/{asid}/prepare`

Capability **`agent_session_resume_v1`** adds an explicit foreground selection
operation. A paired App calls it once when the user actually opens or selects a
conversation. The global alias is `POST /api/agent-sessions/{asid}/prepare`;
prefer the scoped path to carry the selected provider identity. No body is
required. The ordinary content envelope contains `data: {"prepared": true}`.
Read the normal snapshot, history window and catalog afterward.

Preparation may restore one dormant same-ID native runtime. It submits no model
prompt, answers no approval and never replays an ambiguous prompt. Concurrent
same-ID prepares are serialized by the provider. A failed prepare preserves the
retained snapshot and is an explicit error. Automatic snapshot/history resync,
including `refresh=true`, uses the observation hook and must not activate dormant
ACP or Pi children. Background watchers must never call this endpoint. Gate the
new call on the capability; older Gateways keep their existing behavior.

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

### Reader-owned history windows

`agent_history_window_v1` adds `GET /api/agent-sessions/{asid}/timeline/window`
and `POST /api/agent-sessions/{asid}/timeline/page`. Authenticated `history_request`
frames use operations `window` and `page` with the same optional `cursor` field.
Both transports call the same service. Existing snapshot/earlier APIs are unchanged.

For providers supporting older history, opening a window returns the newest 100
rows and `history_cursor` alongside the ordinary snapshot fields. A page request
sends `{ "cursor": "..." }` and returns at most 80 source rows, `history_cursor`
and `exhausted`. Provider backfills may include newly discovered rows after the
boundary; upsert every returned row, while the cursor only moves backward.
Filtering can yield no visible rows without exhaustion. Keep one
cursor per reader, computer and session; never share it through a global store.
Passing that cursor to the window endpoint resynchronizes the reader's loaded
range, including its oldest boundary. Omit it when reopening at the latest page.
Providers without older paging retain their existing full snapshot and omit the
cursor, so cached history does not become inaccessible.

Cursors are scoped to the session incarnation and removal generation. Invalid,
evicted or obsolete boundaries return `409 history_cursor_lost`, not exhaustion.
Recover with a new window; do not silently jump to another reader's provider
cursor. Ordinary cached retries return the same page. Older source reads are
serialized and committed through the existing provider/mirror transaction.
Historical rows do not become new live output for other subscribers; canonical
ordering corrections still propagate. A reader can continue past the mirror's
retention cap, but a lagging reader whose boundary was evicted must resynchronize.

### `POST /api/agent-sessions/{asid}/timeline/earlier`

Gate remote history pulls on **both** global capability
`agent_timeline_earlier_v1` and the owning provider's
`planes.agents.agents[].features.timelineEarlier === true`. Never infer support
from a Gateway version or agent name. Codex, DeepSeek and OpenCode implement
it; T3 returns `501 feature_unsupported`, not successful exhaustion.

OpenCode's message route takes `limit` and `order` and no cursor, so each
earlier batch is read as the newest `held + 80` messages (bounded to 2,000
messages and 8 MiB per read) and cut at the oldest message already delivered;
a read that answers fewer than asked is the end. A session whose history
changed under the cursor answers `409 history_cursor_lost` (below): read the
snapshot again.

DeepSeek pages `session/page` backwards under the frozen `asOfSeq` cut the
snapshot was read under: opening reads the newest hundred messages within a
2 MiB budget, each earlier batch the pages below the oldest row delivered
within 8 MiB, and a page that would exceed the budget waits for the next
pull. A history is never refused for its size. A tool result cut from its
call by a page boundary is joined onto the call when that older page arrives.

No request body or client cursor. Load the selected session snapshot first, then
request one earlier batch at a time. The existing content envelope carries
`data: {"items": [TimelineItem], "exhausted": false}`. A batch contains at most
**80 mapped rows**, oldest first; one native reasoning item can span batches.
Merge by stable row ID and `(message_id, ordinal)`, preserving the current viewport
and existing tail. Historical driver rows retain the existing `seq: 0` semantics;
the mirror records its own upsert sequence as before. This is not an SSE replay
cursor. The legacy session-prefixed alias has the same contract.

`exhausted: true` means the adapter returned an empty batch. A final nonempty batch
can therefore still say false; the following pull confirms the end. Filtering
internal plumbing can yield `items: []` with `exhausted: false`; continue later,
do not infer the end from the visible array alone.

`409 history_cursor_lost` means the gateway holds no earlier-history cursor for
the session: no snapshot was read in this gateway process (a restart, or the
cursor was evicted), or the history changed under it. It is never exhaustion:
read the snapshot again (`GET /api/agent-sessions/{asid}`) and pull on.

State belongs to the provider/session in this Gateway process, not a particular
App: concurrent requests consume successive disjoint batches. Apps must serialize
their own pulls and must not automatically retry a POST whose successful reply
may have been lost. Failed or cancelled provider reads do not consume a batch;
successful canonical snapshot acceptance reestablishes the newest-tail boundary.
A provider tail is prepared first. Initial history validates session incarnation
and removal generation while preserving newer live rows; explicit refresh also
requires an unchanged mirror sequence. Pagination state and mirror rows are
accepted synchronously after the mirror fences and adapter revision check pass.
Failed or cancelled preparation, a newer event during refresh, or
concurrent earlier-page progress discards the candidate without resetting or
restoring shared cursor state. Cached snapshots do not reset pagination. There
are at most four pending Codex tail candidates; dropping one releases its slot.
This acceptance boundary covers pagination and transcript rows, not rollback of
all driver state: sampled usage may update during preparation. Initial pending
request catch-up can still be cancelled after rows and pagination are accepted
together; it does not undo that accepted history.
On restart/refetch, merge repeated IDs rather than duplicating rows.

Codex retains only a raw residual page (or a bounded legacy response), plus an
item's remaining mapped-prefix boundary. History reads are serialized per provider
and use the existing traversal deadline/page/byte/row budgets. Retention is bounded
to 4096 sessions and 32 MiB of serialized residual/cursor state; exhaustion returns
an error without eviction or cursor consumption. Ordering uses a checked `u64`
prefix allocation without renumbering displayed rows; exhausted ordering capacity
or contradictory anchors return an explicit error, never a fabricated partial end.

### `GET /api/agent-sessions/{asid}/children`

Same response as the list route, filtered to this session's children. Accepts
the same `limit` / `order` / `search` / `cursor`. Every adapter honours the
parent filter. DeepSeek has no sub-agent sessions and lists none; T3 lists a
thread's subagent threads on protocol 2 (their `parent_id` is the parent
thread) and none on protocol 1.

### `GET /api/agent-sessions/{asid}/descendants`

`/children` over every depth at once, for opening a workbench with one request
instead of one per descendant. `capabilities.session_descendants: true` on the
envelope says a gateway answers it. The same query as `/children` (`agent_id`,
`limit`, `order`, `search`, `directory`; `cursor` is `400 invalid_query`, a
cursor pages one listing and cannot page a tree), the same authorisation, and
the same error semantics (an agent that fails is skipped while another
answers, unless it was the one named). The answer nests the list, so the walk
can say when it stopped short:

```json
{
  "schema_version": "1.5.0",
  "capabilities": { "...": true, "session_descendants": true },
  "data": {
    "data": [{ "asid": "ses_2", "agent_id": "opencode", "parent_id": "ses_1", "...": "..." }],
    "truncated": true
  }
}
```

`data.data` is every session below `{asid}` (never `{asid}` itself), each
shaped exactly as `/children` shapes a session, newest `updated_ms` first; a
breadth-first walk over the adapters' children listings, at most **8 levels
deep** and **500 sessions**. `truncated` is present, and `true`, only when one
of those bounds cut the walk short: there are deeper or further sessions that
were not asked for. Absent means the list is the whole subtree.

### `DELETE /api/agent-sessions/{asid}`

`{"deleted": true}`. OpenCode removes the session's children too, and announces
each one on the stream.

### `DELETE /api/agents/{agent_id}/sessions/{asid}`

Gate provider-scoped deletion on `agent_session_delete_scoped`, then use the
provider and session ID captured from the selected row. The gateway verifies
that exact attached provider owns the session before deleting it, without
consulting a cached owner or trying another provider. Unknown providers return
`400 invalid_agent`; unavailable providers `503 agent_unavailable`; missing
sessions `404 session_not_found`; a mismatched session response
`409 session_changed`; a failed ownership read `502 agent_error`. The ID must
be 1–128 ASCII letters, digits, `-`, `_`, `.`, or `:`, and cannot be `.` or `..`;
unsafe IDs return `400 invalid_session`. No mutation occurs on these failures.

The legacy route retains its original owner lookup. Older gateways return 404
for this distinct scoped route, even when a client cached the capability before
a downgrade. Clients must never fall back to the legacy route for scoped deletion
or assume that an `agent_id` query on that route constrains its owner.

OpenCode, T3 and Codex implement deletion; DeepSeek returns `501 feature_unsupported`.
Provider deletion removes the conversation according to the provider's contract;
the gateway does not promise that active work or child processes have stopped.
It is separate from closing a terminal pane and from removing a local Home entry.

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
{ "text": "…", "attachments": ["/abs/path or file:// or https:// or data:"], "delivery": "steer", "computer_ids": ["opaque-machine-id"], "client_ref": "prompt-opaque-reference" }
```

`delivery` is `steer` (run at the next step boundary) or `queue` (wait behind
what is already queued). Returns `{"submitted": true}`.

`computer_ids` is optional and requires `agent_computer_context_v1`. It contains
up to eight immutable machine IDs from this Gateway's linked-computer inventory,
never names, URLs, MCP settings, tokens, or a permission grant. Duplicate IDs are
collapsed; an unknown, revoked, or malformed reference refuses the whole prompt
before dispatch. The selected Gateway and route's `asid` identify the destination.
The Gateway prepares provider tools and credentials; the App does not register an
MCP server or understand provider-specific configuration.

`client_ref` is optional, 1–128 ASCII letters, digits, `_` or `-`, and is echoed
beside `submitted` on success. It correlates one submission; it is **not** an
idempotency guarantee. A missing reply is an unknown outcome, not permission to
submit again. Task creation has its separate existing `client_ref` for read-only
reconciliation through the task ledger.

Computer-tool preflight failures use `invalid_computer_references` (400),
`computer_tools_unavailable` (503), or `computer_tools_not_ready` (503). Their
envelope includes `error.delivery: "not_sent"` only when no prompt was dispatched.
Clients preserve the draft and show the refusal without automatically retrying.
Unknown error codes with this exact marker remain explicit rejections; an absent
marker, timeout, or generic upstream failure must not be interpreted as proof of
non-delivery. Responses may add fields without changing these semantics.

There is no model or mode field: **OpenCode model and mode are session state in
v2**. Switch first, then prompt where the corresponding capability is supported.

For Codex, switching stages an ephemeral Gateway-local next-turn model/effort
choice. It remains pending until authoritative upstream normalization confirms
application, and is not a durable session setting or a pre-turn model update.
Busy threads must be interrupted first. Gateway restart loses the pending choice;
see [Codex protocol reference](codex-protocol.md) for model/provider limits.

An attachment the app uploaded is an absolute path into the gateway's upload
directory — see [Attachments](#attachments) for how one gets there, and for the
permission rule the gateway puts on the session so the agent can open it without
an approval prompt.

How attachments reach the agent depends on the agent:

- **`opencode`** (`attachmentsByPath: false`) takes them natively as file
  parts; absolute paths, `file://`, `https://` and `data:` URLs all work.
- **`deepseek`**, **`t3`** and **`codex`** (`attachmentsByPath: true`) use host paths.
  The gateway sends the agent an empty attachment list and appends the paths
  to the prompt text instead:

  ```text
  <your text>

  Attached files (on this host):
  - /home/ryu/.local/share/muqun-gateway/uploads/6f1c….webp
  ```

  The agent reads them with its own file and image tools. Each attachment must
  be an absolute path that resolves (after symlinks and `..`) to a regular file
  inside the gateway's upload directory -- that is, a `path` returned by
  `POST /api/uploads` within its 48-hour retention. Anything else (a relative
  path, a URL, a file elsewhere on the host, an expired upload) is refused with
  `400 invalid_request` before the agent sees the prompt, so a client cannot
  point the agent at a file it did not upload.

### `POST /api/agent-sessions/{asid}/model`

Accepts either shape:

```json
{ "model": { "provider_id": "opencode", "model_id": "union-alpha", "variant": "thinking" } }
```
```json
{ "provider_id": "opencode", "model_id": "union-alpha" }
```

Returns `{"switched": true}`. `variant` is optional and omitted, never null.

### `POST /api/agent-sessions/{asid}/mode`

`{"mode": "build"}` (a bare `"build"` also works) → `{"ok": true}`. Switches the
session's mode (persona). Mode ids are lowercase; a display name such as
`"Build"` is rejected by OpenCode.

### `POST /api/agent-sessions/{asid}/interrupt` · `/abort`

No body → `{"interrupted": true}`. Status goes to `interrupted`.

### `POST /api/agent-sessions/{asid}/command`

```json
{ "name": "review", "arguments": "branch", "delivery": "steer" }
```

A slash command from the catalog's `commands`. A leading `/` on `name` is
stripped; `arguments` fills `$ARGUMENTS`. Returns `{"submitted": true}`.

DeepSeek commands execute the receiving session's native plugin handler through
`commands/execute`, not a model prompt. Its response additionally contains
`command_result: {command_id, kind: "success"|"error", text?, source_event_seq?}`.
An executed handler failure remains HTTP 200 with `kind: "error"`; clients must
show that failure instead of treating `submitted` as success. Unknown commands
are refused; an ambiguous response is never resubmitted. `delivery` has no
queueing meaning for these immediate handlers. Durable `command/run` and
`command/done` events populate existing tool cards, including results joined
across history pages. Clients must not append a second success message from
the direct response.

Codex exposes only `/compact` from its downloaded command catalog, backed by
`thread/compact/start` on an owned, idle session. It takes no arguments or
`delivery` override. Missing/invalid catalogs, or catalogs with no supported
rows, leave `commands` empty and `slashCommands` false. Other Codex commands
(including `/new`, `/export` and custom prompt names) are rejected by the
Gateway, never sent as model prompts. Skills still use the separate `/skill`
endpoint with native skill input.

### `POST /api/agent-sessions/{asid}/skill`

```json
{ "skill": "docs", "resume": true }
```

`skill` is a `skills[].id` from the catalog. `resume` is optional: left out,
OpenCode decides whether the agent loop picks up where the skill left it, which
is what the slash menu wants; `false` appends the skill and leaves the session
idle. Returns `{"status": "ok"}` — OpenCode answers `204`, and what the user
sees is the timeline row below, not this reply. An unknown id is `404` from
OpenCode and comes back as `agent_error`.

DeepSeek rechecks the skill in that exact session's native `skills/list`, then
sends the official `/name` prompt with queue delivery. It neither reads the
instruction file nor invents a `skills/activate` RPC. `resume: false` is
unsupported because the native prompt starts work; unknown/deleted skills are
refused before a prompt is sent.

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
  running is `409` — both arrive as `agent_error`.

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
  `fatal: invalid reference: probe-branch`, as a `502 agent_error`.

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
`agent_error`, and that is the whole of the rule. The gateway does not
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

### Agent image previews (additive `agent_image_previews_v1`)

Timeline `attachments[]` and typed tool `content[].uri` use authenticated,
Gateway-relative **original** URLs for admitted inline/local files. No thumbnail
replaces an existing original URL. Text and tool inputs remain verbatim. The
optional `image_assets` entries match attachment/content URLs through `src`:

```json
{
  "src": "/api/agent-images/ai_…/original",
  "asset_id": "ai_…",
  "url": "/api/agent-images/ai_…/original",
  "mime": "image/png",
  "width": 1200,
  "height": 800,
  "size": 1234567,
  "thumbnail_url": "/api/agent-images/ai_…/thumbnail",
  "preview_url": "/api/agent-images/ai_…/preview",
  "download_url": "/api/agent-images/ai_…/download",
  "version_url": "/api/agent-images/ai_…/version"
}
```

Minimal new-App consumption: negotiate `agent_image_previews_v1`; match `src`
exactly and check actual `mime` (entries may be files/PDFs, not all bitmaps).
Use `thumbnail_url` only in the timeline list, `preview_url` when opening an
image, and `download_url` (or original `url`) for save/download. Fall back to
original bytes via `download_url` on a 415 preview failure; this is also usable
for viewing, regardless of attachment disposition. Legacy text/file-asset `url`
can still name the old size-limited asset endpoint, so use the new download URL
for large originals. Never save a thumbnail. Missing optional
fields retain legacy behavior. No `agent_message_display_v1` or gallery redesign
is required. Older Apps receive original attachment bytes, not implicit smaller
replacements; they do not gain thumbnail bandwidth savings until updated.

`size` is the original file length, not a derivative estimate. An optional
`unavailable_reason` marks a failed reference/encoding/storage resolution:
**skip that entry for URL/image rendering**. Its `url` and `asset_id` are empty,
and no download/preview URL is fabricated. The original attachment/content string
remains unchanged so source information is not silently deleted. Reasons include
`attachment_name_only` (display label, never a filesystem path),
`original_storage_unavailable`, `invalid_inline_encoding`, and
`reference_not_authorized_or_unavailable`. For an unavailable inline source the
error entry's `src` is empty to avoid copying its entire base64 payload again;
the raw source occurs only in the existing attachment/content field. Such an
entry is explicitly **not** a claim that an authorized original URL exists.

`GET /api/agent-sessions/{asid}/file-asset?uri=…` retains its original contract
and adds these same optional preview/download URLs to `data.asset`.

With `agent_file_source_selection_v1`, an explicit open can instead use
`GET /api/agent-sessions/{asid}/file-source?uri=…`. It applies that same policy
and returns `data.asset` when the file is available. An unavailable absolute
host reference returns `data.file_source = {state: "choose_computer", path}`;
relative paths, remote URLs and other errors retain the original behavior.
The Gateway decides when source selection is needed. The App only presents its
paired computers and transports the chosen path to that computer's existing
`POST /api/mcp/files/present` endpoint. Despite its historical name, this endpoint
does not require the path to have appeared in an MCP result. It authorizes and
resolves an explicit open using the ordinary file policy.

No computer is probed automatically, and the App's credential for one computer
is never forwarded to another. A choice is not a file permission grant. The
target Gateway applies its own pairing, filesystem scope and operating-system
permissions on metadata resolution and reads. Existing peer delegation controls
remain server-enforced and distinct from a paired phone opening a file directly.

`GET /api/agent-images/{id}/{variant}` accepts only `original`, `download`,
`thumbnail`, or `preview`. All require an existing paired App device and the
normal encrypted transport when required by pairing; URLs contain no credentials.
Each read re-opens an already registered canonical regular file inside its
server-owned fence (session directory, existing generated-images fence, or
Gateway upload/original storage), including cache hits. No arbitrary path query
exists. IDs are bounded in-memory registrations; refresh the timeline/file-asset
after a Gateway restart or registration eviction. Pairing is owner-wide, not a
per-session user ACL; no new session bearer/capability tokens are issued.

Original reads have no preview-size cutoff: they stream from an authorized open
file handle in 64 KiB chunks. They include weak metadata ETag, private/no-store,
single byte Range support (206/416), and If-None-Match (304). Per HTTP semantics
a weak If-Range cannot establish a strong content match and returns full 200.
These legacy unversioned URLs expose no strong original version conditional;
the derivative's strong WebP ETag is not an original version. Weak metadata ETags
detect changes but do not promise a fixed snapshot across separate range
requests. Consistent multipart originals use the additive version API below.
A source changed during reading aborts the body rather than reporting a complete
successful snapshot. Download differs only by attachment disposition. Existing
`/api/assets/*` and `/api/uploads/*` size/retention/download behavior is unchanged.
Inline image originals are immutable private digest-named files; generated host
originals remain in place. The derivative sweeper never deletes originals.
Inline originals decode to disk in small base64 blocks, not a whole-image byte
allocation, and do not disappear when optional preview budgets are exhausted.
Attachment arrays are not silently truncated at 16/32 entries.

The existing encrypted transport still buffers each sealed response (roughly
32 MiB plaintext maximum); it is not a streaming encryption protocol. Large
original consumers must use bounded standard ranges, for example 8 MiB each,
using `size` or a small encrypted GET range's Content-Range total. Authenticate
the complete AEAD response, check each requested interval/returned length/total
and validator, and abort/restart assembly when they differ. Do not use weak
If-Range, and never append a 200 fallback as if it were a partial response.
Do not rely on an encrypted HEAD response body for authenticated metadata. A
full large response over that transport is not currently supported. Never
downgrade or disable required pairing encryption to bypass that constraint.
The original route itself supports full streaming reads over the existing
authorized transport mode and bounded encrypted range reads; App range-save
consumption must be verified separately before claiming large-file acceptance.

Derivatives are lazy, lossless WebP: thumbnail longest edge 384 (1 MiB output
cap), preview longest edge 1600 (4 MiB cap), no upscaling. Only static PNG/JPEG/
WebP is decoded. Sources over 25 MiB, GIF/APNG/animated WebP/HEIC, malformed content, over 8 million
pixels or 16384 per dimension, a decoded-buffer allowance over 64 MiB, and failed
cache/encoding operations return 415 `image_preview_unavailable`, not an original
disguised as WebP. Originals remain accessible where authorized. Work uses at
most two blocking workers, with one serialized cache/encode transaction; the
versioned source-digest/size cache is bounded at 64 MiB/256 derivative files.

This normalization occurs **after** provider decoding. It does not by itself
repair an upstream Codex WebSocket frame that fails before decoding.

#### Fixed original downloads (`agent_image_versions_v1`)

Successful image/file descriptors additionally offer optional `version_url`.
Negotiate `agent_image_versions_v1`, then GET that URL **before** requesting
multipart original bytes. This lazily acquires a fixed original and returns:

```json
{ "data": { "asset": {
  "id": "ai_…",
  "version": "<64 lowercase SHA256 hex characters>",
  "etag": "\"sha256-<same hex>\"",
  "size": 34603008,
  "mime": "image/png",
  "name": "source.png",
  "url": "/api/agent-images/ai_…/original/<version>",
  "download_url": "/api/agent-images/ai_…/download/<version>"
} } }
```

The digest identifies the **complete fixed bytes acquired**, not an asserted
instantaneous atomic view of a concurrently written source file. Cold acquisition
first streams/hashes the existing authorized handle without temporary disk,
then verifies a matching complete digest object if one is already stored. Such
reuse does not count as a new copy, even after registration loss or at capacity.
Only a missing object reserves the full staging byte/file capacity, rewinds the
same handle, copies and hashes again, checks both digests/size/source identity,
syncs and atomically publishes without replacing any prior digest object.
Detected source change or I/O failure does not publish a partial original.
Existing immutable Gateway inline/upload originals are reused without another
permanent copy. No whole-file buffer, per-range whole-file hash or new database.

Use the returned versioned `download_url`/`url` with standard bounded ranges
(e.g. 8 MiB) and the descriptor's exact strong `If-Match`. Each 206 must pass
complete AEAD authentication and match the expected strong ETag, requested
Content-Range interval, total and body length. The original source may change
inside its fence after part zero: that version URI still reads the acquired
fixed bytes, never silently switches to the changed source. Reacquiring a
descriptor may yield a different digest/URI. Do not combine those versions.

Wrong/weak `If-Match` is **412**; weak `If-Range` on the versioned route is also
rejected with 412. Prefer If-Match only. A mismatched strong If-Range can yield
full **200** by HTTP semantics: never append it as a partial segment. The old
unversioned URL/weak ETag semantics remain unchanged for older Apps.

Unknown/expired version registration or a changed fixed object is **409**:
stop assembly, refresh the descriptor and restart unless the same digest/size
is re-established. Authorization/expiry is rechecked, including the original
source fence; a removed/escaped original reference can still return 404. A
version digest is not a credential and cannot access another asset's versions.
Uploads retain their existing 48-hour retention. In-memory registrations are
bounded at 256 and reset on restart/eviction; disk originals are not deleted by
registration eviction.

Additional local-version copies have a fixed **1 GiB / 4096-file original-store
admission budget** (including existing regular files/staging). Already preserved
inline/upload originals and existing versions remain usable; existing inline
original retention is unchanged. Capacity failure returns **507** before a
new version is promised; it never deletes a source or unique original to make
room. Stored originals have no new automatic destructive expiry. Storage failure
is 503, source change/conflict is 409. These are explicit failures, not fake
download URLs. Snapshot acquisition is singleflight inside the existing bounded
blocking admission; no extra service, queue or configurable transform system.

The shared sealed-response collector remains bounded, not streaming encryption;
versioned clients must still use small authenticated ranges for large files.

### `POST /api/uploads`

`multipart/form-data` with one `file` field. The type is decided by sniffing the
content, never by the filename: images (png, jpeg, gif, webp, heic), MP4 and
QuickTime video (the `ftyp` brands the asset preview plays, with an mp4/mov/m4v
file name), PDF, the Office and OpenDocument containers, and low-control UTF-8
text are accepted, and everything else — executables and scripts first — is
refused with `415`. A video may be up to 64 MiB; anything else up to 25 MiB
(`413 upload_too_large` past it, once the content has been sniffed; a body over
64 MiB is cut off as `413`). `GET /api/discovery` carries both numbers as
`uploads.maxBytes` and `uploads.maxVideoBytes`, for the app to say on a `413`.
The stored name is generated by the gateway (a UUID plus the extension the
content earned), so nothing the client sent ever becomes a path.

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
other path still asks. It is done once per session per attached agent, logged
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
| `PATCH /api/agent-sessions/{asid}/inbox/{inbox_id}` | `{"text":"replacement"}` | `{"edited": true, "inbox_id":"…"}` |
| `POST /api/agent-sessions/{asid}/inbox/{inbox_id}/steer` | — | `{"delivery": "steer", "inbox_id": "…"}` |
| `POST /api/agent-sessions/{asid}/inbox/{inbox_id}/queue` | — | `{"delivery": "queue", "inbox_id": "…"}` |

Items are OpenCode's verbatim, i.e. camelCase:
`{id, sessionID, timeCreated, type: "user"|"synthetic"|"compaction"|"move", payload, delivery}`.
A user item's `payload` is `{text, files?: [{uri, name?, mime?}]}`.

Prompt delivery is provider-neutral. While the Gateway has observed live
compaction, the common prompt service requests `queue`, including when the
caller omitted delivery or requested `steer`. Each adapter implements that
intent using its native inbox or its Gateway-owned queue. Unsupported delivery
must be refused explicitly; a successful submission alone is not a queued
receipt. Apps render the acknowledged inbox item, including its text and stable
identity, through the same UI for every provider. An optimistic row must not
hide its acknowledged queue status. Historical compaction summaries do not
establish live compaction, and completion, failure or an inactive session clears
the observed state. Unknown delivery outcomes are never retried automatically.

DeepSeek Harness exposes its durable `inbox` projection through this same shape:
`next-turn` is `queue`, `next-step` is `steer`, and ids are native MessageIds.
Edit, cancellation and send-now call `session/updateQueue` with `edit`, `remove`
and `steer`; there is no native steer-to-queue demotion. A claim race is returned
without recreating the message. Reconnect snapshots and durable inbox splice
events refresh the canonical projection. Unavailable or malformed reads do not
clear pending items.

T3 protocol 2 exposes native queued runs as user items, with run ids as inbox ids.
Edit and cancellation use `queued-run.edit` and `queued-run.cancel`; send-now
uses `queued-message.promote-to-steer` with the current authoritative root run.
When idle, send-now starts that same queued run with `queued-run.start`.
New prompts accept `queue` and `steer`: steering requires the matching live
provider session's `supportsActiveSteering`, queueing during a run requires
`supportsQueuedMessages`. Neither operation falls back to restarting a run.
An already queued item stays queued when `/queue` is requested. T3 V1 has no
inbox operations. The optional `inboxEdit` capability identifies native text
editing support; edits require nonblank text and preserve T3 attachments.

OpenCode structured errors can additionally carry optional `retry_after_ms`,
copied from the provider's `retryAfterMs`; absent or invalid hints stay omitted.

Codex has no inbox of its own; the Gateway holds one per thread, in memory
(lost on a Gateway restart or a reconnect to Codex), with items in the same
shape (`type: "user"`, ids `inbox_…`). `delivery: "queue"` on a prompt waits
behind the running turn, or starts at once when the session is idle. The next
item (oldest one marked `steer` first, then the oldest `queue` item) is sent as a new
turn only when a turn completes normally; after an interrupted or failed turn
the queue waits for the user. `…/steer` sends an item now: `turn/steer` into the
running turn, or a new turn. A refused send keeps the item; an unknown outcome
removes it, never resends it, and publishes `agent.resync` with reason
`codex_queued_delivery_outcome_unknown`.
During an observed `contextCompaction` item, default and steered prompts join
that same queue. Send-now leaves the receipt queued until compaction ends;
normal successful turn completion drains it as described above.

On the way out (this route, the snapshot's `inbox`, `agent.inbox.changed`), an
item with `payload.files` also carries the two fields a transcript user row has
for its files: `attachments` (one source per file; an authorized file becomes
`/api/agent-images/{id}/original`) and `image_assets` (keyed by those sources,
with `thumbnail_url`, `preview_url`, `download_url`, `version_url`). `payload`
itself is unchanged.

An unknown, cancelled or already delivered item answers
`404 inbox_item_not_found` (Codex); refetch the inbox.

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
agent_error`, not an unscoped list of everything.

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

### Push notifications

Every registered device is pushed once per pending approval and once per
pending form, for any agent session -- a child session's included. A repeat of
the same request id (T3 re-announces open requests on every thread snapshot)
pushes nothing until that request is resolved.

| | approval | question (form) |
|---|---|---|
| `type`, `category` | `approval` | `question` |
| `server_id` | the gateway's server id | the gateway's server id |
| `session_id`, `asid` | the agent session | the agent session |
| request id | `approval_id` | `form_id` |
| `fingerprint` | the request id | the form id |
| `agent_id` | — | the owning agent (`opencode`, `deepseek`, `t3`, `codex`) |

A question push reads "*name* needs your input", where *name* is the session's
title, or the agent's name when the session has none. With `rich_agent_pushes`
on, the body is the form's title instead and the data carries it as `question`.

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
  "reason": null,
  "repo": { "branch": "feat/x", "head": "abc1234", "detached": false,
            "upstream": "origin/feat/x", "ahead": 2, "behind": 0 } }
```

`repo` is the same branch line as on [`vcs/files`](#changes-git), read by
git in the session's directory; it is absent outside a repository and, best
effort, when git cannot answer.

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

### Changes (git)

Capability `agent_vcs_files`. Three routes that ask **git** in the session's
directory rather than the agent, so they mean the same thing for OpenCode,
DeepSeek Harness, T3 Code and Codex. `vcs/diff` above is unchanged. Paths are
relative to the checkout's top level, which may be above the session's
directory. Every git process is bounded by 5 s and 8 MB of output.

A `path` is a file **name**, never a pattern: it must be repo-relative with no
`..`, and it is matched by exact string equality against git's own list
before anything uses it. `*`, `:(exclude)x`, `a[b].txt` and a directory name
select nothing beyond the one file of exactly that name (and a directory
selects nothing at all).

**`GET /api/agent-sessions/{asid}/vcs/files?mode=working|branch`** (default
`working`) — the list, with line totals and no patches:

```json
{ "vcs": "git", "reason": null, "mode": "branch", "base": "origin/main", "truncated": false,
  "repo": { "branch": "feat/x", "head": "abc1234", "detached": false,
            "upstream": "origin/feat/x", "ahead": 2, "behind": 0 },
  "files": [
    { "path": "src/a.rs", "status": "modified", "additions": 3, "deletions": 1, "binary": false },
    { "path": "docs/new.md", "old_path": "docs/old.md", "status": "renamed", "additions": 0, "deletions": 0, "binary": false },
    { "path": "notes.txt", "status": "untracked", "additions": 12, "deletions": 0, "binary": false }
  ] }
```

- `working`: index and working tree against `HEAD`, plus every untracked file
  (each file, not its directory). `base` still names the default branch a
  `branch` list would compare with, when there is one, so a client can offer
  that scope without asking for it first.
- `branch`: everything since the merge-base with the default branch, committed
  or not, plus untracked files. The default branch is `origin/HEAD` when the
  clone recorded it, then `main`, `master`, `origin/main`, `origin/master`:
  the first that exists **and** shares history with `HEAD`; `base` names it.
  With none, `base` is absent, `reason` is `"no_default_branch"` and the list
  is the working one.
- `status` is one of `added`, `modified`, `deleted`, `renamed`, `untracked`,
  `copied`, `typechange`, `conflicted` (an unmerged path). Treat an unknown
  value as `modified`. A file missing from the working tree is `deleted`
  whatever the index holds; a file removed from the index but still on disk
  (`git rm --cached`) is one `modified` row. `old_path` is present for
  `renamed` and `copied`.
- `additions` is `null` for an untracked file whose lines were not counted
  (over 1 MB, past the list's 16 MB read budget, or not a regular file). A
  binary file has `binary: true` and zero totals.
- At most 2000 files, cut before any file is read; `truncated` says the list
  stopped there or git's output reached 8 MB.
- `repo` is the checkout's branch line, read by the same `git status` that
  lists the files, for both modes. `branch` is the checked-out branch, `null`
  on a detached `HEAD` (`detached: true`) and on an unborn branch (no commit
  yet). `head` is the abbreviated commit, `null` only on an unborn branch.
  `upstream` is the tracking branch or `null`; `ahead`/`behind` count commits
  against it and are `0` when there is none. `repo` is absent only outside a
  repository.
- `reason` is `null` inside a repository. Not a repository:
  `200 { "vcs": null, "reason": "not_a_repository", "mode", "truncated": false, "files": [] }`.
- Any other `mode` is `400 invalid_mode`.

**`GET /api/agent-sessions/{asid}/vcs/file?mode=&path=&context=3`** — one
file's whole patch:

```json
{ "path": "src/a.rs", "status": "modified", "additions": 3, "deletions": 1,
  "binary": false, "patch": "diff --git a/src/a.rs b/src/a.rs\n…", "truncated": false }
```

`context` is 0–25 (default 3). The file is looked up by name alone, so a file
past the list's 2000-file cut still answers. An untracked file is an
all-additions patch against `/dev/null`; a binary file has `patch: ""`;
`truncated` is true when the patch reached 8 MB. A tracked file with no
change is `200` with `status: "unchanged"` and an empty patch. Any other path
is `404 unknown_path`; outside a repository, `404 not_a_repository`.

**`POST /api/agent-sessions/{asid}/vcs/discard`** `{ "path": "src/a.rs" }` →
`{ "path": "src/a.rs", "action": "restored" | "deleted" }`. Device token
required, like every session write; logged at info.

- The path must be exactly one row of the **working** list (whatever `mode`
  the app is showing); a file with no working change is `404 unknown_path`,
  not a `200` that did nothing.
- A tracked file is restored from `HEAD` in both the index and the working
  tree (`git restore --staged --worktree`; `git checkout HEAD --` only on a
  git that has no `restore`): `restored`. A staged rename restores its old
  path too. A staged new file is removed: `deleted`. Any other git failure,
  such as a held `index.lock`, is returned, not retried another way.
- An untracked file is deleted: `deleted`. Never a directory. The unlink walks
  the directories with `O_NOFOLLOW`, so a directory swapped for a symlink
  after the checks fails rather than deleting outside the checkout.
- A path that leads out of the checkout through a symlink, the file itself
  or a directory on the way, is `403 path_outside_repository`.
- A checkout whose top level is the user's home directory (a session opened in
  `~` with a dotfiles repository) is `403 repository_is_home`: discard is
  never offered there.
- When git's answer for the path was cut at 8 MB the row cannot be trusted:
  `409 listing_truncated`.

Errors shared by the three routes:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_mode` | `mode` is not `working` or `branch` |
| 403 | `path_outside_repository` | discard: the path leaves the checkout through a symlink |
| 403 | `repository_is_home` | discard: the checkout is the home directory |
| 404 | `unknown_path` | not a row of the list (or, for `vcs/file`, a tracked file) |
| 404 | `not_a_repository` | `vcs/file`, `vcs/discard` outside a repository |
| 404 | `workspace_missing` | the session's directory is gone |
| 409 | `listing_truncated` | discard: git's answer was cut at 8 MB |
| 500 | `discard_failed` | discard: the file could not be removed |
| 501 | `git_missing` | git is not installed on the host |
| 502 | `git_failed` | git exited with an error |
| 504 | `git_timeout` | git took longer than 5 s |

### `GET /api/agent-catalog`

`?directory=` · `?agent_id=` (one agent's catalog; absent merges all attached) ·
`?asid=` (optional exact native session) · ETag + `304`. `data`:

```json
{
  "models":   [ { "id", "name", "provider_id", "family?", "limit?", "variants?": [{"id", "reasoning_effort?"}], "cost?", "enabled", "status?" } ],
  "modes":    [ { "id", "name", "description?", "mode?", "color?", "hidden" } ],
  "mcp":      [ { "name", "status", "error?" } ],
  "skills":   [ { "id", "name", "description", "slash", "autoinvoke" } ],
  "providers":[ { "id", "name", "available", "activation?": "auto"|"enabled"|"disabled",
                  "models": [ { "id", "name", "enabled", "variants": [...], "limit?", "status?" } ] } ],
  "commands": [ { "name", "description?", "mode?", "template?" } ],
  "defaults": { "model?": {"provider_id", "model_id", "variant?"}, "mode?": "build", "effort?": "high" }
}
```

`defaults` is what a new session gets when the App does not choose. With
capability `agent_session_defaults`, a request naming one `agent_id` and no
`asid` fills it from the gateway's own memory, per field: the model (with its
`variant`) and the mode last chosen for that agent in that `directory` (its
real path), else the last chosen on this computer, else the agent's own
default. A remembered model, effort or mode the catalog no longer offers is
skipped. `effort` repeats `model.variant`, where the App keeps the reasoning
effort; it is absent when the model has no variant. The gateway records a
choice itself, there is no write route: the `model` and `mode` named in
`POST /api/agent-sessions` (not when they are omitted), and a later
`POST /api/agent-sessions/{asid}/model` or `/mode`, for that session's folder
and for this computer. It keeps ids only (provider, model, variant, mode), the
64 most recent folders per agent, in the shared store, so every device sees the
same defaults. Sessions started on the computer itself are not recorded.
Without the capability (an older gateway) `defaults` is only the agent's own
and the App keeps its local memory.

ACP agents (Claude Code, Qoder CLI, FX, and the others on the shared ACP
adapter) only show models and efforts inside an open session. With the same
capability, the gateway keeps the model list (with variants) the agent's last
session showed, per agent and agent version, and serves it in this catalog and
in discovery `agents[].models` (with `features.modelSelection` /
`reasoningEffort` following it) while no session is open, so a model and effort
can be chosen when the session is created. A different agent version drops the
cached list; an agent that never had a session open still lists none. The list
is kept in the shared store, so it is there right after a gateway restart,
before any session opens; it is refreshed from every session the App creates.
A session lists effort tiers (`variants`, discovery `reasoningEffortTiers`)
only for its selected model, so the cache keeps the tiers earlier sessions
showed for the other models. The key is the version the gateway probes for the
agent (for Claude Code, the `claude` CLI version, not the npx adapter's).

When an agent lists no models at all (nothing cached yet), `POST /api/tasks`
no longer refuses a requested `model` with `unknown_model`: it passes the
model (`<provider>/<model>`, or a bare id in the agent's own namespace) and
`variant` to the agent, which accepts or refuses it on creation.
`POST /api/agent-sessions` never checked the model against the catalog.

Capability `agent_session_catalog_v1` identifies the `asid` contract. When supplied,
only that session's owning provider is queried; a conflicting `agent_id` is
`400 agent_session_mismatch`. DeepSeek validates a supplied directory against
the session's current projected working directory, then calls real
`skills/list` and `commands/list` with the native session id. Without `asid`,
DeepSeek returns models/presets with empty skills and commands. It never chooses
one of several same-directory sessions or fills a missing list with placeholders.
The Gateway does not cache these scoped catalogs. App caches must include `asid`.

DeepSeek's instance capability flags become true only after a native list method
answers successfully. A preset-specific failure empties that request's list
without revoking capabilities observed in another session. Empty successful
lists clear removed entries, including removal of the last skill. Native
`commands/change`, `plugin-manager/changed` and `agent-preset/selected` cause
`agent.resync` with reason `agent_catalog_changed`. The public rc.3 stream
has no `skills/change`; re-entry and explicit refresh must also reread the catalog.
Skills are cold-readable; the native Agent lookup for commands may resume a cold
session, so command discovery is not described as cold-safe.

`providers[].name` is the display name (T3's own `displayName`, else a
built-in one such as "Claude Code" for `claudeAgent`; OpenCode's name, else the
id). `providers[].available` is `true` when the provider can start a session
now (OpenCode and DeepSeek: always; T3: enabled, installed, not `disabled`, and
offering at least one model). An unavailable provider is still listed so the
App can grey it out; `activation` keeps the finer state. Providers are not part
of `/api/discovery`, only of the catalog.

`skills[].slash` and `skills[].autoinvoke` are `Skill.Info`'s own optional
flags, and both default to `false` when the payload leaves them out — which most
skills do. **A slash menu lists only `slash: true`**; the rest exist for the
agent to reach for, and `autoinvoke: true` says it may do so unasked. Activate
one with [`POST …/skill`](#post-apiagent-sessionsasidskill).

For OpenCode, `?directory=` scopes the catalog **up**, never down: the modes, commands and
skills a project defines are added to the global ones, and a directory never
returns fewer modes than no directory. The gateway enforces that, because
OpenCode's `GET /api/agent` is a snapshot that fills in over roughly a second
for a directory nothing has opened yet — first empty, then the built-ins, then
the user's own modes. A catalog request waits for the scoped list to hold
everything the unscoped one holds — `models`, `providers`, `modes`, `skills`
and `commands` alike (bounded, about 1.5s) — rather than handing over whichever
stage it caught. **A client must never cache an empty catalog.** A catalog whose `models`,
`providers` or `modes` is empty is answered `cache-control: private, no-store`
and **without an ETag**, so there is nothing to hold on to and the next request
asks again. Any one of the three empty makes the catalog useless — a model
picker with no models is as broken as a mode picker with no modes — and all
three come back empty from a directory OpenCode has not loaded yet.

`skills` and `commands` are deliberately not part of that rule: a project really
can have none of either, and refusing to cache on that would mean never caching
for such a project.

User-defined modes come from `~/.config/opencode/agents/<name>.md` (global),
`.opencode/agents/<name>.md` (per project, discovered from the directory up to
the project root) and an `agent` block in `opencode.json`. They arrive with
`mode`, `description`, `color` and `hidden: false` like any other.

A picker should hide `modes[].hidden` and `mode == "subagent"` entries. (OpenCode
calls these agents; the gateway calls them modes.)
A provider with `activation: "disabled"` and a model with `enabled: false` are
carried through rather than filtered — grey them out and say OpenCode on the
host needs configuring. The gateway proxies no credential, integration or OAuth
route: sign-in is done on the host.

### `GET /api/agent-projects`

`?agent_id=` · ETag + `304`. `[{"id", "canonical", "name", "vcs?", "sandboxes": [], "missing?"}]`.

```json
[ { "id": "21eedff2…", "canonical": "/home/ryu/Work/muqun/app", "name": "app",
    "vcs": "git", "sandboxes": [] },
  { "id": "fcb725e4…", "canonical": "/tmp/muqun-c10/repo", "name": "repo",
    "sandboxes": [], "missing": true } ]
```

**`missing: true` means the directory is not on this host any anymore** — the
gateway stats each project's `canonical` path on every list. It is omitted when
the folder is there, so an ordinary list is unchanged, and the ETag follows the
body: a folder being deleted changes the answer and invalidates the cached one.

A picker should hide these, or show them greyed out with an explanation; it must
not offer to open one.

**There is no way to remove a project.** OpenCode 2.0.1 has exactly three
project operations — `v2.project.list`, `v2.project.current` and
`v2.project.update` — and `PATCH /api/project/{id}` accepts only `canonical`,
`name`, `icon` and `commands`, with `additionalProperties: false`. No delete, no
archive, no hidden flag. So a project OpenCode has ever seen is in this list for
good, and `missing` is the whole of what the gateway can say about it.

### `GET /api/agent-directories`

With capability `agent_directory_search_v1`, `?query=keyword` searches folder
names case-insensitively beneath the current user's home. A nonempty query takes
precedence over `prefix`. The shared App workspace picker uses this for Home,
agent sessions, and the session list; selecting a result passes its full path.
Search skips dot directories, the dependency/build exclusions below, and
`Library`, `AppData`, `Applications`, `vendor`, and `venv`. Directory symlinks
are selectable and searched, including workspaces on external disks; canonical
directory identities prevent cycles and repeated traversal. Results retain the
home-relative linked path. It reads at most 10,000 entries, descends at most eight levels, spends
at most 250 ms, and returns at most 30 results. Bounds set `truncated`; results
are suggestions, not an exhaustive filesystem index. No file contents are read.
Explicit path completion remains available independently of keyword search.

`?prefix=` is what the user has typed, completed one level at a time. Local
filesystem only, nothing cached.

- The prefix splits at its last `/`: `~/Work/mu` lists `~/Work` for names
  starting `mu`, case-insensitively; a trailing `/` (`~/Work/`) lists
  everything there; `~` or an empty prefix lists the home directory. `/ho`
  completes against the filesystem root, which is allowed: this lists names one
  level down and never recurses, unlike the asset scan, whose fence keeps `/`
  and a bare home out as *workspaces*.
- A prefix that is neither `~`-rooted nor absolute, `~user`, or a directory that
  does not exist answers an empty list.
- Dot directories only when the partial starts with `.` (`~/.co`).
  `node_modules`, `.git`, `target`, `build`, `dist`, `.cache` and
  `__pycache__` are never offered.
- A symlink is listed when it points at a directory; it is never read into.
- At most 30 answers, sorted by name, from at most 2000 entries read in at most
  50 ms. Hitting any bound sets `truncated`.

`data` is still the bare list, so older Apps keep working; `home` (the absolute
home directory, for display) and `truncated` sit beside it in the envelope:

```json
{ "schema_version": "…", "capabilities": {…},
  "data": [{ "name": "muqun", "path": "/home/u/Work/muqun" }],
  "home": "/home/u", "truncated": false }
```

`path` is under the canonical form of the directory being listed.

---

## Background shells

| Route | Query | Response |
|---|---|---|
| `GET /api/agent-shells` | `directory` | `[Shell.Info]` (verbatim) |
| `GET /api/agent-shells/{shell_id}` | — | `Shell.Info` |
| `GET /api/agent-shells/{shell_id}/output` | `cursor`, `limit` | `{output, cursor, size, truncated}` |
| `DELETE /api/agent-shells/{shell_id}` | — | `{"killed": true, "shell": Shell.Info}` |

`DELETE` on OpenCode finishes the job itself: if the shell is still `running` after OpenCode's own kill (a reparented child), the Gateway verifies the pid belongs to OpenCode, sends SIGTERM, then SIGKILL after 2 s (Linux only). A shell that cannot be stopped answers an error naming the pid; one OpenCode already dropped has `shell.status: "gone"`.

`Shell.Info` is OpenCode's own, camelCase:
`{id, status: "running"|"exited"|"timeout"|"killed", command, cwd, shell, file, pid?, exit?, metadata, time}`.

---

## Agent status

### `GET /api/agent-status`

```json
{ "available": true, "origin": "adopted" | "spawned" | "none",
  "url": "http://127.0.0.1:49374", "version": "2.0.1",
  "stream_connected": true, "autostart": true,
  "kind": "opencode", "agent_id": "opencode" }
```

`?agent_id=<id>` describes that agent instead of the primary; `agent_id` and
`kind` (both the agent's id, omitted while nothing is attached) say which one
this is. With `?agent_id=` this route does **not** answer `available: false`:
an unknown id is `400 invalid_agent` and a known one that is not attached is
`503 agent_unavailable` naming it. A non-primary agent is always an adopted
service (`origin: "adopted"`, `autostart: false`). See [Agents](#agents).

The one agent route that answers `200` when no agent is attached — it exists to
explain why the others are returning 503.

`origin` is how the agent currently attached was obtained: `adopted` for a
service that was already running, `spawned` for one this gateway started,
`none` when nothing is attached.

### Which OpenCode, and who starts it

The gateway keeps an OpenCode agent attached for its own lifetime. It adopts a healthy
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

### Configuring Codex

Codex service support defaults on, including when its config block is omitted.
It uses the existing executable on `PATH` and CLI login/configuration; Gateway
does not install or log in. Set `codex.enabled: false` to disable it, or
`agent_runtime.enabled: false` to disable all service agents without changing
terminal dispatch. `muqun-gateway agent setup codex` only explains setup.
See [Codex protocol reference](codex-protocol.md) for the stable 0.162.1 baseline,
readiness, model scope and unsupported features. Real Codex and paired App/macOS
acceptance remain deferred by user instruction, not passed by protocol fixtures.

### Configuring T3 Code

T3 Code (`t3 serve`) is off until `config.json` asks for it. The gateway never
scans for it: it tries only the URL it is given, or T3's default
`http://127.0.0.1:3773` when `enabled` is set without one.

```json
{ "t3": { "enabled": true, "url": "http://127.0.0.1:3773",
          "pairing_token": "<token printed by t3 pair>",
          "runtime_mode": "full-access" } }
```

| Key | Meaning |
|---|---|
| `enabled` | Attach to T3. Setting `url` also enables it. |
| `url` | The T3 server, `http(s)://host:port`. |
| `pairing_token` | A one-time pairing token. |
| `token` | A bearer you already hold; used instead of pairing. |
| `runtime_mode` | What new threads may do unasked: `full-access` (T3's default), `approval-required`, `auto-accept-edits` or `auto`. |

`T3_URL`, `T3_TOKEN` and `T3_PAIRING_TOKEN` in the gateway's environment fill
the matching keys when `config.json` leaves them unset; they never enable T3
by themselves.

`muqun-gateway agent setup t3` does the pairing on the gateway host: it
mints a bearer with `t3 auth session issue` (or exchanges `--token <code>`),
checks it against the server, stores it, and sets `enabled` and `url`. An
issued bearer asks for a 30-day TTL (T3 may cap it); a paired one lasts what
T3 grants, which setup prints. The gateway cannot renew either: when T3 stops
accepting it, `muqun-gateway agent` shows T3 `unconfigured`, and re-running
`agent setup t3` replaces it. When T3
was already on for that URL the running gateway picks the new credential up on
its next round without a restart.

To pair by hand, run `t3 pair` on the T3 host (with the same `--base-dir` as the
server, if it has one). It prints a pairing URL ending in `#token=…` and the
token on its own line; put that token in `t3.pairing_token` and restart the
gateway. The gateway exchanges it once for a bearer, keeps the bearer in
`t3-credential.json` (mode `0600`) in its state directory, and forgets the
pairing token, which T3 has now consumed; the stale value left in
`config.json` is ignored while the stored bearer is accepted. If T3 revokes
the bearer, pair again with a new token.

### Losing the agent

The supervisor re-discovers whenever the health probe fails or the registration
moves. It also watches the event stream, and **a dropped stream is acted on
within about a second** rather than at the next health poll: the stream going
down is the agent telling us it has gone, and waiting out the poll interval
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

The request is the encrypted transport's own: `X-Muqun-Transport: 1`,
`X-Muqun-Device` and `X-Muqun-Envelope` with the token sealed inside, and no
`Authorization` header. The response carries `content-type: text/event-stream`
and `x-muqun-transport: 1`; the frames are `event: muqun.encrypted`. The key is
HKDF'd under `muqun-transport-v1/sse/{sid}/{request nonce}` — the same
derivation, AAD and record shape as `GET /api/sessions/{id}/events`, which is
the point: one reader serves both. Both the global path and the session-scoped
`/api/sessions/{id}/agent-sessions/{asid}/stream` behave identically.

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

### Optional completed-message generation throughput

`agent_message_generation_v1` declares the optional `TimelineItem.generation`
shape `{ "scope": "message", "tokens_per_second": 123.4 }`. It rides the
existing snapshot, timeline and `agent.timeline.upsert` contracts. The value
is finite and positive; absence means unavailable. Render it once after the
associated assistant message, never as an estimate during streaming. Other
providers currently omit it.

DeepSeek reads only a settled, uninterrupted `assistant/message`: its own
`usage.outputTokens`, the first nonempty text/reasoning/tool delta in its
embedded `stream`, and the event's original Session completion timestamp.
The rate is output tokens divided by the recorded first-token-to-completion
seconds, matching upstream's per-message decode interval. It includes output
reasoning/tool tokens when the provider reports them. It is attached only to
the last text row of that message; a response containing only tools has no
text footer. Attempt-only records, missing usage/timing, malformed or
nonmonotonic timestamps and zero-length intervals produce no metric.
`sessionStats.decodeTokens/decodeMs` are whole-session totals and are never
used as a fallback. Gateway arrival times and App stream clocks are not used.

Sources (DeepSeek Harness `dsh-v0.2.1-rc.3`, also verified against installed
`0.2.1-rc.2`): [message decode readings](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.3/packages/client/ui-chat/src/client/contract/turn-metrics.ts),
[embedded stream timestamps and first-token predicate](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.3/packages/llm/llm/src/assistant-stream.ts),
and [whole-session statistics](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.3/packages/session/session-stats/README.md).

### Task plumbing in the transcript

Muqun MCP tool calls are visible in every session on snapshots, timelines,
event replay, SSE and `/api/ws`, including pending, streaming, running,
completed and failed calls. Provider-specific names remain intact, including
OpenCode's `muqun_tasks_wait`, DeepSeek and T3 tool names, and Codex's
`muqun:exec.run` and dynamic tool names. Catalog discovery and `execute`
calls remain visible even before their code is known. Skill-loading rows
retain their existing presentation policy.

A session a task is (or was) bound to ([docs/mcp.md](mcp.md#finishing-a-delegation))
is served without the gateway's task plumbing, there and
on `GET …/inbox`:

- A user `text` row (or a `synthetic` row) that ends with the completion
  instruction an older gateway sent with a goal (goals now go alone) is served
  with the instruction cut, and is left out when it was only the instruction. Any wording the gateway has sent is recognised, by the
  instruction's fixed opening (`When the whole task is finished, `) and its
  closing placeholder line, never by the goal.
- Each turn's last assistant message is served without the one marker the
  task ledger read the turn's end from: over the message's `text` rows
  joined, the last `MUQUN_TASK_DONE:` or `MUQUN_TASK_BLOCKED:` below any
  restated instruction that starts a line (after bullets and markup) or
  follows the agent's words on it, and is not a placeholder -- cut from the
  marker to its line's end, the words before it kept -- unless it sits in a
  code fence as rendered. Any other marker stays. A row left with nothing else is left out. While the
  session runs a turn, a last line that can only be the start of a marker (or
  is bare list or emphasis markup, such as `- `) is held back too, so the
  marker never shows mid-turn. An `agent.timeline.upsert` whose rows are all
  left with nothing arrives as `agent.timeline.removed` for them. `seq` keeps
  counting, so a client may see a gap where a row was left out.
- The notice OpenCode leaves for the task's per-session instruction
  (`Instructions updated: …muqun-task`) is left out.
- OpenCode's notice whose first line is "The Code Mode tool catalog has
  changed." followed only by the catalog listing is left out.
- An inbox item (`GET …/inbox`, the snapshot's `inbox`,
  `agent.inbox.changed`) is cut the same way as a user row: OpenCode holds
  the instruction there for a moment before the turn runs.

The agent's own transcript, the mirror and the task follower keep the raw text
(the marker is read from it); the task's `summary` is still the marker's line.

### WebSocket events: `GET /api/ws`

One WebSocket per device carrying the agent events of **any number of
sessions**, so an App can watch every session on a gateway over one socket.
It carries exactly what the per-session stream above carries; requests stay on
HTTP, and the SSE stream is unchanged and remains the fallback.

**Feature gate.** `ws_events` in the flat `capabilities` array (`/health`,
`/api/capabilities`, `/api/discovery`), and in the `/api/discovery` body at
top level beside `planes`, shown to every paired caller:

```json
"transports": { "websocket": { "path": "/api/ws", "protocol": 1 } }
```

A gateway without either has no socket; use the SSE stream. `apiVersion` is
1.9.0 or later.

#### Handshake

An ordinary HTTP/1.1 WebSocket upgrade (`GET /api/ws`, `Upgrade: websocket`),
authenticated **exactly like the SSE stream**:

- A device paired without a transport key (`transport_encryption: disabled`):
  `Authorization: Bearer <device token>`. Every frame is plaintext JSON.
- A device with a transport key: the encrypted transport's own request —
  `X-Muqun-Transport: 1`, `X-Muqun-Device: <device id>`, and
  `X-Muqun-Envelope: <base64url of the envelope JSON>`, no `Authorization`.
  The envelope is sealed with the request key over AAD `GET /api/ws` (method,
  one space, then path and query exactly as sent), and its plaintext is
  `{"token": "<device token>", "content_type": null, "body": ""}`. The `101`
  carries `x-muqun-transport: 1`, and every frame is sealed (below). Such a
  device sending a bare `Authorization` header is refused, as on every route.

Refusals are ordinary HTTP answers, never a `101`: `401` (no token), `403`
(unknown or revoked device, bad envelope, missing device proof), `400
websocket_upgrade_required` (authenticated, but not an upgrade), `429
too_many_connections`. On an encrypted device a refusal is sealed like any
other response.

The first frame is always `hello`. Nothing else is sent until the client
subscribes.

#### Keys, nonces and AAD (encrypted devices only)

Let `material` be the device's transport key bytes, `nonce` the upgrade
envelope's `nonce` field **as the base64url string it was sent as**,
`connection_id` the id from the first frame, and `request_aad` the upgrade's
AAD (`GET /api/ws`). HKDF-SHA256 with salt `muqun-transport-v1` and a 32-byte
output:

| Direction | HKDF info |
|---|---|
| server → client | `muqun-transport-v1/ws/{connection_id}/{nonce}` |
| client → server | `muqun-transport-v1/ws-client/{connection_id}/{nonce}` |

Each direction counts its own `seq` from 0, one per frame. A frame is
AES-256-GCM under its direction's key with

- nonce: 12 bytes, four zero bytes then `seq` as a big-endian u64 (the SSE
  record's nonce layout);
- AAD: `{request_aad}\n{connection_id}\n{seq}` (UTF-8; `\n` is a line feed;
  `seq` in decimal);
- ciphertext: the frame's plaintext JSON, tag appended, base64url without
  padding.

On the wire a sealed frame is a text message:

```
{"seq":0,"cid":"<connection_id>","c":"<base64url ciphertext>"}   first server frame only
{"seq":1,"c":"<base64url ciphertext>"}                           every other frame, both directions
```

The first server frame carries `cid` in the clear, because the client needs it
to derive the key that opens that very frame; it is authenticated all the
same, through the key and the AAD. The client checks that each server `seq`
is the next one and that the opened `hello` names the same `connection_id`.
The gateway requires each client `seq` to be the next one — a gap, repeat or
reorder is `out_of_order` and a close — and a client frame that fails to open
is `invalid_frame`. A server frame that cannot be sealed is dropped, never
sent in the clear.

The client direction has its own per-connection key rather than the device's
static request key, because frame nonces are counters that restart at 0 on
every connection: under one shared key, two connections would reuse a
(key, nonce) pair, which in GCM discloses plaintext and the authentication
key.

**Worked example.** Both values were produced by the gateway and by an
independent implementation (Node `crypto`); the gateway test
`websocket_frame_fixture_matches_the_documented_vector` pins them.

| Input / output | Value |
|---|---|
| `material` | the 32 bytes `01 02 03 … 1f 20` |
| `connection_id` | `conn-fixture` |
| `nonce` | `req-nonce-fixture` |
| `request_aad` | `GET /api/ws` |
| server key (hex) | `b2154a41fe5ff8e4d2d48e2452f5ae1ac7db5dde927a6a836cff95affd0617c4` |
| server frame `seq` | `3` |
| its AAD | `GET /api/ws` LF `conn-fixture` LF `3` |
| its plaintext | `{"t":"pong"}` |
| its `c` | `OEEP-5TQCD9LdcHsM3giEcyEyokt8XYFIAIRqw` |
| client key (hex) | `9bce31da785ad2c07b7a1778afc4c1e1ae044ffd3628e917830c069a07065f1d` |
| client frame `seq` | `0` |
| its AAD | `GET /api/ws` LF `conn-fixture` LF `0` |
| its plaintext | `{"t":"ping"}` |
| its `c` | `V6OgtEmv9QE7CX2BmkcpLomsJYuJgW0aMASrVA` |

#### Frames

Every frame is one JSON text message (the plaintext, when sealed). Binary
messages are refused with `invalid_frame`.

Server → client:

| Frame | Meaning |
|---|---|
| `{"t":"hello","connection_id":"<uuid>","protocol":1,"generation":"<id>"}` | First frame, always. `generation` is the gateway instance generation (see Conventions); a client that finds it changed since its last socket is talking to a restarted gateway. Older gateways omit it. |
| `{"t":"subscribed","asid":"ses_1"}` · `{"t":"subscribed","all":true}` · `{"t":"subscribed","tasks":true}` | The subscription is in effect: matching events published from here on reach this socket. Start catch-up after this, not before. |
| `{"t":"event","asid":"ses_1","seq":12,"event":"agent.timeline.upsert","data":{…}}` | One domain event. `event` is the SSE `event:` name and `data` is the SSE `data:` payload **byte for byte**, embedded as JSON rather than as a string. `asid` and `seq` repeat the event's own (`asid` is `""` and `seq` 0 for `agent.worktree.changed` and `agent.resync`). |
| `{"t":"task","asid":null,"seq":12,"event":"task.updated","data":{…}}` | One full `TaskView` on a `subscribe_tasks` socket. |
| `{"t":"defaults","default_cwd":"/home/user/project"}` | On a `subscribe_tasks` socket: the directory a task this device opens without `cwd` runs in (`"~"`, the home directory, when it has no usable previous one; `null` when unknown). See **Task updates** below. |
| `{"t":"resync","asid":"ses_1"}` | This socket fell behind the gateway's event backlog; refetch that session. `asid` `""` means every session. One per subscribed session, or a single `""` under `subscribe_all`. With `"tasks":true` it is about task updates instead: refetch `GET /api/tasks`. |
| `{"t":"pong"}` | Answer to `ping`. |
| `{"t":"error","code":"…"}` | Sent immediately before the gateway closes the socket (close code 1008). |

Client → server:

| Frame | Meaning |
|---|---|
| `{"t":"subscribe","asid":"ses_1"}` | Add a session. The id is not looked up, so subscribing to a session that does not exist yet is allowed, and a subscription survives the agent restarting. Idempotent. |
| `{"t":"unsubscribe","asid":"ses_1"}` | Drop a session. No acknowledgement. |
| `{"t":"subscribe_all"}` | Every session. `unsubscribe` does not narrow it; `subscribe_tasks` switches to task-only delivery. |
| `{"t":"subscribe_tasks"}` | Task updates only. Replaces the socket's session subscriptions; a later `subscribe` or `subscribe_all` switches back to session delivery. That request's `subscribed` acknowledgement includes `"tasks":false` to report that `t:"task"` delivery ended (for example, `{"t":"subscribed","asid":"ses_1","tasks":false}` or `{"t":"subscribed","all":true,"tasks":false}`). Under `subscribe_all`, task updates still arrive as `t:"event"`. |
| `{"t":"ping"}` | Application-level liveness; answered with `pong`. |

Unknown extra fields are ignored. An unknown `t` is `unknown_type` and a
close. `asid` must be 1–128 bytes with no control characters (else
`invalid_asid`).

Events with an empty `asid` (`agent.worktree.changed`, a global
`agent.resync`) reach every socket that has at least one subscription, as they
reach every SSE stream.

`error` codes: `invalid_frame`, `out_of_order`, `unknown_type`, `invalid_asid`,
`too_many_subscriptions`, `frame_too_large`, `superseded`.

#### Lifetime and limits

| Bound | Value |
|---|---|
| Inbound message size | 16 KiB, else `frame_too_large` and a close |
| Individually subscribed sessions per socket | 256, else `too_many_subscriptions` and a close |
| Sockets per device | 4. The newest wins: the device's oldest socket gets `superseded` and is closed. |
| Sockets per gateway | 64. Past it the upgrade is `429 too_many_connections`. |
| Server heartbeat | A WebSocket ping every 25 s; two unanswered pings drop the socket. Any client frame counts as an answer. |
| Revocation | Checked every 5 s; a revoked device's socket is closed without an `error` frame, as its SSE stream is. |

**Task updates.** A socket that sent `subscribe_all` also receives one
`{"t":"event","asid":null,"seq":<n>,"event":"task.updated","data":<TaskView>}`
frame for every change to a task (capability `task_ledger`; see `docs/mcp.md`),
and one for its parent task, whose `children` counts moved, when it has one. A
socket subscribed to individual sessions does not get them. `data` is the whole
task, so a client keeps the newest frame per `task_id`. A socket that falls
behind the task events gets one `{"t":"resync","asid":"","tasks":true}` frame:
refetch `GET /api/tasks` (a client that ignores `tasks` reads it as a resync
of every session, which also covers it). Refetch `GET /api/tasks` after a
reconnect as well.

When `task_ws_subscription` is advertised, `subscribe_tasks` receives the same
task views with `"t":"task"`, and no
agent-session or pane traffic. It still receives the initial `hello` and a
`{"t":"subscribed","tasks":true}` acknowledgement. Task lag uses the same
task resync frame.

When `task_defaults` is advertised, a `subscribe_tasks` socket is also told
where a task this device opens without `cwd` (`POST /api/tasks`) will run,
so the app can show it before the user sends: one
`{"t":"defaults","default_cwd":…}` frame right after the `subscribed`
acknowledgement, another after each task resync frame, and a new one
whenever a task the device opens changes the answer. `default_cwd` is the
directory of the device's newest task while that is still a directory, else
the home directory -- always named from `~` when it is under home (`"~"`,
`"~/src/app"`), so no user name is shown; `POST /api/tasks` expands `~` in
`cwd`. `null` only when the task store cannot be read. `GET
/api/tasks/defaults` answers `{"default_cwd": …}` by the same rule for a
client without the socket.

**Task fields and outcomes** (capability `task_outcome`; full contract in
`docs/mcp.md`):

- `TaskView.title` (always a string): the task's `label`; else, for a session
  delegation, the first sentence of the name the agent session gave itself
  (an untitled session's placeholder -- its asid, `New thread`, `DeepSeek
  Agent Session` -- or the goal restated or cut short is not used); else the goal's first sentence -- cut to 60 characters with
  `…`, back to a word boundary when that keeps at least 30 characters. An
  open task's follows the session's name as it changes; an ended one keeps
  the name it had when it ended. Show it instead of the raw `goal`.
- Agent sessions (`GET /api/agent-sessions`, …) never carry an empty
  `title`: until the provider names one it reads as before -- OpenCode's
  asid, `DeepSeek Agent Session`, T3's `New thread`. A task's `title` never
  takes such a placeholder; it keeps the goal's first sentence until the
  session is really named.
- `tasks.complete` takes `outcome`: `done` (default) or `blocked`. A blocked
  agent -- a missing file, an unclear request, a permission it cannot get --
  may instead print `MUQUN_TASK_BLOCKED: <one-line reason>`, beside
  `MUQUN_TASK_DONE: <one-line summary>`. Either moves the task to `waiting`
  with the reason as `summary` (events carry source `tasks_complete_blocked` or
  `marker_blocked`). It stays open: the user answers in the session or pane
  and the agent goes on, or the device confirms or cancels it. Going idle
  does not turn a blocked task into `idle_unconfirmed`; a new reason
  replaces the old one.
- The second line of a task card: `progress` while the task works, `summary`
  once it pauses or ends (`progress` is absent then). For a `waiting` task
  `summary` is what the user is asked -- "Needs you · <summary>": a blocked
  agent's reason; for a pending permission request, its prompt; for a
  question form, the last line of the agent's last message when that came
  after the turn's last tool call (the agent asked in its own words), else
  the form's title; for a terminal agent on a menu, the screen's last
  meaningful line.

**Catch-up is unchanged.** The socket delivers live events only. After a
(re)connect, re-subscribe, wait for `subscribed`, then catch up each session
exactly as with SSE: `GET …/timeline?after=<last seq>`, and on `410
resync_required` refetch the snapshot. A `resync` frame asks for the same.

---

## Domain events

The SSE `event:` name and the payload's own `type` are the same string.

### `agent.session.updated`

```json
{ "type": "agent.session.updated", "asid": "ses_1", "seq": 12, "info": { …AgentSessionInfo… } }
```

Sent on create, rename (including auto-title), model or mode switch, usage
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

`agent.form.resolved` may include `reason: "answered" | "expired"`.
`answered` means the reply was accepted or a matching committed reply was observed.
`expired` is emitted only for a known unanswered, nonblocking Codex question when
its owning live turn ends. An answer still in flight has an unknown outcome and
receives no expiry reason. Clients may preserve an expired form's local draft in
the ordinary composer, but must not automatically submit it. Missing or unknown
reasons, including disconnect, unload, and stale ownership invalidation, do not
authorize draft restoration. Older providers omit this optional field.

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
  "agent_id": "opencode",
  "backend_session_id": "ses_1",
  "title": "Tool availability and directory file count request",
  "mode": "build",
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

Only `asid`, `agent_id`, `title`, `status` and `updated_ms` are always present
(`agent_id` is the owning agent's id, see [Agents](#agents); `mode` is the
session's mode); every other
field is omitted when unset. **`backend_session_id` is omitted when it would
only repeat `asid`** — which on OpenCode is always, since the gateway mints no
ids of its own; absent means "the same as `asid`", and it is present only for
an agent that really does key sessions differently. `deleted` is omitted unless
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

**`agent_switched`** — `{ "type": "agent_switched", "mode": "plan", "previous": "build" }`

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

## Linked gateways (`fleet_peers`)

Another of the owner's computers is a paired device of `kind: "gateway"`
(`GET /api/pairings` shows the kind). Its token reaches only the task routes
(`POST /api/tasks`, `GET /api/tasks` without `root`, `GET /api/tasks/{id}`,
`POST /api/tasks/{id}/cancel`), `subscribe_tasks` on `/api/ws`,
`GET /api/fleet/self` and `DELETE /api/pairings/{its own id}`; everything
else answers `403 peer_forbidden`.

The owner -- an app device, or the admin token (the `muqun-gateway peers`
CLI) -- links and manages them; a gateway token gets `403 peer_forbidden`
on all of these:

| Route | Body | Answer |
|---|---|---|
| `POST /api/pair/pending` (on the computer to be linked) | `{device_name, kind: "gateway", server_id?}` | `{request_id, code, expires_in_ms, pairing_key?}` for `POST /api/peers` on the other side. `pairing_key` (base64url) is present when this computer runs `transport_encryption: required`: its current pairing transport key, the one its QR carries, for the other side to seal its claim with; absent in `disabled` mode. `400 invalid_request` for any other `kind`; `409 pairing_in_progress` while another pairing is pending, `429 pairing_rate_limited`. A minted code lives one minute (`expires_in_ms: 60000`); a code a device asks for itself (`POST /api/pair/request`, shown on this computer's screen) lives five (`300000`), as `GET /api/pair/pending` reports for each: unclaimed by then (the link failed), it is neither shown by `GET /api/pair/pending` nor claimable, and a new pairing -- a phone's, or the next mint -- takes its slot. |
| `DELETE /api/pair/pending/{request_id}` (same computer, same token that minted it) | none | `{cancelled: true}`: the minted code is gone and the slot free, for when `POST /api/peers` on the other side failed. Only the device (or the admin token) that minted it may; any other pending pairing, an expired one, or none is `404 pairing_not_found`. |
| `GET /api/peers` | none | `{accept_delegations, peers: [{peer_id, server_id, name, url, reachable, last_seen_ms, unauthorized?}]}`; a peer not heard from for a minute is pinged first (3 s). One that answers `401`/`403` (it no longer takes this gateway's token) is kept and listed with `unauthorized: true` and `reachable: false`; a `POST /api/peers` for the same computer or under its name replaces it. |
| `POST /api/peers` | `{url, name, request_id, code, pairing_key?, transport_encryption?, server_id?}` (`name` may not be `self`, in any case; `url` uses HTTP or HTTPS; `server_id` is the id the owner knows the other computer by, and its claim answer must say the same: `409 peer_identity_mismatch`, and what the claim enrolled is taken back) | The new peer. With `pairing_key` (the mint's), the claim is sealed exactly as a phone's QR claim is (`Direction::PairingRequest`, AAD `POST /api/pair/claim`, envelope version 1) and only an answer sealed under that key and bound to the claim's nonce is taken. Without it the claim goes in the clear and the answer is sealed under the code alone; `transport_encryption: "required"` (what the other computer's `POST /api/pair/request` said of itself) then refuses an answer in the clear. `502 peer_unreachable`; `409 name_conflict` (`name` is this computer's own label, any case; checked before the claim, so the code stays good), `409 peer_refused` (`error.reason`: `pairing_code_expired`, `invalid_pairing_code`, `pairing_not_requested`, `invalid_token`, `claim_unsealed` (the answer came in the clear where it had to be sealed), `claim_not_opened` (sealed, but not under the pairing key or not to this claim) or `refused`), `peer_conflict`, `peer_name_taken`; `400 peer_is_self`, `invalid_request` (a `pairing_key` that is not a 32-byte key included). |
| `PATCH /api/peers/{peer_id}` | `{name?, url?}`, at least one (`name` not `self`; `url` uses HTTP or HTTPS) | The changed peer. A new `url` (its address changed) is proven first with one call carrying this link's token: `502 peer_unreachable` when nothing answers there, `409 peer_refused` when what answers does not know the link; neither keeps it. `400 invalid_request`, `409 peer_name_taken`, `409 name_conflict` (this computer's own label), `404 peer_not_found`. |
| `DELETE /api/peers/{peer_id}` | none | `{remote_revoked, counterpart_found, counterpart_revoked}`; this gateway forgets the peer, asks it to revoke this gateway's device, and revokes the peer's own login here (below). `404 peer_not_found`. |
| `PUT /api/fleet/settings` | `{accept_delegations}` | `{accept_delegations}`, written to `config.json` (`fleet.accept_delegations`). |

Manual links accept HTTP and HTTPS URLs, including LAN and Tailscale
addresses. Pairing requests and claims do not follow HTTP redirects.

A peer's name is what `fleet.describe`/`fleet.list` show and what
`POST /api/tasks` `mentions` must use (see
[mcp.md](mcp.md#opening-a-task-from-the-device)). More than 8 names, or a
name that is neither a linked computer's nor `self`, is `400
invalid_mentions` and nothing is created; the app sends the task again,
once, without `mentions`.

`POST /api/pair/pending` takes an optional `server_id`, the claiming
gateway's own as the owner knows it, kept on the device record the claim
makes. `POST /api/pair/request` accepts the field and ignores it: a gateway
asking for a code itself cannot say whose login it is, so its record carries
none. Nothing checks the id, so it never replaces or evicts a device; its only
use is that `DELETE /api/peers/{id}` also revokes the gateway devices here
whose `server_id` is that peer's (its login into this computer), so one call
unlinks both directions. The peer's own `server_id` is what its claim answer
said unless the owner gave one to `POST /api/peers`, which the answer then has
to match; an app that passes it keeps a remote from naming another computer's
id and having that computer's login revoked when it is removed. A peer marked `unauthorized` after a `401`/`403`
keeps its login here; its open mirrors fail (`reason: "peer_unlinked"`), as they do on
`DELETE /api/peers/{id}`.
`DELETE /api/peers/{id}` (the admin token or an app device only) answers
`{"remote_revoked", "counterpart_found", "counterpart_revoked"}`: whether the
peer confirmed it revoked this gateway's token, whether a login of the peer's
was here, and whether it is revoked (found but not revoked: the device list
could not be written). Each runs whatever the other did; the peer is forgotten
either way, and if its row cannot be removed the `500 peer_store_failed`
still carries the three fields.

A delegation to a linked computer is a mirror here with `binding: {kind:
"peer", peer_id, peer_server_id, remote_task_id, peer_name}`: `peer_server_id`
is the id that computer gave for itself, which the phone knows it by, and
`remote_task_id` is empty while a create that got no answer has not found its
task there yet (`tasks.delegate` then carries a `note` saying so). The create
carries a `client_ref`, which the task over there keeps and shows in its view
(with `created_ms`), and that is how it is found.

A mirror's view also carries `reason`, made when the view is (nothing
stored), for the app to word in its own copy; the `summary` stays a plain
line. On a `failed` mirror: `peer_unlinked` (unlinked here, or the other
computer no longer takes this one's token), `peer_lost_task` (the other
computer no longer has the task), `peer_not_confirmed` (a create that got no
answer never turned up there). On an open mirror: `peer_unreachable` while
its computer has not answered in the last 2 minutes, or went away just now (its task socket closed, a call to it failed). Absent otherwise, and on
every task that is not a mirror. (`delegation_loop` is never a view reason:
that delegation creates nothing; it is the MCP error code.) A task
another computer opened here (`origin: "peer"`) can be confirmed from the phone
like a device's; its `requested_by` is `{kind: "gateway", id, name, server_id?}`,
where `server_id` is the id that computer gave for itself when it paired here,
when its device record has one.

At the device limit, a gateway's `POST /api/pair/claim` makes room only by
dropping the oldest gateway device, never one of the owner's apps; with no
gateway device to drop it answers `409 device_cap`.

**Operator note:** a gateway build older than this one does not know
`kind` and reads a gateway device as an ordinary app device, with full
device access. After rolling a gateway back below this version, revoke its
gateway devices (`muqun-gateway revoke <device id>`, or `manage`'s Devices
view, where they show as `name (gateway)`).

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
| Event WebSocket | see "WebSocket events" |

Sessions are also evicted on `session.deleted`.

---

## Things that do not exist in v2, so the gateway does not offer them

- **Session sharing.** Removed upstream. Use `GET …/export`.
- **A model/mode field on a prompt.** Model and mode are session state.
- **`session.status` / `session.idle` events.** Status comes from
  `session.execution.*`; the gateway accepts the others if a later version
  starts sending them.
- **A title endpoint.** Auto-titling is server-side; `rename` is the only write.
- **Credential, integration or provider-auth routes.** The user configures
  OpenCode on the host.

### Optional observed session detail

Providers may include `agent_name` and `agent_role` for an identified child agent,
and `git_info: {sha?, branch?, origin_url?}` for producer-captured Git metadata.
This Git snapshot does not replace the live directory-based Git endpoints.

`context_usage: {tokens: TokensUsage | null, total_tokens: number, limit?:
{context: number}}` is the latest reported context occupancy, distinct from
cumulative session `tokens`. Prefer the explicitly reported `total_tokens` for
occupancy. The token breakdown uses disjoint input, output, reasoning, cache-read,
and cache-write counters. Missing usage, limit, or cost is unknown. Streaming
changes arrive in the existing `agent.session.updated` event.

### Nonblocking questions and scoped file mentions

`FormRequest.nonblocking: true` identifies a question the agent may continue
working through. Missing/false retains the existing blocking form semantics.
Reply through the same `POST .../forms/:id/reply` endpoint; the answer object is
specified by that form's fields. A nonblocking form alone never implies the
session is blocked. `promptSteering` permits `delivery: "steer"` while running;
it does not enable queue/inbox operations. Form handles are opaque and must not
be reconstructed from transcript items.

`GET /api/agent-sessions/:asid/files` and its legacy counterpart use the session's
owner and recorded directory. A directory query cannot redirect a session search.
Root `/api/agent-files` accepts optional `agent_id` plus `directory`. An explicit
`fileMentions: false` capability disables file-mention lookup for that provider.
