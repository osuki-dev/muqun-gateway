# Qoder CLI ACP adapter

The adapter targets [Qoder CLI's ACP interface](https://docs.qoder.com/zh/cli/acp).
The local protocol baseline is Qoder CLI 1.1.81. Versions are diagnostic only:
Gateway checks the actual ACP initialization response before opening a session.

## Configuration and process ownership

The canonical provider id and kind are `qoder`; the display name is Qoder CLI.
The existing terminal launcher keeps its separate `qodercli` identity. Adding
this session adapter does not redirect an existing terminal Qoder launch.

The adapter is enabled by default. It resolves `qodercli` on PATH, or the exact
absolute CLI executable in `qoder.binary`. A command named `qoder` can be an IDE
launcher, so Gateway does not automatically substitute it. An installation whose
CLI uses another name can set `qoder.binary` explicitly. `qoder.enabled: false`
disables Qoder and `agent_runtime.enabled: false` disables all service adapters.

Gateway does not install or upgrade Qoder, change its global settings, log in,
or copy credentials into Gateway configuration. The ACP child uses the existing
CLI authentication on this host. Configure and sign in to Qoder CLI yourself.
An authentication failure is reported; Gateway never starts an authentication
flow or automatically retries the original operation after login.

Discovery runs only a time- and output-bounded `--version` probe. Its `reachable`
status means an installed executable answered, not that model authentication
or a model response has been verified. Each explicitly opened session owns a
private `qodercli --acp` child. Shutdown stops and reaps Gateway-owned children;
external terminal and IDE processes are untouched.

A separately opened Qoder terminal or IDE is not a shared live ACP connection.
Gateway can read saved session metadata without loading that conversation. An
explicit selection may request native load and history replay. This does not
prove exclusive ownership of an external runtime or guarantee coherent concurrent
writes from multiple clients. Gateway never resends an ambiguous prompt to
recover a lost connection.

## Native protocol and Gateway boundary

The transport is newline-delimited JSON-RPC 2.0 over private stdio. The Gateway
adapter reuses the bounded ACP client and session projection with an explicit
Qoder provider profile; Qoder's optional message ids are not treated as FX's
required ids. The App continues to use existing Gateway routes and timeline
parts, with no new protocol or App bundle changes.

| Direction | Method | Relevant fields |
| --- | --- | --- |
| Gateway → Qoder | `initialize` | Protocol 1, client identity, explicit false client filesystem/terminal capabilities |
| Gateway → Qoder | `session/new` | `cwd`, `mcpServers: []`; reply supplies `sessionId` |
| Gateway → Qoder | `session/list` | Optional `cwd`, `cursor`; reply supplies saved metadata |
| Gateway → Qoder | `session/load` | Exact `sessionId`, `cwd`, `mcpServers: []` |
| Gateway → Qoder | `session/prompt` | `sessionId`, text content blocks |
| Gateway → Qoder | `session/cancel` | Notification with `sessionId` |
| Qoder → Gateway | `session/update` | Session identity and typed update |
| Qoder → Gateway | `session/request_permission` | Native request id and offered options |

One ordinary prompt can run per Gateway-owned session. Native text, reasoning,
tool updates and explicit turn results update the existing mirror. Load replay
replaces the prepared snapshot atomically rather than appending a second copy of
history. A terminal stop or disconnect is not a fabricated successful answer.

Permissions retain their request identities and offered option ids. The owner
must answer explicitly. Unknown, missing or ambiguous choices are refused;
timeout and disconnect are not approval. Gateway does not pass bypass flags or
change Qoder's permission mode.

Qoder advertises several native abilities beyond this first Gateway contract.
That advertisement alone does not enable App controls. Model and reasoning-effort
selection, modes, skills, slash commands, attachments, compaction, background
shells, worktrees, revert, inbox editing, rename, delete, fork, historical paging,
account limits and computer tools are explicitly disabled. Permission-mode and
configuration setters remain unavailable while their persistence and security
scope are unverified. Native image/MCP/session-management capability declarations
are not promises that Gateway implements those surfaces.

## Verification

Mock protocol tests and compilation do not prove live model delivery. Paired App
acceptance must separately record creation, an actual response, follow-up,
permission/cancel behavior when exercised, and preservation of terminal focus.
Reports must distinguish observed native behavior from tested Gateway behavior.
