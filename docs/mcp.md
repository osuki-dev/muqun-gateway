# Gateway as an MCP server

Capabilities: `mcp_server` (the `/mcp` endpoint; present only while
`mcp.enabled` is true), `fleet`, `task_ledger`, `task_ledger_create`
(`POST /api/tasks` and the task tree), `task_ledger_pane` (a terminal agent
as `agent_id`, bound to its pane), `task_ledger_complete` (a delegation is
`done` only on an explicit signal: `tasks.complete`, the `MUQUN_TASK_DONE`
marker, or the device's `POST /api/tasks/{id}/complete`; see
[Finishing a delegation](#finishing-a-delegation)), `task_ws_subscription` (`subscribe_tasks`
on `/api/ws`), `task_outcome` (`tasks.complete`'s `outcome` and the
[blocked](#blocked) state, a `waiting` task's question as `summary`, the
task view's `title`, `progress` in words) and `task_defaults` (`GET
/api/tasks/defaults` and the `defaults` frame). Discovery also announces
`transports.mcp`. (`tasks` is a different, older capability: `POST
/api/sessions/{id}/tasks`.)

An assistant running on this machine (Claude Code, OpenCode, Codex, Pi) can use
the gateway as a tool server: ask what this computer offers, run a command in a
visible terminal pane, or hand a goal to an agent and follow it to the end.
There are three channels onto the same work. The assistant speaks MCP to
`/mcp`; the paired phone reads and cancels the same work through the Tasks API
below and a live `task.updated` frame on `/api/ws`; and the owner manages
assistant tokens with `muqun-gateway mcp ...` or `/api/mcp/tokens`. Everything
the assistant starts is a task, so the phone sees it.

> **Warning: `exec.run` runs outside the host's sandbox.** It types the
> command into a terminal pane of the user's own shell. The host's sandbox,
> permission rules and approval prompts do not apply to what that command
> does; the host only approves the `exec.run` tool call itself. An approval
> card on the phone for destructive commands is planned for P2 and does not
> exist yet. Codex users: `default_tools_approval_mode` (and per-tool
> approval settings) for the `muqun` server decide whether Codex asks before
> calling `exec.run` at all -- leave it on approval if you want to see each
> command first.
>
> A delegated session is an ordinary agent session with the same `muqun`
> tools, so it can delegate again. P1 does not bound that recursion.

### What pane binding is

A pane-bound token says which pane's assistant made a call, so its work is
attributed to that pane and only it can wait on or cancel that work. It is
attribution, not isolation: any process running as the same user that can
read the gateway's `pairing.json` can mint a token for any pane, and every
OpenCode session shares the one agent-bound principal `agent:opencode`.

## Endpoint and authentication

`POST /mcp` on the gateway's own listener (MCP Streamable HTTP, stateless, JSON
responses; MCP 2026-07-28, older handshakes are served per request too). A
request is checked in this order and refused at the first failure:

1. The peer address is loopback (`127.0.0.1`, `::1`). Otherwise
   `403 mcp_loopback_only`.
2. The request carries none of the proxy headers `Forwarded`,
   `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Real-IP`,
   `Tailscale-User-Login`. A request relayed by a reverse proxy or Tailscale
   Serve arrives from loopback too; these headers tell it apart. Otherwise
   `403 mcp_loopback_only`.
3. An `Authorization: Bearer <token>` header. Otherwise `401`.
4. The token resolves to a live assistant token. An expired one is
   `401 token_expired`; an unknown or revoked one is `403 invalid_token`; a
   store failure is `500 token_store_unavailable`.

An assistant token authorises `/mcp` and nothing else, and a paired device's
token is not an assistant token. Each call is served for the session its token
resolves to, and every task it creates is owned by that session: it can read,
wait for and cancel its own tasks, and another assistant's task id answers
`task_not_found`.

## Tokens

Tokens are stored as hashes; the token itself exists only in the response that
issued it.

- **Pane-bound** (Claude Code, Codex, Pi, and OpenCode run by hand). The host
  runs `muqun-gateway mcp token-helper` before connecting. The helper sends its
  own process ancestry to the gateway (`POST /api/mcp/tokens`, authenticated
  with the local admin token), the gateway finds the terminal pane whose shell
  is the nearest ancestor, and answers with a token bound to that pane. It
  lives 7 days, and the gateway revokes it within 60 seconds of the pane
  closing. A revoked or expired token row is deleted 7 days later. The helper prints `{"Authorization": "Bearer <token>"}`, or only the
  token with `--bearer`. Run outside any managed pane it exits 2 with a message;
  with MCP disabled it says so and exits 1; with the gateway down or refusing,
  it exits 1 with one fixed sentence (`MUQUN_LOG=debug` shows the details).
  `--config-dir <dir>` names the gateway's config directory; `mcp enable`
  writes it into the helper command whenever that directory is not the
  default, because hosts may run the helper without your environment (Codex
  does), and `MUQUN_GATEWAY_CONFIG_DIR` would then be lost.
- **Agent-bound** (the OpenCode service the gateway starts). The gateway issues
  a token bound to the agent `opencode` that does not expire (`expires_ms` is
  `null`), hands it to the service with `opencode service set
  env`, and also sets it in the service's process environment, as
  `MUQUN_MCP_TOKEN` and `MUQUN_MCP_URL`. The next OpenCode start the gateway
  makes replaces it (the old one is revoked only after the start worked), and
  `mcp revoke` ends it.
- **OpenCode run by hand** is not pane-bound by itself: it reads
  `MUQUN_MCP_TOKEN` from its environment. From inside a pane the gateway
  manages, `export MUQUN_MCP_TOKEN=$(muqun-gateway mcp token-helper --bearer)`
  before starting it; the token is then bound to that pane.

When a token expires, `/mcp` answers 401 so the host fetches a new one:

- Claude Code runs `headersHelper` on every connection and, when a tool call
  returns 401 or 403, re-runs it, reconnects and retries the call once
  (Claude Code MCP documentation).
- Codex caches the helper's headers per connection and, after a 401 or 403,
  runs `http_headers_helper` again once and retries if the headers changed
  (Codex MCP documentation).

`MUQUN_MCP_URL` and the default URL `mcp enable` uses are
`http://<listen host>:<port>/mcp`, with `127.0.0.1` for a wildcard listen
address (`0.0.0.0`, `[::]`). `/mcp` answers loopback peers only. A gateway
bound to one non-loopback address (a tailnet IP, say) therefore also listens
on `127.0.0.1` and `::1` for `/mcp` alone -- the configured port when it is
free there, else any free port -- and that is the URL every agent it starts
is given (Claude Code's `--mcp-config`, Codex's overrides, OpenCode's
registration), resolved by the running gateway at each launch. On the
configured port nothing is recorded: every reader derives it. Only when
that port is taken on loopback does the gateway record the other one in
`<state>/mcp-endpoint` (written atomically, 0600), and it removes the file
when it stops (SIGTERM/SIGINT or a normal exit). `mcp enable` reads it, and
`manage` shows the address on the overview as `MCP: 127.0.0.1:<port>`. The
configured address keeps refusing `/mcp` (`403 mcp_loopback_only`).

Identity comes before credentials: `muqun-gateway mcp token-helper` first
asks the loopback address for the public `GET /api/capabilities`, with no
credential, and compares its `serverId` with this gateway's own. Anything
else answering there -- a process that took a stale port -- gets nothing:
the helper prints `another process answers on 127.0.0.1:<port>; not sending
the token` and exits 1, so the host never sends a token there.

`muqun-gateway mcp revoke --all` or `--id <token-id>` revokes tokens from the
CLI; a running gateway sees it on the next call. Paired devices can also
`GET /api/mcp/tokens` (the list never contains tokens) and
`DELETE /api/mcp/tokens/{id}` (`204`, or `404 token_not_found`).

## Tools

Every result is one JSON text block of at most 16 KiB; a larger result loses
the tail items of its largest array first, then has its longest strings
clipped (keeping their ends), and carries `"truncated": true`. A
`tasks.read` timeline page keeps the newest items that fit about 12 KiB and
says `"truncated": true` when it left older ones out. `tasks.wait` and
`exec.run` block for at most 45 seconds. Errors are tool errors whose text is
`{"error": "<code>", "message": "..."}`.

| Tool | Arguments | Result |
|---|---|---|
| `fleet.list` | none | `{"machines": [{machineId, label, os, online, suitableFor, tasksActive}]}`: this computer first, then every linked computer (`machineId` is the id it gave for itself, which `fleet.describe` takes; `label` is the name this gateway gives it; `online` whether it is reachable, `null` while it was not checked in the last minute; `os`, `suitableFor` and `tasksActive` come from its cached manifest, empty or 0 while none was fetched). Answered at once from what this gateway last heard (see [Linked computers](#linked-computers)). List the computers this gateway can hand work to: this one and every linked one, with OS, what each is suitable for, whether it is reachable and how busy it is. |
| `fleet.describe` | `machine?`: `"self"` (default, any case), this computer's `machineId` or label, or a linked computer's name (any case) or `machineId` from `fleet.list`; a linked computer's name wins over this computer's label | A compact manifest of that computer: `GET /api/fleet/self`'s fields, but each agent is only `{id, kind, status, version, modes, modelCount}` (mode ids, no model catalog) and each terminal backend `{sessionId, label, kind, connected, version}`. For this computer it also carries `peers`: one `{name, reachable, lastSeen, capabilities, agents, suitableFor}` per linked computer, answered at once from what this gateway last heard (`reachable` is `null` while it was not checked in the last minute; see [Linked computers](#linked-computers)). Naming a linked computer answers its cached manifest at once when it is known to answer; only a computer not checked lately, or with no manifest yet, is waited on: one check and, when it answers, one manifest fetch. What a computer offers: its agents, terminal backends, toolchains with versions, task counts and what it is suitable for. Without machine, this computer plus a short entry per linked computer (peers). |
| `exec.run` | `cwd`: absolute directory; `command`: shell command, run as `sh -c <command>` (at most 16 KiB); `timeout_s?`: 1..=45, default 30 | `{task_id, state, exit_code, stdout_tail, duration_ms, pane}`. Run a shell command in a visible terminal pane on this computer and return its exit code and output tail. Blocks up to timeout_s (max 45); if still running, returns a task_id for tasks.wait. `exit_code` is null while it runs; `pane` is `{session_id, pane_id}`. |
| `tasks.delegate` | `goal` (at most 32 KiB); `cwd?`: absolute directory; `agent_id?` (default: the primary agent; a terminal agent kind such as `codex` or `claude` starts it in a new pane instead, see [Delegating to a terminal agent](#delegating-to-a-terminal-agent)); `mode?` (for example `build` or `plan`); `model?`: a model id from the agent's catalog for `cwd` (for example `claude-haiku-5-5`), or `<provider_id>/<model_id>` to pick one provider; default: the agent's default. An unknown id is `invalid_arguments` and nothing is created; `variant?`: the model's variant (reasoning effort, for example `high`), applied when the session is created so the first turn uses it; applied only together with `model` (ignored otherwise), and must be one of the model's catalog variants when it lists any (else `invalid_arguments`); `machine?`: `"self"` (default, any case), this computer's `machineId` or label, or a linked computer's name (any case) or `machineId` -- a linked computer's name wins over this computer's label, and `self` always means this one; `cwd`, `agent_id`, `model` and `variant` are then checked by that computer (`~` allowed in `cwd`) | A task view (see [Tasks API](#tasks-api)), filed under the caller's task when the gateway can tell which one it is (see [Task tree](#task-tree)); for a linked computer, a mirror of the task there (see [Delegating to another computer](#delegating-to-another-computer)), with a `note` when the request got no answer. Start a sub-task on an agent, on this computer or (machine) on a linked one: a new session in cwd with your goal as the first prompt (for a terminal agent such as codex or claude, a new terminal pane running it). Returns task_id. If the work will take long, end your turn after delegating; you will be told when it finishes. Use tasks.wait only for short jobs. One hop only: a task another computer handed here cannot hand work to a linked computer (delegation_loop). |
| `tasks.wait` | `task_id`; `timeout_s?`: 1..=45, default 30 | The task view, plus `stdout_tail` for a command, and `answer` for a session delegation that is done or `idle_unconfirmed`: the agent's last message in full (`summary` holds only its first 240 characters, on one line). Returns once the task is done, failed, cancelled, `waiting` or `idle_unconfirmed` (its agent stopped without saying it is finished), or when the time is up. Wait up to timeout_s (max 45) for a task to reach done/failed/waiting, or idle_unconfirmed (its agent stopped without completing). Returns the current state and summary. If the work will take long, end your turn after delegating; you will be told when it finishes. Use tasks.wait only for short jobs. |
| `tasks.read` | `task_id?`: default, the task the calling session works on; `what`: `summary` or `timeline`; `cursor?` (timeline only) | `summary`: the task view. `timeline`: `{task_id, items, cursor}`, the most recent agent items after `cursor`; pass the returned `cursor` to read on. Read a task you created, or (without task_id) the one this session was given: its summary or recent timeline. Your own task's summary lists the computers the user named for it (`mentions`). A command has no timeline. The session's own task is found as `tasks.complete` finds it (see [Finishing a delegation](#finishing-a-delegation)); none found is `task_not_found`. |
| `tasks.cancel` | `task_id` | The task view after the cancel. Interrupt a task you created. |
| `tasks.complete` | `summary`: what was done -- or, blocked, why the agent stopped -- one line (non-empty, at most 32 KiB; stored cut to 240 characters); `outcome?`: `done` (default) or `blocked`; `task_id?`: default, the task the calling session works on | The task view: `done` with that summary, or, for `outcome: "blocked"`, `waiting` with the reason as its `summary` (see [Blocked](#blocked)). Say how the task you were given ended: finished, or stopped because you need the user (a missing file, an unclear request, a permission you cannot get). Only the session working on the task may complete it (see [Finishing a delegation](#finishing-a-delegation)); any other task, none found, or a task that has already ended is `task_not_found`, whose message tells the agent to print the `MUQUN_TASK_DONE: <one-line summary>` (or `MUQUN_TASK_BLOCKED: <one-line reason>`) line instead. A task already `waiting` that is blocked again is answered as it stands. |

Error codes:

| Code | When |
|---|---|
| `invalid_arguments` | A bad argument: an empty or oversized `command`, `goal` or `summary`, a `cwd` that is not an existing absolute directory, an unknown `model` for `tasks.delegate`, `tasks.read` timeline of a command. |
| `task_not_found` | No task with that `task_id`, or it belongs to another assistant. |
| `machine_unknown` | `machine` names neither this computer nor a linked one; the message lists the names that work. |
| `machine_unreachable` | `fleet.describe` or `tasks.delegate` named a linked computer that does not answer now; nothing was started there. |
| `delegation_loop` | `tasks.delegate(machine)` named a linked computer from a task another computer handed here (itself or an ancestor `origin: "peer"`): one hop only, so two computers never hand a goal back and forth; nothing is created. |
| `delegations_disabled` | `tasks.delegate` named a linked computer whose owner switched off work from other computers. |
| `unauthorized` | `tasks.delegate` or `fleet.describe` named a linked computer that no longer takes this one's token ("mini no longer accepts this computer; link again from your phone"); the link is kept, marked `unauthorized`, until the owner's phone links it again. |
| *the computer's own code* | `tasks.delegate`: the linked computer refused the task (for example `directory_not_found`, `agent_unavailable`); an argument it refused (`invalid_request`, `unknown_model`, `unknown_variant`) is `invalid_arguments`. The message names the computer. |
| `manifest_unavailable` | `fleet.describe` named a linked computer that answers but gave no manifest, and none is cached (for example an older gateway). |
| `agent_unavailable` | The requested agent is not available on this computer. |
| `no_terminal` | `exec.run` found no reachable terminal session. |
| `internal` | Anything else; the details are in the gateway log. |

## Host setup

An agent gets the muqun server one of two ways:

1. **The gateway hands it to the agents it starts**, at launch, without
   writing any of the user's files (`mcp.inject_into_agents`, on by default;
   off with it or with `mcp.enabled: false`; "Give started agents the muqun
   tools" in `manage` › Settings):
   - Claude Code starts with `--mcp-config <state dir>/mcp-claude.json`: the
     same entry `mcp enable claude` writes (URL and token helper, no token),
     added to the user's own servers (`--strict-mcp-config` is never used).
   - Codex starts with `-c mcp_servers.muqun.url="…"` and
     `-c mcp_servers.muqun.http_headers_helper="…"`, which set those keys for
     that run only and leave the user's other servers alone.
   - OpenCode, a running service, is given the server through its runtime
     API before a delegation (`PUT /api/experimental/mcp/muqun` for the
     task's directory, an agent-bound token in its `Authorization` header,
     Code Mode off so `tasks.complete` is a direct tool) when it is not
     already connected; it lasts until the service restarts and is given
     again on the next delegation; each directory has its own token, and
     with injection off they are revoked at startup. The service is shared,
     so every OpenCode session in that directory sees the tools -- phone chats
     and the user's own sessions too. None is told it is a task: all get the
     conditional wording, and `tasks.read` without a task_id tells a session
     without a task to ignore the protocol.
   - T3 Code and DeepSeek take no MCP server: their tasks end
     `idle_unconfirmed`, or on a marker.

   How a task's agent learns to report its end comes three ways, none of
   them in the conversation: an OpenCode session created for a task carries
   the sentence below as a per-session instruction entry
   (`PUT /api/experimental/session/{id}/instructions/entries/muqun-task`,
   set before the goal, removed when the task ends -- read from the task's
   binding, so a gateway restart in between does not leave it; the "instructions
   updated" notice it leaves is not served; an OpenCode without the route
   leaves a note on the task's dispatch event and the session gets only the
   conditional server instructions); any host that reads them gets the
   server's `initialize` instructions; Claude Code and Codex, which do not act on those, are given
   one sentence out of band at launch -- "You were started for a Muqun task:
   when it is finished call the muqun tool tasks.complete (outcome blocked if
   you need the user)." -- Claude Code through
   `--append-system-prompt-file <state dir>/task-instruction.md` (0600; left
   out when `agent_launch_args.claude` already appends a system prompt),
   Codex through `-c developer_instructions=<the user's own
   developer_instructions, read from their config.toml and never written,
   then the sentence>` for that run (only the top-level value is read: a
   Codex launched with `-p <profile>` has that profile's
   `developer_instructions` replaced by this for the run). Only a Claude Code seen to start that
   way and gone again at once, its pane back at a shell with no Claude Code
   left under it, is started once more with the sentence typed after the
   goal; a start that times out is never retried. The launch line shows in
   the pane as any terminal's does.
2. **The user registers it** with `mcp enable` below, for agents they start
   by hand.

`muqun-gateway mcp enable <host>` points a host at the gateway. `--url`
overrides the default `http://127.0.0.1:<port>/mcp`; `--project <dir>` is
where a project file is written. `muqun-gateway mcp disable <host>` removes the
`muqun` entry again and touches nothing else. No entry ever contains a token.

| Host | `enable` writes (with `--project <dir>`) | `enable` prints |
|---|---|---|
| `claude` | `<project>/.mcp.json`: `mcpServers.muqun` with `"type": "http"` and `"headersHelper": "muqun-gateway mcp token-helper"` | Without `--project`: the `claude mcp add-json --scope user muqun '<entry>'` command. |
| `opencode` | `<project>/opencode.json`: `mcp.muqun` as a remote server whose `Authorization` header is `Bearer {env:MUQUN_MCP_TOKEN}` | Without `--project`: the entry for `opencode.json` in OpenCode's user config directory. It also says the gateway sets `MUQUN_MCP_TOKEN` only for an OpenCode service it starts. |
| `codex` | Nothing | A `[mcp_servers.muqun]` table for Codex's `config.toml`, with `url` and `http_headers_helper = "muqun-gateway mcp token-helper"`. |
| `pi` | Nothing | `{"mcpServers": {"muqun": …}}` for `~/.pi/agent/mcp.json` (all projects) or `.pi/mcp.json` (one project, once trusted), whose `Authorization` header is the command `!echo Bearer $(muqun-gateway mcp token-helper --bearer)`. |

With a non-default config directory, every helper command above reads
`muqun-gateway mcp token-helper --config-dir <dir>`. Running `enable` twice
changes nothing the second time.

## Tasks API

Tasks are the work the gateway tracks: an agent session an assistant or the
phone delegated a goal to, or a command run through the gateway. A paired
device sees all of them. Announced as the `task_ledger` capability;
`POST /api/tasks`, `?root=` and the tree fields of a task view as
`task_ledger_create`.

A delegation follows its agent session as the agent reports it: when the
session finishes a turn (idle after it was busy) the task is `done` if the
agent's last message carries the completion marker, `waiting` if it carries
the blocked marker, and `idle_unconfirmed` otherwise (see [Finishing a
delegation](#finishing-a-delegation)); a paused task whose session runs
again is `working` (a [blocked](#blocked) one only on a later turn). The paused task's `summary` is
the last assistant message as a hint, on one line and cut to 240
characters, whichever agent it is (the agent's own timeline read first; when
that holds no answer written since the task began -- DeepSeek announces the
end of a turn from its live stream before its page has the final message --
the gateway's copy of the streamed timeline is read); when the session fails the task fails, with the error
(`<name>: <message>`, one line) as its `summary`; an interrupted session
cancels it; a pending permission or question makes it `waiting` until it is
answered. These moves are recorded with the source `task_follower` and
published at once. A command's `summary` is its output tail, as before.

Tasks are reconciled at startup and every 60 s, so a task moves even when
no assistant waits on it: a command whose pane is gone fails with summary
`pane closed`; a command whose pane is there gets one read of its exit
marker (and completes, closing an exit-0 tab, if it has exited); a
delegation whose agent no longer has the session fails with `session gone: <the agent's error>`;
a delegation whose session failed fails with its error line as `summary`;
a delegation whose session went idle after the task was created is `done`
when the agent's last message carries the completion marker (read first, so
a turn that ended while the gateway was down is settled by it), `waiting` when
it carries the blocked marker, and `idle_unconfirmed` with the last assistant
text as a hint otherwise (a [blocked](#blocked) task stays `waiting`); a
paused delegation whose session runs again is `working` (a blocked one, on a
turn after the one that said so); a task still bound to nothing
10 minutes after it was created (the gateway stopped before dispatching it)
fails with `never dispatched`. A task whose terminal backend
or agent cannot be reached is left as it is.

All routes need a device token. `{id}` is the `task_id` of a task view; an
unknown id is `404 task_not_found`; any other failure is `500 task_failed`.

| Route | Answer |
|---|---|
| `GET /api/tasks?state=a,b&limit=50` | `{"tasks": [TaskView]}`, newest update first. `state` is a comma list of the states below; an unknown name is `400 invalid_request`. `limit` defaults to 50 and is capped at 200. |
| `GET /api/tasks?root=<task_id>` | The same list, of that task and every task below it (its children, theirs, and so on); `state` and `limit` apply too. An unknown id lists nothing; an empty `root` is `400 invalid_request`. |
| `POST /api/tasks` | Opens a delegation from the device; see below. |
| `GET /api/tasks/defaults` | `task_defaults`. `{"default_cwd": string \| null}` for the calling device: the directory a delegation it opens without `cwd` runs in -- its newest task's directory while that is still a directory, else the home directory -- named from `~` when under home (`~`, `~/<path>`; `null` only when the task store cannot be read). Any query string is ignored. The task WebSocket subscription carries the same answer as a `defaults` frame (see [Agent API](agent-api.md)). |
| `GET /api/tasks/{id}` | `{"task": TaskView, "events": [TaskEvent]}`; `events` is the task's state history, oldest first. |
| `POST /api/tasks/{id}/cancel` | The `TaskView` after the cancel. Interrupts the agent or sends Ctrl-C to the command. Cancelling a finished task changes nothing. |
| `POST /api/tasks/{id}/complete` | The `TaskView`, now `done`. The user confirms that a delegation opened on a device (`origin: device`; any paired device may confirm it, as any may cancel it) that is `idle_unconfirmed` or `waiting` is finished. Body `{"summary"?: string}`, optional (the body may be left out): the new `summary`, one line, cut to 240 characters; without it the task keeps the hint it had. Published as `task.updated`. A task already `done` is answered as it stands; any other state or origin is `409 task_not_confirmable`; a body that is not that JSON object is `400 invalid_request`. |

`TaskView`: `task_id`, `state`, `kind` (`delegation` or `command`), `goal`
(the delegated goal or the command line), `summary`, `exit_code`, `binding`,
`updated_ms`, and:

- `parent_task_id`: the task this one was started from, or `null`.
- `origin`: `device` (opened with `POST /api/tasks`), `agent` (started by work
  the gateway tracks: a session of the agent service the gateway runs, or an
  assistant in the pane of an open task) or `mcp` (any other MCP host). Tasks
  stored before origins existed read as `mcp`.
- `label`: the short title the requester gave, or `null`.
- `title` (`task_outcome`): what to call the task, never empty: its
  `label`; else, for a session delegation, the first sentence of the name its
  agent session gave itself (not used while the session is untitled -- empty
  -- nor when it is the asid, or the goal restated or cut short); else the
  goal's first sentence (up to the first `.`, `?` or `!` followed by a space,
  a `.` running one sentence into the next as in `done.Next`, a `。`, `？` or
  `！`, or a line end; a goal that starts with a stop gives its first line).
  At most 60 characters, ending in `…` when cut: cut back to a word
  boundary when that keeps at least 30 characters, else cut hard at 60.
  An open task's follows the session's name as the agent renames it (a rename re-publishes the task;
  the name is held in memory, and the 60 s reconcile reads it again after a
  restart); when a session delegation ends, the name it had is stored with
  the task, which keeps it. A terminal agent's tab and pane are named the
  same way.
- `requested_by`: `{"kind": "device", "id": "<device id>", "name": "<device
  name>"}` for a device's task; `null` otherwise.
- `mentions` (present only when there are any): the computers the user
  `@`-named when opening the task (see [Opening a task from the
  device](#opening-a-task-from-the-device)), as this gateway names them, or
  `self`.
- `progress` (present only while the task is open and something is known;
  `task_ledger_complete`): what it is doing now, one line of at most 120
  characters -- for a session delegation the agent's current tool call in
  words (its title as the provider wrote it when that says more than its
  argument; else `<tool>` and the argument it acts on -- a file, a patch's
  files included, else a command's first line, a pattern, a query or a url
  -- as inline code, since the app reads the line as markdown and a path
  like `src/__tests__/a.ts` must not turn bold; a title that only restates
  the argument, as OpenCode's adapter derives them, counts as the argument;
  the argument is cut with `…` inside its backticks so the line fits; a
  failed call with its error; never a bare tool name -- a call whose
  arguments name nothing readable, or have not streamed in yet, leaves the
  line before) or
  the start of its latest message, for a terminal agent the pane's last prose
  line that is not decoration (rules, box drawing, the input prompt,
  spinners, footer hints such as "bypass permissions on"). Every line is kept
  as it comes; `task.updated` for it is published at most once every 2 s per
  task, and a line held back is published when the 2 s are up (a state change
  publishes it at once). It is kept in memory, so after a gateway restart it
  is absent until the agent moves again, and it is dropped when the task
  pauses (`idle_unconfirmed`, `waiting`) or ends: the `summary` then says
  why -- for a `waiting` task, what the user is asked (a pending request's
  question, a terminal agent's menu line, or a blocked agent's reason).
- A `failed` task's `summary` is the error as the agent, provider or backend
  gave it: `<name>: <message>` from the session, else the session's last
  failed tool call, status line or message (`the agent session failed:
  <detail>`), `session gone: <the agent's error>`, `the agent exited before it
  finished: <last screen line>`, `the pane closed: <last screen line>`, a
  launch step's `<step>: <message>`, a command's `exit <code>` and output
  tail, or `the command could not be typed: <error>`.
- `children`: `{"total", "active", "waiting", "failed"}`, the task's direct
  children by state, counted when the view is made (`active` is every state
  that is not finished and not `waiting`; `done` and `cancelled` count only in
  `total`).

`TaskEvent`: `task_id`, `seq`, `at_ms`, `from`, `to`, `source`, `note`.

### Opening a task from the device

`POST /api/tasks` with a device token does what `tasks.delegate` does: it
creates a session on the agent, sends the goal as its first prompt, and
records a `delegation` task bound to that session, with `origin: "device"`,
the device as its owner and the device's name in `requested_by`.

```json
{ "goal": "fix the failing test", "agent_id": "opencode", "cwd": "~/Work/app",
  "mode": "build", "model": "anthropic/claude-haiku-5-5", "label": "fix test" }
```

Only `goal` is required (non-empty, at most 32 KiB). `agent_id` defaults to
the primary agent; `cwd` must be an existing absolute directory (`~`
expands). When omitted, it uses the cwd of the most recent task opened by
the same device, or the home directory if that device has no previous task.
`model` is a model id from the agent's catalog or
`<provider_id>/<model_id>`; `variant` is that model's variant (reasoning
effort, for example `high`), applied when the session is created so the first
turn already runs with it -- it is applied only together with `model` (ignored
otherwise) and must be one of the model's catalog variants when the catalog
lists any; `label` is at most 200 characters; `mentions` names the
computers the user `@`-named in the goal: at most 8, each a linked
computer's name (any case) or `self`. They are stored on the task as this
gateway names them at that moment (a later rename of a computer does not
change the tasks that named it), once each in the order given, and shown as the task
view's `mentions`; the goal is sent as it is, and the agent finds them with
`tasks.read` of its own task.
Fields the gateway does not know are ignored. The answer is
the task view plus the session to open:

```json
{ "task_id": "t_…", "state": "working", "kind": "delegation", "goal": "fix the failing test",
  "summary": null, "exit_code": null,
  "binding": { "kind": "agent_session", "agent_id": "opencode", "asid": "ses_…" },
  "updated_ms": 1760000000000, "parent_task_id": null, "origin": "device", "label": "fix test",
  "requested_by": { "kind": "device", "id": "dev_…", "name": "Pixel" },
  "children": { "total": 0, "active": 0, "waiting": 0, "failed": 0 },
  "asid": "ses_…" }
```

| Error | When |
|---|---|
| `400 invalid_request` | An empty or oversized `goal`, a `label` over 200 characters, an empty or oversized (256 bytes) `agent_id`/`mode`/`model`/`variant`, or `mentions` that is not a list of strings. Nothing is created. |
| `400 invalid_mentions` | `mentions` holds more than 8 names, or names a computer that is neither linked nor `self` (it may have been unlinked since the phone offered it). Nothing is created; the same body without `mentions` would be taken. |
| `400 invalid_directory`, `directory_not_found`, `not_a_directory` | `cwd` is relative, missing, or not a directory. Nothing is created. |
| `400 unknown_model` | `model` is not in the agent's catalog for `cwd`. Nothing is created. |
| `400 unknown_variant` | `variant` is not one of the resolved model's catalog variants (a variant without a `model` is ignored). Nothing is created. |
| `503 agent_unavailable` | That agent (or, without `agent_id`, any agent) is not attached. |
| `503 no_terminal` | A terminal agent was named and no terminal session answers. Nothing is created. |
| `400 invalid_request` (body) | The body is not a JSON object of the shape above. Authentication is checked first, so without a valid device token this is `401`/`403` whatever the body. |
| `502 agent_error` | The agent could not create the session or take the prompt; the task is recorded as `failed`, with the error as its `summary`. |

If the session fails (or finishes) while the prompt is being sent, the
answer is still `200` with the task as it then stands -- `failed` with the
error line as `summary`, or `done` -- since the task and its session exist.

### Delegating to a terminal agent

`agent_id` may also name a terminal agent the agent catalog lists (`GET
/api/agents/catalog`: `codex`, `claude`, ...) that is not a session agent
(`opencode`, `deepseek` and `t3` are session agents: unattached, they are
`503 agent_unavailable`, never started in a pane). Then, for `POST /api/tasks`
and `tasks.delegate` alike, the gateway opens a new tab in `cwd` in the first
reachable terminal session, names the tab and pane after the task (its
`title`, see [Tasks API](#tasks-api)), starts the agent in it
exactly as `POST /api/sessions/{id}/agents/spawn` does (its bypass launch
arguments, `agent_env`, folder trust for `cwd`) and gives it the goal as its
first prompt. Claude Code and Codex on a tmux session get it on their command
line, `claude <launch args> -- '<goal>'` and `codex <launch args> -- '<goal>'`
(one shell word, quoted so that sh, bash, zsh and fish all read it back
unchanged: newlines, quotes, `$` and backticks are data, never shell syntax).
That line is typed at the pane's shell, so the goal ends up in that shell's
history and, while the agent runs, in the local process list.
A goal with a backslash or any other control character (a Tab, an Esc, ...), any
other agent, and every agent on Herdr (whose start waits for an idle agent)
has it typed once the agent is up instead. Folder trust: Codex gets a `-c
projects={...}` override for this run (only for directories inside the
user's home); for Claude Code,
`projects["<cwd>"].hasTrustDialogAccepted` is set in its user config
(`$CLAUDE_CONFIG_DIR/.claude.json`, from `agent_env` or the gateway's
environment, else `~/.claude.json`): merged in place, key order kept, only
for a directory inside the user's home named without `..`, skipped when the
file cannot be read. Neither agent's folder trust is set with
`"agent_trust_folders": false` in `config.json` (see [Agent API](agent-api.md)). `cwd` defaults to the most recent
task directory for that device (or home if it has none) for `POST /api/tasks`,
and to wherever the backend opens the tab for `tasks.delegate`; it is not held
to the spawn route's "a directory the session already has open" fence, as no
other `POST /api/tasks` directory is. `mode`, `model` and `variant` do not
apply and are ignored.

The task is a `delegation`, bound to the pane:

```json
"binding": { "kind": "pane", "session_id": "default", "pane_id": "w1:p9",
             "tab_id": "w1:t9", "shell_pid": 4242, "agent_id": "codex" }
```

The answer comes as soon as the pane exists: the task is `dispatching`, and
the answer of `POST /api/tasks` carries `"pane_id"` (the pane to open)
instead of `"asid"`. The agent is started and given the goal in the
background (that can take up to half a minute); the task then moves to
`working`, or to `failed` with a `summary` naming the step (`agent: …`,
`prompt: …`, or `the agent start failed: …` if the start itself broke),
published as `task.updated`. A typed goal (not one given on the command
line) must leave the agent's input
box: the pane is read every second, Enter is pressed once more after 8 s,
and a goal still sitting there 8 s later fails the task with `the goal was
not accepted by the agent` (a pane with no input box drawn as rules -- Claude
Code's -- cannot be told and counts as sent). A task cancelled (or failed) while its agent
starts is not typed into. A command task's pane binding has
no `agent_id`, so `binding.kind == "pane"` with an `agent_id` is how a client
tells a terminal agent's task from a command. A terminal agent that is not on
`PATH` is `503 agent_unavailable`, and no reachable terminal session `503
no_terminal` (the same codes over MCP), with nothing created; a pane that
could not be made is `502 agent_error`, the task recorded `failed`.

An idle terminal agent is not a finished one -- it pauses to think, run a
tool or wait on a long build, and comes back -- so only an explicit signal
finishes the task. The agent is given its goal alone (the stored `goal`
is the goal as given); how to finish is declared by the MCP server, not
written into the conversation (see [Finishing a delegation](#finishing-a-delegation)).
A marker line the agent prints
stays in its pane: a terminal shows what was written. The marker is looked for in the
pane's recent output (its last 200 lines, unwrapped), not only on its screen,
so one that scrolled off still counts. It counts when `MUQUN_TASK_DONE:`
starts a line (after any bullet or markup the agent draws) or follows the
agent's own words on it (`Done.MUQUN_TASK_DONE: …`; not inside a word, not
quoted after a backtick or quote mid-sentence), runs to the line's end
(that rest is the summary), lies below the last line that echoes the
goal's end (the shell's launch line and the agent's echo of the goal; nothing
in the goal can complete its task, and a marker whose text the goal holds
never counts), carries a summary that is not the placeholder or a piece of
it (`<…`, `…`), and the agent is not working, starting or blocked at that
poll. The task is then `done` with that summary (one line, at most 240
characters). The blocked marker, `MUQUN_TASK_BLOCKED: <reason>`, is read by
the same rules (the last marker line of either kind counts) and makes the
task [blocked](#blocked).

The task follows the pane as the terminal backend reports it (Herdr's own
agent state; on tmux, the state read off the screen), polled every 2 s (every
12 s while it is `idle_unconfirmed` or blocked, backing off to every 60 s
while nothing changes), recorded with source `pane_follower` (a blocked
marker: `marker_blocked`) and
published as `task.updated` like every change. The marker is checked first on
every poll:

| The pane | The task |
|---|---|
| the marker, as above | `done`, `summary` = the marker's summary |
| a blocked marker not acted on before, the agent not busy | `waiting`, `summary` = the reason |
| blocked: the agent idle, or still busy before it was seen idle on two polls | stays `waiting` |
| blocked: the agent working after it was seen idle on two polls (the user answered) | `working` |
| the agent working | `working` (from `dispatching`, or back from `waiting` or `idle_unconfirmed`); an agent still starting, or idle, while its task is `dispatching` (within the start grace) leaves it there. Claude Code counts as working only with its `esc to interrupt` footer on screen or a new `⏺` row since the poll before |
| the agent blocked on a menu: an approval or a question | `waiting`, `summary` = the screen's last meaningful line (as for a pause's hint) |
| the agent idle on two polls in a row, after it was seen working (or after its Herdr state sequence moved), or 30 s after the start | `idle_unconfirmed`, `summary` = a hint: the agent's last prose block when the gateway has a transcript dictionary for it (Claude Code, Codex, ...), else the screen's last line with any letter or digit in it |
| no agent running in the pane, on two polls in a row, and no marker (a `dispatching` task's agent is given its start timeout plus 15 s first) | `failed`, `summary` = `the agent exited before it finished: <last screen line>` |
| the pane is gone (or another shell holds its id), and no marker was read before | `failed`, `summary` = `the pane closed: <the last screen line read>` |

`idle_unconfirmed` is a pause, not an end: the task stays open, and moves back
to `working` when the agent runs again. The backend exposes no assistant
message and no exit code for a pane agent, so there is no `answer` from
`tasks.wait` and no `exit_code`, and an agent that leaves its pane without
printing the marker fails whatever its exit status. The marker is checked on
the follower's first poll too, so after a gateway restart a task whose agent
finished meanwhile is settled `done` at once. An assistant in the pane that
holds the pane's own token can also call `tasks.complete` (see [Finishing a
delegation](#finishing-a-delegation)).

`tasks.wait` on such a task waits for that follower to record `done`,
`failed`, `cancelled` or `waiting`; `tasks.read` has no timeline for it
(`invalid_arguments`); cancelling sends the agent its interrupt key (the key
the pane interrupt route sends) and marks the task `cancelled`, leaving the
pane open. The startup and 60 s reconcile restarts the follower of such a task
when none runs, so a gateway restart does not orphan it. An assistant running
in the pane that calls `exec.run` or `tasks.delegate` with a pane-bound token
is filed under this task (see [Task tree](#task-tree)).

There is no separate rate limit, as for the other device routes that start
work (`POST /api/agent-sessions`, `POST /api/sessions/{id}/tasks`).

### Finishing a delegation

An agent that stops is not finished: it may be asking something, have
stopped early, or go on after a tool round. So a delegation, to an agent
session or to a terminal agent, is `done` only on one of three explicit
signals:

1. **`tasks.complete`**, called by the agent working on the task with a
   one-line `summary`. Ownership: a pane-bound token completes only the open
   delegation bound to its own pane; an agent-bound token (shared by every
   session of the agent service) completes only the open delegation to that
   agent whose session is running this very `tasks.complete` call, with the
   same arguments -- the rule [Task tree](#task-tree) uses for parents. A
   `task_id` narrows the choice and must name that task. Anything else is
   `task_not_found`. Recorded with source `tasks_complete`.
2. **The marker** `MUQUN_TASK_DONE: <one-line summary>` on a line of the
   agent's final message (a session) or of its pane's output (a terminal
   agent) -- at the line's start or right after the agent's words on it, not
   inside a word or quoted mid-sentence -- and not the placeholder or a piece
   of it. The summary is what follows the marker to the line's end. The
   lines the ledger shows of its own (an idle task's hint, a question,
   `progress`) never carry marker text. Recorded with the follower's source.
3. **The device**, `POST /api/tasks/{id}/complete`, for a task it opened that
   is `idle_unconfirmed` or `waiting` ("确认完成" on the phone). Recorded with
   source `device_confirm`.

Every delegation is given its goal alone. How to say it ended is
declaration-level knowledge, never conversation text: the MCP server's
`initialize` answer carries `instructions` written for the caller. A
pane-bound token whose pane holds an open task is told plainly, first and
imperatively, that it was started for one task and must call
`tasks.complete` as its last action (outcome `blocked` when it needs the
user; a marker line if it cannot call it). Any other caller -- an
agent-bound token is shared by every session of its service, task or not --
is told the same only on condition: if this session was started as a Muqun
task, which `tasks.read` without a task_id answers (the session's task, or
"this session has no Muqun task: ignore the task protocol"). `tasks.complete`
from a session without a task answers `task_not_found`: not a task, do not
call again, print no marker. Both also say that `tasks.delegate(machine)`
sends work to a linked computer and that the task's summary lists the ones
the user named (`mentions`). An agent
without the muqun tools (T3 Code, DeepSeek) is told nothing: its task ends
`idle_unconfirmed` for the device to confirm, unless it prints a marker of
its own accord. The machines a task mentions will not be written into its
goal either: the `tasks.read` of the agent's own task is planned to list
them (`mentions`, with the mesh's G5).

Sessions from before goals went alone still hold the completion
instruction this gateway used to send; the served transcript cuts it and the
consumed marker ([transcript rules](agent-api.md#task-plumbing-in-the-transcript)).

A session delegation moves like this (moves pass the states between, so a
`waiting` task that is completed passes `working`):

| The session | The task |
|---|---|
| the turn ends (idle after busy), the last message carries the marker | `done`, `summary` = the marker's summary |
| the turn ends, the last message carries the blocked marker | `waiting` ([blocked](#blocked)), `summary` = the reason |
| the turn ends without the marker | `idle_unconfirmed`, `summary` = the last message as a hint; a blocked task stays `waiting` |
| a permission request or question form is pending | `waiting` (back to `working` when answered), `summary` = what the user is asked: a permission request's prompt; for a question form, the last line of the agent's last message when that came after the turn's last tool call, else the form's title |
| busy again after a pause | `working`; a task blocked by `tasks.complete`, only once the gateway has seen its session go idle since the call (or when the gateway started after it, so never saw that turn); one blocked by the marker (read once its turn had ended), whenever the session runs again |
| the turn ends with the blocked marker while blocked | stays `waiting`, `summary` = the new reason |
| the session failed / was interrupted | `failed` with the error line / `cancelled` |

A question the agent asks in plain text, without a form the gateway can see,
cannot be told from a pause: it is `idle_unconfirmed`, and the question is
its hint -- unless the agent says it is blocked. `exec.run` command tasks are
not delegations: their exit code finishes them, as before.

### Blocked

An agent that stops because it needs the user -- a file that does not exist,
a request it cannot make sense of, a permission it cannot get -- says so
instead of finishing: `tasks.complete` with `outcome: "blocked"` and the
reason as `summary` (recorded with source `tasks_complete_blocked`), or the
line `MUQUN_TASK_BLOCKED: <one-line reason>`, read like the done marker
(source `marker_blocked`). The task moves to `waiting` with the reason as
its `summary`, so the phone never shows a finished task over a failure.

Blocked is not an end. The task stays open while the user answers in the
session or pane; a gateway restart keeps it (the task's last event says it
is blocked; a marker's event carries the reason as its note). While it is
blocked:

- the agent going idle, or a turn ending without the done marker, leaves it
  `waiting` -- it is never `idle_unconfirmed` for that;
- the rest of the turn that reported the block is not an answer: a session
  delegation blocked by `tasks.complete` goes back to `working` only on a
  turn after the gateway saw that one go idle (by the order of the agent's
  events, not its idle time stamp, which some agents never give; a gateway
  started after the block takes the next turn as the answer); one blocked by the marker (read
  only once that turn had ended) as soon as its session runs again; a
  terminal agent's only once it was seen idle on two polls and then works
  again;
- a new reason (another blocked marker or `tasks.complete` call) replaces the
  old one;
- the done marker or `tasks.complete` (outcome `done`) makes it `done`, and
  the device can still confirm (`POST /api/tasks/{id}/complete`) or cancel it.

A terminal agent's blocked line stays in the pane's output; once the task
has moved on, that line does not block it again -- a new reason does, and so
does the same reason printed again (the follower counts how many lines read
as it). A restarted follower takes the lines the task's history already
acted on as read. A pane that closes, or an agent that exits, right after a
blocked line fails with that reason: `the pane closed: <reason>`.

### Being told how a delegation ended

An agent need not poll a long delegation. `tasks.delegate` records the
session it was called from: a pane token's pane, or -- for the agent service's
shared token -- the one session of that agent running this very call right
now (found as for the [task tree](#task-tree), among all its sessions in a
turn, not only task sessions, since a person's own session delegates too).
When the child reaches `done`, `failed`, `cancelled`, `waiting` or
`idle_unconfirmed` and that session's turn is over, the gateway sends it one
plain line, as a user message would be:

- `mini finished: <summary>` (a session delegation on this computer: its
  last message, up to 4000 characters; `The task "<title>"` names a local
  child);
- `mini could not finish: <reason>` (failed, or `it was cancelled.`);
- `mini needs an answer: <question>` (`waiting`);
- `mini stopped without saying it is finished: <its last message>`.

A session still in its turn -- inside a `tasks.wait`, say -- is sent nothing:
the wait answers it, and a state a wait returned is never sent again. A pane
is told when its own delegation's follower saw the agent stop
(`idle_unconfirmed` or `done`) and that agent still lives in the pane (the
backend names it, it is the foreground program, or it runs under the pane's
shell); otherwise nothing is typed -- a pane back at its shell would run the
text -- and that state is given up, not retried. The line is sent the way a
goal is (the agent's prompt call, a bracketed paste on tmux, Enter once it
has settled). What the child wrote is untrusted, so the line is one line of
plain text: escape sequences and control characters removed, line breaks
and tabs turned into spaces, capped after that, and it always begins with
the computer's name or the task's title, never with `/`, `!` or `:`. Each state is told once, recorded in the task
store (`task_notices`), so a restart does not tell it twice; a state not told
within a day is dropped. When the calling session cannot be told apart (none
or several fit), nothing is recorded and the caller learns the result from
`tasks.wait` alone.

### Task tree

A task started through `exec.run` or `tasks.delegate` is filed under the
open task it was started from, when the gateway can tell which one that is.
A token names who is calling, not from which task, so the gateway looks:

- **Pane-bound token**: the open task bound to the caller's pane (an
  assistant running inside an `exec.run` pane).
- **Agent-bound token** (every session of the agent service the gateway runs
  shares it): among the open delegations to that agent, the one whose session
  is running this very call -- its latest timeline shows a *running* call of
  the same tool (OpenCode names it `muqun_tasks_delegate`, `muqun_exec_run`)
  whose arguments are the request's own: `command` and `cwd` for `exec.run`,
  `goal` for `tasks.delegate`, as the caller sent them.

When no task, or more than one, fits, the new task has no parent: a wrong
parent would misfile work, a missing one only leaves it at the top level.
So a session the gateway does not track, calling the same tool while a
tracked one waits in it, is not filed under the tracked task; two tracked
sessions running the identical call at the same moment give their children
no parent.

`children` is computed at read time. When a task with a parent changes, the
parent's view is published again too (with its `seq` unchanged), so a
client's roll-up stays current without refetching.

A command task is bound to its pane's shell process, not only to the pane id
(`binding.shell_pid` when the backend reports it): a pane that has since been
replaced by another shell under the same id (tmux reuses `%N` after a server
restart) counts as gone, so it is never read or interrupted and the task fails
with `pane closed`.

`GET /api/fleet/self` (capability `fleet`, device token) answers with this
computer's full manifest, model catalogs included; `fleet.describe` returns
the compact form described under Tools. Manifest fields are camelCase, like
discovery (the task API stays snake_case).

### Linked computers

Another of the owner's gateways that this one was paired with (capability
`fleet_peers`; see [Linked gateways](agent-api.md#linked-gateways-fleet_peers)).
There is no heartbeat, and `fleet.describe` and `fleet.list` never wait on
another computer: they answer with what this gateway last heard. A computer
neither heard from nor pinged in the last 60 s is `reachable: null` (not
checked yet); the call starts a check of it in the background -- one at a
time per computer -- so the next call knows (one that no longer knows this
gateway's token is marked `unauthorized` and `reachable: false`). After a check it is `reachable`
while its last answer is under 2 minutes old. Its manifest (`GET
/api/fleet/self` on it, compacted to the keys above: strings of at most 256
characters, numbers and booleans, each list at most 64 items) is asked for
again in the same background step at most once a minute, answered or not,
and kept in the peer row, so a computer that does not answer still
shows its last agents and `suitableFor`. `lastSeen` is ms since the epoch,
null if it never answered. Names match in any case (Unicode-aware); no
linked computer may be called `self`.

### Delegating to another computer

`tasks.delegate(machine: "mini")` runs the goal on that linked computer as an
ordinary task of its own (`origin: "peer"`, `requested_by.kind: "gateway"`,
named from the device record its token belongs to); this computer keeps a
mirror of it under the caller's task, `binding: {kind: "peer", peer_id,
peer_server_id, remote_task_id, peer_name}`.

- A computer not heard from in the last 2 minutes is checked first; if it
  still does not answer, `machine_unreachable` and nothing is created.
- One hop only: an agent working on a task another computer handed here
  (the task or an ancestor has `origin: "peer"`) gets `delegation_loop` when
  it names a linked computer; it may still delegate on this computer. This is
  best effort: it needs the caller's own task, found as for the
  [task tree](#task-tree); a call the gateway cannot tie to one task (none
  or several fit) is not checked.
- The mirror follows the task there over one task socket per computer
  (`subscribe_tasks`, the frames the phone reads), while any mirror of it is
  open; when the socket is down, the driver's pass (every 60 s) reads the
  task instead. State, summary (cut to one line), exit code and the progress
  line are copied; nothing moves backwards.
- `tasks.wait` on a mirror blocks as for a local delegation and wakes on
  every move. While the computer does not answer it returns at once with
  `peer_reachable: false` and `peer_last_seen_ms`: the mirror never fails
  because its computer went quiet, and the agent decides how long to wait.
- `tasks.cancel` on a mirror cancels the task there too (best effort) and
  the mirror here either way. `tasks.read` gives the summary; a mirror has no
  timeline here.
- A create that went out but got no answer leaves the mirror `dispatching`
  with an empty `remote_task_id`, and the answer carries a `note`: the task
  may have started there, so do not delegate it again. The mirror finds that
  task by the `client_ref` it sent with the create, or fails as not confirmed
  after 2 minutes.
- If the computer is unlinked here, or turns out to no longer know this
  one's token (unlinked or revoked there: it answers `401`/`403`), its open
  mirrors fail (view `reason: "peer_unlinked"`, a plain summary) -- the task
  there can never be read again, since a new link is a new login -- and the
  computer is marked `unauthorized`, not forgotten: the next delegation
  there answers `unauthorized` (never `machine_unknown`, which is for names
  never linked), and the phone's next link under its name replaces it. A mirror's
  `reason` field is described in [agent-api.md](agent-api.md#linked-gateways-fleet_peers).

### States

`queued`, `dispatching`, `working`, `waiting`, `idle_unconfirmed`, `done`,
`failed`, `cancelled`. The last three are terminal.

| From | May move to |
|---|---|
| `queued` | `dispatching`, `cancelled` |
| `dispatching` | `working`, `failed`, `cancelled` |
| `working` | `waiting`, `idle_unconfirmed`, `done`, `failed`, `cancelled` |
| `waiting` | `working`, `idle_unconfirmed`, `done`, `failed`, `cancelled` |
| `idle_unconfirmed` | `working`, `waiting`, `done`, `failed`, `cancelled` |

A cancel always ends the task as `cancelled`: interrupting the agent or the
command is best effort, and an agent or pane that cannot be reached does not
keep it open. A task that ended more than 30 days ago is deleted with its
events (not while a child of it is still open).

### Live updates

A socket on `GET /api/ws` that sent `subscribe_all` also receives every task
change, with no session:

```json
{"t":"event","asid":null,"seq":12,"event":"task.updated","data":{"task_id":"…","state":"working","…":"…"}}
```

A phone that only needs the task ledger can send `{"t":"subscribe_tasks"}`
when the gateway advertises `task_ws_subscription`.
After `{"t":"subscribed","tasks":true}`, it receives task changes as
`{"t":"task","asid":null,"seq":12,"event":"task.updated","data":<TaskView>}`
and no pane or agent-session events. If it falls behind, it receives
`{"t":"resync","asid":"","tasks":true}` and should refetch `GET /api/tasks`.
The initial `hello` and subscription acknowledgement still precede updates.

`data` is the whole `TaskView`, so the latest frame for a task replaces what
the client holds. A change to a task with a parent is followed by a frame for
the parent, whose `children` moved (its `seq` is the parent's own, unchanged).
That parent frame is built after the child's change and sent without
ordering against the parent's own changes, so it can arrive after a newer
frame for the parent: keep the view with the larger `updated_ms` (on a tie,
the later frame).
Frames are not replayed: after a reconnect, `GET /api/tasks`.

## Disabling

```json
{ "mcp": { "enabled": false } }
```

in the gateway's `config.json` (the default is `true`; there are no other `mcp`
keys, and an unknown key is a config error). With it off, the gateway:

- does not mount `/mcp` or `/api/mcp/tokens*`, so they answer 404;
- does not run the pane-token sweeper;
- gives the OpenCode service it starts no `MUQUN_MCP_*` environment from then
  on. OpenCode's saved service environment keeps the earlier values until you
  remove them: `opencode service unset env MUQUN_MCP_TOKEN` and
  `opencode service unset env MUQUN_MCP_URL`;
- omits `transports.mcp` from `/api/discovery` and `mcp_server` from the
  capability lists.

`/api/tasks*` and `/api/fleet/self` stay, and tasks are still reconciled,
because the phone uses them regardless.

## Platform notes

- macOS: the toolchain probes behind `fleet.*` run `python3 --version` and
  `git --version`. On a Mac without the Xcode Command Line Tools, those
  commands are stubs that may open the "install the command line developer
  tools" dialog.

## What is still out

- Relaying through the phone: a linked computer is reached at the URL the
  phone used for it, or not at all.
- Queueing for an offline computer: nothing is held for a computer that
  does not answer until it is back.
- Remote files: `cwd` and `@`-mentioned files are always on the computer the
  task runs on; nothing is copied between computers.
