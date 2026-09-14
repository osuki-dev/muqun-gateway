# Task workflow implementation contract

Status: architecture baseline for implementation, authorized by the user.
The full design and safety requirements remain applicable; this file records
concrete decisions and implementation evidence as work progresses.

## One execution foundation

Manual collaboration, legacy `/tasks`, legacy `/spawn`, and managed tasks share
workspace preparation, agent start, prompt submission and native adapter use
cases. Durable task services record and validate those operations; they do not
implement another agent launcher or AI scheduler. Preserve old response envelopes.

## Wire model and routes

New routes are under `/api/sessions/{session_id}/work` with authenticated device
access. Scoped local result-reporting actors use the shared result service,
not the mobile token.
Use snake_case JSON and UUID strings. Existing entity IDs remain opaque.

- `Task`: `id`, `session_id`, `repo_path`, `title`, `brief`, `parent_task_id`,
  `revision`, `created_at_ms`, `updated_at_ms`, `paused`, `policy`.
- `TaskPolicy`: `allowed_agents`, `max_workers`; parent policy limits descendants.
  Child scopes cannot widen the canonical project or permitted agent set.
- `Attempt`: `id`, `task_id`, `agent_kind`, `role` (`lead` or `worker`),
  `created_at_ms`, and flattened native binding fields `instance_id`, `target`,
  `pane_id`, `workspace_id`, `tab_id`, and `worktree_path`.
  Native binding fields can be null before confirmed startup, never invented.
- `Operation`: `id`, `task_id`, optional `attempt_id`, `kind`, `state`, confirmed
  resource references, optional typed failure code, timestamps. Request keys and
  payload digests are durable internal fields; sensitive payloads stay out of logs.
- `ResultSubmission`: `id`, `task_id`, `attempt_id`, `summary`, `artifacts`,
  `evidence`, `created_at_ms`. Each artifact has a validated version/digest.
- `Review`: submission ID, actor identity, `accepted` or `changes_requested`,
  optional message, time. Human review is separate from local reading preferences.

Creation takes a request key plus repo path/title/brief/policy and optional parent.
List/detail reads are paginated and revisioned. Attempt creation takes expected
revision, request key, agent kind, role, and optional branch name. Initial prompt
delivery is a separate recorded operation. A delivery takes request key, expected
revision, expected instance, and text. Result registration and review use exact
attempt/submission IDs. The current managed-start endpoint does not adopt an
existing terminal; ordinary collaboration retains that separate interaction.

The HTTP integration publishes OpenAPI 3.1 schemas through `work_schema.rs`.
The reserved public capability names are `work_tasks_v1` for records and
`work_execution_v1` for native execution; neither is a substitute for testing
its complete contract before advertisement. Storage core exposes typed operations rather
than let route handlers issue SQL. Repo path authorization belongs at the service
boundary; canonical path validation precedes state creation or external execution.

## Persistence and side-effect ordering

Use bundled SQLite via rusqlite in the Gateway state directory. Schema migration
version 1 creates tasks, attempts, operations, results, reviews and bounded task
change records with session/task indexes. Foreign keys are enforced. SQLite work
must not block async executor threads during native I/O; database transactions
never span awaits to terminal operations.

Store supports in-memory isolated tests and protected on-disk initialization.
Task mutation and change event commit atomically. Request-key uniqueness is scoped
by authenticated actor/session/operation; exact duplicates return the original
receipt and changed payloads conflict. Revision checks reject stale writes.

Operation states: `prepared → submitting → acknowledged | refused | unconfirmed`.
Persist intent before effects. A restart recovers submitting operations as
unconfirmed; do not replay them. A refusal means no effect for that step, while
confirmed prior resources remain attached. Unknown launches retain concurrency
reservations until resolved. No operation state means development work completed.

## Validation and capability boundaries

Keep task title/brief/prompt/request counts and artifact sizes bounded. Do not
silently evict active tasks. Scope every read and mutation. Reject malformed,
foreign or cyclic dependencies and identity changes before native input.
Strong native guarantees must be established by adapter evidence, not a version
string, random UUID or pane preflight alone. tmux ordinary operations remain;
managed parity requires real process lifetime/readiness/input ownership tests.

## Validation platform

The user authorized the existing Omarchy Android AVD for local testing. Inspect
pairing state before automation; never erase unrelated app or server data. iOS
runtime tests are deferred to the user and remain explicitly unverified. Full
repository checks and real paired Android delivery evidence precede ready status.

## Native prerequisite discovered during implementation

Herdr 0.9.0 prompt targets resolve to a current pane or mutable agent alias.
`AgentPromptParams` has no expected launch generation. Renaming and reusing an
alias can change its destination; queued input does not carry the expected
conversation identity. A Gateway lookup and mutex cannot supply an atomic
native precondition. Strict managed delivery remains disabled for unmodified
Herdr. The local native implementation carries immutable launch identity through
target resolution, input queueing and delayed Enter, with replacement/exit tests.

Inspection of the official Herdr v0.9.0 source found that legacy `agent.start`
writes a launch command into a persistent shell. A terminal UUID alone therefore
does not identify an assistant lifetime: after the assistant exits, the shell
can receive later input. The native implementation under development uses a
separate direct-agent PTY and a per-launch identity, validated at both queued
text and submit-key writes. Existing shell-based startup remains a legacy API.
Gateway's bound-start and bound-prompt ports now use explicit native method
evidence. Unmodified Herdr and tmux remain unsupported. Native startup accepts
the canonical resolved native agent executable; arbitrary shell overrides and
script wrappers are refused before launch. A known profile name by itself does
not establish ownership of an assistant process.

An isolated native-process smoke test exercised the built Herdr implementation
with a private compiled, explicitly simulated assistant. It verified a 22-byte
UTF-8 submission receipt, distinct returned output, wrong-launch refusal and
post-exit refusal, then stopped its private server cleanly. This exposed a real
headless dispatch defect that was corrected and regression-tested. The latest
native checks passed 3,281 Rust tests, strict lint and the supporting repository
suites. This is process-transport evidence; the real AI conversation remains a
separate pending gate. Installed Herdr binaries were not replaced.

The existing native approval lookup selects the newest OpenCode session by cwd.
Task approvals instead require the captured instance, native session and native
request identity. Never answer whichever request happens to be pending now.

The existing `supervision.rs` detects Gateway's service manager; it is not an
agent supervisor. Managed tmux requires owned child input and launch generation,
with terminal panes used only for presentation. Native tmux pane paste alone
does not meet the managed contract.

## Current implementation boundaries

The durable domain is exposed from `src/lib.rs` for reuse by HTTP and future
local workflow clients. SQLite stores typed records, receipts and changes;
the HTTP boundary authenticates devices and resolves session/project scope.
Storage failure preserves ordinary terminal service and returns 503 for work.
Result submissions capture bounded, digest-verified immutable bytes through
descriptor-relative file access. Authenticated artifact reads resolve a stored
submission and index, never a caller-selected filesystem path. No managed
execution capability is advertised by these record handlers alone. Selected-session
metadata adds `work_tasks_v1` when the record and artifact stores are available,
and adds `work_execution_v1` only when the connected backend supplies both bound
native methods and the local result-reporting service is running. Storage tests
and UI components are not a completed workflow.

Artifact storage is limited to 1 GiB and 16,384 entries across the store,
including interrupted captures and orphaned files. Descriptor-relative scans
reject symlinks and unsupported entries. A bounded cross-process lock covers
quota measurement and publication; contention or exhausted capacity returns
`resource_limit`. Verified digest reuse does not require another temporary copy.
No automatic garbage collection or deletion of existing artifacts is performed.

## Delegation pause contract

`POST /api/sessions/{session_id}/work/tasks/{task_id}/delegation` accepts
`{request_key, expected_revision, paused}` and returns a task mutation receipt.
The transaction records the policy operation and advances the task revision.
An identical retry returns its original receipt; changing its payload conflicts.

Pausing a task prevents future attempts in that task and its descendants. The
store checks the ancestor chain both at preparation and before an operation
enters submission. Existing assistant follow-ups and result reviews remain
available: pause does not stop a process or cut off communication. Resuming
updates policy only and does not execute prepared or uncertain operations.
The execution service must revalidate policy before each later native creation
step; a database preflight alone cannot cancel work already submitted.

The worker budget applies across descendants: a child lead consumes its
ancestor's delegation allowance, as does a worker at any descendant level.
The ancestor's own coordinating lead is excluded. Preparation checks all
ancestors transactionally, preventing fanout through child tasks from bypassing
the allowance. An uncertain or partially created launch retains its reservation;
only a definitive refusal with no recorded resources releases it.

## Reconnect notification contract

`GET /api/sessions/{session_id}/work/events?after_cursor=<snapshot cursor>`
streams committed task changes independently of the native backend connection.
`work.changes` contains a bounded `ChangePage` (at most 100 records); backlog
pages drain immediately and a caught-up stream checks every two seconds.
Readers keep the returned cursor for reconnect. They must not replace a pinned
task, output or result snapshot simply because a notification arrived.

An expired or future cursor produces `work.reset` and closes the stream. The
client obtains a fresh snapshot explicitly; the server never silently advances
past an unavailable interval. `work.unavailable` closes on storage failure.
The existing encrypted SSE envelope seals every task event, and paired-device
authority is checked before every page, including on an already-open stream.
The endpoint does not infer a resume cursor from `Last-Event-ID`.
