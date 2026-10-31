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
| `initialize` | protocolVersion 1, `fs`/`terminal` false, `session.notices` | `loadSession: true`; `sessionCapabilities` `list, resume, fork, delete, close, additionalDirectories, systemPrompt`; `promptCapabilities` image, embeddedContext; `mcpCapabilities` http, sse; `authMethods: []`; `agentInfo.version` |
| `session/new` | `cwd`, `mcpServers` (muqun, below) | `sessionId` (UUID), `modes`, `configOptions` `mode, model, effort, fast, ultracode, agent`. No transcript is written until the first prompt. |
| `session/load` | reopen with replay | replays `user_message_chunk`/`agent_message_chunk` (with `messageId`) and `available_commands_update`; same `configOptions`, keeping the session's model |
| `session/set_config_option` | `model`, `effort` | answers the full `configOptions` |
| `session/prompt` | text and image parts | `stopReason` `end_turn` / `cancelled`; `usage` with `inputTokens, outputTokens, cachedReadTokens, cachedWriteTokens` |
| `session/cancel` | notification | the running prompt answers `stopReason: "cancelled"` |
| `session/fork`, `session/delete` | when advertised | (shared ACP operations) |
| `session/list` | not used | the adapter scans the same transcripts; Gateway does so without a process |

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
`ultracode` and `agent` are not exposed. No model is listed before a session
has been opened in this Gateway run (no static catalog is guessed).

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

## Errors

A prompt without a Claude Code login fails with JSON-RPC `-32000`
`"Authentication required"` (and `_auth/status_update` kind `none`); Gateway
shows "Claude Code is not signed in; run claude in a terminal on this computer
and sign in". Other native messages are not relayed. No prompt is ever
retried automatically.

## Validation boundary

Verified against the real adapter: initialize, new, set model/effort (haiku,
low), prompt, cancel, load replay, list, the not-signed-in error, idle
retirement followed by transcript listing and a reload (`cargo test --
--ignored real_turn_retires_lists_and_reloads` with `CLAUDE_ACP_TEST_ADAPTER`
and `CLAUDE_ACP_TEST_WORKSPACE`). The muqun MCP entry's shape is accepted by the
adapter; a live muqun tool call from a native Claude session and the paired App
flows still need a deployed Gateway.
