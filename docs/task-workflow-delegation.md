# Bounded lead delegation and dependency contract

Status: store, paired/local adapters, scoped CLI, and dependency updates are implemented.
The isolated simulated-assistant process proof passed; the unpublished development
candidate now advertises capability prerequisites for paired App validation. Real
managed-AI and App device validation remain pending. Scope is one
Gateway, one session/project, and the existing managed execution service. Nothing here adds a
scheduler, terminal launcher, automatic replay, worktree deletion, or human approval
rights for an assistant.

## Existing foundation and gaps

Current WorkStore already persists parent_task_id, narrows child policy, bounds
parent depth, checks ancestor pause, and counts every reserved descendant attempt
(including child leads) against ancestor max_workers. Lifecycle release is explicit;
unknown/partial launches retain their slots. Task creation itself consumes no launch
slot. Input claims and result versions are immutable and scoped. Reuse all of these.

ExecutionPort currently exposes optional commit_authority; its None branch bypasses
admission guards. Never reuse that branch for local delegation. Replace it with a
required authenticated actor at the shared service boundary (test fixtures receive
an explicit test authority, not a production bypass).

Execution already implements durable preparation, dispatch claiming, placement
checkpoints, bound native startup, independent prompt delivery, and receipts. Its
native I/O runs outside SQLite transactions. Reuse Execution and its ports.

LocalState/Registry already implement same-UID Unix transport plus a protected,
expiring per-attempt token. Current local commands are context, receipt, and own
submit-result only. Scope has session/task/attempt, but no delegation permission or
current coordinator binding. Worker grants must retain their present narrow rights.

Missing pieces are explicit delegation consent, coordinator authority fencing,
child control scope, dependency records/readiness, shared mutation authorization in
Execution, and local commands. Current dependencies are documentation only. The
existing max_workers check limits concurrent reservations, not the total number of
child task records; an additional bounded child-creation allowance is needed.

## Policy and authority

Use Task.delegation.policy, default disabled for old wire/persisted tasks. Existing
TaskPolicy agent and worker fields remain unchanged:

```json
{
  "enabled": false,
  "max_children": 16,
  "max_depth": 1,
  "dependency_requirement": "result_available"
}
```

Current bounds: max_children 0..64, max_depth 0..1. Recursive depths up to four are
a later extension requiring ancestor control enforcement and tests. These are lifetime per-parent child-record
and remaining subtree-depth bounds, distinct from existing max_workers 0..16. The
root max_children is also an aggregate descendant-record ceiling to prevent fanout;
validate every ancestor in the creation transaction. Records are never silently
evicted to regain allowance. max_workers remains the one execution reservation
budget; do not introduce another semaphore with different release rules.

Only paired users can enable/widen delegation. Children inherit a narrowed policy:
allowed_agents subset; max_workers and remaining max_depth cannot widen; child
max_children cannot exceed any ancestor's remaining aggregate allowance. Zero values
are valid and deny the corresponding action. The first delivery can use max_depth=1
(root controls direct children; child agents cannot delegate).

`dependency_requirement` is `result_available | human_accepted`. The latter is a
stricter policy; a local actor cannot downgrade it. Result availability permits
another assistant to consume an explicit candidate without claiming that the work
is accepted. Human acceptance still requires the existing paired review route.

Task.delegation also stores coordinator_attempt_id (nullable, old default null)
and a monotonically increasing coordinator_epoch. Bind it only when a paired user's explicit lead-start
or replacement is confirmed. Never derive it from pane names or idle status. A local
delegation grant contains scope, permissions, coordinator_epoch, and the native
owner/launch identity. It has the existing maximum 24-hour expiry and count/file
bounds. A worker or child lead with delegation disabled receives result-only rights.

An assistant cannot request broader grant permissions, edit policy, review results,
release reservations, reconcile lifecycle, adopt another coordinator, or change
project scope. Same-UID token protection is application authority, not a sandbox
against another hostile same-UID process. Do not put tokens/prompts in argv/logs.

## Local command contract

All mutations take JSON on stdin through the existing CLI/socket, with bounded
request keys and optimistic revisions. Session and parent are derived from the grant;
reject any conflicting caller-supplied scope. Reuse existing receipt semantics.

- `work children`: paginated direct-child summaries/operations/results, read-only;
  no arbitrary session lookup or terminal snapshots outside the delegation tree.
- `work child-create`: request_key, expected_parent_revision, title, brief,
  narrowed policy, dependency task IDs. Canonical project and parent come from the
  grant. Initial input references may only reuse explicitly authorized parent-owned
  immutable references through a dedicated reference-sharing relation; until that
  relation exists, this increment accepts no local child input_refs. Never steal or
  reassign a parent input's one-task claim.
- `work child-start`: request_key, child_task_id, expected_child_revision,
  agent_kind, branch_name. The target must be a direct child. Child lead execution
  uses the existing StartRequest/Execution service; require a fresh isolated worktree
  branch for delegated work. Never fall back to the parent's working directory.
- `work child-deliver`: request_key, child_task_id, attempt_id,
  expected_child_revision, expected_instance_id, text. Use the existing independent
  delivery service, exact-generation checks, and 64 KiB composed prompt bound.
  No implicit initial send and no inherited historical attachments.
- `work child-dependencies`: request_key, child_task_id, expected_child_revision,
  exact dependency bindings described below. Metadata mutation only.
- `work request-receipt`: kind and request_key for this exact local actor; existing
  operation-ID receipt reads remain bounded to its own task or authorized children.

Include the originating coordinator attempt/epoch and expected parent revision in
child-create request digests. Grant issuance precedes native startup today: possession
of a token is insufficient until the originating attempt has a confirmed launch and
owner epoch.

Local actor identity is stable per grant/attempt (not the plaintext token) and scoped
by session in receipt keys. A replacement lead is a different actor; it can read
child operation IDs through authorized history but cannot impersonate the old actor's
request key. Uploading files, arbitrary command strings, arbitrary filesystem paths,
worker-to-worker input, and cross-machine delegation are not local capabilities.

## Dependency records and readiness

Add bounded TaskDependency records: dependent_task_id, prerequisite_task_id,
submission_id nullable, requirement, and revision/timestamps. At most 16 edges per
task. Require same session/project and same direct parent in this first increment;
reject self-links, duplicates, cycles, and depth over 16. Root/coordinator is not a
dependency endpoint. Dependencies do not modify the parent tree.

Child creation may declare prerequisites before they produce results. It remains
waiting until the coordinator explicitly pins an immutable submission from each
prerequisite through child-dependencies. Do not select a mutable “latest result” on
its behalf. Validate the result belongs to that exact prerequisite and its attempt.
For human_accepted, require the latest review for that exact submission to be accepted
at the dispatch fence, ordered by durable insertion/change order rather than client
or wall-clock timestamps. An assistant cannot satisfy that requirement itself.

Only unstarted tasks can edit dependency edges/bindings: reject once any start attempt
has dispatch_claimed/launch_confirmed/legacy_unknown, including uncertain launches.
Definitively released not-dispatched attempts do not prevent a new explicit plan.
A result_available binding means explicit candidate data exists; it does not label
either task successful. A failed/nonzero native exit never satisfies a dependency.

Check dependencies before start preparation so waiting tasks do not reserve slots.
Recheck them in the same transaction as dispatch claiming, after ancestor pause and
coordinator authorization. Freeze exact dependency result/review evidence into the
start operation. Reserve its storage during preparation, preserving current quota
safety. A later review change does not retroactively cancel an already claimed
launch; history shows the exact evidence that permitted it. No event automatically
starts a newly ready task. The coordinator must issue child-start explicitly.

Expose readiness as `ready | waiting_for_result | waiting_for_acceptance |
paused | authority_unavailable | capacity_unavailable`, derived from current facts.
Do not persist a second task-progress state machine or fabricate completion percent.

## Shared-service authorization and ordering

Extract a small WorkActor enum with PairedDevice and LocalLead authority. HTTP and
local adapters build it; they do not implement separate task transitions. Store-facing
mutations accept a validated delegation fence containing coordinator task/attempt/
epoch, while shared service code owns token verification and native observation.

For local mutation, validate token/scope/expiry and obtain explicit `live` evidence
for the coordinator's immutable owner/launch through the existing lifecycle port.
Missing/unknown evidence refuses new local control; it does not release a slot.
Perform this native read outside grant/DB locks. A live observation is not an eternal
process-liveness guarantee. Admission is ordered by the grant/DB authorization fence;
revocation before that fence prevents admission, revocation after dispatch claiming
does not undo native work already admitted.

Use the existing lock order: grant guard, then short WorkStore transaction. Recheck
grant permissions, current coordinator epoch, direct-child relation, inherited
policy, parent and child revisions, dependencies, and budgets in the authoritative
transaction. Release locks before native I/O. Recheck local authorization and ancestor
pause before each later creation step and in claim_start_dispatch. Add the authority
fence to the existing claim API instead of a separate local launcher. Originating
lead identity is a separate precondition from the child recipient input identity.
For recursive delegation, retain ancestor control epochs and check each ancestor
control lease/lifecycle observation before new control operations; observed root
coordinator loss denies new descendant delegation without disabling result reports.

When authorization is lost before dispatch, use existing no-dispatch refusal/
reconciliation semantics and preserve partial placement resources. After claiming,
ambiguous outcomes remain unconfirmed/reserved. Child creation, child startup, and
initial prompt are separate receipts. Local receipt lookup never resends any of them.

## Lead loss and recovery

Observed owned exit, explicit revoke, expiry, or coordinator epoch replacement denies
new commands from the old lead. Reconciliation/replacement continues through paired
user controls only. Unknown ownership refuses new delegation without guessing exit.
Do not stop workers, delete worktrees, clear dependencies, or discard results when
the coordinator disappears. Child agents keep their own result-only grants until
normal expiry/revocation, so in-flight work can report back independently.

Show coordinator unavailable plus children and their actual operations/results.
The paired user can reconcile and start a replacement lead through the existing
lifecycle flow. Re-enabling delegation for that replacement is an explicit control;
it grants access to preserved direct-child history under a new coordinator epoch.
The paired takeover command names the parent task, replacement attempt, expected
revision, and prior coordinator epoch; it atomically changes the control epoch and
revokes predecessor control authority while preserving child result grants. New
local child mutations must present the new parent epoch. Grant renewal after 24 hours
or Gateway restart requires explicit paired regrant after exact live-identity
validation. Reading context never renews or restores a grant automatically.
Never automatically resend an old lead's unknown child-create/start/deliver request.
Exact task/operation IDs, rather than old actor request-key impersonation, support
inspection and deliberate continuation.

## Concrete ownership and implementation sequence

1. Store owner: src/work/model.rs, store.rs, new store/delegation.rs and tests.
   Add default-disabled policy, coordinator binding, dependency schema/migration,
   bounded graph/record checks, authority fence validation, and immutable dispatch
   evidence. Preserve existing parent/budget/lifecycle predicates.
2. Shared-service owner: work_execution.rs plus a focused work_delegation.rs adapter
   around the same Execution. Add WorkActor/fence to existing preparation/claim paths;
   reuse bound startup/delivery and scoped immutable result access. No backend launcher.
3. Authority/local owner: work_authority.rs, work_local.rs, CLI registration in main.
   Add narrow permission bits/epoch, stdin commands, request receipt read, and atomic
   revoke/commit ordering. Extend existing bootstrap guidance with these commands.
4. HTTP/schema/App owner: paired policy/coordinator controls, OpenAPI, readiness and
   child/dependency views; read-only events reuse current cursor protocol. Advertise
   work_delegation_v1 only after complete integration and paired validation.

First implement level-one delegation; do not expose recursive depth before the same
ancestor/record limits and graph tests cover it. No device actions are part of this
proposal. Required gates include all normal repository checks, real isolated paired
Android lead-to-child delivery/results, and explicit deferred iOS status.

## Acceptance tests

- Worker/result-only grant cannot create/start/deliver/review/reconcile another task.
- Foreign child/project/session and stale coordinator epoch fail before native effects.
- Concurrent child creation cannot bypass ancestor record limits; concurrent child
  starts cannot bypass mixed worker/child-lead reservation limits, including zero.
- Pause/revoke/dependency-review changes racing placement or dispatch have one ordered
  winner; late placement resources remain recorded; no locks span native waits.
- Cycles, repeated edges, foreign submissions and unaccepted bindings refuse; exact
  pinned candidate can make a child ready without declaring the task accepted.
- Near-quota admitted operations can still persist all terminal evidence/results.
- Lead exit/unknown/restart leaves child resources/results and reservations intact;
  stale grant cannot act, replacement cannot impersonate request keys, no autosend.
- Lost local ACK resolves by read-only receipt; changed payload conflicts; explicit
  child start and prompt remain separate operations using one shared native path.

Independent service-boundary review: app_history_foundation confirmed the optional
authority bypass, local child-receipt scope gap, grant-before-launch distinction,
24-hour/restart regrant requirement, and immutable input one-task-claim restriction.
The independent review informed the implementation handoff below.

## Store implementation handoff

The implemented first increment keeps existing TaskPolicy Rust/wire fields intact
and adds default-disabled `Task.delegation` (policy plus coordinator attempt/epoch).
`Task.dependencies` is a bounded typed relation list in the existing task row;
`Operation.dependency_snapshot` and `delegation_fence` default empty/null. Depth is
currently 0..1; enabled delegation is root-only and children inherit disabled policy
with remaining depth zero. No production local authority or capability is enabled.

Store APIs: configure_delegation; create_delegated_task (and a with_inputs variant
that explicitly returns capability_unavailable for nonempty inputs);
prepare_delegated_attempt; prepare_delegated_delivery; set_dependencies;
assert_delegation_authority; dependency_readiness. Existing claim_start_dispatch and
begin_operation recheck stored control fences; dependency evidence is checked again
at dispatch. Parent revision advances atomically when delegated child creation wins.
All local authorization still requires the service's live native observation and
registry guard; a persisted fence alone is not an authenticated principal.

Pausing prevents new child creation/start, while identity-authorized explicit
follow-up communication remains possible. Disabling delegation or changing control
epoch removes local control authority. Dependency snapshots occupy the already
reserved operation row, so record-cap pressure cannot prevent terminal persistence.

The next child-input increment must add an explicit immutable sharing relation:
source task/input ID, recipient child task, originating coordinator epoch, pinned
content identity, and bounded use/caption metadata. Creation must validate the
source is parent-owned and both tasks share the authorized session/project; it must
never change work_inputs.claimed_task or accept legacy filesystem upload paths.
Resolve child references through this grant and the existing immutable blob verifier.
A sharing grant cannot widen reference-only into may-include without paired consent.
Freeze its identity into each explicit delivery; preserve prior instruction history
if future-use permission is revoked. Reserve relation capacity before admitting
creation/delivery. Implement and test that relation before removing the current
explicit unsupported response; child attachment usage remains required follow-up
scope, not an implicit waiver.
