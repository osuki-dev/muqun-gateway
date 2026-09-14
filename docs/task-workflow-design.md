# Task workflow: Gateway architecture and execution plan

Status: proposed design for review, not implemented functionality.
This is the implementation handoff following merged Gateway #26 and App #77.
The source baseline is Gateway `f48568c` on 2026-09-14; relevant source files
match the earlier reviewed `b6bca76` PR head. App baseline is `a178514`.

The App repository owns `docs/task-workflow-design.md` and
`docs/task-workflow-interactions.md`. Read the local
[interaction and safety contract](./task-workflow-interaction-contract.md) for
approval identity, authorization, privacy and their implementation packages.

The target experience is goal entry, explicit decisions and versioned result
review. The App does not run a scheduler. The lead agent owns planning; Gateway
owns durable task facts and executes explicit requests through the terminal
adapters. Ordinary terminal use remains supported.

## A. Merged compatibility foundation

Gateway #26 now reports collaboration capability per session. Prefer the selected
session list; an empty list and an absent legacy field have different meanings.
`agent_spawn` remains distinct and can support ordinary tmux startup. The
top-level collaboration entry is a conditional compatibility exception for old
clients, not an unconditional build capability.

The merged change is not proof of successful task delivery or of the device
checks previously reported as outstanding. New managed operations revalidate
backend eligibility and target identity at execution. Adapter-owned neutral
capability evidence should replace backend-specific branching in HTTP when
extracting these services. Keep legacy endpoints and response envelopes intact.

## B. Domain and module boundaries

Use the existing `TerminalBackend` port and Herdr/tmux adapters. Introduce one application service used by both HTTP and a local agent-facing command adapter.

Suggested module responsibilities, not a requirement to create every file immediately:

| Module                    | Owns                                                                                                               | Must not own                                   |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------- |
| `work/model`              | Task, attempt, delivery and result identities; transition rules; typed domain failures                             | HTTP, SQL, terminal commands                   |
| `work/service`            | Create/assign/follow-up/submit-result use cases, authorization context, resource limits and optimistic concurrency | AI routing decisions or vendor-specific flags  |
| `work/store`              | Transactions, request-key uniqueness, task revisions, bounded event records and migrations                         | Starting processes or sending prompts          |
| `work/delivery`           | Recorded operation execution, target validation, acknowledgment classification and reconciliation                  | Automatic replay of an ambiguous send          |
| `work/results`            | Explicit artifact registration, provenance, version checks and retention policy                                    | Inferring success from prose or terminal state |
| HTTP/local CLI adapters   | Authentication, request parsing, error mapping and calling the service                                             | A second implementation of task transitions    |
| Existing backend adapters | Native identity, readiness, launch/input/output, topology and lifecycle evidence                                   | Task-parent relationships and user acceptance  |

Extract only the startup/delivery code the first use case needs from `main.rs`. Preserve current routes and envelopes while reusing the extracted use cases. Keep agent profile/catalogue maintenance separate from this domain; the domain consumes a validated launch specification rather than embedding per-vendor knowledge.

## C. Persist the minimum useful records

Recommended first storage option: one local transactional SQLite database in Gateway's existing state directory, behind a small repository interface. Confirm dependency/platform compatibility before implementation. This module does not require PostgreSQL, Redis or a network service. Store structured records and references, not continuous terminal recordings.

| Record             | Minimum contract                                                                                                              |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| Task               | UUID, session/project scope, optional parent, brief/title, revision, timestamps                                               |
| Attempt            | UUID, task ID, role/kind, lifecycle, opaque agent instance identity when verified, current native locator, worktree reference |
| Delivery/operation | UUID, attempt ID, operation kind, scoped request key, payload digest, outcome, timestamps and bounded failure detail          |
| Result submission  | UUID, task/attempt IDs, immutable artifact version references, summary, evidence and provenance                               |
| Task change        | Monotonic event sequence, task revision, event type and entity reference; sufficient for bounded SSE recovery                 |

Dependencies can be a later small relation keyed by task IDs. Validate same-session/project scope, reject cycles, and bound depth/count. Gateway validates a requested transition; it does not automatically launch every task whose dependencies appear satisfied.

Keep delivery acknowledgment, observed agent status, result submission and human acceptance separate. Replacing an assistant creates a new attempt. Sending a follow-up creates a new delivery within the intended attempt. Old results remain attached to their original attempt. Keep local “reviewed/hidden” history preferences separate from shared acceptance.

Commit a task mutation, its revision and its notification record in one transaction. Do not hold a database transaction across native I/O. Define a migration version, back up before incompatible changes, and test upgrading an existing populated store. Do not import legacy device-local history as live assignments without an explicit verified adoption operation.

## D. Proposed API and local agent interface

Use an additive namespace for the new durable resource until compatibility is reviewed. For example, `/api/sessions/{sid}/work/tasks` avoids changing what the existing `/tasks` POST means. Names below are illustrative; the semantics are the important part.

| Operation                                         | Proposed behavior                                                                            |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `POST /work/tasks`                                | Persist a task; return its ID/revision. Does not implicitly start a process.                 |
| `GET /work/tasks`, `GET /work/tasks/{id}`         | Paginated task records and detail including attempts/results and a snapshot revision/cursor. |
| `POST /work/tasks/{id}/attempts`                  | Record and request one launch or verified existing-agent assignment; return an operation ID. |
| `POST /work/tasks/{id}/attempts/{aid}/deliveries` | Record one initial instruction or follow-up with request key and expected instance.          |
| `GET /work/operations/{id}`                       | Read the durable outcome; safe after a timeout/reconnect.                                    |
| `POST /work/tasks/{id}/results`                   | Register a submission for its originating attempt; validate every artifact reference.        |
| `POST /work/tasks/{id}/reviews`                   | Record a user's acceptance or request for changes against a specific submission/revision.    |

A launch request may include its initial instruction, so the user need not coordinate readiness. Internally retain separate launch and prompt outcomes: a successful launch plus unconfirmed prompt is not a failed-to-create task. Return `202` only after durable acceptance; retain the old endpoints' current partial-success behavior for existing clients. Reject stale revisions and conflicting request-key reuse with explicit errors.

A local command adapter should expose only the operations needed by the lead/worker skill: create child, request an attempt, inspect a receipt, read task context and submit a result. It calls the same service, with an explicit actor scope. Use narrow per-attempt authority rather than giving workers mobile bearer credentials. Worker result submission must not grant user-acceptance rights. Revocation and scope checks happen server-side.

Direct Herdr/tmux commands remain available to users, but bypassed work is merely observed/unattributed in Muqun until explicitly registered and verified. Naming prefixes are display labels, never a recovery protocol.

## E. Delivery algorithm and recovery

For each mutating operation:

1. Authenticate the actor and validate session eligibility, project scope, arguments and request key. Persist the intent before causing external effects.
2. Serialize operations against the relevant target. For a launch, persist each confirmed resource/step; for an existing agent, verify the expected instance and current readiness.
3. Mark the operation as entering external execution, then invoke the adapter once. Do not use a database lock to cover terminal I/O.
4. Persist an acknowledged result or a classified refusal. Notify clients only after commit.
5. If external effects may have occurred but acknowledgment is missing, persist or recover to `unconfirmed`. Never automatically resend that operation.

Request-key deduplication prevents repeating the same recorded request; it does not make TTY delivery exactly once. A crash between terminal input and receipt persistence cannot be resolved by inventing success or replaying the prompt. A user-requested retry is a new, explicit operation after inspecting the uncertain result.

Recovery should distinguish at least: an intent never entered execution; execution may have begun; a known pane/worktree exists; a verified instance is still live; and identity can no longer be verified. Where evidence is insufficient, retain resources/history and request inspection. Listing a pane is not proof that its old process still owns it. Do not blindly delete resources to make a partial launch appear atomic.

A stronger future backend operation should accept the expected instance together with the prompt and report either a confirmed submission, a no-input refusal, or an ambiguous outcome. A Gateway mutex and a preflight lookup alone do not prevent a user/native process replacing the pane occupant between lookup and write. If the native backend cannot provide the stronger guarantee, expose the limitation rather than claiming it through a capability string.

## F. Herdr first; managed tmux as a separate milestone

For Herdr, use its available native identity/lifecycle evidence through the adapter. Gateway-launched aliases are currently part of the identity scheme. Test replacement, alias changes/loss, pane moves and session integration arriving later. Missing or changed identity invalidates the old assignment; do not fall back to pane ID. Verify the actual prompt operation's guarantees independently from the version gate.

For tmux, today's `start_agent` returns no instance identity and recognizes process presence rather than proven prompt readiness. Simply adding a UUID or setting a tmux pane option would not close the gap.

An implementation spike for managed tmux should prove:

- A launch generation bound to the tmux server incarnation, pane incarnation and owned process lifecycle.
- An owned launcher/supervisor or equivalent mechanism with observable process exit, so subsequent input cannot reach a replacement shell.
- A readiness contract distinct from foreground-command detection; unsupported prompt shapes remain unknown.
- Identity invalidation on exit, respawn, server restart and uncertain Gateway restart recovery; a PID alone is insufficient.
- Lifecycle operation without changing the user's selected pane/layout.

Only enable managed task controls once these contracts pass. Existing external tmux panes stay ordinary terminal/observation surfaces. Keep the existing Herdr-only collaboration contract intact and add narrowly defined capabilities for newly supported operations rather than quietly broadening its meaning.

## G. Result delivery, isolation and synchronization

Reuse artifact validation/rendering but register task results explicitly. The current shallow asset discovery skips build and hidden directories and has bounded retention, so finding a `RESULT.md` or generated file is not a reliable task-result protocol.

For each submission, bind validated files to an attempt and a content version. Prefer content-addressed copies in managed result storage within explicit size limits when the product promises durable review. If only a reference is retained, verify its digest on read and report changed/missing content. Handle canonical paths, symlink escape and files changing during registration. An agent's statement that tests passed should be labeled agent-reported unless evidence supports a stronger claim.

Concurrent writing attempts get isolated worktrees; read-only work may share a checkout. The lead/integration attempt owns combining changes and reporting final validation. Worktrees do not isolate ports or databases. Acceptance does not delete worktrees, stop agents, merge branches or publish output.

Use task revisions and a bounded event log for SSE. A consistent snapshot/cursor handshake must prevent missing a change between the initial read and subscription. Expired cursors trigger a fresh snapshot. Cache observations separately from durable task facts; do not persist every terminal frame. New events can update attention indicators but must not replace the user's pinned output or result view.

## H. Executable delivery sequence

| Increment                                               | Deliverable                                                                 | Acceptance evidence                                                                                             |
| ------------------------------------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| 0. Merged capability contract + paired App verification | Selected-session compatibility contract                                     | Positive/negative session cases and the outstanding real paired delivery check; ordinary terminal use preserved |
| 1. Task persistence                                     | Task/detail queries, revisions, storage/migrations, request-key handling    | Restart recovery, duplicate/conflicting keys, actor/project scope and migration tests                           |
| 2. One managed Herdr attempt                            | Recorded launch, initial prompt, follow-up and operation query              | Real returned output; partial launch; lost acknowledgment; replacement refusal; no focus change                 |
| 3. Result/review loop                                   | Registered versioned artifacts, explicit user review, App task view         | View result, request changes, reconnect, handle changed/missing files; no automatic publish/cleanup             |
| 4. Delegation                                           | Scoped local adapter, lead skill, explicit children/dependencies and limits | Independent work in isolated worktrees; lead loss preserves workers/results; no duplicate dispatch on recovery  |
| 5. Managed tmux                                         | Proven identity/readiness/input ownership                                   | Exit/respawn/server-restart tests and real paired App workflow; advertise only verified guarantees              |

Complete increments 1–3 as one usable single-assistant product loop before presenting multi-agent orchestration as delivered. Do not make a broad registry rewrite or automatic task scheduling a prerequisite for that loop. Defer cross-machine delegation, distributed queues, automatic recovery resends and automatic worktree deletion.

Each implementation PR needs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked`, plus the relevant isolated adapter contracts. App changes need its five gates and new native full-suite flows. Startup/delivery changes require the real paired check on dedicated QA infrastructure, including returned output, follow-ups and preserved focus. Trust/approval prompts remain explicit user decisions. Test crash boundaries and stale target identities, not only happy-path HTTP responses.

For maintenance, keep one state transition implementation, one API contract, one registry owner with distinct support dimensions, bounded records/events/artifacts, and migration coverage. Log task/attempt/operation IDs and bounded failure codes; do not log secrets or terminal transcripts by default. Persisting task facts is the necessary additional complexity; AI scheduling policy remains in the lead agent.

## I. Current collaboration findings that affect this plan

The [App #77 implementation review](https://github.com/osuki-dev/muqun-app/pull/77#issuecomment-5659062620) identified a concrete path where an armed Herdr assignment survives a session switch and is submitted to tmux. Gateway's ordinary spawn endpoint accepts tmux intentionally. Fix App's tracked-assignment dispatch guard; do not fix this by disabling ordinary tmux startup globally.

Existing-agent submission also remains a lookup followed by a separate target write: `AgentSendBody` at `main.rs:1014` contains only `text`; `send_agent` at `:6541` calls `prompt_agent(target, text)`; the Herdr adapter at `backend/herdr.rs:600` passes a target and text. There is no expected-instance precondition in this HTTP operation. The App's preflight check is useful but not atomic with native delivery. This is an existing limitation, predating merged #26, and the stronger operation in section E must be proven before it is advertised.

The existing legacy submission helper can send repeated Enter keys when it cannot prove advancement (`main.rs:6620–6709`). It is skipped for Herdr versions that own submission; do not generalize that workaround into managed tmux task delivery or claim it cannot answer a subsequent dialog. The managed path needs explicit acknowledgment/readiness semantics and no blind key replay.

There is also a current App history-management defect: historical records remain in local storage but their only UI discards the history partition. That is an App fix independent of durable Gateway tasks. Current collaboration and the future task workflow share execution infrastructure; they are not two competing orchestration systems.

## J. Contract baseline for agents implementing an approved design

These are recommended defaults to confirm with the overall design. Once approved, use them as the implementation baseline rather than independently redesigning each PR. A material deviation should return for architectural review.

- **Storage and scope:** a single Gateway-owned SQLite store; tasks restricted to one Gateway session/project in the first release; UUID identifiers serialized as opaque strings. No cross-machine parents/dependencies.
- **Endpoint namespace:** `/api/sessions/{sid}/work/...`; preserve legacy `/tasks`, `/spawn`, agent send and their response shapes. Task creation and execution are separate durable operations, even if the App presents one Send action.
- **Core request fields:** creation takes `project_ref`, `title`, `brief`, optional `parent_task_id`; assignment takes `agent_kind`, explicit launch/settings profile, optional initial instruction and either a new placement request or an existing verified target. Follow-up takes `attempt_id`, `expected_instance_id`, and `text`. The service resolves/validates project paths; clients do not gain arbitrary host-path execution.
- **Mutation preconditions:** every side-effecting request has a request key; task mutations also carry `expected_revision`. Scope request-key uniqueness by actor/session/operation type, bind it to a canonical payload digest and return the original receipt on an exact duplicate. Reject a changed payload under the same key. Persist pending payloads within protected task storage only as needed for the operation/history policy; exclude them from routine logs.
- **Operation response:** `operation_id`, `task_id`, `attempt_id` when allocated, `state`, confirmed resource references, and a typed failure code. `acknowledged` means the relevant launch/input operation was acknowledged; it never means the requested development work succeeded. `202` means durably accepted for processing, not delivered.
- **Operation states:** `prepared → submitting → acknowledged | refused | unconfirmed`. A crash in `submitting` becomes `unconfirmed` unless native evidence proves an outcome. `refused` requires evidence of no relevant input/effect for that step. Confirmed earlier startup steps remain recorded even if a later step is refused. Explicit user retries create new operations; uncertain operations are not replayed.
- **Domain failures:** define at least `capability_unavailable`, `scope_mismatch`, `revision_conflict`, `request_key_conflict`, `instance_changed`, `not_ready`, `approval_required`, `delivery_unconfirmed`, `artifact_changed`, and `artifact_missing`. Map deterministic precondition conflicts to 409, unauthorized scope to 403, unsupported operation to 501, and invalid input to 400. An uncertain previously accepted operation is queried as an operation state, not collapsed into a generic 500 that encourages resubmission.
- **Results:** submissions are versioned and tied to an attempt; review references a specific submission. Agent submission and user acceptance have different authorities. Accepted historical submissions remain identifiable after later changes or attempts. Artifact content, provenance and validation claims are separate fields.
- **Transport:** bounded SSE events over committed records, with a snapshot cursor and a reset response when replay is unavailable. No task-state inference from terminal byte changes. No automatic pane focus, terminal navigation or snapshot replacement.
- **Capability vocabulary:** introduce explicit versioned task-operation contracts only when implemented; capability names and OpenAPI schemas are frozen in the first contract PR. Backend eligibility comes from neutral adapter metadata, not new `if herdr/tmux` branches in route handlers. Per-attempt identity and readiness checks still apply even when session capability is present.

## K. Work-package ownership and completion artifacts

The following packages are sized for separate implementation agents after approval. They must run serially where contracts or shared source files overlap. Independent review/testing can follow the repository's own agent rules; this design does not authorize parallel edits to a shared checkout.

| Package                                   | Depends on                  | Primary change locations                                                                     | Required completion artifact                                                                                                    |
| ----------------------------------------- | --------------------------- | -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| GW-C1: compatibility boundary             | Merged #26 contract         | `backend/model.rs`, `backend/herdr.rs`, `backend/tmux.rs`, health serialization in `main.rs` | Adapter-owned capability evidence, preserved response compatibility, paired App gate evidence                                   |
| GW-T1: contract and store                 | Confirmed section J         | `work/model`, `work/store`, OpenAPI and architecture notes                                   | Frozen JSON schemas/errors, migration, transaction/restart/request-key tests, documented limits                                 |
| GW-T2: managed Herdr operations           | GW-T1                       | `work/service`, `work/delivery`, extracted startup/send use cases, Herdr adapter             | One task/attempt round trip, initial/follow-up receipts, fault-injection evidence, exact native guarantee/limitation documented |
| GW-T3: results and recovery reads         | GW-T1; integrate with GW-T2 | `work/results`, artifact serving, task snapshot/events                                       | Versioned submission/review contract, stale/missing artifact tests, reconnect/cursor tests                                      |
| GW-T4: local agent control and delegation | GW-T2 + GW-T3               | Local CLI adapter, scoped authority, lead/worker skill integration                           | Child creation/dispatch/submission through the shared service, actor-scope tests, bounded delegation and lead-loss recovery     |
| GW-T5: managed tmux feasibility spike     | GW-T2 contract frozen       | Isolated tmux adapter prototype/contracts                                                    | Demonstrated process/input ownership across exit/respawn/restart; explicit go/no-go result                                      |
| GW-T6: managed tmux implementation        | Successful GW-T5            | tmux adapter and managed lifecycle support                                                   | Same neutral operation contract and real paired workflow; no capability enabled for unsupported targets                         |

GW-T2 must not claim exactly-once delivery or solve an unavailable native identity precondition with documentation alone. If the required strong native operation cannot be implemented, document the narrower guarantee and keep the stronger capability disabled; return that decision for review. GW-T5 may legitimately conclude that native tmux alone is insufficient and a supervisor is required. Do not ship fabricated parity to satisfy the package name.

Every package handoff must include: pinned base/head, exact contract changes, changed modules, test commands and actual results, known limitations, compatibility matrix, and any unresolved architectural decision. PR descriptions must report missing device/paired evidence honestly. No agent should interpret “implementation complete” as permission to tag, merge, release, answer a trust prompt, or operate on a user's active workspace.
