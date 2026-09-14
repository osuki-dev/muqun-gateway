# Strict decisions and scoped interruption: executable proposal

Status: source-reviewed proposal, not implemented or runtime-verified. Reviewed the local Gateway and Herdr worktrees on 2026-09-14. No provider prompts were answered. Provider statements below describe checked-in adapters and captured fixtures, not claims about the latest external provider releases.

## 1. Supported capability truth

| Existing path | Actual evidence | Strict decision support now |
| --- | --- | --- |
| Herdr managed Codex, Claude, OpenCode native executables | Immutable direct-process launch ID, owner epoch, guarded prompt writes, successful-child-wait lifecycle receipts | No. These prove process/input ownership, not approval occurrence identity. |
| Gateway OpenCode native adapter | Captured provider permission ID; answers a session/request URL | No. `src/native.rs:429` selects `newest_session` by directory; environment endpoint plus working directory does not bind a provider session to the managed process. |
| Gateway Codex | Terminal detector; module commentary mentions app-server but `ADAPTERS` at `src/native.rs:81` registers only OpenCode | No implemented provider request adapter. |
| Claude and other terminal providers | Parsed prompt/options/fingerprint | Observation only. Identical later requests can have identical fingerprints. |
| tmux or arbitrary existing terminals | Reusable pane destination and ordinary terminal input | No strict decisions or strict managed interruption. Ordinary terminal controls remain available. |

Do not advertise `work_decisions_v1` for any current adapter. Implemented observation can be exposed separately with `answerable: false` and a reason. Do not fabricate a request ID from a fingerprint, launch ID, timer, or screen revision. Native hooks reporting blocked/unblocked also do not establish an answerable occurrence.

Scoped interruption is a separate deliverable and capability: `work_interrupt_v1`, backed by native `agent_interrupt_bound`. It must not wait for an approval-provider integration. Unix can qualify after native actor tests; Windows stays unavailable until an equivalent writer exists.

## 2. Existing gaps to preserve as legacy, not promote

`AnswerApprovalBody` (`Gateway src/main.rs:965`) has no expected native request ID. `answer_native_approval` (`:8910`) reads the currently pending request and answers it, so a stale A decision can target B. A native transport error is collapsed through `unwrap_or(false)` into `approval_changed`, which loses uncertainty. The terminal path (`:8764`) uses optional content fingerprint, then sends keys followed by delayed Enter if the same fingerprint remains; equal-looking later occurrences and external menu transitions remain unsafe for a strict claim.

`interrupt_pane` (`:7574`) resolves the current pane's observed agent/title, then sends a key through that reusable pane. It has no expected launch, durable request key, or native write receipt. Keep existing endpoints and response shapes for compatibility; new task actions must not call them as fallback.

## 3. Strict decision protocol

Add `GET /api/sessions/{session}/work/tasks/{task}/attempts/{attempt}/decisions` and `POST .../decisions/{decision_id}/answers`. GET is read-only; it never creates a permission in the provider. The POST body is strict JSON:

```json
{
  "request_key": "client-owned-key",
  "expected_revision": 12,
  "expected_instance_id": "launch-opaque",
  "expected_native_owner_epoch": "owner-opaque",
  "expected_decision_revision": 1,
  "expected_options_digest": "sha256-hex",
  "option_id": "provider-option-opaque"
}
```

Persist a decision with session/task/attempt, exact launch/epoch, immutable provider binding ID, provider connection generation, provider session/thread ID, provider request ID, request revision, context digest, and an ordered canonical options digest. Option entries preserve provider identity, semantic response, grant scope and displayed meaning. Localized labels are presentation; index alone is not authority. Unknown persistent-grant scope is explicitly unknown, never inferred as a project-wide permission. Context is untrusted display data, not an instruction.

Provider occurrence identity must have a documented uniqueness scope and replacement policy. Bind that scope into the Gateway identity. Reused IDs after reconnect require a new provider epoch; an unprovable epoch makes old records unavailable. A Gateway UUID names the record but does not manufacture provider evidence.

Admission transaction validates the paired human, session/task/attempt, exact launch/epoch, task revision, immutable occurrence/options and `pending`. It inserts a normal durable operation and consumes one occurrence through a unique constraint on `(provider_binding_id, provider_epoch, provider_request_id, request_revision)`. The body digest includes every expected identity and option. Same-key identical POST returns the recorded operation without provider I/O; changed payload conflicts. Another key/device cannot acquire the same consumed occurrence.

After committing `submitting`, dispatch exactly once to the captured immutable provider channel. A native provider compare-and-answer must atomically verify the occurrence and option meaning with the actual provider response. A Gateway read-then-HTTP-write does not supply this atomicity. Provider response evidence returns `acknowledged`, `not_pending`/`changed` with explicit no-effect proof, or `unconfirmed`. A missing occurrence after a lost response means resolved elsewhere/unknown outcome, not proof of our decision. Do not retry automatically, including after restart. Uncertainty consumes the occurrence until authoritative receipt reconciliation; no automatic retargeting or unlocking.

The operation receipt GET is the existing task operation query. It returns the original selected option and occurrence. An acknowledged answer means the provider accepted that answer, not that the approved command succeeded or the task completed.

## 4. Provider integration prerequisite

OpenCode is the closest existing candidate but remains disabled until all of these are implemented and tested:

1. Establish an explicit provider session/channel during managed launch and bind it to launch ID/owner epoch. Never choose the newest session by directory. Two agents in one repository must remain distinct.
2. The provider channel must have a known lifetime and reconnect generation. Endpoint URL equality is not lifetime evidence. Use server-owned launch registration or a trusted provider handshake whose child/session relationship is established by the launch adapter; arbitrary child-submitted claims are insufficient by themselves.
3. Verify that the provider compares an outstanding request ID atomically, cannot reinterpret an option on a reused ID, and classifies refusal versus uncertain transport. Captured JSON fixtures establish shape only. Validate actual provider behavior before capability publication.
4. Native binding closure must not let a stale Gateway call answer a surviving provider session after the owning launch exits. A native admission ticket can linearize acceptance before exit; accepted in-flight work may finish after exit and is reported as such. Anything admitted after closure is refused. The provider channel itself must target only the bound session, never a replacement.

Codex requires a separate actual app-server adapter and managed session binding before the same gate can pass. Claude and generic terminal menus remain inspect-only until a provider protocol supplies the required evidence. No live external documentation was needed to establish these source gaps.

## 5. Exact-instance interruption

Add `POST .../attempts/{attempt}/interruptions` with `{request_key, expected_revision, expected_instance_id, expected_native_owner_epoch}`. The user chooses the exact attempt, not a current-lead alias. Require a paired human; local result/delegation grants cannot invoke this endpoint.

Add native RPC `agent.interrupt_bound` with `{operation_id, expected_launch_id, expected_owner_epoch}` and response:

```json
{
  "type": "agent_interrupted_bound",
  "operation_id": "operation-opaque",
  "launch_id": "launch-opaque",
  "owner_epoch": "owner-opaque",
  "receipt_id": "interrupt-receipt-opaque",
  "input_disposition": "written",
  "bytes_written": 1,
  "key": "Escape"
}
```

The key comes from the immutable validated launch profile and a tested mapping, never the current terminal title, arbitrary caller bytes, or a generic shell fallback. Unsupported mappings refuse before input. A one-byte Escape is a cancellation request for supported profiles, not a signal/kill and not proof of provider cancellation. Expose UI wording “Interrupt requested”; only existing successful-wait lifecycle evidence can establish process exit. Do not release reservations or revoke result authority merely because this key was written.

Native admission validates epoch and resolves the exact owned runtime. Add an ownership-only control permit that works while the agent is working or blocked; the current prompt readiness permit deliberately refuses these states and must remain unchanged. Keep the permanent invalidation fence shared between prompt and control permits. Native registry operation IDs deduplicate interruption; same ID/digest returns retained receipt, changed digest conflicts. Use a separate bounded registry rather than consuming the existing start-operation capacity. Reject before side effects when receipt capacity is exhausted; do not evict dedup keys in a way that permits replay. After restart the old owner epoch refuses without input.

At the actor, interruption is one typed control command with a single actual write and no delayed Enter. Serialize with ordinary managed prompt chunks. If interruption cancels a queued managed prompt, remove all remaining chunks, invalidate its permit, and return zero-byte refusal or partial-delivery uncertainty according to its actual byte count. Complete a currently executing syscall first; linearize interruption at the actor control write. Do not allow an old queued Enter to run afterward. Unrelated legacy terminal input remains legacy and cannot be represented as request-bound approval input.

Native zero-byte typed refusal yields `refused`; short/failed write, lost completion receipt or disconnect after admission yields `unconfirmed`. For a one-byte successful syscall, the receipt means exactly that byte was written. No blind retry, synthetic success from screen change, delayed Enter, signal escalation or process replacement.

## 6. Lock ordering and recovery

- Gateway lock order: paired-device registry, then WorkStore for authorization/admission; release both before filesystem/native/provider I/O. Use the existing guarded mutation approach so revocation before the admission boundary prevents new action. A revocation after committed admission may leave an in-flight action; document that boundary rather than implying retroactive cancellation.
- SQLite transactions own revision checks, occurrence consumption and request-key receipts. They never wait on the native actor or provider HTTP. Do not hold a global async mutex across provider latency. A crash after `submitting` leaves an unconfirmed operation; recovery never redispatches it automatically.
- Native actor owns ordering of prompt/control writes; owner lock protects the actual syscall and permanent invalidation. Establish owner-state then receipt-counter ordering, matching the existing guard. Never hold registry-global locks while awaiting an actor/provider response. Capture a local admission ticket under the registry lock and perform I/O after release; completion updates the same operation identity.
- Provider decision authority lives at the provider/channel compare-and-answer boundary. Neither the lifecycle registry nor a PTY-owner lock can freeze an independently changing external approval menu. A strict implementation must reject unsupported channels instead of pretending those locks solve the external race.
- Bounded terminal outcomes and consumed occurrence facts survive Gateway restart. Do not delete them to free a concurrency slot. Start with explicit quota refusal and no GC; a later archival scheme must preserve replay rejection. Native epoch changes make missing receipts unknown, never permission to resend.

## 7. Required tests and file ownership

| Owner boundary | Exact files / changes | Required evidence |
| --- | --- | --- |
| Gateway domain/store | `src/work/model.rs`, new `src/work/store/decisions.rs`, schema initialization in `src/work/store.rs` | A→B stale answer, same-text different ID, same ID/new epoch, changed options, duplicate key, two-device occurrence race, unknown receipt recovery, capacity/transaction rollback |
| Gateway service/HTTP | New `src/work_decisions.rs`, new `src/work_interrupt.rs`, `src/work_http.rs`; parent alone wires `src/main.rs` routes | Wrong task/session/actor, local grant denial, paired revocation before commit, frozen actor/identity during await, no native invocation on any rejection, no fallback to legacy routes |
| Gateway backend/provider | `src/backend/model.rs`, `src/backend/mod.rs`, `src/backend/herdr.rs`; new strict provider module adjacent to `src/native.rs` | Explicit capability only; exact epoch/launch/receipt validation; lost provider response remains unconfirmed; two sessions in same directory never cross; current legacy adapter unchanged |
| Native schema/dispatcher | `src/api/schema/agents.rs`, `server.rs`, `response.rs`, schema registration/tests; `src/api/server.rs`, `src/app/api/agents.rs`, `src/server/headless.rs` | Additive JSON methods; headless and attached dispatch share completion semantics; frozen codec fixtures untouched; unsupported Windows capability false |
| Native runtime/actor | `src/pty/bound_submission.rs`, new bounded control receipt registry, `src/pty/actor/unix.rs`, `src/pane.rs`, `src/terminal/runtime.rs` | Exit/replacement before write gives zero bytes; active/blocked control admission; prompt text written then interrupt removes queued Enter; duplicate operation writes once; lost ACK returns stored receipt; saturated registry refuses before write |
| Native future provider binding | New provider-session registry/module owned separately from PTY readiness; lifecycle integration via explicit admission tickets | Child exit/reconnect closes admission, same repo cannot identify binding, no lifetime adoption by alias, provider compare-and-answer atomic tests before capability enabled |

Deterministic tests assert actual zero native/provider writes on refusal, not just error status. Real isolated simulated native-process tests establish interruption wire/PTY ordering only. Real provider approval proof requires explicit permission before answering trust or tool prompts; no fixture or detector test can replace it. Full Gateway/native gates and paired App tests remain required before shipping the relevant surface.

Recommended implementation sequence: (1) scoped interruption end-to-end, (2) strict decision storage and inspect-only unavailable responses, (3) one explicitly bound real provider channel, (4) publish strict capability only for that proven channel. The App can expose terminal inspection throughout without offering a structured approval guarantee it does not yet have.
