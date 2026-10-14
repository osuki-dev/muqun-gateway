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
or authentication is nonfatal and does not disable other agents. Ready Codex is
preferred first, OpenCode second, then DeepSeek and T3. Explicit provider and
recorded session selections keep their routes; unavailable Codex never displaces
a connected fallback. HTTP and CLI discovery use the same ordering. Existing
terminal Codex sessions are never migrated; eligible new terminal tasks use the
shared startup described below.
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

When no compatible verified daemon is available, Gateway runs
`codex app-server daemon start` once. That official command starts the shared
daemon if the socket is absent and leaves an already-running daemon unchanged,
so a Codex opened later in a terminal joins the same socket and the same
threads. Gateway does not start a private `codex app-server`, and it never
stops or restarts the daemon. A dropped connection is adopted again on the
next pass. No remote-dispatch bypass flags, trust edits, global config writes,
credential-file reads/copies, browser login or package installation are
performed. Gateway never supplies a silent replacement CODEX_HOME.
`agent setup codex` explains setup without launching Codex or authentication.

## Readiness and transport

Both JSONL stdio and Unix WebSocket clients perform `initialize`, `initialized`, then read-only
`account/read` with `refreshToken: false`. Initialization declares
`experimentalApi: false`, `requestAttestation: false` and
`explicitGatewayOauth: true`; no experimental client requests are sent. The
returned user agent must be version 0.162.1 or newer. No ordinary RPC is admitted
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
Cancelled or expired read-only catalog/history RPCs do not close an otherwise
healthy connection. Once queued, abandoned reads still occupy the bounded
pending-request table until their late replies arrive or the transport closes.
The transport actor owns complete frames and still closes on actual I/O failure.
Unknown methods, thread creation/resume, turns and callback replies keep the
mutation cancellation fence. A disconnected client refuses new requests before
writing them; cached discovery and status no longer advertise it as connected.
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

Opening a requested timeline or explicitly controlling a session on the shared
transport can rejoin a session that a fresh `thread/read` reports as loaded
(`idle` or `active`). A selected `notLoaded` session can also recover after a
Gateway restart when the shared SQLite store records an accepted native create,
fork or prompt submission for its exact thread ID and verified canonical Codex
home. This evidence stores no prompt, credential or workspace path. Listing and
metadata reads do not trigger recovery, and old unregistered sessions are not
backfilled from terminal observations or transcript metadata.
Selection prepares ownership before serving a retained snapshot, including when
marking a session viewed has already populated its metadata cache. A cache hit
must not skip recovery. A rejected preparation is returned as an error while
the retained history remains untouched; it is not converted to native ownership.
Metadata caching and initial transcript hydration are tracked separately. The
first selected snapshot reads a bounded canonical history page even if a viewed
marker or resume event has already cached metadata. Successful empty history is
cached too; repeated or concurrent snapshots do not reload it. Initial hydration
merges history without replacing newer live rows, and failed reads remain retryable.
`thread/resume` carries only its exact ID and `excludeTurns: true`; it does not
apply model, instructions or permission overrides. A one-turn metadata page
(`itemsView: notLoaded`)
recovers the current turn ID without hydrating all history. Accepted rejoin
is native runtime proof even when a terminal client is also attached. Only
accepted explicit start/fork/rejoin sessions enter the live projection;
unrequested shared-server thread notifications and callbacks are ignored.
Closed/deleted/notLoaded notifications invalidate that proof. Opening a
`notLoaded` external CLI transcript without that evidence does not resume or
migrate it; its existing terminal/unobserved provenance remains authoritative.
Recorded origin permits only the resume request, never ownership: the official
server must acknowledge the same ID and may reject it when another writer owns
the thread. Such rejection does not launch a fallback process or submit a turn.
Unloading retains the evidence; successful deletion removes it. Failure to write
the local evidence after an accepted mutation is logged without reporting that
mutation as failed, preventing a retry of an already accepted prompt.

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
| Idle prompt/follow-up | `turn/start` with the exact selected thread ID |
| Explicit active-turn steering | `turn/steer` with the exact `expectedTurnId` |
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
derived from exhaustive budgeted session enumeration, not project CRUD.

Prompt attachments are restricted to Gateway uploads. PNG, JPEG, WebP and GIF
are sniffed from their bytes and sent as official `localImage` input parts on
both `turn/start` and `turn/steer`; image-only prompts are supported. Other
documents retain their validated host-path text representation. Image paths are
never substituted for image parts in prose. The adapter rechecks upload-root
confinement, readable regular files, a 16-file limit and a 25 MiB aggregate limit
before any rejoin/resume or prompt mutation. External URLs, data/file URIs,
unsupported image formats and mislabeled image files are rejected. No new
file-read route or authentication exception is introduced.

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
an ordinary idle prompt from accidentally steering work already in progress;
active steering must be explicitly requested. A late
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

## Typed tools, child sessions, and usage

The pinned `v2/ThreadItem.ts` is projected into the neutral timeline. Commands
use `shell`, retain output, `metadata.exit`, duration, source, process and plugin
script identity. A process identifier alone never means a background/detached
command. Stable command lifecycle events continue to update the original turn,
including an exit after a later turn begins. Background-terminal inventory and
control are experimental upstream and remain unavailable (`backgroundShells:
false`); inline command observation does not enable those mutations.

File changes use `edit` and `metadata.files` with the supplied path, unified patch,
change kind and optional move destination. Addition/deletion counts are computed
from that patch. MCP, dynamic tools, web searches, sleeps, image tools and function
outputs retain their typed input/output and diagnostics. Turn plan updates replace
a stable todo row; aggregated turn diffs retain the actual patch with an empty
single-file label because the patch may span several files. Compaction boundaries
are observable without advertising a user-triggered compact operation.

Collaboration calls project one `subagent` tool row per exact receiver thread ID;
each row's `child_session_id` opens that session. A completed spawn/wait call does
not imply child completion: child status comes only from its `agentsStates` entry,
or the explicit subagent activity. Provisional no-receiver rows are removed when
targets arrive. Child inventory explicitly includes the pinned subagent source
kinds; upstream otherwise defaults to interactive sessions only. Session
`parent_id`, `agent_name`, and `agent_role` come from `parentThreadId`,
`agentNickname`, and `agentRole`. No progress percentage is generated.

`thread/tokenUsage/updated` updates the session through `agent.session.updated`.
Session `tokens` is cumulative; `context_usage` describes the latest context:
`{tokens: TokensUsage | null, total_tokens: number, limit?: {context: number}}`.
Codex's cached/read and cache-write counters are included in input, and reasoning
is included in output. The adapter subtracts those subsets so the five neutral
counters are disjoint. Incomplete or inconsistent breakdowns remain unknown;
context occupancy uses upstream `last.totalTokens`, independently of the breakdown.
Only a positive reported `modelContextWindow` supplies a context limit. Cost is
unknown, not zero. Last observed usage survives independent metadata refreshes.
The stable thread metadata does not contain usage; a metadata-only warm resume
also omits restored usage replay. Until a usage notification or verified persisted
replay is observed, initial totals remain absent. Billing `account/usage/read` is
not substituted for context occupancy.

Optional `git_info` (`sha`, `branch`, `origin_url`) is the producer's Git snapshot,
not a claim about the current worktree. Existing directory-based Git file/diff
routes remain the source for live working-tree changes. Discovery extras
`typedToolItems`, `subagentSessions`, `sessionTokenUsage`, `sessionContextUsage`,
and `sessionGitInfo` describe these additive read surfaces.

Counter semantics are also verified against the pinned upstream
[`parses_cache_write_token_usage` fixture](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/codex-api/src/sse/responses.rs),
and context occupancy against `TokenUsage::tokens_in_context_window` in
[`protocol.rs`](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/protocol/src/protocol.rs).

## Asynchronous questions and steering

`agentMessage.questions` is a separate protocol from blocking
`item/tool/requestUserInput`. The former has titles/options, no server RPC request
ID, and does not stop the agent. Live native messages in an exactly identified,
owned active turn create `FormRequest.nonblocking: true`; each question uses its
own connection- and subscription-scoped form handle. Ordinary historical and
persisted messages retain the question text/options without creating reply callbacks.

After an accepted shared-daemon rejoin, opening its timeline also recovers unanswered
questions from the exact currently active turn. Paginated storage uses
`thread/items/list` filtered by `turnId`; legacy storage uses a single
`thread/turns/list` page with `itemsView: full`, requiring the returned active turn
identity to match. Recovery is bounded to 1,600 items, 16 pages, 1 MiB of retained
question/reply items, and the existing 20-second/32-MiB traversal and 4-MiB frame
limits. It publishes no partial result. Committed answer envelopes are applied before
question forms; bounded answer identities also prevent a concurrent TUI answer from
being resurrected by recovery. Turn changes, completion, disconnect, or an unload/rejoin
of the same thread fence the result. Explicit interrupt and prompt control do not wait
for question recovery. This does not make another turn, an unowned transcript, or a
persisted follower actionable.

Replies follow the pinned official TUI's
[`async_questions/state.rs`](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/tui/src/bottom_pane/async_questions/state.rs)
and [`AnsweredQuestion`](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/context-fragments/src/answered_question.rs):
`questionItemId` is the serialized `["request_user_input_async", itemId, index]`
array, and the answer is normal text inside the
`send_user_message_question_reply` envelope. Gateway sends stable `turn/steer`
with the exact `expectedTurnId`. It never invents a JSON-RPC question callback,
resumes an unrelated runtime, starts a replacement turn after a race, or retries
ambiguous delivery. Ordinary active-turn follow-ups also support `delivery:
"steer"`; queue delivery remains unsupported. Discovery reports `promptSteering`
and `asyncQuestions`, independently of `inbox: false`.

A question remains pending until the steering response or a committed matching
user reply confirms acceptance. In-flight claims prevent duplicate writes.
Another client's committed reply resolves only matching question identities in
that thread. Completion, unload, and connection loss remove live actions; neither
ordinary history replay nor an old connection handle can resurrect them. The bounded
registry offers up to 32 question handles and remembers up to 1,024 committed answer
identities until thread/turn cleanup; exhausted answer tracking suppresses new forms.
Unsupported oversized question shapes remain readable in the transcript and may be
answered through ordinary steering.

`hookPrompt` fragments are synthetic transcript content, preserving their text.
`functionCallOutput` preserves its published string or content-array output.
Image-generation tool output remains typed tool data; this alone does not claim
an inline generated-image viewer. User audio inputs remain visible as audio
attachment notices, but native audio upload/playback has no neutral adapter
contract yet. Experimental realtime audio operations are not enabled.

## File mentions

Codex declares `fileMentions: false`: its adapter does not implement file search.
The agent-session files endpoint selects the exact session owner and recorded
absolute directory, ignoring caller directory overrides. Unknown session
workspaces are rejected. It never queries another provider's default project.
Root file searches can explicitly select `agent_id`; omitted selection retains
the legacy primary-provider behavior for existing clients. Unsupported selected
providers return `feature_unsupported`, rather than falling back to OpenCode.

## Shared task startup

`codex_shared_task_start_v1` prepares eligible new Codex task sessions through
the connected shared daemon's official `thread/start` API. The existing
per-invocation project trust, developer instructions, Muqun MCP entry, model,
effort, approval and sandbox choices become thread parameters. The Gateway
keeps its subscription and persists the existing task title with the official
`thread/name/set` API before launching the TUI. New threads are lazy-persisted;
even attaching to a loaded thread with `thread/resume` requires its stored
record. This acknowledged metadata write materializes that record without a
model turn or a fabricated user message. The TUI starts with
`--remote unix://<verified-socket> resume <exact-thread-id>`. The original
goal is delivered once through the existing terminal startup path: Herdr waits
for TUI readiness before `agent.prompt`; backends that accept launch-time goals
keep their argv delivery. Preparing the thread does not submit a turn.

The MCP helper uses an immutable task binding because the shared daemon is
not a descendant of the task pane. Its per-thread configuration contains the
helper command, not a bearer token. Task completion cannot select a later
assignment that reuses the pane. Task records retain their existing pane
presentation and instance-bound follow-up behavior; native conversation
controls observe and act on the same shared thread through the official API.

This path requires a connected verified local daemon, a known absolute cwd,
and the launcher's existing per-invocation trusted-folder override. The last
condition prevents `thread/start` from automatically persisting project trust.
Custom executable/environment selections and unsupported CLI settings retain
the existing terminal startup path and record why shared startup was skipped.
That reason is retained in the existing task event history when the task becomes
working, explicitly identifying terminal mode and its linked-terminal controls.
An uncertain creation or title-persistence result never falls back to a second launch. A task
cancelled during preparation does not submit its goal. Existing conversations
are never migrated or restarted by this feature.

Validation of a release candidate must include an isolated paired App task:

1. Create a new Codex task in an allowed trusted test directory. Check that the
   returned native thread and the TUI's session identity match, both are loaded
   in the same daemon, task instructions and MCP are available, and the user's
   prior pane retains focus.
2. Observe the actual response and task completion in both views. Send one
   task follow-up and one native conversation follow-up after the previous turn
   settles; verify each appears once in the same thread.
3. Trigger a real supported question in the isolated CLI session. Confirm the
   native App displays the official pending/async form and the correct thread
   receives the explicit answer. Ask the user before answering trust/approval
   requests; absence of a question is not a passing question test.
4. Cancel or remove the isolated task and verify its helper cannot renew or
   use task authority; reuse of the pane must not transfer the old credential.
5. Disconnect only the test client and verify the shared daemon and unrelated
   sessions remain running. No active user session is resumed, interrupted,
   migrated or stopped for this validation.
