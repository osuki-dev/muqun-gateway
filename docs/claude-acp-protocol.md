# Claude Code ACP adapter

Claude Code itself does not speak ACP. Gateway drives it through Claude Code's
official ACP adapter, [`@agentclientprotocol/claude-agent-acp`](https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp)
(binary `claude-agent-acp`, Node.js 22 or newer), which wraps the Claude Agent
SDK and reuses Claude Code's own login. The verified baseline is adapter 0.99.1
(SDK 0.3.312) with Claude Code 2.1.314. Versions are diagnostic only: Gateway
checks the actual `initialize` response and never pins a version.

## Configuration and process ownership

The provider id and kind are `claude`; the display name is Claude Code. The
terminal launcher keeps using the same `claude` id for panes, as Codex does: a
delegated goal without an explicit delivery still starts in a pane, and
`delivery: "session"` uses this adapter.

The adapter is enabled by default and needs no configuration. It needs
`claude` on PATH (signed in), `node` 22 or newer, and a runner: the
`claude-agent-acp` executable on PATH, else `npx`. Lookups never run the
runner. The `agent` table and discovery name whichever prerequisite is missing
instead of hiding the agent. Optional settings, omitted from `config.json`
while default:

```json
{ "claude": {
  "enabled": true,
  "binary": "/absolute/path/to/claude-agent-acp",
  "acp_package": "@agentclientprotocol/claude-agent-acp@0.99.1",
  "idle_timeout_secs": 3600
} }
```

`binary` names the adapter (not `claude`); `acp_package` is what `npx` runs
(default: the latest release). The child is started as
`claude-agent-acp`, or `npx -y --omit=optional <package>`, with:

- `CLAUDE_AGENT_ACP_PROTOCOL=v1`: the adapter serves ACP v1 only (it otherwise
  also serves the draft v2 router);
- `CLAUDE_CODE_EXECUTABLE=<claude on PATH>`: the adapter runs the user's own
  Claude Code instead of the SDK's bundled copy, so its version, login,
  settings and plugins are the terminal's. `--omit=optional` therefore skips
  the bundled ~200 MB binary; a first `npx` start installs the adapter in
  about a minute, so `initialize` may take up to 180 s.

Gateway never edits `~/.claude`, installs packages globally or logs in.

Startup only probes `claude --version` and `node --version`. The ACP child
starts on the first session open, create or prompt, serves that session,
and is released after `idle_timeout_secs` of verified idleness (below).
Listing sessions never starts it.

## Methods used

| Method | Use | Observed shape (0.99.1) |
|---|---|---|
| `initialize` | protocolVersion 1, `fs`/`terminal` false, `session.notices`, `elicitation.form` | `loadSession: true`; `sessionCapabilities` `list, resume, fork, delete, close, additionalDirectories, systemPrompt`; `promptCapabilities` image, embeddedContext; `mcpCapabilities` http, sse; `authMethods: []`; `agentInfo.version`; top-level `_meta.steering.supported` |
| `session/new` | `cwd`, `mcpServers` (muqun, below) | `sessionId` (UUID), `modes`, `configOptions` `mode, model, effort, fast, ultracode, agent`. No transcript is written until the first prompt. |
| `session/load` | reopen with replay | replays `user_message_chunk`/`agent_message_chunk` (with `messageId`) and `available_commands_update`; same `configOptions`, keeping the session's model |
| `session/set_config_option` | `model`, `effort` | answers the full `configOptions` |
| `session/set_mode` | permission mode | answers `{}`; the change arrives as `config_option_update` (the `mode` option), not `current_mode_update` |
| `session/resume` | re-apply the task instruction (`systemPrompt`) to a live, idle session | a changed `systemPrompt` makes the adapter rebuild the session; one never prompted comes back with the settings' default model, effort and mode, which Gateway selects again |
| `_session/steering` | a prompt sent with `delivery: "steer"` while a turn runs, with `_meta.steering.idleBehavior: "promptRequired"` | `{"outcome":"injected"}`: the text joins the turn (no user echo; the reply has a new `messageId`) and the running `session/prompt` answers once it is answered. `{"outcome":"promptRequired"}` when no turn runs: Gateway sends an ordinary `session/prompt` |
| `session/prompt` | text and image parts | `stopReason` `end_turn` / `cancelled`; `usage` with `inputTokens, outputTokens, cachedReadTokens, cachedWriteTokens` |
| `session/cancel` | notification | the running prompt answers `stopReason: "cancelled"` |
| `session/fork`, `session/delete` | when advertised | (shared ACP operations) |
| `session/list` | not used | the adapter scans the same transcripts; Gateway does so without a process |
| `elicitation/create` (agent → client) | AskUserQuestion, `mode: "form"` | below (Questions) |

Notifications seen: `session/update` (`agent_message_chunk`,
`user_message_chunk`, `usage_update` with `cost`, `session_info_update` with
the generated title, `available_commands_update`, `notice`) and
`_auth/status_update` (`{"authStatus":{"kind":"account"|"none",...}}`).
`_auth/status_update` is metadata, not work: it does not keep a child alive.
Account details in it are never read or forwarded.

## Models and effort

Model choices come from the `model` config option (`default`, `opus`,
`sonnet`, `haiku`, pinned ids), effort from `effort` (`default, low, medium,
high, xhigh, max`). They map to the App catalog under provider `claude` and are
set with `session/set_config_option`; `mode` (permission mode), `fast`,
`ultracode` and `agent` are not exposed (permission modes: below). No model is listed before a session
has been opened in this Gateway run (no static catalog is guessed).

On `session/load` the session's last model and effort are read from the
transcript's final assistant record (`requestedModel` or `message.model`,
`effort`) and set with `session/set_config_option` before any prompt: the exact
offered value, else its family alias (`claude-haiku-5-5` → `haiku`, logged).
A model that is not offered leaves the adapter's choice, which the session
catalog shows. Without a read-only catalog, discovery keeps offering the models
the last live session listed after its process is released; before any session
has been opened in a Gateway run there are none (no process is started to read
them).

## Permission modes

`modes.availableModes` (adapter 0.99.1: `default` "Manual", `acceptEdits`,
`plan`, `auto`, `bypassPermissions` when allowed) are the session's catalog
modes; the current one is the session's `mode`. Picking one calls
`session/set_mode`, as does creating a session with a mode. The mode belongs
to the session: the adapter calls Claude Code's `setPermissionMode` for that
query only, and a load continues in the transcript's last mode (verified).
A new session starts in the user's `permissions.defaultMode`. Observed:
switching into and out of `plan` can change the session's model (Claude Code's
plan-mode model); the App sees the new model in the session update.

## Prompts while a turn runs

`delivery: "steer"` steers (`_session/steering`, above). The adapter also
accepts a second `session/prompt` during a turn and runs it after the turn
(`agentCapabilities._meta.claudeCode.promptQueueing`, verified), but such a
native queue cannot be listed, edited or cancelled item by item, so Gateway
keeps no inbox for Claude Code: `delivery: "queue"` answers unsupported and
discovery declares `inbox: false`, `promptSteering: true` (the App then offers
steering only). A slash command is never steered.

## Questions

With `elicitation.form` advertised, AskUserQuestion reaches the client as
`elicitation/create` (a JSON-RPC request; its id may be the number `0`):

```json
{"mode":"form","sessionId":"…","toolCallId":"toolu_…","message":"Which fruit do you pick?",
 "requestedSchema":{"type":"object","properties":{
   "question_0":{"type":"string","title":"Fruit","oneOf":[{"const":"Apple","title":"Apple","description":"…"},…]},
   "question_0_custom":{"type":"string","title":"Other","description":"…"}}}}
```

It is shown as the App's question form (`agent.form.pending`, `forms` in the
snapshot). Each property becomes a field: a string (with `oneOf`/`enum`
choices), number, boolean, or an array of choices (multi-select). A single
choice and its `<key>_custom` companion are one field that also takes typed
text: an offered choice is sent as `<key>`, other text as `<key>_custom`. The
answer is `{"action":"accept","content":{…}}`; Claude Code then reports
`"Question"="Answer"` to the model. A schema with other shapes is answered
`{"action":"cancel"}` instead of being half-shown. Pending questions expire
when the turn ends. URL elicitation is not advertised.

## Rename

`_session/rename` is offered only to JetBrains AIR clients that declare its
session index (`Method not found` otherwise, verified). Rename therefore
sends Claude Code's own `/rename <title>` as a `session/prompt`: it runs no
model turn (zero tokens), writes the transcript's `custom-title` record, and
emits `session_info_update` with the title. Its output ("Session renamed to:
…") and, on a later load, its replayed `/rename …` user message leave no rows.
The session must be idle. The transcript keeps the command record; it is not
a prompt for idle-retirement evidence.

## Delegated tasks

A task's session gets the task sentence (`mcp::inject::TASK_SENTENCE`) as an
appended system prompt: `systemPrompt: {"mode":"append","content":[{type:
"text",…}]}` on every `session/new`, `session/load` and, when it is set on a
live idle session, `session/resume` (above). The instruction is held in memory
under its key, re-sent at each later start while the task is open, and
removed (followed by a resume without it) when the task ends.

## Deleting

`session/delete` is the SDK's transcript delete, Claude Code's own semantics
(an id without a transcript succeeds). Only sessions this Gateway created or
forked are deleted. Their ids are kept in `claude-owned-sessions.json` in the
Gateway state directory (at most 4096, newest kept), so a session created
before a Gateway restart is loaded and deleted by its adapter after it; a
session started elsewhere answers unsupported, and one held by a terminal
`409`. Discovery declares `sessionDelete` and `sessionFork` before any
adapter has started.

## muqun MCP

The muqun server is passed in every `session/new`, `session/load` and
`session/fork` as an ACP `mcpServers` HTTP entry:

```json
{ "type": "http", "name": "muqun", "url": "http://127.0.0.1:<port>/mcp",
  "headers": [{ "name": "Authorization", "value": "Bearer <token>" }] }
```

The token is bound to `claude` native sessions (a runtime binding, like
OpenCode's), minted once per Gateway run when MCP injection is on; earlier
ones are revoked. It travels only over the child's stdin, never argv or a file.
A server that fails to start is reported by the adapter as a notice, not as
transcript text.

## Session listing without a process

Transcripts live in `$CLAUDE_CONFIG_DIR/projects` (default
`~/.claude/projects`), in a directory named after the workspace: every UTF-16
unit that is not ASCII alphanumeric becomes `-`; a name over 200 characters is
cut and gets a hash suffix, which Claude Code and its SDK compute differently,
so long names match by prefix. Names are never decoded; a lossy bucket's
transcripts are kept only when their recorded `cwd` is the workspace.

For each `<uuid>.jsonl`, like the SDK, only the first and last 64 KiB are read:
title = custom title, else generated title (`ai-title`), else last prompt,
summary or first typed prompt; `updatedAt` = file modification time;
sidechains and title-less files are hidden. Records whose leading key is a
`type` other than the title records (request blobs, cost, queue) are skipped
without parsing. Results are cached per file by length, mtime and inode, so a
repeated global listing of ~1000 sessions takes milliseconds.

## Idle retirement evidence

A verified idle child is released like FX and Qoder. The evidence is read from
the exact transcript, streamed line by line (request blobs skipped unparsed,
256 MiB cap): the main chain is followed back from the last chat record through
`parentUuid`, and each prompt whose `origin.kind` is `human` starts one entry
holding that prompt and the text of the turn's final assistant message (one
record per content block, joined by `message.id`). Interrupt markers, task
notifications, tool results and sidechains are not prompts. If the transcript
is unsettled, too large, or does not prove the turn the Gateway saw, the child
is simply kept.

## A conversation that is open in a terminal

The core case: a conversation started in a terminal is continued from the
phone while that terminal keeps running, and both stay usable. Gateway never
starts a second `claude` for it.

**Correlation.** Every running Claude Code (terminal, IDE, another ACP
client, the Gateway's own adapter child) records its current session in
`<config>/sessions/<pid>.json` (`pid`, `sessionId`, `procStart`, `status`
busy/idle). This is Claude Code's own registry, so no argv or cwd heuristic is
needed: `claude --resume <id>`, `--continue` and a fresh `claude` all appear
there. An entry counts when its process is alive, same user, not a zombie,
with the recorded start time (Linux `/proc/<pid>/stat`; macOS `proc_pidinfo`
for liveness only) and is not a descendant of the Gateway. The pane is the
configured tmux/Herdr pane whose shell pid is an ancestor of that process;
exactly one must match. Failure modes: a Claude Code too old to write the
registry is not seen (the session then looks saved, and opening it would
start a second process); on macOS a reused pid could look alive until Claude
Code cleans its registry file.

**Presentation.** Listings, `get_session` and snapshots carry the runtime the
App already renders for Codex threads bound to a TUI: `source: "terminal"`,
`interaction: "terminal"`, `status` from the registry, `terminal: {session_id,
pane_id, instance_id: "claude:<pid>:<start>"}`. History is the transcript,
read read-only (main chain: prompts, text, thinking, tool calls and results)
through the same projection as `session/load` and mirrored whole, so earlier
pages come from the mirror.

**Input.** Discovery declares `features.terminalInput: true`. A prompt from the App is typed into that pane through the pane
agent's prompt path (with its delayed Enter), so the terminal user sees it;
Stop sends Escape there. After input the transcript is followed for ten
minutes; each change refreshes the mirror and sends `agent.resync`.
Attachments are refused for such a session. Every read (snapshot, refresh,
earlier pages, pending approvals, the session catalog, foreground and
background preparation) answers from the transcript; model switches, fork,
delete, and prompts or Stop without a pane answer
`409 agent_terminal_interaction_required`. Without a pane (Claude Code
outside tmux/Herdr) the session is shown read-only: `interaction:
"unavailable"`, `reason: "terminal_unavailable"`, and prompts get
`409 agent_terminal_interaction_required`.

**Hand-over.** When the terminal process exits, the session is an ordinary
saved session again and opens with `session/load`. When the Gateway holds a
native child and a terminal resumes the same session, the next observation
(list, read, prompt, Stop) releases the child if it is idle; a child in the
middle of a turn keeps it and is released after it (logged). In that window,
at most one turn, both processes can write the transcript; Claude Code itself
does not lock it.

## Parity with the other native agents

| Capability | Claude (adapter 0.99.1) | Status |
|---|---|---|
| Models | `model` config option | implemented (`session/set_config_option`) |
| Effort tiers | `effort` option, per model | implemented; tiers are listed only for the model selected in an observed session (shared ACP catalog rule) |
| Permission mode | `modes`, `session/set_mode` | implemented (session catalog modes, `features.modes`, `modeSwitching`); verified live |
| Interrupt | `session/cancel` → `stopReason: "cancelled"` | implemented (Escape for a terminal-held session) |
| Steer while running | `_session/steering` | implemented (`promptSteering`); verified live |
| Queue while running | native `promptQueueing` (verified) | not offered: the native queue has no list, edit or per-item cancel, and Gateway keeps no ACP inbox |
| Fork / delete | `sessionCapabilities.fork`, `delete` | implemented for Gateway-created sessions, also after a Gateway restart; delete verified live |
| Rename / title | generated title (`session_info_update`); `/rename` command (`_session/rename` is AIR-only) | implemented through `/rename`; verified live |
| Image / file attachments | `promptCapabilities.image`, `embeddedContext` | implemented (shared prompt parts) |
| Approvals | `session/request_permission` | implemented (shared); asked only when Claude Code's own permission mode asks |
| Questions / elicitation | AskUserQuestion as form `elicitation/create` | implemented (App question form); verified live. URL elicitation not advertised |
| Delegated tasks | muqun MCP per session, `systemPrompt` append | implemented (task sentence appended, kept across restarts of the child); verified live |
| muqun MCP tools | `mcpCapabilities.http` | implemented (runtime-bound token) |
| Worktrees | `additionalDirectories` only | not supported as worktrees |
| Changes panel (`agent_vcs_files`) | tool calls carry inputs; no VCS listing method | not implemented for any ACP agent (`get_vcs_diff` unsupported) |
| Compaction | `/compact` slash command | available as a slash command (`available_commands_update`) |
| Token usage / context pill | `usage_update {used, size, cost}`, prompt `usage` | implemented (shared reducer) |
| Account limits | no ACP method | not supported |
| Idle retirement | transcript evidence | implemented |
| Terminal-held sessions | Claude Code session registry | implemented (above) |

## Errors

A prompt without a Claude Code login fails with JSON-RPC `-32000`
`"Authentication required"` (and `_auth/status_update` kind `none`); Gateway
shows "Claude Code is not signed in; run claude in a terminal on this computer
and sign in". Other native messages are not relayed. No prompt is ever
retried automatically.

## Validation boundary

Terminal-held sessions are verified with fakes (correlation table, hand-over
driver test) and the transcript reader against real transcripts; typing into a
real pane needs the deployed Gateway and App.

Verified against the real adapter: initialize, new, set model/effort (haiku,
low), prompt, cancel, load replay, list, the not-signed-in error, idle
retirement followed by transcript listing and a reload (`cargo test --
--ignored real_turn_retires_lists_and_reloads` with `CLAUDE_ACP_TEST_ADAPTER`
and `CLAUDE_ACP_TEST_WORKSPACE`); permission modes on create and switch, the
task instruction with model and mode kept, an AskUserQuestion answered
through the form, steering, rename and deleting a session after a driver
restart (`real_modes_instruction_question_steer_rename_and_owned_delete`,
same variables). The muqun MCP entry's shape is accepted by the
adapter; a live muqun tool call from a native Claude session and the paired App
flows still need a deployed Gateway.
