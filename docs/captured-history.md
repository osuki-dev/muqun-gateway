# Gateway captured terminal history

Capability: `pane_captured_history` in `/health`, `/api/meta` and discovery's
capability list. Discovery also announces `planes.terminal.features.capturedHistory`.
This is independent of `backends[].features.pagedHistory`, which still describes
**native** ranged `/output` reads. No App UI or App support is implied.

`GET /api/sessions/{session_id}/panes/{pane_id}/history`

Uses the same device authentication and encrypted transport as the other pane
routes. Cursors are not credentials. Terminal contents remain memory-only; a
Gateway restart loses captures and cursors. Existing `/output` tail stitching and
native `start`/`end` range behavior are unchanged.

## Request

| Query | Default | Contract |
| --- | --- | --- |
| `limit` | `200` | Integer 1–500; fixed throughout a snapshot traversal. |
| `before` | absent | Opaque server-issued cursor for the next older page. |
| `format` | `text` | `text` or `ansi`; fixed throughout traversal. |
| `source` | `recent-unwrapped` | Only this read shape is supported. |

Unknown query fields (including native `start`/`end`) are refused. IDs must be
nonempty, at most 256 bytes, without slashes or control characters. First pages
do **not** read terminal output or seed a capture. They use only what existing
Gateway readers/streams have already observed in that exact source/format. ANSI
and text buffers are independent; there is no escape-stripping conversion and
no fallback from one format to another.

Every request obtains a complete live pane listing, without holding the capture
mutex across I/O. A live ordinary pane with native backlog but no Gateway capture
returns an empty `not_captured` answer, **not** a backend error. A backend failure
while checking topology returns the normal backend error, never an empty capture.
If the backend reports an empty topology (including tmux's no-server convention),
the target is missing: 404 on a first page, 410 during traversal. Only the existing capture policy
(alternate-screen/zero-native-backlog panes) contributes rows.

## Response

The existing versioned content envelope (`schema_version`, `capabilities`, `data`)
contains:

```json
{
  "session_id": "default",
  "pane_id": "w1:p1",
  "generation": "opaque-gateway-instance",
  "source": "gateway-captured",
  "read_source": "recent-unwrapped",
  "format": "text",
  "availability": "captured",
  "snapshot_id": "opaque-snapshot-identity",
  "capture_epoch": "opaque-capture-identity",
  "rows": ["older row", "newer row"],
  "row_count": 2,
  "has_more": true,
  "next_before": "opaque-cursor",
  "order": "oldest-to-newest",
  "includes_live_viewport": false,
  "complete_archive": false
}
```

Rows are normalized line-ending strings (not newline-joined text). They retain
the captured format verbatim, including empty rows and ANSI sequences. Render
them as untrusted terminal content, never HTML. No native backend range/revision
is advertised: these rows have no trustworthy native absolute coordinates.

First page is the newest historical slice. Follow `next_before` to older slices
and **prepend** each returned page. Each page is internally oldest-to-newest;
adjacent pages tile the frozen snapshot without overlap. Replaying a cursor is
idempotent. `has_more` means only more rows in this snapshot, not a full archive.
When empty, `snapshot_id`/`capture_epoch`/`next_before` are null and `has_more`
is false. `availability: captured` with zero rows means a captured buffer exists
but contains no historical prefix; `not_captured` means no eligible buffer for
the requested shape is available.

The last observed mutable screen is excluded using the existing fold's screen
boundary. This is a sampled, heuristically folded history—not a recording of
every terminal event. It can omit output between observations and cannot recover
output from before observation started. Use `/output` separately for the live
viewport; never merge it by native coordinates with these pages.

## Snapshot lifetime and reset

Nonempty snapshots are frozen for **60 seconds from creation**, not from last
access. First-page requests with the same device/session/pane/format/limit and
Gateway generation reuse that snapshot only while the capture has not moved:
same epoch, no rows trimmed and none appended. A first page after any change
drops the old snapshot and pins the current rows, so a client restarting after a
410 never lands in the stale snapshot again; an idle pane is still not copied
on every poll. After expiry a first page creates a fresh snapshot. Pages belonging to different snapshot IDs must not be
merged. Cursors bind to the authenticated device and complete request shape.

A capture at its row or byte cap drops its oldest rows as new ones arrive. That
front truncation slides the capture; it is not a reset. A cursor keeps serving
while its page's rows are still held. Once they have been trimmed away it
answers 410 and its snapshot is dropped at once, since the client is told to
start again without a cursor.

Capture-buffer eviction, detected resize, policy switching to
native history, observed pane disappearance or changed terminal/workspace/tab
identity invalidate snapshots. A detected topology resize makes pagination
unavailable until the next captured frame, without changing legacy tail folding.
An observed disappearance removes retained rows as well as snapshots before an
ID can be reused. A change into or out of full-screen editor ownership and a full
editor/resize-settling replacement also advance capture epochs, even when pane
identity and dimensions have not changed. Unchanged normal polls do not reset
epochs.

Every production capture path (HTTP tail output, parts, streamed polling and
Gateway-enriched events) takes an internal request-start fence **before backend
I/O**. Only an already-observed, capture-eligible pane can issue a fence. Identity,
policy, size, disappearance, replacement and eviction resets invalidate
pending reads. Thus an old A response arriving after an observed switch to B
cannot recreate B's buffer or become its history. A rejected capture still returns
or forwards the backend's raw output, without stitching B's cache underneath it.
An initially unknown pane cannot become capture-eligible retroactively during
its read. Backend inline-output events without a certified sampling boundary are
forwarded unchanged, not retained by guessing their origin. No lock is held across
the read.

This fixes observed-reset/in-flight races; it is separate from the following
backend visibility limitation. Like existing observation policy, this is based on backend
observations; backends do not expose a universal pane-incarnation ID, so an
unobserved destroy/recreate with identical native identity cannot be detected.

| Status/code | Meaning / recovery |
| --- | --- |
| `400 invalid_history_limit`, `invalid_format`, `invalid_source`, `invalid_history_identity`, `invalid_history_cursor` | Invalid input. Correct it before retrying. Malformed/unknown query fields are Axum 400 query rejections. |
| `404 session_not_found` / `pane_not_found` | First-page target does not exist. |
| `409 history_cursor_mismatch` | Known cursor used with another device, session, pane, format, limit or generation; also a cursor request naming another source. Drop traversal and start without `before`. |
| `410 history_cursor_gone` | Unknown/forged, expired, evicted or reset cursor, a page whose rows the capture has since trimmed off its front, or vanished pane during traversal. Drop traversal and start without `before`. A cursor from before Gateway restart is unknown and returns 410. |
| `413 history_snapshot_too_large` | A historical row or snapshot exceeds byte limits; no partial/truncated row is silently served. |
| `503 history_storage_unavailable` | Captured storage failed; an empty success is not substituted. |
| normal backend error | Live topology could not be checked; not proof that captured history is empty. |

## Resource bounds

No new dependencies or persistence. Existing live retention stays at 5,000 rows
and 2 MiB per read-shape buffer, 48 buffers/24 MiB total. Snapshots add at most
**32 snapshots / 8 MiB** (row capacities and row/page-vector allocations counted),
at most **8 per session**, **2 per pane**, and **2 MiB of row text per snapshot**
(the live capture's own cap, so a full pane always pins; its row/page-vector
allocations count toward the 8 MiB total). Each page has at most `limit` rows and **256 KiB**
of raw UTF-8 row bytes plus one byte per row; JSON escaping/envelope overhead
can enlarge the serialized response. Oldest-created snapshots are evicted to
honor all limits. Cursor/page records are bounded by the 5,000 captured rows,
not by how often a cursor is replayed. Expired/reset entries are pruned lazily on
history repository operations; idle memory remains capped.

This optional API is additive. Older Apps keep using `/output`; newer clients
must check the capability and explain that a Gateway upgrade is needed when it
is absent. There is no SQLite archive, native unified pagination, startup or
prompt-delivery change in this feature.

## Architecture and a future persistence switch (not available now)

`terminal/history.rs` owns the stable `read_page` application use case, row/page
contract, cursor parsing/binding, capture-epoch/TTL checks and immutable page
intervals. Its small `HistoryRepository` port covers read-only capture projection
(`capture`, including a metadata-only epoch probe), lookup of pinned traversals
(`find`/`load`), atomic bounded pinning (`pin`) and invalidation (`remove`). The
port uses boxed Send futures, like the existing backend ports. Adapter failures
are typed, not empty histories. The use-case contract tests use a genuinely
asynchronous fake adapter, not the memory adapter's HashMap.

`terminal/history_memory.rs` is the **only shipped adapter**. It reads the
historical prefix from `ScrollbackStore` using short memory-only locks and keeps
its separate bounded pinned-snapshot cache. `ScrollbackStore` still owns capture
eligibility, live viewport replacement, heuristic folding and capture epochs; it
does not hold cursors or perform pagination. HTTP authenticates, validates,
resolves/checks backend topology and calls the use case—it neither indexes the
snapshot dictionary nor knows any database coordinates.

`platform/server.rs` composes `AppState.history: Arc<dyn HistoryRepository>` with
the memory adapter. That is the future **storage selection boundary**. There is
no SQLite dependency, database, migration, or persistence configuration setting
today; setting an invented SQLite flag will not enable anything. A later reviewed
change can add a validated setting (for example a `history.storage` enum with
`memory` as the default) in `platform/config.rs`, select a SQLite adapter here,
and keep this App API unchanged. It must also add an explicit bounded ingestion
path from the live fold's historical-prefix/epoch projection into that adapter:
the current memory adapter queries the projection directly, and the read port
does not pretend that persistent ingestion is already implemented. Ingestion
must preserve the same capture policy and not persist the mutable viewport just
because a backend read arrived.

That future switch must be explicit opt-in: terminal history can contain
credentials, source code and personal data. Turning persistence **on** would make
selected captured rows survive process exit on disk; turning it **off** must
stop writes and define whether existing records are deleted (with a separate
confirmed purge) or merely retained but hidden. Default memory mode must never
silently open/import an old database. Storage files/directories need the repo's
secret-file permissions and a documented cleanup/backup policy; encryption at
rest would require a separate reviewed key-management design, not a claim made
by transport encryption.

A SQLite adapter must retain quotas, row/page byte limits and TTL/pruning, add a
bounded disk quota and explicit retention age, and run database operations in
an async driver or bounded blocking worker. No live-fold mutex guard can be
held across disk I/O or passed into such a worker. Ingestion should copy only a
bounded epoch-tagged batch under the live lock, release it, then queue the write
with backpressure; recheck epoch before publishing the retained version. Pinning
must atomically deduplicate concurrent first-page requests, just as memory does.

Persisted capture records would be keyed by stable configured session/backend
identity, observed terminal incarnation/capture epoch and exact source/format—not
by reusable pane ID alone. Removal/reconfiguration must invalidate active
traversals and apply a documented delete/orphan-retention policy; re-adding a
backend or reusing a pane ID must never automatically expose orphaned rows as
the new pane's history. Backend-unobservable identical ID reuse remains an
identity limitation to solve before promising durable cross-restart adoption.

Pinned cursors are bound to the selected repository instance, Gateway generation,
device and request shape. **Persisted records may outlive cursors; current cursors
do not survive a Gateway restart or storage switch.** A new adapter/process must
start with a new cursor namespace and invalidate old traversals, even if it can
read older persisted records. Config changes should require a controlled restart
or an explicit generation/storage-epoch transition. Schema migrations, rollback,
corruption recovery and cleanup belong in that future adapter/composition change;
database row IDs and migration versions must not enter the wire capability or
cursor contract.
