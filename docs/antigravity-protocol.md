# Antigravity ACP adapter

The adapter drives Google Antigravity's own ACP stdio server, `agy acp`, through
the Gateway's shared ACP client (protocol 1). The local baseline is `agy` 1.4.12
(`agentInfo` "Antigravity ACP Server" 1.4.12). Versions are diagnostic only:
every session checks the actual `initialize` answer, and each optional method
is used only when advertised.

## Configuration and process ownership

The provider id, kind and discovery id are `antigravity`; the display name is
Antigravity. The terminal launcher keeps its separate `agy` kind, so starting
`agy` in a pane is unchanged.

```json
{ "antigravity": { "enabled": true, "binary": "/absolute/path/to/agy", "idle_timeout_secs": 3600 } }
```

All three keys are optional and the block is omitted from a saved config while
it is the default. The adapter is on when `agy` is on PATH (or at
`antigravity.binary`). `agent_runtime.enabled: false` disables it with the other
service adapters.

Gateway does not install, update or configure Antigravity and never writes to
its settings or `~/.gemini`. Discovery runs a bounded `agy --version`; nothing
else starts until a session is opened, created or prompted. Each such session
owns a private `agy acp` child started in the session's folder; shutdown stops
and reaps Gateway-owned children. A conversation open in a terminal `agy` is not
shared with a Gateway child.

## Sign-in

`agy acp` refuses `session/new`, `session/load` and `session/list` with
`-32000 Authentication required` until the client sends `authenticate`, even
when a Google login is already stored on the computer (agy 1.4 keeps it in the
system keyring). Gateway therefore sends `authenticate` with method
`oauth-personal` right after `initialize`, when that method is offered:

- Signed in: answered in milliseconds; the result carries the account tier in
  `_meta.antigravity.tier`.
- Not signed in: agy starts a loopback browser sign-in
  (`oauth_loopback.go: Opening browser for ACP sign-in`) and does not answer
  until it completes; a cancelled sign-in answers `-32800 Sign-in was
  cancelled.`. Gateway waits at most 20 seconds, then stops that child and
  reports *Antigravity is not signed in on this computer; run `agy` in a
  terminal and sign in*. The same sentence replaces the two native refusals
  above if they ever reach a later request. On a computer with a desktop
  session, agy may have opened a sign-in page by then; Gateway does not
  complete or retry the sign-in.

The headless `agy --print` mode waits 60 seconds for authentication; it is not
used here.

## Session listing

Listing reads `~/.gemini/antigravity-cli/conversation_summaries.db` read-only
and never starts `agy`. Rows are top-level conversations (`nesting_depth = 0`,
no parent; subagent conversations are separate rows there) that have at least
one step and a `file://` workspace folder. The title is agy's generated title,
else the first prompt preview. `updatedAt` is the later of the last modification
and the last user input (agy writes year 1 for a time it never recorded). The
read is bounded to 4096 rows and shares the driver's 30-second list cache.

`agy acp` also implements `session/list`, but it needs a running, authenticated
child, and it reports the child's own folder for conversations whose folder was
never recorded. It is not used.

## Models and thinking

`session/new`/`session/load` return two config options:

- `model` (category `model`): for a non-allowlisted client, agy 1.4.12 offers
  only `gemini-3.8-flash`. Other models (Claude, GPT) are offered to clients
  agy recognises by `clientInfo` name, such as Zed; Gateway identifies itself
  honestly as `muqun-gateway` and does not get them.
- `thought_level` (category `thought_level`): `low`, `medium`, `high`; the
  default is `high`. It is the App's reasoning effort for this provider.

agy also still returns 1.3's effort-suffixed ids (`gemini-3.8-flash-high` …) in
`models.availableModels`; they duplicate the two options above and are not
offered. Choices are applied with `session/set_config_option`. agy forgets
`thought_level` on `session/load` (it comes back `high`); a reopened
conversation re-applies the effort it had before the process was released.

## Prompts, history and cancel

`session/prompt` streams `agent_message_chunk` updates and answers
`{"stopReason":"end_turn","usage":{...}}` with input, output, cached and
thought token counts for the turn. `session_info_update` carries agy's
generated title; `available_commands_update` lists its slash commands (`goal`,
`compact`, `schedule`, `browser`, `plan`, …). `session/cancel` during a turn
ends it with `stopReason: "cancelled"`. `session/load` replays the user and
agent messages as updates. agy also implements `session/resume` and
`session/close`; the shared driver does not use them.

agy advertises no `promptCapabilities`, so the App's attachments stay off for
this provider. Gateway advertises no client file system or terminal (`fs`
writes would switch agy to whole-file client edits).

Native questions arrive as `session/request_permission` with a `toolCallId`
starting `interaction_`; their options are the answers. They go through the
shared ACP approval path like any permission request; a dedicated question
form for them is not implemented and was not exercised against a real agy.

## muqun tools

Each `session/new`, `session/load` and `session/fork` carries the muqun server
as an ACP HTTP `mcpServers` entry with a token bound to Antigravity's native
sessions. agy 1.4.12 connects to it (MCP `2025-11-25`), lists its tools and
calls them as `mcp_muqun_<tool>` tool calls without asking for permission. The
user's own agy MCP configuration is untouched.

## Idle retirement

Verified idle conversations release their child after
`antigravity.idle_timeout_secs` (default 3600, checked every 30 seconds). The
evidence is the conversation's own `~/.gemini/antigravity-cli/conversations/<id>.db`
read-only: one entry per user-input step (kind 14) with its text (protobuf field
19.2), the text of each model reply step (kind 15, field 20.1) and the kinds of
the steps between. A turn counts as stored when the checkpoint taken before the
prompt is extended by exactly one entry whose user text and last reply text
match what the live stream showed. Anything else keeps the child running.

A child whose session ran a tool is kept: Antigravity has no record that says
whether work a tool started (a background command) still runs, and closing
the child could kill it. Only sessions that ran no tool are released.

## Feature parity

| Capability | Antigravity ACP | Gateway |
|---|---|---|
| Model selection | `model` config option | Implemented (one Gemini model offered to this client) |
| Reasoning effort | `thought_level` config option | Implemented; re-applied after reopen |
| Modes (accept edits / auto / plan) | No ACP session modes; no mode config option; `session/set_mode` is "Method not found". `--mode` exists only for interactive `agy`; `agy acp` has only a per-process `--dangerously-skip-permissions` | Not supported |
| Interrupt | `session/cancel` | Implemented (verified: `stopReason: cancelled`) |
| Queue / steer while running | No | Not supported (same as the other ACP agents) |
| Fork | Not advertised | Not supported |
| Delete | Not advertised (`session/close` only closes) | Not supported |
| Title | `session_info_update` | Implemented (read-only; no rename method) |
| Image / file attachments | No `promptCapabilities` advertised | Not supported |
| Approvals | `session/request_permission` (allow once / always / reject) | Implemented |
| Questions | Permission requests with `interaction_` ids | Supported by agy, not implemented as questions (shared approval path; not verified live) |
| Delegated tasks and muqun tools | ACP HTTP `mcpServers` | Implemented (verified against a probe MCP server); the task sentence ends the first prompt and is cut from the served transcript, since `_meta.antigravity.systemPrompt.append` (advertised by agy 1.4.12) is unverified |
| Subagents | Ordinary tool calls (`invoke_subagent`), no child ids | Shown as tool rows; child conversations not listed |
| Worktrees | No | Not supported (same as the other ACP agents) |
| File-change listing | No | Not supported |
| Compaction | `/compact` slash command | Available as a slash command; no dedicated compaction control |
| Slash commands | `available_commands_update` | Implemented |
| Token usage | `usage` in the prompt answer | Implemented (per turn) |
| Session listing without a process | Native index | Implemented |
| History | `session/load` replay | Implemented |
| Idle retirement | Native conversation store | Implemented |
| Account limits | Tier name only (`_meta`) | Not supported |
