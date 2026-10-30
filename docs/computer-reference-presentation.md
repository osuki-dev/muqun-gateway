# Computer references and user-message presentation

Computer references were introduced by `e0b367c6089be42189648ed1c2939da37324252d`
(PR #67). `agents/computer_context.rs` validates linked computer identities and
appends model-facing instructions to the prompt. Those instructions contain
opaque machine IDs; labels are display data, and credentials are excluded.

The model must retain this context. A transcript should display the user's
original text without claiming that matching words in arbitrary user input are
Gateway-generated metadata.

## Codex correlation

Gateway passes original display text separately through its internal prompt
port. Other adapters retain their existing delivery and projection behavior.
Codex keeps the complete prepared native input, including computer references
and uploads. Immediately before dispatch, it creates a random
`clientUserMessageId` and durably records the original display text and the
complete native text projection, scoped to the verified Codex home and thread.
Queued input carries this typed presentation separately; the inbox displays the
original text. Both normal and steering submissions retain the native client ID.

The pinned official protocol supports this field in
[turn/start](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/TurnStartParams.ts)
and
[turn/steer](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/TurnSteerParams.ts).
A native
[user message](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/app-server-protocol/schema/typescript/v2/ThreadItem.ts)
reports it as `clientId` in live events and canonical history.

Presentation changes only when the thread, native client ID, and complete native
text match a retained record. Live events and history use the same projection.
Non-text attachments retain their native identities and paths. A different
thread/home, missing or unknown client ID, altered text, and user-authored copies
of the old context suffix retain their full native text. There is no text marker
or regular-expression deletion. Existing plain history is not rewritten.

The client ID correlates presentation. It does not grant runtime ownership or
make prompt submission idempotent. Existing ambiguous-delivery handling and the
rule against automatically resending prompts remain unchanged.

## Storage and failures

A separate `codex_prompt_presentations` migration namespace adds its table;
the existing `codex_native_sessions` namespace and version remain unchanged.
Older Gateway versions can still open their known namespaces after this additive
migration. The table shares Gateway's
existing SQLite/SQLCipher store and its configured encryption; no separate file
or key is introduced. Original native-session ownership records are unchanged.
Presentation records alone never authorize a native session resume.

The metadata store is bounded to 4,096 records and 16 MiB of native/display text.
A new referenced submission that cannot retain its presentation fails before its
model request is written. Existing presentation records are not evicted to make
space. Confirmed thread deletion removes its presentation records through the
existing native-session cleanup. The in-memory lookup cache is bounded too.

If presentation metadata cannot be read, or an older native producer omits
`clientId`, Gateway preserves the full native text. It never guesses which text
the user wrote or hides content using the old suffix's wording.

## Verification

Core tests cover migration from the prior namespace version, persistence across
reopen, home/thread/client-ID/text fences, native input and attachment retention,
live and reopened-history projection, and presentation across queued and steering
submissions. A real paired App check remains the deployment acceptance: send a
computer reference in an isolated Codex session, verify original text in the live
transcript and after history reload, and verify that the model can still address
the referenced machine by its immutable ID. Do not automatically approve agent
trust or tool requests during this check.

## Automatic Codex titles

On verified shared Codex daemons, a complete automatic preview can also repeat
model-facing context. Gateway projects that preview only after a fresh ascending
canonical history read proves that the first native user item has the retained
client ID and exact complete text. A matching later submission is insufficient.
Explicit native names, truncated previews, older messages without client IDs,
unknown history formats and unverified/private transports retain their native
titles. User-authored copies of the context instructions are not parsed.

These optional reads use a separate verified read-only connection, without
resuming a thread, changing history cursors, renaming it or sending a prompt.
All title proofs in one inventory share a two-second allowance, including
storage, admission and connection initialization. A failed or incomplete proof
preserves the native preview and does not fail the inventory. The connection may
be reused; first-item evidence is read afresh because native rollback can change
canonical history. The same title projection feeds session reads and live
metadata mirrors. No native title or stored transcript is rewritten.
