# Codex app-server adapter

## Baseline and ownership

This adapter targets the **stable `rust-v0.162.1`** release, commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`. Protocol facts come from that
commit's published `codex-rs/app-server-protocol/schema/typescript` and
`src/protocol/common.rs`, not main, alpha releases or generated local schemas:

- [Published protocol](https://github.com/openai/codex/tree/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol)
- [Request declarations and experimental gates](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/src/protocol/common.rs)

**Requirement revision:** Codex support defaults **enabled**, including when
the `codex` configuration block is omitted. Set `codex.enabled` to `false` to
disable it. `agent_runtime.enabled: false` disables all service-agent adapters
without disabling terminal agents. Both blocks default enabled and reject
unknown nested fields; Gateway configuration has no Codex credential fields.
`codex.binary`, when present, names an absolute executable; otherwise
normal PATH lookup selects `codex`. Gateway does not install it. Missing binary
or authentication is nonfatal and does not disable other agents. Codex follows
OpenCode, DeepSeek and T3 in primary preference; existing terminal Codex sessions
and terminal-task launch behavior are unchanged.
Explicit task `agent_id: codex` continues to use a terminal pane even when the
private service adapter is connected; Codex service sessions use the agent API.
This revision covers all seven transport/lifecycle and semantic review findings,
with the subsequently authorized evidenced process-tree strategy, plus the two
P2 follow-ups: accepted-turn metadata reconciliation and forward-only pages.

Gateway supervision starts one private child with fixed argv:
`codex app-server --listen stdio://`. It is not a shared daemon, network listener
or terminal pane. No remote-dispatch bypass flags, trust edits, global config
writes, credential-file reads/copies, browser login or package installation are
performed. The child inherits normal upstream configuration and authentication;
Gateway never supplies a silent replacement CODEX_HOME. `agent setup codex`
explains setup without launching Codex or authentication.

## Readiness and transport

The JSONL duplex client performs `initialize`, `initialized`, then read-only
`account/read` with `refreshToken: false`. Initialization declares
`experimentalApi: false`, `requestAttestation: false` and
`explicitGatewayOauth: true`; no experimental client requests are sent. The
returned user agent must identify version 0.162.1. No ordinary RPC is admitted
before authentication/readiness. The private child also reads its effective
provider selection before becoming ready, retaining only necessary selection
fields. `requiresOpenaiAuth: true` with a null account
produces a host-configuration hint, not an automatic login attempt.

Numeric client request IDs route concurrent replies independently of upstream
server requests, whose original numeric/string IDs are retained for replies.
URI-safe neutral callback handles also carry a connection generation, so stale App
answers cannot authorize a new callback that reuses a native RPC ID after restart. Unknown
notifications are ignored; unsupported server requests receive a generic
method-not-supported error, never approval. JSON bytes are framed before UTF-8
decoding. A malformed envelope, malformed JSON, oversized frame, event overload
or lost connection closes the connection and requests resynchronization rather
than silently skipping/truncating transcript data.

Bounds: 4 MiB per inbound/outbound frame or streamed text/output row, 32
pending RPCs/server callbacks (callbacks also share an 8 MiB payload bound),
32 scheduled metadata-read workers,
16 queued events, 4,096 explicitly loaded
threads, 8,192 item-order anchors, and 512 parts in each reasoning summary/raw
array (with equally bounded delta indices). A request/write has a 20-second deadline.
Stderr is continuously drained through a fixed 8 KiB buffer and discarded
entirely, avoiding both pipe deadlocks and diagnostic credential leaks. Raw RPC
error messages are not forwarded or logged. Writes succeed only after bounded
`write_all` and flush completion, not queue enqueue. Callback replies keep the
original native ID and emit resolution only after that acknowledgment; failed,
cancelled or partial writes fence the connection without retry.
Child shutdown closes stdin and allows up to 46 seconds for upstream cleanup;
normal cleanup returns as soon as the child exits. If that grace expires,
Gateway attempts cleanup of proven owned descendants, allowing 250 ms for
upstream reaping before killing/reaping the root.
An ownership ledger polls every 25 ms, bounded to 1,024 process incarnations;
Linux signals only proven pidfds. macOS observes native start identities but
does not signal descendant bare PIDs, avoiding check-to-signal reuse races.
Gateway never signals process names, adopted services or a known recycled incarnation.
**Limitation:** unobserved descendants that orphan before collection, process
ledger exhaustion and unavailable native identity acquisition cannot guarantee
complete descendant cleanup. Acquisition, enumeration or signal failures are
not treated as an empty, successfully cleaned tree: unverified cleanup reports
an error and leaves the adapter Offline. This is evidenced tracking and a
conservative upstream grace, not strict OS containment. No lifecycle/service-manager
deadline is changed.
The runtime exposes `shutdown_codex` for graceful
Gateway shutdown, cancelling initialization and preventing supervisor restarts
without stopping terminal agents or adopted services.
Dropping the last external Codex manager cancels its pump/transport and initiates
the same owned-child cleanup; explicit shutdown also fences late domain events.
The HTTP server owns SIGTERM/SIGINT handling and waits for private-child cleanup
before returning or dropping MCP identity/endpoint file guards. It does not
wait indefinitely for open SSE/WebSocket clients. Bind and other startup
validation failures occur before agent supervision starts.

## Sessions, history and model selection

| Gateway operation | Pinned operation |
| --- | --- |
| Create | `thread/start` |
| Metadata | `thread/read`, `includeTurns: false` |
| Ordered session page | `thread/list` |
| First prompt in a selected saved thread | read, then explicit `thread/resume`, then `turn/start` |
| Prompt/follow-up | `turn/start` with the exact selected thread ID |
| Whole-thread fork | `thread/fork`, `excludeTurns: true` |
| Rename | `thread/name/set` |
| Delete | `thread/delete` |
| Interrupt | `turn/interrupt` with the exact observed/acknowledged thread and turn IDs |
| Catalog | authenticated `config/read` selection fields, then `model/list` |

Listing and metadata reads never resume saved sessions. Session lists use
`useStateDbOnly: true` to avoid rollout scan/repair; directory/search/order/limit
and opaque forward cursors are forwarded. `nextCursor` becomes neutral `next`;
`previous` is always null. Native `backwardsCursor` requires opposite order,
which does not satisfy the neutral same-order cursor contract, so backward
paging is not advertised or exposed. Parent filtering applies to each returned page;
a page may therefore be empty while retaining a continuation cursor. Limits are
1–200 for an explicit neutral page. Legacy list follows cursors only to its
caller-specified limit or verified end; project inventory follows to verified
end, never treating the newest 100 threads as a complete inventory.

History supports both published `historyMode` values. `paginated` threads use
bounded descending `thread/items/list` pages (upstream caps each at 100), reversed
into timeline order until the requested **mapped** tail is satisfied or verified
history end, including pages of unmapped items;
`legacy` threads use `thread/read` with `includeTurns: true`. Legacy reads are
bounded by the transport frame limit and fail instead of clipping a JSON body.
The neutral history API returns the requested tail, at most 2,000 rows; it does
not expose an invented history cursor. Unknown storage modes are Unsupported.
Traversals share a 20-second total deadline, 128-page, 20,000-raw-row and 32-MiB
actual-response-body budget, in addition to the 4-MiB frame bound. Malformed
pages/cursors, cursor cycles, RPC errors or exhausted budgets return errors, not
partial inventories or an unsatisfied history tail advertised as complete.

Producer thread/turn/item identities define stable row IDs. Item IDs are opaque,
not assumed sortable UUIDs. Presentation order uses authoritative history order
and first live observation as a fallback; shared anchors keep overlapping tails
consistent. Unix timestamps remain metadata, not order/replay guarantees. Text and
indexed summary/raw-reasoning deltas update the same rows as final items and
history; a final body replaces the accumulated delta body. Command/file/MCP
items map to tool lifecycle/result cards. Threads keep distinct streams,
ordering anchors, approval callbacks and timeline rows.

Model and reasoning effort come from actual start/resume/read responses, never
the requested selection. Catalog options use the authoritative configured
provider from `config/read` in the private child's startup cwd (a cwd-less read
would exclude project layers while `model/list` uses startup configuration);
only `model_provider`, `model` and
`model_reasoning_effort` survive the client boundary. Other configuration,
credential fields and origin/layer data are discarded, never returned/logged.
A null configured provider uses [pinned Codex's built-in `openai` default](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/config/mod.rs#L3835-L3838).
External provider-configuration changes require Gateway restart: the adapter
rejects a changed provider rather than relabeling the startup-owned model catalog.
Switching an idle model stages a catalog-validated next-turn model/effort;
`thread/resume` ignores overrides on loaded subscribed threads and is not used
for switching. The next real `turn/start` carries the chosen `model`/`effort`.
The stable [`TurnStartParams`](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/TurnStartParams.ts)
applies those overrides to this and subsequent turns. After acknowledgment,
Gateway schedules `thread/read` with `includeTurns: false`; the stable
[`Thread`](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/Thread.ts)
reports current configured model/effort for loaded threads, not per-turn telemetry.
Only that actual response (including upstream normalization) updates the manager
mirror, cached session snapshot and `SessionUpdated`. No pre-turn fake upstream
model update is emitted. The accepted local choice remains pending confirmation
until an authoritative read succeeds, but its overrides are not resent on another
turn. A failed read clears the mirror's stale model and emits a resync request;
the accepted prompt remains successful and is **not retried**. A later explicit
metadata refresh can reconcile the selection without submitting another prompt.
Reads run outside the event pump so streaming and approvals continue; revisions
fence old replies against newer turns/reads, deletion and connection replacement.
No experimental `thread/settings/updated` event is required. Busy threads must first
be interrupted. Provider changes on loaded threads are rejected, not silently
routed to OpenAI. Features declare `modelSelectionScope: next_turn` and
`modelSelectionPersistence: gateway_until_turn_start`; no experimental settings
RPC or promise of durable pre-turn upstream state. Known project directories are
derived from exhaustive budgeted session enumeration, not project CRUD. Attachments use the
existing validated host-path attachment flow, not native image upload.

## Interactions and recovery

Command/file approval callbacks map only exact one-action Allow/Deny decisions.
`acceptForSession` and policy amendments **do not** become permanent
`AllowAlways`. Additional permissions use the exact requested profile and
`scope: turn`; denial returns no permissions. Unstable file `grantRoot`
callbacks are Unsupported rather than misrepresented as one-action approval.
Rejection reasons unsupported by the published response are not silently
forwarded. Answered, expired, cancelled and cross-thread requests cannot be
answered again.

Ordinary incoming `item/tool/requestUserInput` option questions use neutral
forms and validate answers against the offered options. The generated type and
source comment label this request experimental, but the pinned server-request
declaration has no experimental gate. Gateway does not opt into experimental
client APIs or promise that upstream will emit this request in every mode.
Secret questions, complex/URL MCP elicitation and client-executed dynamic tools
are Unsupported, not disguised as ordinary forms.

An interrupt acknowledgement is not completion: only observed upstream turn
status makes the mirror Interrupted. Active-turn acknowledgements also prevent
another prompt from accidentally steering work already in progress. A late
acknowledgement cannot resurrect a turn whose completion was already observed.

On disconnect, pending calls fail, callbacks expire, busy sessions become
Unknown and clients receive a resync request. Reconnection starts a fresh
private process but never resumes arbitrary sessions, replays prompts or
replies to approvals. Authoritative metadata/history is re-read on explicit
selection. Exactly-once delivery cannot be guaranteed after a lost mutation
acknowledgement: the result is reported unknown/error, **not resubmitted**.

Per-provider flags declare only the implemented features. File-restoring revert
and staged revert remain false: Codex's transcript-only `thread/revert` does
not restore workspace files. Worktrees, modes, skills, inbox, slash commands,
compaction, background shells, durable instructions and runtime MCP injection
remain unsupported.

## Acceptance evidence and limits

Tests use in-process duplex RPC fixtures or explicitly supplied fake shell
children only. Golden payloads are synthetic examples based on the pinned
schema, not captured local conversations or credential-bearing replies.
They cover handshake/readiness, split UTF-8, malformed/oversized JSONL,
deadlines/disconnect/overload, first/follow-up prompt IDs, read-only selection,
resume/fork/rename/delete, both history modes, cursors, two interleaved threads,
reasoning/tools, callback decisions/expiration/forms, interrupt status, dynamic
model catalog, stderr flooding and private-child reaping. Additional synthetic
manager/SessionService tests cover normalized post-accept model/effort in cached
snapshots/events, metadata failure with resync and no prompt retry, delayed old
reads with live callbacks, and neutral forward-only ASC/DESC HTTP pagination
including parent filtering and opaque continuation cursors.

Real Codex execution, user credentials/databases/terminals, paired App/isolated
Herdr acceptance and macOS certification are **not performed in this round**.
The repository-required real paired App check remains deferred by explicit
user instruction; mocked protocol tests do not replace it. No App change,
installation, service restart, commit or release is part of this worker's work.
