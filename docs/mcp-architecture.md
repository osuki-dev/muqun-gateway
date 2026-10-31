# MCP authority and App contract

Status: implementation and acceptance in progress. This document specifies the
target contract; it does not assert that deployment or device acceptance has
passed. See the implementation report for measured results.

## Purpose and ownership

Gateway owns tool registration, credentials, authorization, provider adapters,
remote execution and task recovery. App supplies user intent and renders the
existing session and task streams. Adding a provider or changing MCP transport,
credential renewal or storage must not require an App protocol change.

This does not promise that every future product feature needs no App changes.
A new user interaction can require a new client. Backend implementation changes
must remain behind this contract.

There is one MCP endpoint in each Gateway process, with one named `muqun`
registration in a provider's supported configuration scope. A per-task
credential is an authorization record, not another MCP server or daemon.

## Findings in the previous implementation

1. A shared agent credential identifies a provider runtime, not a particular
   conversation. Remote execution nevertheless scanned running tool calls in
   provider histories to infer its caller. Missing child sessions, bounded
   history windows and unrelated provider failures could reject valid calls.
2. That scan did not create a security boundary: sessions holding the same
   credential have the same authority. Inferring task ancestry cannot safely
   reduce a bearer credential's permissions.
3. Provider history inspection was coupled to App pagination and presentation
   order. Authorization must neither consume an App cursor nor repair its
   history cache.
4. Token lifecycles differ between helper-based and environment-based hosts.
   A connected MCP transport alone does not prove its token remains usable.
5. A peer without a transport key could send its bearer over plain HTTP or WS.
   Pairing with a plaintext code also exposed the secret from which the claim
   encryption was derived. Encrypted responses alone do not fix that.
6. App already sends immutable computer identities, but some pre-dispatch
   failures were flattened into generic errors. A known refusal could be
   presented as an unknown delivery outcome.

## Authority model

Authentication, permission checks and task attribution are separate operations.

| Credential | Meaning | Remote authority | Attribution |
| --- | --- | --- | --- |
| Paired App device | User-controlled client | Existing device/peer policy | Device identity |
| Registered runtime | Owner-authorized local provider registration | Explicit linked-computer grant | Runtime principal; optional task metadata |
| Task-bound assistant | One immutable stored assignment | Restricted by that assignment and ancestry | Exact task |
| Pane-bound assistant | One managed execution context | Existing bound-context policy | Exact pane, resolved assignment |
| Peer Gateway | One paired remote computer | Receiver's authorized local operation only | Authenticated peer identity |
| Legacy shared agent | Existing credential without explicit runtime grant | No implicit elevation | Existing principal |

The server derives authority from its token record. A body containing a computer
ID, task ID, provider ID or parent ID does not grant authority. Names never
select the security principal. Runtime credentials do not automatically take
ownership of tasks belonging to legacy credentials.

Ordinary runtime `exec.run` and `tasks.delegate` do not read provider histories
to authorize execution. Missing task attribution is not an execution failure.
They return a durable task handle; the caller can use `tasks.read` and
`tasks.wait`. Gateway must not send unsolicited results to a guessed session.
Task- and pane-bound calls can retain exact result notifications.

Completing an assignment is a separate operation. A runtime cannot guess another
session's assignment from its display title or declare it complete merely
because the provider became idle.

### Remote delegation and credential isolation

Incoming remote work must not inherit the receiving host's owner-level runtime
credential. Before creating a task or provider session, Gateway checks whether
the selected delivery mechanism can bind restricted credentials to it.

When a provider supports only a shared credential, that restricted delivery is
unavailable. Reject it before dispatch with an actionable machine-readable
error. Do not create the session and then try to remove its permissions. Do not
silently switch agent, delivery mode or credentials.

Local children of restricted tasks retain the restriction. A remote-origin task
cannot regain remote authority by creating a local intermediate assignment.
Existing restricted sessions must also be considered before registering a new
owner-level runtime credential. Cancellation alone does not prove a provider
session, or its children, has disappeared; its deletion does, and then the
restriction clears. Uncertain migration fails closed;
there is no automatic deletion of user conversations.

These are credential and workflow boundaries, not an OS sandbox. A process
running as the owner that can read administrative bootstrap secrets already has
owner-level access. Shell execution is not isolated by an MCP token.

## App request contract

Keep the existing endpoints and wire vocabulary. No arbitrary `options` object
or provider-specific MCP fields are added.

| Input | Meaning and ownership |
| --- | --- |
| Selected Gateway connection | The authenticated execution host; not a display name |
| Route `asid` | Opaque conversation identity on that Gateway |
| `computer_ids: string[]` | Opaque referenced computer identities, validated by Gateway; context, not a permission grant |
| `text` / `goal` | The user's message or new task goal |
| `attachments` | Existing validated upload references on the execution host |
| `delivery` | Existing explicit session/pane or prompt delivery choice |
| `agent_id`, model choices | Existing discovered catalog values for new tasks; provider setup remains server-side |
| `client_ref` | Opaque correlation reference for one logical submission; not permission to replay it |

The execution Gateway and referenced computers are distinct: sending a message
to a conversation on computer A while referencing B does not move that
conversation to B. App state is keyed by Gateway identity plus conversation or
task identity. Renaming a computer does not change either identity. Re-pairing a
replacement identity must not silently retarget a saved reference.

Gateway bounds and deduplicates computer references. It checks all references
before dispatch; no partial send with silently dropped references. Existing
`mentions` names are display/legacy input only when authoritative IDs are
present. Neither prompts nor responses contain peer credentials, transport keys,
local MCP URLs or helper commands.

### Capability and response rules

- Preserve `agent_computer_context_v1` semantics. Negotiate optional behavior by
  capabilities, not guessed version numbers or provider-name checks in App.
- Keep task state, session output and results on the existing task/session
  streams. Do not add readiness polling or a second MCP-specific App store.
- Add stable error codes for reference validation, computer-tool readiness and
  unsupported restricted delivery. Unknown codes retain a safe generic fallback.
- Mark `delivery: "not_sent"` only when Gateway proves dispatch did not start.
  An HTTP timeout or a lost response after dispatch is not such proof.
- `client_ref` is initially correlation only. Existing code does not establish
  end-to-end exactly-once execution. No automatic replay of ambiguous prompt,
  command or task creation is allowed. A future receipt implementation can use
  this field without changing the request shape; a new recovery UI would still
  require client work.
- Adding optional response fields must not break existing decoders. Never
  reinterpret an existing flag to mean a stronger safety guarantee.

## Registration and credential lifecycle

Use the existing provider adapter seam. Core policy asks for supported behavior;
Codex, DeepSeek, OpenCode and T3 adapters translate their official APIs. T3 reuses
its underlying provider's tools when supported; its name alone is not evidence
of credential isolation or tool readiness.

Registration is single-flight within the provider's actual configuration scope.
Two simultaneous sends must not mint competing credentials or create duplicate
registrations. Different providers must not block one another behind an
unbounded global registration operation.

Helper-based hosts obtain credentials from a verified local Gateway endpoint.
Issuance requires local administrative authority; knowing a task or provider ID
is insufficient. Configuration stores the helper command, not a bearer.
Environment-based hosts receive the credential only in their owned process
environment. An adopted process is not silently restarted to inject tools.

Credential policy distinguishes expiration, rotation and disabling a grant:

- Expiration rejects the presented token. Renewal uses the host's supported
  helper or registration path, without resending a user operation.
- Replacement becomes active only after supported registration/readiness
  succeeds; failed replacement does not destroy a working registration.
- Disabling a registration or ending a task grant prevents renewal, as well as
  invalidating existing tokens. Revoking one token alone must not be described
  as disabling an administrative helper that is still authorized to mint more.
- Process exit, task cancellation/deletion and provider disablement have
  explicit retirement rules. Reopening a retained task requires its original
  binding, never a reused pane ID.
- Runtime registration state and bounded token retirement are Gateway-owned.
  App does not maintain refresh timers or credentials.

Exact lifetimes remain server policy. Implementation must report which hosts
support renewal instead of claiming that every host automatically renews.

## Encryption and secret handling

Reuse the existing authenticated transport and encrypted store. Do not invent
MCP-specific cryptography.

| Material or channel | Required handling |
| --- | --- |
| MCP access-token verification | Store a token hash; do not persist a recoverable bearer just to authenticate it |
| Recoverable peer credentials and transport keys | Existing SQLCipher store, private directories, no plaintext log/config copies |
| Cross-computer authenticated HTTP/WS | Existing authenticated encryption, or certificate-validated HTTPS/WSS; no redirect or plaintext fallback |
| Pairing claim | Protect the pairing secret before sending it; encrypted response derived from a transmitted plaintext code is insufficient |
| Local MCP | Verified loopback endpoint, bearer validation on every request, proxy rejection; local HTTP is not encrypted |
| Provider Unix socket | Verify owner and permissions before adoption |
| Bootstrap/master key | Owner-only secret storage; cannot be protected by placing an equivalent decryption key beside its ciphertext |
| Logs, task text, error responses and App payloads | Exclude credentials and helper authentication output |

Literal loopback may be used for explicit local development transports. A DNS
name such as `localhost` is not accepted merely because of its spelling.

Current at-rest limitations are material: SQLCipher uses a private file key,
and some legacy device/bootstrap JSON files use filesystem permissions rather
than encryption. Do not call these files encrypted. Moving recoverable
credentials requires transactional import, verified readback and a rollback
plan. A missing encryption key must never cause silent key regeneration or a
plaintext fallback for an existing store.

## Availability and bounded work

Authentication is a bounded local-store operation; it is independent of agent
history, model response latency and App pagination. The remote transport keeps
existing deadlines and task handles. A disconnected peer affects that peer's
operations, not all local tools or other computers.

Serialize registration mutations, not all tool execution. Reuse existing task
workers and peer subscriptions. Clean up cancelled waiters and process/token
state, bound history and registration caches, and retain task records needed to
reconcile interrupted remote delivery. Never retry an ambiguous mutation to
make a health check green.

## Acceptance gates

1. Root and native child sessions use the same registered endpoint to run a
   harmless command on a paired computer, without SSH or history inspection.
2. Missing, wrong, expired and revoked tokens fail before execution. App and
   peer credentials cannot stand in for assistant or administrative credentials.
3. Restricted remote work cannot forward onward or inherit an owner runtime
   grant, including through a local child and across restart/upgrade.
4. Unknown or same-named computer identities cannot redirect an operation.
   Invalid references and unsupported delivery return proven `not_sent` only
   before dispatch; the draft remains available.
5. Unencrypted non-loopback credential transmission and insecure pairing fail
   before network I/O. Encrypted links reject tampering and downgrade.
6. Concurrent preparation reuses registration. Failed rotation preserves a
   usable predecessor; disablement prevents helper renewal.
7. Restart, peer disconnect and lost responses preserve truthful task state.
   No duplicate execution or invented success is accepted.
8. Run focused protocol/security tests, final Gateway checks and paired
   App-to-Gateway-to-agent acceptance. Five-device acceptance remains a separate
   merge gate; a local test suite is not evidence of untested platforms.

## Implementation boundaries

- `mcp/auth`, `tokens`, `issue`: authentication, authority issuance and retirement.
- `mcp/inject` and provider adapters: registration and readiness.
- `mcp/lineage`: task attribution and assignment completion, not ordinary runtime authorization.
- `tasks/service`: task creation and explicit remote/delegation policy.
- `fleet/peer_client`: authenticated remote transport and timeout outcomes.
- `agents/computer_context` and API routes: reference validation and pre-dispatch errors.
- Existing session/task stores and streams: App-visible state; no second MCP data model in App.

Related contracts: [MCP tools](mcp.md),
[Codex history consistency](codex-history-consistency.md), and
[codebase ownership](../CONTEXT.md).
