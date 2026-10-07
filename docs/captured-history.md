# Gateway captured terminal history

Capability: `pane_captured_history` in `/health`, `/api/meta` and discovery's
capability list. Discovery also announces `planes.terminal.features.capturedHistory`.
This is independent of `backends[].features.pagedHistory`, which still describes
**native** ranged `/output` reads. No App UI or App support is implied.

`GET /api/sessions/{session_id}/panes/{pane_id}/history`

Uses the same device authentication and encrypted transport as the other pane
routes. Cursors are not credentials. Storage defaults to memory. Optional SQLite
storage can recover a verified live pane's last captured-prefix **checkpoint**;
cursors always expire on Gateway restart. Existing `/output` tail stitching and
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

Existing live retention stays at 5,000 rows
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
is absent. There is no complete archive, native unified pagination, agent startup or
prompt-delivery change in this feature.

## Optional SQLite checkpoints

While Gateway is stopped, set this block in its `config.json`, then start it
through the normal lifecycle command. Changing storage requires a controlled
restart; it is not a live toggle or a Manager UI setting.

```json
{"history": {"storage": "sqlite"}}
```

The accepted storage values are `memory` (default) and `sqlite`; unknown values
are rejected. Omitting the block keeps existing config files compatible. The
additional capability `pane_captured_history_sqlite_checkpoint` declares support
for the option, **not** that the owner has enabled it or that disk writes have
succeeded. `pane_captured_history` and the history response contract remain
unchanged. No App source change or complete-archive UI is provided.

SQLite stores only historical prefixes in the exact `recent_unwrapped`/text or
ANSI shape. It never stores the live viewport, pinned snapshots, device tokens,
cursor tokens, or a native absolute range. The current projection is replaced in
a transaction, including shrink, repaint, reset and deletion; it is not an
append-by-length log. Revisions coalesce to the latest fold state, so a crash can
lose dirty work not yet flushed. This is a sampled checkpoint, not a write-ahead
record of every accepted terminal frame.

After restart, a history request first verifies live topology. Only a verified
same native server and root-pane process can adopt an older prefix. That prefix
gets a new runtime capture epoch with row base zero and remains separate from
the live fold and `/output`. A mutable screen alone does not remove it. When the
new run produces its first nonempty historical prefix, that fresh projection **replaces** the
checkpoint outright; it is never concatenated across runs. This deliberately
does not promise a seamless archive. The response still says
`complete_archive: false` and `includes_live_viewport: false`. Old cursors return
410 even when the checkpoint is readable with a new first-page request.

Identity includes the installation `server_id`, configured backend type and
canonical endpoint, a persisted per-backend configuration incarnation, native
server/root-process incarnation, pane/workspace/tab identity, dimensions/editor
policy, and source/format. tmux gets server and pane PIDs in the same listing;
Herdr uses the socket peer and that pane's `process_info.shell_pid`. Adapters
verify same-user process ancestry and kernel incarnation: Linux boot UUID plus
PID start ticks; macOS kernel boot time plus PID start timestamps. PIDs alone,
unknown peers, unavailable kernel probes, old Herdr without `process_info`, and
unrelated root processes cannot authorize cross-restart recovery. Current-run
capture still works under a run-private namespace when identity is unknown.
Acquiring or losing verification on an already-observed pane discards its prior
projection and invalidates outstanding reads; unverified rows are never promoted
into a newly verified process's checkpoint. A first-ever verified observation
can still recover its matching checkpoint.
Native process metadata is private and never added to public pane JSON.

The private SQLite backend registry is reconciled against startup configuration.
Observed removal/re-addition and endpoint/install changes rotate its incarnation;
orphan records are inaccessible and expire under retention. An identical manual
remove/re-add entirely between SQLite-enabled runs (including while storage is
off) cannot be detected from the final config alone. It must not be used as an
implicit purge; stop Gateway and remove the history directory if that is the
intent. A changed native server/root process still denies adoption independently.

### Privacy, failure and disk bounds

Files live under the resolved Gateway state directory's `history/`:
`captures.sqlite3` and its SQLite sidecars. The directory is 0700 and files are
0600. Symlinks, nonregular files, foreign ownership and multiply linked files
are refused. Stored terminal text is **plaintext at rest** and may include
credentials, private source or personal data. HTTP transport encryption does not
encrypt the database.

One dedicated blocking thread owns one bundled SQLite connection (rusqlite).
No SQLite operation runs on the Tokio executor, and no fold mutex is held during
disk I/O. Query/health requests have a bounded 16-command channel; dirty revisions
remain coalesced in the bounded live fold. Each pass copies at most one 2 MiB
projection under a short lock and writes it off-lock. Late recovery and write
acknowledgements recheck the exact observation fence/revision; reset work cannot
make an older revision clean. The reused memory snapshot cache retains its
60-second TTL, atomic first-page deduplication and all device/scope quotas.

Durable payload retention is at most **48 captures / 24 MiB**, **5,000 rows / 2 MiB
of row text per shape**, for **7 days since the last actual projection change**,
not the last read. Reads and unchanged polling do not extend age. Age pruning
runs at startup and periodically for inactive records; logical quotas also run on writes. Oldest actual updates are
evicted to satisfy logical quotas. Recovered checkpoints share the existing
48-buffer/24-MiB live-memory budget; snapshots keep their separate 8-MiB budget.
The bounded four-byte-per-row encoding overhead counts toward disk quotas,
and recovered row/vector allocations count toward the total live-memory budget,
not a second per-shape row-text cap. Front trims slide a live capture's row base
without changing its epoch; checkpoints contain only retained rows, never trimmed
rows or old cursor coordinates.
The worker retains a bounded last-processed content fingerprint independently of
the database row. An unchanged poll cannot recreate an age-pruned or quota-evicted
projection; a genuine change in that exact format can be persisted again.

The main database has a **64 MiB physical ceiling**, enforced with SQLite's page
limit; freed pages are reused rather than automatically vacuumed. WAL has a
**16 MiB ceiling** with bounded payload transactions and explicit truncate
checkpoints before/after writes and retention deletes. `journal_size_limit` is
not treated as a hard cap by itself. If a reader prevents checkpoint shrink,
the worker stops history operations instead of continuing to grow WAL. External
processes must not write this private database; Gateway does not promise to bound
files another process deliberately enlarges.

Initialization, worker loss, queue overload, lock/checkpoint failure, full disk,
corruption or unsupported newer schema return `503 history_storage_unavailable`,
including when a pinned snapshot exists. No empty result or successful memory
fallback conceals the selected store's failure. Output/control/SSE continue
independently. Permanent worker failures require resolving the cause and a
controlled restart. Schema version 1 is initialized transactionally; migration
failure rolls back. Corrupt/newer files are preserved, not automatically deleted
or downgraded. Unknown identity without an eligible current-run capture returns
the normal `not_captured`, not a claim that all old history was recovered.

Turning storage off and restarting stops writes, leaves disk files retained but
hidden, and does **not** resolve/open/import the history database in memory mode.
For cleanup, stop Gateway and remove the **whole** `state/history/` directory;
this destroys backend registry/checkpoints, not terminal sessions. For a coherent
backup, stop Gateway and copy that whole directory, or use SQLite's backup API
with an appropriately authorized local tool. Never copy just a live main file:
its committed pages can still be in WAL. Do not unlink live sidecars.

## Architecture

`terminal/history.rs` owns the stable `read_page` application use case, row/page
contract, cursor parsing/binding, capture-epoch/TTL checks and immutable page
intervals. Its small `HistoryRepository` port covers read-only capture projection
(`capture`, including a metadata-only epoch probe), lookup of pinned traversals
(`find`/`load`), atomic bounded pinning (`pin`) and invalidation (`remove`). The
port uses boxed Send futures, like the existing backend ports. Adapter failures
are typed, not empty histories. The use-case contract tests use a genuinely
asynchronous fake adapter, not the memory adapter's HashMap.

`terminal/history_memory.rs` reads the
historical prefix from `ScrollbackStore` using short memory-only locks and keeps
its separate bounded pinned-snapshot cache. `ScrollbackStore` still owns capture
eligibility, live viewport replacement, heuristic folding and capture epochs; it
does not hold cursors or perform pagination. HTTP authenticates, validates,
resolves/checks backend topology and calls the use case—it neither indexes the
snapshot dictionary nor knows any database coordinates.

`platform/server.rs` selects the repository at composition from the validated
config. `terminal/history_sqlite.rs` owns secure initialization, schema/registry,
the worker, transactions, retention and health. It wraps the existing memory
adapter's bounded snapshot policy rather than duplicating it. `ScrollbackStore`
owns dirty projection revisions, reset fences and separate recovered checkpoints;
it performs no SQLite I/O. The async application repository port and HTTP cursor
contract are unchanged.
