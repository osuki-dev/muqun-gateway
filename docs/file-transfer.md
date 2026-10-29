# Files, uploads and terminal delivery budgets

`session_asset_pages` adds keyset pagination and server search to the existing
tab assets route. The envelope remains compatible: `data.assets` is an array,
alongside `next_cursor` (string or explicit null), `truncated` (whether a next
page exists), `inventory_limited`, and the applied `query`.

Send `limit`, `since`, `kind`, `query`, and optionally `cursor` to
`GET /api/sessions/{session}/tabs/{tab}/assets`. Search matches name or path,
case-insensitively, before the page is cut. Continue with the returned cursor
and the same filters; do not advance `since` while walking older pages. An
invalid or differently scoped cursor returns `400 invalid_asset_cursor`.
A Gateway restart returns `410 asset_cursor_expired`: preserve the visible
list and offer a fresh query. Newer/modified files belong to a refreshed list.

Pages traverse the bounded workspace index, not the entire host filesystem.
Root provenance, scan depth, dependency exclusions, and total index budgets
remain enforced. `inventory_limited` reports an index at its retention ceiling;
it must not be displayed as a promise of complete filesystem coverage.

## Bounded file reads

`bounded_file_ranges` supports one `Range: bytes=start-end` on stored-upload,
asset content and asset download URLs. Each range is at most 1 MiB. An open
range (`bytes=start-`) returns at most the next 1 MiB. The reply is `206`, with
`Content-Range`, `Content-Length`, `Accept-Ranges`, and
`X-Muqun-File-Version`. Invalid, multi-range or oversized ranges return
`416 invalid_file_range`. The same authorization and path containment rules
apply to every chunk, including a file above the ordinary preview budget.

Pin `X-Muqun-File-Version` from the first response and echo it on all remaining
range requests. Authenticate each sealed reply and verify its status, exact
Content-Range offsets/total, body length and version before appending bytes;
never append a full `200` fallback as a range. A changed representation returns `412 file_changed`; discard
the incomplete local download and start again explicitly. The token is a
filesystem identity/size/change-time fingerprint. The ETag is deliberately
weak: a mutable workspace file is not an immutable content snapshot. A weak
ETag is not accepted as an HTTP `If-Match` precondition. File metadata is
rechecked during each read, and a truncated or changed read fails rather than
silently assembling mixed bytes. Image original/version endpoints retain
their existing stronger, content-addressed contracts.

An upload accepted under the advertised 64 MiB video limit remains readable
without a range. Large legacy sealed read-backs have a separate, single
response memory budget, held until the response drains. Prefer ranges for
large files. Ordinary full previews remain bounded at 32 MiB; a large asset
download should use ranges. No endpoint buffers an unlimited file.

## Upload and message admission

Discovery announces upload file limits, the 180-second server body deadline
and one upload in flight per device. File-size limits exclude bounded
multipart framing. Upload and interactive message admission use separate
bounded device/IP slots; an upload cannot occupy the message slot. Pool
exhaustion remains an explicit `503 busy` before mutation. Clients should
queue uploads and preserve staged files rather than treating admission as a
lost attachment. The idle deadline is 60 seconds since the last nonempty network chunk, with no minimum transfer rate. The absolute body limit remains 180 seconds on both transports; an existing sealed request also retains its five-minute clock-skew window. Large transfers taking longer need a future authenticated chunk-upload protocol, not a larger anonymous read budget. A slow/deadline failure is `408 body_timeout`, not a false
413 size error. Client timeouts should leave room for the body deadline and
the response, while cancellation still releases the request's budget.

## Terminal text and captured history

Discovery `terminalInput.maxTextBytes` is the current 64 KiB UTF-8 text budget;
`maxKeys` is the keyboard batch budget. Serialized requests also have their
HTTP body ceiling. Clients must preflight and preserve a refused draft.
The current endpoint has no multi-request paste transaction: never split one
shell command into independent submissions or retry a possibly delivered
paste. Larger text can be handed off as an uploaded file, or requires a future
explicit paste transaction with ordered chunks and a single commit.

Captured history still targets 256 KiB pages. A larger retained row is placed
alone on a page, losslessly, within the existing 2 MiB pane budget. It cannot
prevent smaller current pages from opening. Storage, total retention, opaque
cursors and snapshot expiry keep their existing bounds.
