# Task workflow interaction and safety contract

Status: proposed, not implemented. Read the
[Gateway architecture and execution plan](./task-workflow-design.md) first.
The App repository owns `docs/task-workflow-interactions.md`, including screens
and user-facing copy. These contracts define the backend guarantees supporting
that interaction design. Gateway #26 and App #77 are already merged; the new
task and safety packages remain future work.

The UI should make normal work straightforward, but each claim shown to the user must have a corresponding backend precondition or recorded outcome. A disabled button, displayed UUID, or skill instruction is not an authorization mechanism.

## 1. Bind every action to what the user saw

| UI action           | Required server preconditions                                                                     | Outcome visible to the user                                              |
| ------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| Start task          | Authorized project/session, allowed launch profile, request key, fresh eligibility                | Durable task/operation ID; separate creation, launch and prompt outcomes |
| Send to lead/worker | Task/attempt scope, expected live instance, valid attachment references, permitted delivery state | Acknowledged, refused before input, or unconfirmed                       |
| Answer approval     | Exact request occurrence + instance + selected option, still pending                              | Answer acknowledged, resolved elsewhere, changed, or unconfirmed         |
| Accept result       | Authorized human actor, specific submission/version and expected revision                         | Recorded review of those bytes, or a conflict                            |
| Interrupt assistant | Expected instance and explicit target                                                             | Interrupt requested; do not claim process termination without evidence   |
| Pause delegation    | Current task revision and actor authority                                                         | Policy persisted; new managed launches refused after the policy boundary |
| Open preview        | Authorized versioned asset/reference and safe content handling                                    | Requested artifact/version or a clear missing/changed response           |

Do not retarget a stale request to a new lead, a replacement pane occupant, a newer approval or the latest result. Conflicts must return enough typed context for the App to offer review, without performing the replacement action first.

## 2. Approval identity is a concrete prerequisite, including an existing source gap

At reviewed Gateway head `b6bca76`, [`AnswerApprovalBody`](https://github.com/osuki-dev/muqun-gateway/blob/b6bca766858545d9628bcc261e933134f528c624/src/main.rs#L953) accepts an option/decision and optional fingerprint, but not the native approval ID the caller saw. [`answer_native_approval`](https://github.com/osuki-dev/muqun-gateway/blob/b6bca766858545d9628bcc261e933134f528c624/src/main.rs#L8754) fetches the request pending now and answers that request. The native ID protects that fetched request from changing afterward; it does not establish that it is the request the user originally reviewed.

Concrete source-level failure scenario for a native-aware client: user sees request A; A resolves elsewhere and request B becomes pending; the client posts A's intended `allow`; the handler fetches B and applies that decision to B. This has not been reproduced against a live provider. The current App's fingerprint-only parser may not render these native requests at all, so this is not a claim that its existing banner already exposes that exact scenario. It is a backend contract gap that must be closed before the proposed native-aware decision UI ships. It predates merged Gateway #26.

For the new structured decision contract:

- Observation returns a typed identity: native request ID when available, or a Gateway-observed menu occurrence plus fingerprint. Include session, attempt, instance, source, options and observation revision.
- The answer carries that exact identity and chosen option. Compare before sending anything. A missing identity is not “answer whichever request is current.”
- A fingerprint identifies content, not necessarily an occurrence. Two identical-looking prompts at different times must not share authorization. If the menu path cannot establish a fresh occurrence and verify delivery, expose it as inspectable terminal content rather than a strongly safe structured approval action.
- Prefer provider request-ID APIs. For terminal menus, serialize answer attempts and revalidate before each permitted input step; do not claim this makes an external terminal transition atomic. Never replay Enter to force a decision through an uncertain outcome.
- Concurrent answers from two devices must consume one matching occurrence at most once through Gateway. If a native response is lost, persist uncertainty and reconcile; do not re-answer a newer request or replay the old one.
- Bind option meaning as well as its index. The App must not translate an old “Allow once” into the same numeric slot of a changed menu.
- Expiry, disconnection, identity loss or a changed request yields `approval_changed`, `approval_not_pending` or a typed unavailable outcome with no input. A backend/provider unable to satisfy the advertised guarantee gets a narrower capability.

Keep legacy compatibility explicit. Add a strict contract/capability or safely compatible request fields, but do not label an old unbound decision endpoint as the new guarantee. Extend App parsing to distinguish native identity from menu identity; do not manufacture fingerprints from native IDs merely to pass existing UI validation.

## 3. Execution scope and capability evidence

The approved launch settings and delegation policy belong to a task revision. A lead cannot add an agent kind, broaden allowed project paths, raise concurrency, enable unattended approval flags or give a worker user-acceptance authority by editing its prompt. Validate all managed operations against the saved policy.

Use scoped local authority for agent-facing calls, distinct from mobile pairing/admin credentials. A worker can submit its result or inspect permitted context; a lead can request bounded child work; only an authorized human client can grant the relevant user decisions. Parent/child IDs do not themselves confer access. Use the authenticated actor, not a client-supplied role field.

Enforce the delegation limit transactionally: reserve a launch slot before external execution; uncertain launches retain their reservation until reconciled. Otherwise concurrent requests or lost acknowledgments can exceed the limit. Pause policy and dispatch claiming must share a defined ordering: a launch already claimed may be in progress, while requests not yet claimed are refused. Show that distinction; do not promise that pause undoes an in-flight launch.

These controls cover Gateway-managed operations. They do not revoke an agent's independent shell, Herdr socket or network access. Actual OS sandboxing depends on verified runtime configuration. Neither a Git worktree nor a skill file is a security boundary. The UI must not claim restricted execution unless the underlying environment enforces it.

## 4. Destination, draft and attachment safety

Uploads and registered references retain their server/session/project scope and content identity. Revalidate them when the task destination changes and before delivery. A new task must not receive a raw file path staged for another machine or a stale local handle. Canonicalize paths and validate regular files under allowed roots; account for symlink replacement and changes between validation and reading.

Task creation and sends use the durable request-key semantics from the architecture plan. Double taps and reconnect queries return the same receipt. A new draft typed while an earlier send resolves must not be cleared by an old acknowledgment; return the submitted draft/message identity so App can reconcile it precisely.

The first version has no invisible offline input queue. Reconnection refreshes state and receipts; it does not send drafts, approvals or uncertain operations automatically. Distinguish positive no-input refusal from a failed network response after possible effects.

## 5. Results, previews and private data

A submission contains versioned artifact references and evidence provenance. Review targets a specific submission; it must not resolve a moving “latest” alias after the click. Retain prior acceptance as a historical fact if a later result is submitted. Current-task review state must clearly distinguish that older acceptance from a new unreviewed submission.

Treat generated Markdown, HTML, filenames, links and terminal content as untrusted output. Result text cannot trigger a tool, grant permission, change a task policy or create an authenticated request merely by containing an instruction/link.

Generated web previews must not share an authenticated Gateway origin, cookie/token context, native bridge or privileged WebView messaging channel. Default artifact previews should not fetch arbitrary remote content referenced by Markdown. If a server-side preview fetch is introduced, it needs a separate URL/network policy; otherwise it could become a path to private services. External preview navigation is explicit and identifies the destination. Do not put paired credentials in URLs or logs.

Keep notifications minimal by default: task/decision identifiers and generic attention text, not commands, prompts, file contents or secrets. A notification/deep link selects a view; authorization and current request identity are checked after opening/unlocking. Revoked pairing invalidates task queries, SSE and mutations as well as terminal access. Local worker authority must have its own revocation/expiry rules.

Apply protected permissions to task storage and avoid retaining full terminal recordings as an incidental task feature. Persisted App drafts require protected local storage; history removal must not be described as deleting already uploaded or agent-held data. Define task/artifact retention and deletion semantics separately from hiding an item in one device's history.

## 6. Additional executable work packages

| Package                             | Changes                                                                                                                         | Acceptance tests                                                                                                                                                             |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| GW-S1: request-bound decisions      | Extend approval observation/answer models and provider/menu adapters; persist occurrence and answer outcomes; strict capability | A→B stale answer refused; same-text later prompt distinguished or unsupported; duplicate and concurrent answers; instance replacement; lost response; zero input on conflict |
| GW-S2: policy and actor enforcement | Task policy revisions, scoped local actor, transactional limits and pause/claim ordering                                        | Worker cannot accept results/change policy; cross-task/session calls denied; simultaneous launch limit; unconfirmed launch retains slot; pause race classified accurately    |
| GW-S3: preview/result privacy       | Version-bound review, validated artifact serving, isolated preview handling, minimal notifications                              | Changed/deleted file; symlink escape; malicious HTML/link; no Gateway credentials in preview; old submission acceptance never targets new bytes                              |
| APP-S1: decision/recovery UI        | Native/menu identity-aware parsing, stale/uncertain states, explicit persistent-grant scope                                     | Actual options preserved; no preselected answer; no auto-resend; stale prompt retains safe view; unknown grant scope not misrepresented                                      |
| APP-S2: usability and accessibility | Destination labels, stable list/detail focus, task/result version selection, protected drafts                                   | Recipient identifiable before send; background update preserves snapshot/focus; large text/touch targets; offline history; lock-screen link cannot act                       |

GW-S1/APP-S1 are prerequisites for the new structured decision screen, not a reason to disable existing ordinary terminal use. GW-S2 depends on task policy/storage; GW-S3 depends on the result model. Coordinate schema/capability changes before separate agents implement each side.

For every security-sensitive test, assert the absence of native input or side effects on rejection, not merely the HTTP error status. Use deterministic adapter fakes for ordering/fault injection, isolated real backends for native guarantees, and native App flows for visible behavior. The repository's full checks and paired-device requirements still apply. Design review and documentation checks do not establish runtime security guarantees. Run these tests in the implementation packages.
