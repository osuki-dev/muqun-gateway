# Bounded work record reads

Task detail retains the full record shapes in `attempts`, `operations`, `results`, and
`reviews`, but each array is a first page. The additive `pages` object contains one
entry for each array:

```json
{"snapshot_revision":42,"after_id":null,"next_after_id":"opaque-record-uuid","has_more":true}
```

A page contains at most 20 records and at most 2 MiB of JSON-encoded record bytes.
The limit counts escaping, including control characters. Detail combines four such
pages and the task metadata; it does not return the entire history. Clients must
check every `has_more`, visibly distinguish incomplete history, and avoid selecting
an agent or inferring absence of pending operations from an incomplete first page.

Continue with `GET /api/sessions/{session}/work/tasks/{task}/records` and query
parameters `kind` (`attempt`, `operation`, `result`, or `review`),
`snapshot_revision`, `after_id`, and optional `limit` (1–20, default 20).
The response is `{ "items": [...], "page": {...} }`. IDs sort ascending; pass the
returned `next_after_id` unchanged. An absent next cursor ends the scan.

Each read is a transaction. If the task revision changed since the initial page,
the continuation returns `409 revision_conflict`. Refresh explicitly and restart
that history scan; never merge pages from different revisions. This is a checked
snapshot revision, not a retained historical database snapshot.

An exact historical submission remains accessible through
`GET /api/sessions/{session}/work/tasks/{task}/results/{submission}`. An exact review
is accessible through the corresponding `reviews/{review}` route. These immutable
reads require no snapshot revision, validate the task and session scope, and remain
available after newer results or reviews appear. Artifact reads continue to use the
selected immutable submission ID.

Pagination affects transport reads only. Internal consistency checks retain complete
records. Existing valid data is preserved, and no new admission quota may prevent
checkpointing or finalizing already-created native resources.
