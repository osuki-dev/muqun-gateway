# Local lead delegation adapter

This adapter reuses managed work records and the existing `Execution` and `SessionPort` launcher. It is not a second collaboration system. The unpublished development candidate advertises its implemented delegation contract when record storage and the private local channel are available. This permits paired App validation; it is not a release or a claim that a coordinator grant is active.

## Activate a coordinator

A paired device posts to `/api/sessions/{session_id}/work/tasks/{task_id}/delegation-config`:

```json
{
  "request_key": "configure-unique-key",
  "expected_revision": 7,
  "input": {
    "policy": {
      "enabled": true,
      "max_children": 4,
      "max_depth": 1,
      "dependency_requirement": "result_available"
    },
    "coordinator_attempt_id": "confirmed-lead-attempt-id"
  }
}
```

The selected lead must have a confirmed reserved launch, an existing unexpired local reporting grant and an exact native LIVE observation. The service revalidates device token, encrypted transport identity, grant, native binding and store state before committing. The lock order is devices, local grants, then work store; no native I/O runs under these locks. Configuration increments the persisted coordinator epoch and activates only that exact grant. A repeated configuration receipt does not reactivate authority. Restart never recreates grants; a paired device must explicitly restore reporting authority before reconfiguring. The existing `/delegation` pause route retains its original behavior.

Disable removes control authority while preserving existing processes and independent result-reporting grants. Local commands cannot configure delegation, reconcile/release reservations, or accept results as a human.

## Local commands

Use the exact Gateway executable supplied by managed onboarding. Commands read bounded JSON from stdin; the existing context-file loader supplies the private socket and token. Never put the token or context file contents in arguments or prompts. The request limit is 256 KiB, including the private transport envelope. Responses are bounded to 3 MiB; record pages retain the store's 2 MiB limit and at most 20 records.

- `work create-child`: `{ "input": { "request_key", "expected_parent_revision", "title", "brief", "policy", "dependencies": [], "input_refs": [] } }`. The service derives parent task, session and project from the principal. References are explicit eligible parent-initial immutable inputs; follow-up-only parent inputs are not inherited. Sharing preserves the original parent claim.
- `work start-child`: `{ "task_id", "input": { "request_key", "expected_revision", "agent_kind", "role", "branch_name": null } }`. The first assistant must have role `lead` for that child task; the parent coordinator does not satisfy this requirement. This does not enable delegation on the child. Use an explicit branch for isolated work. Role `worker` is only for additional assistants after the child has its own confirmed lead, and requires a branch name. Startup does not deliver the brief.
- `work deliver-child`: `{ "task_id", "attempt_id", "input": { "request_key", "expected_revision", "expected_instance_id", "text", "input_refs": [] } }`. Initial and follow-up prompts both contain only explicit references. The complete rendered prompt is checked before admission. Startup and delivery have separate operation receipts.
- `work child`: `{ "task_id" }` returns task metadata and cursor, without pretending omitted history is complete. Add `kind` (`attempt`, `operation`, `result`, `review`), `snapshot_revision`, and optional `after_id` to retrieve a record page. A changed revision requires a fresh metadata read. For one immutable result/review, provide `kind` and `record_id` without a revision or cursor.
- `work child-receipt`: `{ "kind": "create_task|start_attempt|deliver_prompt", "request_key" }` reads an original actor/epoch-scoped receipt. This recovers a lost create reply before the caller knows the child ID.
- `work child-operation`: `{ "operation_id" }` reads an operation belonging to a currently authorized direct child. A configured successor can inspect inherited history without gaining the previous coordinator's request-key identity.

The examples above describe object fields rather than complete JSON literals. All commands reject unknown fields. IDs and expected revisions must come from actual returned records. No command automatically retries an uncertain mutation. Read an existing receipt and return control to the paired user when the outcome remains uncertain.

## Authority and execution lifetime

The controller is the configured parent lead; the recipient is the exact child attempt. Each child receives its own reporting token. A valid reporting token alone never grants delegation. The local principal has a private exact coordinator fence and an actor ID scoped to its attempt and coordinator epoch. A generic store callback rejects local principals; only closed create, prepare, advance and read methods admit them.

Before each new admission and each later startup effect, the service observes the controller's exact native instance and supervisor epoch outside locks, then revalidates the captured grant and persisted fence under grants-to-store locking. Delivery independently verifies the child target and readiness, and the final native prompt guard remains unchanged. Dependencies and ancestor budgets use the existing transactional store checks. Pause blocks new children and launches; explicit follow-up to an existing child retains its separate policy.

Unknown/unavailable controller observations block all new effects while preserving already-authorized bounded read-only recovery. Exact exited evidence invalidates only the captured control binding; it must not invalidate a successor that became current while the query was running. Own and worker reporting grants remain independent. The native adapter currently folds unproven epoch mismatches into Unknown; it does not fabricate authoritative exited evidence.

After exact exit invalidates control, local child reads and child receipt lookup are refused. Use paired-device recovery for those records; this increment does not create a separate post-exit descendant-read grant. Own-attempt reporting and receipt access retain their existing scope until their reporting grant expires or is revoked.

A LIVE observation cannot prove that a process will remain alive until the subsequent native call. This implementation fences observed loss and concurrent grant/configuration changes, not unobserved physical exit. Native outcome bookkeeping remains permitted after caller revocation so resources and actual receipts are not lost; it cannot authorize another effect.

## Validation boundary

Unit tests cover reporting-only token rejection, actor substitution, generic callback rejection, direct-child scoping, original actor receipt access, coordinator generation changes, paired revocation during recipient preflight, preserved reporting, and CLI command parsing. Store tests separately cover immutable sharing, dependencies, admission/dispatch races, and provenance. The isolated process check in `/tmp/gwp-tgm712tk/summary.json` passed 24 actual scoped CLI jobs using explicit simulated ELF assistants and native PTYs. It updated the same waiting child to an exact dependency result, then explicitly started and delivered to that child; it also verified immutable inputs/results, replay, pause, controller exit, retained worker reporting, and one-byte interruption without replay. Gateway SHA-256 was `7e029b6d763850fef135cf1d4c4da498e1819ef1e328249775125a32cffd1d71`. Owned processes were cleaned up. This does not replace the remaining real managed-AI and paired App workflow checks; subsequent source changes require appropriately scoped validation.
