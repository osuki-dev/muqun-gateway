# T3 Code V2 wire protocol

The Gateway supports orchestration **protocol 2 only**. The public descriptor
must explicitly advertise 2. Missing (legacy V1), explicit V1, unknown, and
malformed values are rejected before exchanging a one-time pairing credential.
Upgrade older T3 Code servers before connecting. No App API change is required.

V2 reference: stable **v0.1.0** (released 2026-10-15), commit
`81493efb71e8e8244f95d3e9f71d8369654ab151`, which makes Orchestrator V2 the
stable, V2-only server. Its diff against the nightly this implementation was
written to (`v0.0.46-nightly.20261013.3042`, `5d6e4192`) is contract-additive
only: every RPC, command and endpoint identifier the adapter uses is
unchanged; the one new event type, `thread.pull-request-link-updated`, is
accepted and ignored (`validate_event`).
Sources are the pinned `packages/contracts/src/orchestrationV2.ts`,
`environmentHttp.ts`, `project.ts`, `apps/server/src/ws.ts` and
`apps/server/src/orchestration-v2/` handlers. Synthetic, trimmed fixtures and
source provenance live in `fixtures/v2/`; they are not live captures. No claim
of real provider/App acceptance follows from these tests.

### Transport and lifetime

The same client owns bearer authentication, ticket minting, Effect RPC and
socket cancellation. V2 supplies `orchestrationProtocol=2` to
the ticket-authenticated WebSocket query and
`x-t3-orchestration-protocol: 2` to orchestration HTTP reads. RPC names remain
`orchestration.dispatchCommand`, `subscribeShell` and `subscribeThread`.
Project mutation is **HTTP POST `/api/projects/mutate`**, using `ProjectMutation`,
not an orchestration command. Existing URL/bearer persistence is unchanged.

Reconnect checks the descriptor's environment identity and protocol before
ticket/auth use. A changed identity/protocol makes the old driver unhealthy
and requires fresh runtime adoption; stream cursors are not reused across
that generation. V2 installs a fresh HTTP snapshot sequence before subscribing
with `afterSequence`. Sequence gaps alone are not errors: these streams filter
a global log. Duplicate events are discarded. Known malformed events or lost
streams trigger resync; unknown future event types advance the cursor without
inventing domain events. Cursor advancement follows validated forwarding.
Marked `resolvedRepositoryIdentityRoots` shell snapshots are metadata-only
enrichment, not application-log baselines. They merge only repository identity
for matching, still-existing projects in the supplied roots. Their high-water
sequence never advances the replay cursor or installs stale thread rows, so an
enrichment at 12 cannot suppress application replay at 11 and 12.
One-time pairing exchanges and ambiguous mutations are never automatically
resubmitted after a lost response. A new pairing token may be needed when an
exchange's result is unknown.

### Commands and existing App API

`thread.create` carries required `createdBy: "user"`, `creationSource: "mobile"`,
`modelSelection`, `runtimeMode`, `interactionMode`,
`branch: null` and `worktreePath: null`. Prompts use a flat command:

```json
{"type":"message.dispatch","createdBy":"user","creationSource":"mobile","commandId":"fixture-command","threadId":"fixture-thread","messageId":"fixture-message","text":"Prompt","attachments":[],"dispatchMode":{"type":"start_immediately"}}
```

The default is `start_immediately` when no root work is active, otherwise
`queue_after_active`. It never silently steers, restarts or interrupts work.
A concurrent rejection is returned, not retried under another mode.
Explicit `queue` uses the same queued/start decision; explicit `steer` uses
`steer_active` with `targetRunId` after checking the active root's matching live
provider session and `supportsActiveSteering`. Queueing during a run requires
`supportsQueuedMessages`. A provider advertising only interrupt/restart steering
is refused. Interrupt targets the
current authoritative **root run ID**, not a V1 turn or provider subrun.
Rename uses `thread.metadata.update`; model changes use
`thread.model-selection.set`. Pinned upstream planning explicitly performs
provider-switch planning for that command, so V2 can switch enabled catalog
providers.

Approval/form replies use `runtime-request.respond`. Only pending requests
with a matching request item are actionable. Live replies also require the
matching active provider session/thread identity (and active provider turn,
when specified). Resolved, expired, cancelled and `not_resumable` requests
cannot be clicked. The neutral choices map **only** to offered `accept`,
`acceptAlways` and `decline`; `acceptForSession` is not misrepresented as
permanent approval. Nonempty approval reasons return Unsupported: V2 has no
reason field. Dynamic-tool/auth-refresh requests, attached answers and
multiselect answer encoding are not supported.

`responseCapability: {"type":"message"}` is supported only for normal
user-input questions. The pinned `Orchestrator.ts` handler validates answers,
resolves the request and dispatches its generated answer in the same event
transaction. The Gateway sends `runtime-request.respond`; it does not replace
that operation with an ordinary prompt. Required questions, offered choices
and string answer shapes are validated before submission.
Custom answers default to allowed when `allowCustomAnswer` is omitted, including
questions offering options; only explicit false restricts answers to those
options. Live editable questions carrying `initialAnswer` distinguish a missing
required answer from an explicitly provided empty/whitespace string and preserve
that string verbatim. Message-capability replies still require nonblank text.

These adaptations stay behind the existing Gateway session, timeline,
permission and form API without requiring an App API change. Older T3 servers
must be upgraded to a server that explicitly advertises protocol 2.

### Native projection and bounded history

HTTP reads and V2 socket snapshots carry `{snapshotSequence, projection, ...}`;
events carry `{kind:"event", sequence, event}`, with sequence on the **outer**
envelope. The native projection contains runs, nodes, provider sessions,
runtime requests, messages, turn items, visible turn items, checkpoint scopes
and checkpoints. `message.updated`/`turn-item.updated` are entity replacements,
not text deltas.

The Gateway uses `/threads/:id/bounded`, preserves server-authored
`visibleTurnItems.position` order **within each response** (including inherited
rows), and follows
opaque `/threads/:id/history?cursor=...` pages for timeline reads. Historical
pages merge rows only: they never replace live control state or its sequence.
Upstream `threadHistoryPaging.ts` renumbers every page and recent window from
zero. Positions therefore cannot be compared between responses. Older cursor
pages join at the chronological prefix by `(sourceThreadId, sourceItemId)`;
rolling windows join through shared identities. A bounded client-owned order
cache assigns stable labels without changing held rows' order keys when older
history arrives. An older in-flight history read also retains any newer cached
live entity payload on overlap, without installing its page control state.
Each thread incarnation retains one accepted-complete sequence watermark.
An identity absent from that complete projection cannot be inserted by a read
at or before its watermark: delayed history must not resurrect remotely rolled
back/deleted rows. This uses retained identities plus one watermark, not an
ever-growing list of tombstones. Legitimate older pages remain mergeable before
complete authority exists, and retained older identities remain readable.

Stale historical responses carry only effective window coverage, even if their
original response claimed end of history. Stale live/control snapshots are
rejected before forwarding or manager reconciliation, so they cannot prune new
rows, replace fresh session/request state, or advance a stream cursor.
In-flight HTTP reads capture a per-thread incarnation before I/O; queued
projections carry that same internal incarnation. Forgetting/recreating a thread
invalidates old deliveries and starts fresh deletion/order state, while a
replacement client has its own namespace. These internal generation/coverage
markers are neither App contract fields nor persisted credential formats.
Conflicting overlaps or a fresh partial window with no shared identity produce
an explicit ordering/resync error: bridging history or a complete read is needed;
the Gateway never guesses ordering from text, timestamps or workspace paths.
The cache uses LRU retention for 128 thread orders and a 32 MiB serialized-data
budget, including native rows awaiting delivery. In-flight HTTP preparations pin
their incarnation. Evicting an inactive order never relabels a retained mirror
window: live reconciliation asks for resync until an explicit refresh atomically
accepts its replacement. A 20,000-row or 8 MiB order window compacts old payloads
while retaining the incoming rows and prefix/tail anchors. Older cuts that lost
their canonical replacement payload require refresh; they cannot replace newer
text or resurrect rows removed by accepted complete coverage.

V2 implements the existing earlier-timeline API with the native opaque cursor.
Rows from a large native page that did not fit the requested visible tail are
delivered before requesting its next cursor. Dropping a prepared read consumes
neither these pending rows nor the upstream cursor. Only verified native end
with no pending rows returns successful exhaustion. An older prefix page never
claims deletion authority over newer retained rows.

Cycles and oversized cursors still fail explicitly. A walk is bounded to 64
pages and 16 MiB of actual response bytes, but a useful partial window retains
`hasMoreHistory` and its unconsumed cursor instead of refusing the session.
Each response is read under the remaining wire allowance, including chunked,
pretty-printed and escaped JSON; an individual response over 16 MiB remains
refused. A bounded snapshot upserts its window without deleting held older rows.
Only complete coverage or explicit deletion permits removal. A newly confirmed
`rolled_back` run in a partial projection requests explicit refresh while keeping
the reader's old window, rather than forcing a whole-conversation download.
The refresh fences older in-flight cuts and accepts a fresh canonical window;
partial coverage never pretends to identify every remotely deleted row.

For live changes, the current implementation refreshes the authoritative
bounded projection instead of inventing visible positions from local ordinals.
The returned snapshot sequence covers already-committed updates; queued older
events are deduplicated. This is correct but adds HTTP reads during streaming;
real-provider throughput has not been measured. Timeline text/reasoning, tool
cards, diffs, plans, notices/errors and representable requests map to the same
neutral parts as other agents.

### Honest optional capabilities and remaining acceptance

Attachments remain **ByPath**, with native wire `attachments: []`. No native
upload, worktree, mode, staged-revert, compaction or shell-control feature
is claimed. `sessionPagination` is false: internal V2 history loading is not the
App's session-page port. `orchestrationProtocols: [1,2]` is additive metadata.

V2 declares `inbox` and `inboxEdit`: native queued runs supply the shared App
inbox. Native cancel/edit/promote commands retain run identity; edits omit
attachments to retain the native list. Promotion requires active native steering
and the authoritative root run id. No Gateway queue or automatic retry is added.
These commands are checked against released v0.1.2 (`044f072`); real paired App
acceptance remains separate from protocol fixture tests.

V2 **revert is disabled** and returns Unsupported. Upstream
`checkpoint.rollback` takes exact `scopeId`, `checkpointId`, `restoreFiles`,
and its execution restores provider conversation and files, but file restoration
also requires workspace-isolation proof. The Gateway has not established the
existing App message-target-to-checkpoint mapping and verified completion/failure
contract across providers. It neither guesses a turn count nor reports command
receipt as successful restoration. V2 cumulative VCS-diff queries are likewise
unsupported; legacy turn-count queries are not sent.

Local fake HTTP/WebSocket and native-fixture tests cover negotiation, pairing
guards, commands, nonretry, model switching, request validation, sequence/resync
and bounded history. Live provider and real paired-App checks were not run in
this implementation task: no user credentials, agents or live services were
authorized. Production acceptance and throughput measurement remain separate.
