# Managed assistant result reporting

A managed assistant receives versioned Gateway reporting instructions on its first
managed prompt delivery. The original task brief and user prompt remain unchanged
in task records. Follow-up deliveries contain the user's text and only explicitly
selected input references, without repeating onboarding. This extends
the existing execution and per-attempt local authority; it does not add a second
agent launcher or a public bearer-token API for assistants.

`Operation.bootstrap_version` records the onboarding version selected atomically
when reserving delivery. Request digests bind the original user text and offered
instruction version. A repeated request key returns the existing receipt and
never injects or delivers again. A refused first delivery does not consume the
initial-instruction opportunity; an unconfirmed delivery does, because the
assistant may already have received it. Unknown versions fail before native input.
Rendered prompts remain bounded to 65536 UTF-8 bytes, including the instructions.
An oversized initial prompt is definitively refused and the preparation is
released; shorten the user instruction before making a new explicit request.

New initial deliveries use `managed-delegation-v2`. The `result-reporting-v1`
renderer remains available unchanged for recorded deliveries. Version 2 explains
the bounded child CLI, separate startup and initial-delivery receipts, immutable
inputs, paginated result inspection and read-only recovery. These instructions do
not grant permission: child commands require a separately activated, exact lead
coordinator grant. A worker or reporting-only token cannot acquire that authority
by following the examples. See [the local delegation adapter](local-delegation-adapter.md)
for activation and failure semantics. Its real-process validation remains pending.

The instructions quote `std::env::current_exe()` as a POSIX shell executable path.
They never resolve a potentially older binary from `PATH`, execute a shell while
formatting, or include credentials or the contents/path of the context file.
Spaces, apostrophes, dollar signs and backticks remain quoted data. Relative,
non-UTF-8 and control-character executable paths are refused.

## Local CLI

Use the exact executable shown in the initial instructions. The following uses
`/absolute/path/to/gateway` only as an illustrative placeholder:

```sh
'/absolute/path/to/gateway' work context
'/absolute/path/to/gateway' work submit-result < result-submission.json
'/absolute/path/to/gateway' work receipt OPERATION_ID
```

The CLI obtains its narrow authority from the inherited `MUQUN_WORK_CONTEXT_FILE`.
Do not print the file, move its contents into arguments, or expose them in logs or
prompts. `work context` returns non-secret task/attempt metadata in `result`:
`result.attempt.id` is the exact submission attempt and `result.task.revision` is
the revision precondition. Read it immediately before preparing a submission.

`work submit-result` accepts one JSON object on standard input:

```json
{
  "request_key": "unique-key-for-this-submission",
  "expected_revision": 7,
  "input": {
    "attempt_id": "exact-id-from-work-context",
    "summary": "Implemented the requested change; Android checks passed, iOS not tested.",
    "artifacts": [],
    "evidence": ["Name the checks actually run and their observed outcomes."]
  }
}
```

The revision and attempt above are illustrative. Use the current scoped values.
The CLI limits stdin to 256 KiB; keep the document below that limit to leave room
for the local protocol envelope. The request key is nonblank and at most 128 UTF-8
bytes, the summary at most 16384 bytes, and there may be at most 32 evidence strings
of at most 4096 bytes each. Evidence is always agent-reported, not independent
verification or proof of human acceptance.

An artifact entry has `path`, lowercase hexadecimal `sha256` (64 characters), and
`size_bytes` for the actual file. The source must be a regular file inside this
attempt's worktree, or the task repository when no worktree exists. Symlink and
parent traversal are refused. A submission allows at most 32 files, 50 MiB per
file and 100 MiB total. The Gateway captures digest-verified immutable bytes; a
later edit to the source never changes an existing submission.

A successful submission returns `result.value` and `result.replayed`. A timeout or
lost reply remains ambiguous. Do not automatically submit again or change the
request key. Report the uncertainty and ask the user to inspect the task's result
history in the App. Receipt lookup is read-only for a known operation ID; it does
not discover an unknown result submission. A definitive revision conflict allows
refreshing context and reconsidering the payload; it does not authorize blind
retry against a newer revision.

The local actor cannot accept results, impersonate a paired human, control native
terminals, or operate on another task/attempt. Registry scope, expiry and revocation
are checked through the same result-submission actor boundary. The same operating
system user can access its own files and processes; this local authority is not an
OS sandbox. Human review, merging, publishing and stopping assistants remain
separate operations.
