# Managed attempt lifecycle and safe replacement

Status: implementation contract with Gateway store implementation in progress;
the complete route/native/App feature is not a claim about shipped behavior. This document refines
`task-workflow-implementation.md`'s conservative reservation rule. Implement the
contract across Gateway, native backend, and App before claiming replacement support.

## Problem and scope

A start can create a workspace and then definitively refuse to create an assistant.
Currently any recorded resource retains the lead reservation forever. A successfully
started assistant also retains its reservation after exit. Both prevent a new lead
on the same task; descendant attempts consume ancestor budgets indefinitely.

Separate execution reservations from resource ownership. Release a reservation only
when startup was durably prevented or the native owner proves that the exact child
process exited. Keep workspaces, worktrees, attempts, operations, results, and human
reviews as history. Release does not mean task completion, successful work, process
termination, workspace deletion, or permission to replay an earlier request.

This increment covers refusal before assistant creation and observed owned-child
exit. Native restart, lost ownership, evicted exit evidence, and an unknown launch
outcome remain unknown and reserved. There is no force-release switch.

## Additive durable schema

Add `Attempt.lifecycle` with the following structure; retain all existing fields:

```json
{
  "launch_phase": "not_dispatched",
  "reservation": "reserved",
  "native_owner_epoch": null,
  "release": null
}
```

`launch_phase` is `not_dispatched | dispatch_claimed | launch_confirmed | legacy_unknown`.
It describes durable evidence, not assistant activity. `reservation` is
`reserved | released`. A released attempt never returns to reserved; replacement
always creates another attempt. `native_owner_epoch` is an opaque native supervisor
incarnation identifier, paired with the existing immutable `instance_id` launch ID.

`release`, non-null only after release, is:

```json
{
  "reason": "startup_not_dispatched",
  "evidence": {
    "kind": "gateway_dispatch_fence",
    "start_operation_id": "opaque operation ID"
  },
  "reconciliation_operation_id": "opaque operation ID",
  "released_at_ms": 1
}
```

Allow exactly these reason/evidence pairs:

| Reason | Evidence kind | Additional required evidence fields |
| --- | --- | --- |
| `startup_not_dispatched` | `gateway_dispatch_fence` | `start_operation_id` |
| `startup_refused_without_process` | `native_start_refusal` | `start_operation_id`, `native_owner_epoch`, `native_receipt_id` |
| `owned_process_exited` | `native_exit_tombstone` | `instance_id`, `native_owner_epoch`, `native_receipt_id` |

Evidence is constructed by the service from trusted storage/native responses; clients
cannot supply it. Native receipt IDs and owner/launch IDs are nonempty bounded opaque
strings (maximum 256 UTF-8 bytes, no control characters). Timestamps are observational;
they do not establish identity or authorize release. Preserve original start outcomes,
including `unconfirmed`, rather than rewriting historical delivery facts.

Migrate old attempts to `legacy_unknown/reserved`. The existing definite-refused,
completely empty-resource exception may migrate to released only if its original
operation contract establishes that no process could have started. Never infer this
from a missing `instance_id` alone. Existing acknowledged bindings without an owner
epoch cannot gain exit proof by consulting a reusable pane or alias.

The store implements this compatibility rule through a default for missing JSON
`lifecycle`, without rewriting old records or changing SQLite's existing table
schema. Old attempts remain reserved. Newly prepared attempts start at
`not_dispatched/reserved`. Preserve the existing empty-resource, pre-dispatch
refusal optimization: it now records a durable `startup_not_dispatched` release
in the refusal transaction. Its `reconciliation_operation_id` refers to that
start operation because no separate reconciliation request occurred. Partial
resource refusals require explicit reconciliation.

## Startup phase ordering

1. In the existing preparation transaction, reserve the attempt and create its start
   operation with `launch_phase=not_dispatched`. Resource creation and checkpoints
   continue through the existing service. No new launcher is introduced.
2. Immediately before the sole native assistant-start call, atomically check task and
   ancestor pause policy, reservation, operation state, and phase; change the phase
   to `dispatch_claimed`. This transaction is the dispatch fence. Only its successful
   caller may invoke native startup. Never hold a database lock over native I/O.
3. Persist successful immutable launch identity and owner epoch with
   `launch_phase=launch_confirmed` before acknowledging the start. The native start
   reply must bind the existing start operation ID to that exact generation.
4. A refusal before the dispatch fence can carry `gateway_dispatch_fence` proof.
   Reconciliation must finalize/fence the start operation in the same transaction
   that releases the reservation, so a suspended execution continuation cannot later
   dispatch. Every continuation rechecks the fence after any external await.
5. After the fence, release requires an explicit native `effect=not_started` receipt
   for this start operation and owner epoch, or matching owned-exit evidence.
   A generic refusal, unsupported code, readiness timeout, missing ID, or transport
   error is insufficient. Successful process creation followed by readiness failure
   is not a no-process refusal.
6. A crash after claiming dispatch but before sending is intentionally conservative:
   the recovered operation is unconfirmed and reserved. A crash before the fence may
   be reconciled without dispatch. Any uncertain workspace creation remains separately
   recorded and visible; releasing an assistant slot must not erase resource uncertainty.

When placement returns after a no-dispatch reconciliation won the fence, the store
may checkpoint workspace/tab/pane/worktree references into the refused historical
operation. It must reject incoming launch identity/target, retain the released
reservation and refused outcome, and publish a revision/event. Execution must stop
when that checkpoint returns refused; it must not proceed to another native step.

A native no-process receipt means spawn was never attempted, or the owner has verified
that no child was created. If a child might exist, return uncertainty instead. The
receipt must remain distinguishable from a prompt's zero-byte input refusal.

## Native lifecycle evidence

Add a read-only native port, separate from readiness polling:

```text
agent.lifecycle_bound({ expected_launch_id, expected_owner_epoch })
-> { type: "agent_bound_lifecycle", launch_id, owner_epoch,
     state: "live" | "exited" | "unknown", receipt_id? }
```

`live` requires ownership of that exact current child generation. `exited` requires
retained supervisor evidence of observed child exit/reaping for that generation;
`receipt_id` is mandatory and immutable. `unknown` is required after native restart,
ownership loss, or tombstone eviction. An epoch mismatch cannot return `exited`.
A timeout, unavailable backend, malformed response, or mismatched identity is also
unknown to Gateway. Existing `agent.get_bound` errors and visible terminal status
are not lifecycle proof. Exit of the directly owned assistant does not assert that
all descendant processes have stopped or release/delete filesystem resources.

Maintain a server-owned registry keyed by `(owner_epoch, launch_id)`, independent
of pane lifetime. Register the successful direct spawn before its watcher can finish.
The watcher records exit only after successful wait/reap, before UI event delivery;
`WaitFailed`, runtime invalidation, shutdown, or a discarded `RuntimeDied` event is
not proof of exit. On a wait failure retain unknown evidence. Reserve registry capacity
before spawn and refuse before process creation when full; never evict live records.
A live observation means an owned, not-yet-reaped generation, not readiness.

Use a bounded tombstone store with a documented retention limit. Eviction is allowed
only by changing future answers to unknown, never by manufacturing exit evidence.
Publish explicit capability `agent_lifecycle_bound`; Gateway's adapter exposes
`instance_bound_lifecycle` only after verifying this contract. Never infer it from a
version string. Native start replies additionally return `owner_epoch`; structured
no-process refusals return `operation_id`, `owner_epoch`, `receipt_id`, and
`effect: "not_started"`. Native owners must not reuse an epoch after restart. Echo the current owner epoch
in capability discovery and start receipts. A lost start receipt with no known launch
ID cannot be resolved through this endpoint: retain its reservation. An exact
operation-to-launch/no-process evidence query would be a separate future contract.

## Authenticated reconciliation API

```text
POST /api/sessions/{session_id}/work/tasks/{task_id}/attempts/{attempt_id}/reconciliations
```

```json
{
  "request_key": "bounded key",
  "expected_revision": 12,
  "expected_instance_id": null,
  "expected_native_owner_epoch": null
}
```

Require paired-device authority scoped to this session/task/attempt. Both expected
identity fields must exactly match stored values, including null; mismatches refuse.
No local assistant grant may reconcile, review, replace, or release itself. Keys use
the existing 128-byte bound. Reject unknown fields and oversized requests.

Add operation kind `reconcile_attempt`. Deduplicate by authenticated actor, session,
kind, and request key; digest the task ID, attempt ID, revision, and expected identity.
Check an exact replay before revision validation; changed payloads conflict. The
receipt is an immutable reconciliation result, not an instruction to execute again:

```json
{
  "value": {
    "operation_id": "opaque ID",
    "attempt_id": "opaque ID",
    "observation": "unknown",
    "reservation": "reserved",
    "release": null,
    "task_revision": 13
  },
  "replayed": false
}
```

`observation` is `not_started | live | exited | unknown | already_released`.
Return HTTP 200 for a completed check, including live/unknown; neither permits a
replacement. Existing bounded error envelopes handle invalid input, scope mismatch,
revision conflict, and unavailability before a durable receipt exists. Record intent
before the native query. Restart of an interrupted reconciliation produces an
unconfirmed operation; request-key GET lookup can return that operation, and must not
query native or resend anything. A new explicit check uses a new request key.

Extend the existing authenticated receipt lookup to `kind=reconcile_attempt`: return
the original reconciliation receipt when finalized, otherwise its current operation
with an explicit `receipt_type: "operation" | "reconciliation"` discriminator for
this new kind. A 404 remains absence of a committed receipt, not proof of no in-flight
request. GET reads never release reservations.

The exact new-kind `value` envelope is either
`{ "receipt_type": "reconciliation", "receipt": <ReconciliationReceipt> }` or
`{ "receipt_type": "operation", "operation": <Operation> }`.

## Release transaction and local authority

Perform native lookup outside both database and grant-registry locks. Before commit,
acquire the existing grant guard, then the database transaction, in that order (the
same order as local result commit). Revalidate revision, attempt binding, dispatch
phase, and operation under these locks. Fence any unclaimed startup, persist release,
reconciliation receipt, revision, and change cursor atomically. While still holding
the grant guard after a successful commit, remove all local grants for the exact
attempt from the in-memory authority registry before letting another local result
authorization proceed. Then unlock. Split in-memory revocation from context-file
cleanup rather than invoking a helper that performs filesystem I/O under the
database transaction.

If commit fails, do not revoke or publish a release. A crash after commit invalidates
all in-memory grants on Gateway restart. File cleanup occurs after authorization is
revoked; inability to remove an owned context file does not restore authority. Every
future grant issuance must reject released attempts under the same guard/database
ordering. Captures already in progress recheck authorization before final commit.
Historical result reads by paired users remain available; local writes after release
are refused. Never retain the database transaction across native or filesystem I/O.

Count only reserved attempts in both local lead/worker limits and recursive ancestor
budgets. Use one shared predicate. Starting a replacement still checks policy,
revision, pause, and budgets through the existing start service; reconciliation never
starts it automatically. No stop, delete, prompt resend, or automatic retry belongs
in either reconciliation or release.

## App interaction

Show the historical attempt with its actual start outcome and resource links. For a
blocked replacement, offer **Check assistant lifecycle**. Explain that this checks
whether a replacement can safely start and does not stop the old assistant. Journal
the reconciliation request before POST, hydrate its guard on restart, and recover a
lost acknowledgment by the read-only receipt route.

For `live`, show **Assistant still running** and keep replacement disabled. For
`unknown`, show **Could not verify assistant exit** with connection/native-upgrade
context where applicable; retain the reservation and offer another explicit check.
Never offer a force-release control. For released, show the evidence category and
**Start replacement** as a separate deliberate action with a fresh request key and
current revision. Keep prior results and terminal snapshots pinned; do not navigate
to a terminal or label the task complete. A paused task can reconcile, but cannot
start a replacement until the user resumes it.

## Implementation ownership and acceptance tests

Gateway store integration APIs are now available:

- `claim_start_dispatch(session, start_operation_id, now)` establishes the fence.
- `confirm_start_launch(session, start_operation_id, binding, owner_epoch, now)`
  records generation/epoch and rejects an already recorded no-process refusal.
- `record_start_refusal(session, start_operation_id, owner_epoch, receipt_id, now)`
  records trusted no-process evidence while dispatch is claimed; its read-only
  counterpart is `get_start_refusal`.
- `prepare_reconciliation(actor, session, task_id, attempt_id, ReconcileInput, now)`
  returns the operation; call `begin_operation` before reading native evidence.
- `finish_reconciliation(session, operation_id, expected_current_revision, evidence,
  now)` validates stored scope/identity, fences/releases atomically, and returns an
  immutable receipt. `get_reconciliation` reads that receipt. Caller owns grant
  guard ordering; the store never acquires native or registry locks.

`ReconciliationEvidence` is an internal typed service input, never deserialized
from a mobile request. `NativeNotStarted` must match a previously persisted native
refusal exactly. Released attempts cannot begin a prepared delivery. Both local
limits and ancestor budgets use the same reserved-attempt predicate. Resource
references do not decide reservation state.

Admission reserves terminal-fact storage within the existing 1,024-record task
limit. Attempt preparation inserts a private `reserved_start_refusal` row;
reconciliation preparation inserts a private `reserved_reconciliation` row alongside
its operation and intent. Saving native refusal evidence or a reconciliation receipt
updates that reserved row's kind/body without allocating another record. Existing
prepared operations lacking a slot must reserve one before dispatch/native lookup.
Unused slots remain bounded history; they are not permission to exceed the task cap.
Do not defer this quota check until after native execution. Filesystem/storage failure
can still prevent persistence, but unrelated results cannot consume admitted capacity.

Only unresolved native start and prompt operations participate in input single-flight.
A read-only reconciliation that loses a revision race is recorded as refused with
`revision_conflict`; it releases nothing and revokes no authority. An interrupted
reconciliation remaining unconfirmed also cannot block future explicit prompts.
Unknown native starts and deliveries continue to block input.

Implement in dependency order: native evidence contract and tests; Gateway model and
migration/store fences; shared execution/reconciliation service plus authority ordering;
HTTP/OpenAPI/receipt lookup; App decoder/journal/action; paired device validation.
Keep existing record-only clients compatible with additive fields. Do not advertise
replacement capability (`work_attempt_reconciliation_v1`) until all layers pass.

Required deterministic tests:

- Workspace checkpoint followed by pre-dispatch refusal releases only the assistant
  reservation; resource references and history survive restart.
- Reconciliation races a suspended startup continuation: exactly one fence wins,
  and a released attempt never dispatches a native start.
- Dispatch-claimed transport loss, process-created readiness failure, native restart,
  unknown generation, and evicted tombstone remain reserved.
- Matching owned-child exit releases once; live or mismatched evidence never does.
  Cover immediate exit before startup acknowledgment, delayed exit after pane
  replacement, wait failure, shutdown before reap, imported runtime without a wait
  handle, and owner epoch/PID reuse. A nonzero exit code proves termination only,
  never successful work. Old-generation events cannot affect a replacement pane.
- Concurrent replacement requests consume one lead slot; mixed descendant worker and
  child-lead budgets release exactly one reservation after verified exit.
- Exact request-key replay returns the immutable result; cross-device/session/attempt
  lookup fails; conflicting payloads conflict; GET performs no native calls or writes.
- Local result capture racing release cannot commit after revocation; grant issuance
  for released attempts fails; database failure does not revoke a valid grant.
- App process reconstruction preserves the reconciliation key and sends no POST;
  live/unknown cannot enable replacement; history and pinned output remain unchanged.
- A paired Android run observes an isolated owned assistant exit and explicit
  replacement with a new generation. iOS execution coverage remains pending until
  tested on iOS. Offline fixtures alone do not establish native lifecycle safety.
