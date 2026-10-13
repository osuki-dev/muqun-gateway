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

Gateway first connects to an existing, verified local Codex daemon on Unix.
Discovery uses `CODEX_HOME/app-server-control/app-server-control.sock` (the
upstream default home when `CODEX_HOME` is absent), verifies its official
hashed alias under the fixed system temporary root, the current-user-owned
0700 directory and 0600 socket, and the connected peer UID. The WebSocket
handshake runs over that Unix socket. Initialization verifies both the pinned
version and exact canonical `codexHome`. Gateway never creates, starts,
restarts, updates or stops the shared daemon. An adopted connection reports
origin `adopted` and endpoint `unix://shared-codex-app-server`, without exposing
its host path. `codex_shared_daemon_v1` and the agent's `sharedDaemon` feature
advertise this support (Unix only). Existing Gateway authentication is unchanged.

When no compatible verified daemon is available, Gateway starts its existing
private child with fixed argv `codex app-server --listen stdio://`, reporting
origin `spawned` and endpoint `stdio://private-codex-app-server`. It does not
migrate threads between transports or replace a healthy private connection
when a daemon later appears. No remote-dispatch bypass flags, trust edits, global config
writes, credential-file reads/copies, browser login or package installation are
performed. The child inherits normal upstream configuration and authentication;
Gateway never supplies a silent replacement CODEX_HOME. `agent setup codex`
explains setup without launching Codex or authentication.

## Readiness and transport

Both JSONL stdio and Unix WebSocket clients perform `initialize`, `initialized`, then read-only
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
Private-child shutdown closes stdin and allows up to 46 seconds for upstream cleanup;
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
Dropping the last external Codex manager cancels its pump/transport. Only the
private transport initiates owned-child cleanup; shared disconnect drops just
the connection without shutdown RPCs, signals or thread-unsubscribe calls.
Explicit shutdown also fences late domain events.
The HTTP server owns SIGTERM/SIGINT handling and waits for private-child cleanup
before returning or dropping MCP identity/endpoint file guards. It does not
wait indefinitely for open SSE/WebSocket clients. Bind and other startup
validation failures occur before agent supervision starts.

## Sessions, history and model selection

Opening a requested timeline or explicitly controlling a session on the shared transport rejoins it only if a fresh
`thread/read` proves that daemon already has it loaded (`idle` or `active`).
`thread/resume` carries only its exact ID and `excludeTurns: true`; it does not
apply model, instructions or permission overrides. A one-turn metadata page
(`itemsView: notLoaded`)
recovers the current turn ID without hydrating all history. Accepted rejoin
is native runtime proof even when a terminal client is also attached. Only
accepted explicit start/fork/rejoin sessions enter the live projection;
unrequested shared-server thread notifications and callbacks are ignored.
Closed/deleted/notLoaded notifications invalidate that proof. Opening a
`notLoaded` external CLI transcript does not resume or migrate it; its existing
terminal/unobserved provenance remains authoritative.

The shared transport supports live callback replay, but a warm metadata-only
resume does not replay historical token totals. Reading history is not evidence
of runtime ownership. CLI launches with non-whitelisted `-c` overrides may use
an embedded app-server and therefore remain outside the shared daemon.

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
would exclude project layers while `model/list` uses startup configuration).
Shared connections use the daemon's default configuration scope;
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

Incoming native `userMessage` image URLs (including data URIs) and `localImage`
paths are preserved in the neutral row's `attachments`; image-only messages keep
their row. On authenticated transcript egress, typed local image references are
converted to bounded, content-sniffed data URIs. These references are user image
input variants, never paths extracted from ordinary text. No arbitrary-file read
API is added. `image.fileId` references have no download contract in this adapter.

`codex_transcript_images_v1` also declares optional terminal prompt `attachments`.
Gateway uses the backend's explicit Codex conversation ID and process incarnation,
validates the exact rollout's `session_meta.id`, then reads at most the last 16 MiB
and 64 user events. It never finds conversations by working directory or newest
file. Only unique complete prompt-text matches with matching numbered image count
and increasing producer order are enriched. Ambiguous repeats, truncated prompts,
unsubmitted composer text and unavailable images retain their original text.
The process/conversation binding is rechecked before returning image bytes.

The rollout reader understands typed `UserMessage` events, older `user_message`
events, and encoded user `response_item` images paired with their typed event by
exact text and image count. Local image files must be regular, opened without
following a final symlink, and recognized by magic bytes. Limits are 2 MiB per
image and 8 MiB of encoded images per response; oversized or invalid files are
omitted. Images ride the existing authenticated/encrypted transcript response,
with no cached filesystem access grant or separately reusable asset URL. Rollout
search uses the Gateway's `CODEX_HOME` or default Codex home; an externally
configured different home or missing backend identity leaves terminal text intact.

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

Default tests use in-process duplex RPC/WebSocket fixtures, isolated socket
paths or explicitly supplied fake shell children. They never discover the
user's daemon. An explicitly ignored read-only probe can verify an existing
daemon with a supplied home; it initializes, reads one loaded-thread page and
disconnects, without resuming or controlling any thread. Golden payloads are synthetic examples based on the pinned
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
