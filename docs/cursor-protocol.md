# Cursor ACP adapter

The adapter drives the Cursor CLI's own ACP stdio server, `agent acp`, through
the Gateway's shared ACP client (protocol 1). The local baseline is Cursor CLI
2026.10.22. Versions are diagnostic only: every session checks the actual
`initialize` answer, and each optional method is used only when advertised.

## Why ACP

Cursor documents `agent acp` as its integration surface for custom clients
(<https://cursor.com/docs/cli/acp>: stdio, JSON-RPC 2.0, newline-delimited;
`initialize`, `authenticate` with `cursor_login`, `session/new|load|list`,
`session/prompt`, `session/update`, `session/request_permission`,
`session/cancel`, and Cursor extension methods). The other candidates were
checked and rejected:

- **`agent persist`** is a tmux wrapper, not a server. `agent persist` refuses
  to run without tmux ("Persistence requires tmux"), and `persist attach` runs
  `tmux attach-session` on a Cursor-named tmux session. There is no socket,
  HTTP or RPC that a second client could use; a second viewer is a second tmux
  client on the same terminal, which the Gateway already offers as a pane.
- **No other local server.** `agent worker` registers this computer with
  Cursor's cloud (outbound only; "My Machines" or a team pool) so that cloud
  agents run tools here; it serves no local session API. The hidden
  `worker-server` the CLI starts per workspace (a Unix socket under
  `~/.cursor/projects/<workspace>/worker.sock`) is an internal tool and index
  helper, not documented and not a session server.
- **`agent -p --output-format stream-json`** (one process per turn) shares its
  chat store with the terminal UI, but has no approvals (print mode "has access
  to all tools"), interrupts only by killing the process, reports no model
  catalog, and offers no history read: history would have to be decoded from
  Cursor's private blob store.

So the shared server here is the Gateway-owned `agent acp` child, the same
shape as FX, Qoder and Antigravity. A conversation is one process with many
App views; a Cursor terminal is not one of those views (see below).

## Configuration and process ownership

The provider id, kind and discovery id are `cursor`; the display name is
Cursor.

```json
{ "cursor": { "enabled": true, "binary": "/absolute/path/to/agent", "idle_timeout_secs": 300 } }
```

All three keys are optional and the block is omitted from a saved config while
it is the default. The adapter is on when `agent` is on PATH (or at
`cursor.binary`). `agent_runtime.enabled: false` disables it with the other
service adapters.

Gateway does not install, update, log in or configure Cursor and never writes
to its settings. Discovery runs a bounded `agent --version`; nothing else
starts until a session is opened, created or prompted. Each such session owns a
private `agent acp` child started in the session's folder; it is released after
`idle_timeout_secs` (default 300) of verified idleness (below), and shutdown stops and reaps
Gateway-owned children.

The CLI keeps its files in `CURSOR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cursor`,
else `~/.cursor`, and so does the Gateway's reader. The child inherits the
Gateway's environment, so a service manager that sets `XDG_CONFIG_HOME`
differently from the user's shell would point both at a folder without the
login; the login check below then says "not signed in".

## Sign-in

`agent acp` refuses `session/new`, `session/load` and `session/list` until the
client sends `authenticate` (`cursor_login`), even with a stored login. With a
stored login it answers in 4–18 s here. **Without one it opens a browser
sign-in** and waits for it (the CLI source: "Starting browser login flow").
Gateway never lets that happen:

1. Before starting a child, Gateway runs `agent status --format json` (bounded
   to 20 s, `NO_OPEN_BROWSER=1`), which only reads the stored login. A positive
   answer is remembered for ten minutes.
2. `isAuthenticated: false` refuses with *Cursor is not signed in on this
   computer; run `agent login` in a terminal*, and no child starts. A status
   command that fails or prints no answer refuses with "could not check the
   Cursor login".
3. Only then is `authenticate` sent (90 s bound). `-32000 Authentication
   required` from any later request maps to the same sentence.

An exhausted plan does not fail the turn: Cursor answers with an ordinary agent
message ("Upgrade your plan to continue") and `stopReason: end_turn`, so the
App shows that text as the reply.

## Session listing and history

Listing reads `<config>/acp-sessions/*/meta.json` (`cwd`, `title`) and never
starts `agent`. Like `agent acp`'s own `session/list`, only titled sessions
(those that were prompted) are listed; `updatedAt` is the newest modification
time of `store.db-wal`, `store.db` or `meta.json`. Opening a listed session
uses `session/load`, which replays the conversation (user, thought and agent
chunks, tool calls with their results; tool call ids are `replay-N-M`).
Cancelled turns are not replayed.

### Terminal chats are a separate store

Chats started in the terminal UI or with `agent -p` live in
`<config>/chats/<md5(folder)>/<chatId>/`, which `agent acp` does not read:
`session/load` of such an id answers `-32602 Session "<id>" not found`, and
`agent --resume <acp-session-id>` silently starts a new, empty terminal chat
with that id. The two stores never share a conversation, so there is no second
writer to guard against, and terminal chats are not listed in the App. Showing
them read-only would need a decoder for Cursor's private blob store (and
typing into the pane, as for Claude Code); that is not implemented.

### Idle retirement evidence

A child is released only when Cursor's own store proves the last turn. The
reader opens `acp-sessions/<id>/store.db` read-only: `meta['0']` is hex JSON
naming `latestRootBlobId`; field 1 of that protobuf root lists the 32-byte ids
of the conversation's messages, each a JSON `{role, content}` blob. A user
message whose text carries `<user_query>\n…\n</user_query>` starts a turn;
the last assistant message's text parts are the reply. Anything that does not
match (summarized history, a changed format) keeps the child.

Cursor has no session registry, so a child, also one whose session ran tools, is
released only when, after the idle window (checked every 30 seconds), there is
no turn in flight and nothing waiting for an answer, the child's process tree
holds no process that was not there before the session's first prompt (a
background command or script keeps it), and the session's store (`store.db`) was not
written during the whole window (SQLite's `-wal`/`-journal` included). A signal
showing work starts the window again; one that cannot be read keeps the child.
An in-process subagent that neither writes the store nor starts a process
cannot be seen.

## Models and modes

`session/new` and `session/load` return `models` and a `model` config option
with about 50 values such as `default[]` (Auto) or
`gpt-5.4-nano[reasoning=medium]`. Effort is part of the value; there is no
separate effort option, and only listed values are accepted (an unlisted
bracket variant is `-32602 Invalid model value`). Gateway offers the listed
values as models. Choosing one is persisted by Cursor itself as
`selectedModelVariantId` in `<config>/acp-config.json`, Cursor's default for
later ACP sessions (Gateway does not write that file).

The `mode` option (`agent`, `plan`, `ask`) and ACP `modes` are offered, but the
shared ACP driver has no mode switch yet, so sessions run in `agent` mode.

## muqun tools

The muqun MCP server is passed as an ACP HTTP `mcpServers` entry with a
`Bearer` token bound to `cursor` on every `session/new`, `session/load` and
`session/fork`. Cursor's `mcp.json` is never edited. A probe server saw
`initialize` (client "Cursor"), `notifications/initialized` and `tools/list`
with the bearer header within a second of `session/new`.

Cursor reports every MCP call as `kind: "other"`, `title: "muqun: <tool>"`
and `rawInput: {providerIdentifier, toolName, args}` (it sends no `name`).
The Gateway tells which session makes a `tasks.complete`, `exec.run` or
`tasks.delegate` call by finding that call running in a session's timeline,
so it reads Cursor's tool as `<providerIdentifier>_<toolName>` and the
arguments from `args`. The model reaches the tools through Cursor's
`GetDynamicTools` / `CallDynamicTool`, which run as these MCP calls.

## Feature parity

| Capability | Cursor ACP (CLI 2026.10.22) | Gateway |
|---|---|---|
| Model selection | `model` config option (effort inside the value) | Implemented |
| Reasoning effort | Not separate; part of model values | Shown as model choices |
| Modes (agent / plan / ask) | `mode` option, `current_mode_update` | Not implemented (no shared mode switch) |
| Interrupt | `session/cancel` | Implemented (verified: `stopReason: cancelled` in 2 ms) |
| Queue / steer while running | No | Not supported (same as the other ACP agents) |
| Fork / delete | Not advertised | Not supported |
| Title | `session_info_update` on the first prompt | Implemented (read-only; no rename method) |
| Image attachments | `promptCapabilities.image` | Implemented (shared prompt parts; not verified live) |
| Approvals | `session/request_permission` (allow-once / allow-always / reject-once) | Implemented (shared); asked only when Cursor's own approval mode asks, not verified live |
| Questions / plan approval | `cursor/ask_question`, `cursor/create_plan` (blocking) | Not implemented: answered "method not supported", not verified live |
| Todos / subagents / images | `cursor/update_todos`, `cursor/task`, `cursor/generate_image` notifications | Not shown |
| Delegated tasks and muqun tools | ACP HTTP `mcpServers` | Implemented (verified against a probe MCP server); no system prompt, so the task sentence ends the first prompt and is cut from the served transcript |
| Worktrees | `agent -w` only in the terminal UI | Not supported |
| File-change listing | No | Not supported |
| Slash commands | `available_commands_update` (commands and skills) | Implemented; a name listed twice (a command and a skill) is offered once |
| Token usage | Not reported by `session/prompt` | Not available |
| Session listing without a process | `acp-sessions/*/meta.json` | Implemented |
| History | `session/load` replay | Implemented |
| Idle retirement | `acp-sessions/<id>/store.db` | Implemented (verified live) |
| Terminal chats | Separate store, not loadable over ACP | Not listed |
| Account limits | No | Not supported |
