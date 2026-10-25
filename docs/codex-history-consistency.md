# Codex history consistency review

Status: implementation under test; paired-device acceptance is still required
before declaring the recurring production failure fixed.

## Original failure mechanism

- `CodexDriver::item_position` assigns a presentation position from first live observation.
- The previous `set_history_order_at` treated retained positions as immutable anchors. Reversed anchors or an exhausted integer gap rejected an otherwise valid history page.
- Codex 0.164.0 upstream `thread_history.rs` explicitly documents delayed, out-of-order command completions. Arrival order therefore cannot be authoritative history order.
- Producer thread/turn/item IDs already produce stable row IDs. The adapter separately rewrites `message_id` into a numeric presentation group. This conflates grouping/reference identity with order.
- The previous `prepare_timeline` deferred cursor acceptance but mutated ordering during preparation. The cursor transaction did not cover every state changed by the read.
- Earlier history previously committed its adapter cursor before the service awaited mirror insertion, then returned the original rows rather than mirror-stamped accepted rows.
- App upserts reject lower row revisions. A correct transport alone cannot repair inconsistent revisions or competing Gateway writers.

These findings identify concrete defects. They do not establish that every reported occurrence has the same cause; the affected real session still needs trace-driven acceptance.

## Rejected experimental repair

The experimental full-turn reread plus asynchronous mirror reindex was removed before deployment. Two focused tests passed, but they did not prove concurrent snapshot/replay consistency.

Reasons to reject it:

- Order changed before the mirror correction was committed.
- In-flight prepared rows and replay events could carry an older projection.
- A generic mirror callback had to recover producer identity by parsing formatted row IDs.
- App-visible permission references required additional repair.
- Reading a complete large turn on every conflict could add substantial work.

Do not suppress the protocol error, repeatedly clear caches, replace content with an older authoritative read, or call fixture success production acceptance.

## Target ownership and transaction

Keep provider interpretation in the Codex adapter and use the existing session/mirror acceptance infrastructure. Avoid a second cache-repair pipeline or a new service.

A session has one serialized acceptance path for live updates, prepared history and ordering corrections. Network reads happen outside the acceptance critical section. Other sessions remain independent.

1. Capture a session incarnation and read revision before a history request.
2. Fetch and validate a bounded page without changing accepted order or cursor state.
3. Submit the candidate with explicit producer identities, canonical order evidence, completeness information and the proposed next cursor.
4. Under one session acceptance boundary, reject obsolete incarnations; retain newer live content; merge canonical order evidence; update affected presentation keys and references; accept the cursor; assign emitted row revisions.
5. Return and broadcast the same accepted rows. Publish only after commit.

Stable identities never depend on position, display names, timestamps or notification arrival order. Internal producer identity must be explicit, not reconstructed from a formatted row ID. Changing the public identity/order fields is a separate contract decision; a wire migration is not a prerequisite for fixing atomic acceptance.

## Ordering policy

- Canonical provider arrays determine the order they actually cover.
- Partial pages must not be interpreted as complete turns. Respect `itemsView`.
- Live events without canonical position are provisional; their arrival order cannot become an immutable constraint.
- Prefer an ordered identity sequence over a finite integer-gap allocator. Reindex the bounded affected turn when needed, without changing row identities or contents.
- Preserve rows absent from a partial response. Missing data does not prove deletion.
- Use existing cursor-backed pages first. A coalesced, bounded authoritative reread is a recovery path only when order evidence cannot be reconciled; it must not block unrelated sessions or trigger an unbounded retry loop.
- A failed or cancelled candidate must not consume its cursor or partially publish a new order.
- MCP lineage inspection remains independent of App history state and pagination.

## Required validation

Exercise the complete service acceptance path, not only the ordering helper:

- Completion arrives before a missing earlier start; late completion belongs to an older turn.
- Overlapping, partial and disjoint pages; duplicated/malformed upstream identities.
- Live text advances while a historical request is in flight: latest contents survive.
- Two reads finish in reverse order; an old response arrives after an order correction.
- Cancellation before and at cursor/mirror acceptance; the same page remains fetchable when uncommitted.
- Reconnect and session deletion/recreation reject old candidates.
- Reordering preserves permission references, stable list identities and idempotent replay.
- Same item IDs on different sessions/computers never collide.
- Long histories remain bounded without dropping unconsumed pages or disconnecting the primary stream.
- Paired App acceptance: repeatedly open the affected Codex session, page backwards during live output, leave/re-enter and reconnect. Check message continuity, titles, ordering and viewport behavior.

Run the final Gateway checks and real App acceptance before replacing the diagnostic build or deploying to the other computer. Passing focused tests alone is insufficient.

## Implemented acceptance boundary

- History and complete-turn arrays build private order candidates. A typed
  rebase condition triggers a bounded canonical read of only the affected turn;
  it does not parse a human-readable protocol error or mutate accepted state.
- Deferred commits validate the retained cursor revision and per-thread order
  epoch. A completion that wins while a page is in flight rejects the old
  candidate without consuming its cursor.
- A session projection gate serializes Codex live item projection with mirror
  acceptance. Network reads run outside that gate. The cursor, order map,
  retained rows, permission references and replay events are committed in one
  synchronous mirror write boundary.
- Earlier-page responses use accepted mirror revisions while preserving producer
  ordering in their identity/order fields. Upstream sequence values are not
  reused as Gateway revision numbers. A newer live row wins over historical
  contents sampled before it.
- Completed collaboration items replace their own provisional child-row
  identities within the same transaction. Removal and replacement are replayed
  in order; unrelated rows and stable child identities remain intact.
- Unload no longer holds the history-state mutex while acquiring the order
  mutex, eliminating the inverse lock order against history acceptance.
- Exhausting the item-order bound refuses the affected operation without
  stopping the shared Codex connection.

## Remaining acceptance limits

The order cache remains bounded to 8,192 retained producer items across the
adapter. There is no new automatic eviction of anchors still referenced by a
mirror or retained page: clearing these independently would recreate ordering
inconsistency. Long-history capacity behavior must be reported as a limit, not
as unlimited retention or a completed memory-pressure optimization.

The regression tests cover staged canonical repair, preservation of newer live
content, cursor acceptance, epoch rejection, permission references and replay,
and provisional child replacement. They do not substitute for repeatedly
opening and paging the affected real conversation during ongoing output.
