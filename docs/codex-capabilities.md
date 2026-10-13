# Codex capability discovery

The protocol baseline remains `rust-v0.162.1` at commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`. Stable requests and experimental
annotations are defined in upstream `app-server-protocol/src/protocol/common.rs`.
Gateway keeps `experimentalApi: false`.

Discovery describes adapter support, connection availability, and model catalogs
separately. A feature flag never grants control of an external session. The App
must combine the selected provider's enabled/connected status with the selected
session's `runtime.source` and `runtime.interaction`. Missing runtime metadata on
an older Gateway retains that older contract; missing optional feature flags are
not proof of support.

Ready Codex appears first and ready OpenCode second; connected alternatives
precede unavailable providers. Unavailable Codex remains after existing fallbacks.
Implicit runtime selection uses Codex/OpenCode/DeepSeek/T3 preference among
attached providers. Explicit provider selections and recorded session ownership
do not change. HTTP, CLI, and bounded discovery fallback snapshots share this
ordering.

| Surface | Gateway contract | Boundary |
| --- | --- | --- |
| Models and effort | `modelSelection`, `reasoningEffort` | True only with an actual connected catalog; tiers are per model |
| Shared transport | `sharedDaemonConnected`, `runtimeTransport` | Actual attached transport: `shared_daemon`, `private_stdio`, or `unavailable` |
| Shared task startup | `sharedTaskStartup` | True only on a connected verified shared daemon; launch eligibility is checked separately |
| Shell tool output | `shellToolItems`, `typedToolItems` | Command execution transcript and streamed output; no shell process controls |
| Background terminals | `backgroundShells: false` | Upstream list/clean/terminate requests are experimental |
| Changed files | `fileChangeItems` | Producer tool patches, separate from current repository changes |
| Git identity | `sessionGitInfo`, `sessionGitInfoScope: producer_snapshot` | Stored producer branch/SHA/origin; not live status |
| Live repository changes | Gateway `agent_vcs_files` capability | Existing provider-independent Git routes resolve the selected session directory |
| Legacy agent diff | `legacyVcsDiff: false` | Codex does not implement the provider-specific `/vcs/diff` operation |
| Subagents | `subagentSessions` | Typed child identities/status and session navigation; controls depend on each child's runtime |
| Usage | `sessionTokenUsage`, `sessionContextUsage` | Actual observed totals/context metadata; absence remains unknown, not zero |
| Questions and approvals | `forms`, `asyncQuestions`, `toolApprovals` | Live runtime callbacks/active-turn steering, never historical approval authority |
| Attachments | `attachments`, `attachmentsByPath`, `transcriptImages` | Uploaded host paths in prompts and resolved transcript images; no host path rendered as a phone image |
| Audio/generated images | `audioPlayback: false`, `generatedImagePreview: false` | Typed output may be retained without a media player or preview |
| Compaction | `compaction` | Stable `thread/compact/start`; owned native idle sessions only, no queued delivery or automatic resume |
| Fork | `sessionFork` plus `agent_session_fork_v1` | Whole-thread fork for Codex; message-boundary fork is unsupported |

`POST /api/agents/{agent_id}/sessions/{asid}/fork` is the provider-scoped fork
route. `POST /api/agent-sessions/{asid}/fork` uses recorded session routing.
Both require paired-device authentication, accept an optional JSON object with
`message_id`, and return the same content envelope containing `AgentSessionInfo`
as session creation. `{}` requests a whole-session fork. The returned session
is recorded under the original provider and added to its mirror. Neither route
submits a prompt or retries an ambiguous fork.

Remaining stable upstream surfaces are not automatically equivalent to Gateway
operations: `thread/shellCommand` executes an explicitly unsandboxed shell and is
not a slash-command API; `skills/list` alone does not satisfy skill activation;
upstream thread rollback does not restore worktree
files and therefore does not satisfy Gateway's file-restoring revert contract.

Compaction uses the existing `POST /api/agent-sessions/{asid}/compact` route with
an absent body or `{}`. A non-null `delivery` is unsupported. Gateway requires a
previously accepted live thread, a current shared subscription when applicable,
native interaction authority and an exact fresh idle status under its mutation
lock. It never rejoins or cold-resumes a session for this operation. The response
acknowledges the request; official events describe actual progress. No automatic
retry occurs. Upstream has no conditional turn/version field for compaction, so
the Gateway lock cannot exclude concurrent actions by another official client.
