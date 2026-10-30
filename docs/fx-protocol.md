# FX ACP adapter

The Gateway adapter targets the official [FX ACP interface](https://fx.sh/docs/using-fx/acp).
The implementation was developed against FX 0.0.16. A version is diagnostic
information, not a capability promise: the driver initializes ACP and checks the
actual response when opening a session.

## Configuration and ownership

FX is enabled by default when installed. Gateway uses `fx` from PATH, or the exact
absolute executable in `fx.binary`. `fx.enabled: false` disables FX, and
`agent_runtime.enabled: false` disables all service adapters. FX owns its existing
host configuration and authentication. Gateway does not install packages, log in,
change permission policy, or update a global FX configuration file.

Discovery executes only a time- and output-bounded `--version` probe. A
`reachable` result means an executable answered, not that a model request has
succeeded. The first session open is the ACP negotiation boundary. A missing or
invalid executable does not block other adapters.

Each Gateway session owns one private `fx acp` stdio child. The ACP connection
carries JSON-RPC messages, `session/update` notifications and
`session/request_permission` requests. Initialization and session creation happen
before a prompt. One ordinary prompt may be in flight per session. Cancellation
uses ACP's native cancellation notification; it is not evidence that a failed or
lost prompt never ran. Ambiguous delivery is never retried automatically.

An external FX TUI has its own connection and session lock. Gateway does not
adopt it or claim that it shares a live ACP session. Session listing reads
native saved metadata without claiming a live runtime.
An explicit session open may use native `session/load` to resume a saved
conversation and replay its history; an active external TUI may hold an
exclusive lock, which Gateway must respect. Timeline data is retained in the
Gateway mirror for this run, with no separate historical paging. Rename, delete
and fork are not advertised. A saved FX conversation alone does not establish
that Gateway owns a live runtime or may replay a prompt.

## Native messages

The private stdio connection uses newline-delimited JSON-RPC 2.0. Methods and
fields keep ACP's spelling; the App continues to use Gateway's existing contract.

| Direction | Method | Relevant fields |
| --- | --- | --- |
| Gateway → FX | `initialize` | `protocolVersion: 1`, `clientInfo`, explicit false filesystem/terminal client capabilities |
| Gateway → FX | `session/new` | `cwd`, `mcpServers: []`; reply carries `sessionId` |
| Gateway → FX | `session/list` | Optional `cwd`, `cursor`; reply carries `sessions`, optional `nextCursor` |
| Gateway → FX | `session/load` | `sessionId`, `cwd`, `mcpServers: []`; history arrives through session updates |
| Gateway → FX | `session/prompt` | `sessionId`, `prompt: [{type: "text", text}]`; response completes the turn |
| Gateway → FX | `session/cancel` | Notification with `sessionId` |
| Gateway → FX | `session/set_config_option` | `sessionId`, exact native `configId`, offered `value` |
| FX → Gateway | `session/update` | `sessionId`, typed `update` with `sessionUpdate` discriminator |
| FX → Gateway | `session/request_permission` | Request id, `sessionId`, `toolCall`, offered `options` |

A permission answer selects exactly one offered `optionId` in
`{outcome: {outcome: "selected", optionId}}`. Unknown or ambiguous choices are
refused. A native load replays a snapshot using new native row ids, so the driver
validates the replay and replaces the prepared mirror revision atomically;
concatenating it onto the old timeline would duplicate history. Live user text
is projected only after successful prompt admission because FX does not echo it
as a normal live user event.

## App contract

The provider id and kind are `fx`. It uses the existing Gateway session, prompt,
interaction and event routes. No App bundle changes are required: provider
selection is dynamic, and discovery states each control explicitly.

Streaming text, reasoning and tool updates enter the existing timeline mirror.
Permissions retain their native request identity and offered choices. The owner
must answer a request explicitly; missing or unsupported choices are refused.
No allow-all policy is installed and a timeout/disconnection is not approval.

Model selection is enabled only after a live native session supplies actual
model `configOptions`. Reasoning-effort selection additionally requires native
effort options for that model. Choices use session-scoped `session/set_config_option`
and refuse unknown values or changes while a turn is busy. A new session may
apply explicitly selected native provider/model/effort choices before its first
prompt. Unobserved choices are not fabricated from version numbers or global
configuration. These operations never write an FX mode/profile.

Modes, skills, slash commands, attachments, worktrees, revert, compaction,
background shells and inbox editing remain disabled. Optional
`sessionRename`, `sessionDelete`, `sessionFork`, `sessionPagination`,
`timelineEarlier`, `accountLimits` and `computerTools` are explicitly false.
A mode being mentioned by native ACP does not enable mode switching: FX's mode
setter changes host configuration, so it is outside this session adapter.

## Validation boundary

Protocol fixtures exercise adapter behavior without model credentials or a
running user's session. Real model delivery and paired App acceptance must be
reported separately, including follow-up output, cancellation, permission
handling and terminal-focus preservation. A successful build or mock test alone
does not establish those behaviors on a live device.
