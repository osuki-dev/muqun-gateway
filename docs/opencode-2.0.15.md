# OpenCode 2.0.15 API synchronization

The adapter targets the current v2 API without retrying removed legacy actions.
The App-facing Gateway routes and response envelopes remain unchanged.

Verified against the official [v2.0.15 tag](https://github.com/anomalyco/opencode/tree/v2.0.15)
and its [OpenAPI document](https://github.com/anomalyco/opencode/blob/v2.0.15/packages/protocol/openapi.json):

| Gateway operation | OpenCode request |
| --- | --- |
| Rename session | `PATCH /api/session/{sessionID}` with `title`; success is 204 |
| Replace session permission rules | The same PATCH endpoint with `permissions` |
| Change queued-message delivery | `PATCH /api/session/{sessionID}/inbox/{inboxID}` with `delivery` |
| Clear staged revert | `DELETE /api/session/{sessionID}/revert` |
| Activate skill | `POST /api/experimental/session/{sessionID}/skill` with the skill's `id` and optional `resume` |
| Wait for idle | `POST /api/experimental/session/{sessionID}/wait` |
| Export session | `GET /api/experimental/session/{sessionID}/export?sanitize=...` |

These repairs address drift already present before 2.0.15. Comparing the official
2.0.14 and 2.0.15 OpenAPI documents shows one changed route: session PATCH adds
optional `metadata`. The only changed named schema is `Project.Time`, which adds
`active`. The tagged source also adds the `session.metadata.updated` event. The
Gateway does not expose session metadata or project timestamps, so those additive
fields do not require an App contract change. Session rename still emits
`session.renamed`, which the Gateway already projects into its session mirror.

The official tag page currently contains only the release commit description;
there is no separate published GitHub release note for 2.0.15. The tagged protocol
and server sources, rather than an inferred changelog, are the compatibility
evidence. Local HTTP tests verify the methods, paths, payloads, and 204 handling.
They do not claim end-to-end mutation coverage against a user's running engine.

Deploy the rebuilt Gateway and restart it to apply the adapter changes. No App
migration is needed. Development verification does not restart an existing service.
