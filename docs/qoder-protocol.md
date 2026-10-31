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
flow or automatically retries the original operation after login. Qoder answers
an unauthenticated ACP request with ACP's `auth_required` code (-32000), which
Gateway reports as `Qoder ACP request rejected (code -32000): authentication
required; sign in to Qoder on this computer`. Other ACP error codes keep only
`Qoder ACP request rejected (code N)`; Qoder's own error text is never relayed.

Discovery uses a time- and output-bounded `--version` probe and may read the
native `--list-models` catalog after attachment. No ACP session is created for
that catalog read. Its `reachable`
status means an installed executable answered, not that model authentication
or a model response has been verified. Each explicitly opened session owns a
private `qodercli --acp` child. Shutdown stops and reaps Gateway-owned children;
external terminal and IDE processes are untouched.

Verified idle conversations release their private process after
`qoder.idle_timeout_secs` (default 3600 seconds, checked every 30 seconds). The
driver checks the exact session's bounded native JSONL journal without loading
it: the last matching `active-leaf` selects the branch, `parentUuid` orders its
records, and native human-input metadata distinguishes prompts from tool-result
user records. A normal nonempty `end_turn` must extend the checkpoint captured
before that prompt by one human turn, with unchanged earlier context and exact
user/final-assistant text hashes. Context attachments, reasoning and tool
records remain part of the canonical checkpoint. A clean final native
`end_turn` is required; cancellation/error records do not qualify.

Observed live tool execution, native command delivery, permission requests or
unknown extension/subagent control activity protects that entire process
generation from automatic retirement, even after later ordinary turns finish.
Native EOF can stop background jobs; a completed reply does not prove those
jobs ended, and these CLIs expose no supported read-only job inventory.
Historical replay alone does not pin a new generation. Custom hooks that launch
silent work without observable execution/control records are outside this guard;
Gateway does not disable user extensions or claim universal job-absence proof.
Explicit configured timeouts remain unchanged, and history polling does not
renew the idle timer.

Busy/in-flight work and pending approvals prevent retirement. Gateway closes
stdin in FIFO order, drains stdout/stderr, waits for a successful exit, then
checks the same native checkpoint again. The live child and projection are
released; only bounded identity/workspace metadata and the ordinary bounded
mirror cache remain. A verified metadata-only empty session may also retire.
Recovery descriptors are capped at 512 per provider and 16 KiB serialized info
each (8 MiB payload plus bounded indexes). Oversized metadata prevents retirement
before EOF; an unexpected exit keeps only bounded recovery identity and resolves
the full display title through the existing bounded mirror/native metadata.
Inspection admission remains owned by the blocking scan/read/parse job even if
its caller is cancelled, so another inspection cannot exceed its memory budget.
Background snapshot/history/catalog reads do not wake a dormant conversation.
Explicit foreground preparation or a new user send restores the same native
ID once and replaces the prepared history atomically, without replaying an old
prompt. Unexpected or uncertain exits stay Unknown and require explicit
foreground preparation before another prompt.

Native compacted/repaired/sidechain formats or unknown record kinds currently
fail recovery inspection rather than being mistaken for a simple active
branch. Missing, ambiguous, changing or oversized journals leave the live
process intact. A `session/close` acknowledgement alone is never a persistence
guarantee.

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
That advertisement alone does not enable App controls. Model choices come from
the actual native catalog; effort choices require observed native config options.
Session config selection uses Qoder's temporary setting contract and rejects
unoffered values. It does not change the global CLI profile.

Native uploads require observed image or embedded-context capabilities and strict
upload fences. Slash commands require an actual native catalog; native fork and
delete require advertised operations plus ownership/idle guards. Modes, skills,
compaction, background shells, worktrees, revert, inbox editing, rename, native
history paging, account limits and computer tools remain disabled. Earlier
timeline reads may page the complete bounded replay already held by Gateway;
unverified or oversized native history is an explicit error. Permission-mode switching remains unavailable while its persistence and security
scope are unverified. Native image/MCP/session-management capability declarations
are not promises that Gateway implements those surfaces.

## Verification

Mock protocol tests and compilation do not prove live model delivery. Paired App
acceptance must separately record creation, an actual response, follow-up,
permission/cancel behavior when exercised, and preservation of terminal focus.
Reports must distinguish observed native behavior from tested Gateway behavior.
