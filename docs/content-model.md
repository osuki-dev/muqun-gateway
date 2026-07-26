# Unified Content Model (RFC, v1 draft)

The gateway normalizes every agent's output and every artifact it produces into
one versioned schema. The app couples to this schema only: **adding a new agent
must never change the app** — only the gateway's adapters. New display needs
(markdown, images, todos, artifacts, approvals) are new consumers of the same
model, not new protocols.

## Design contract (the three rules that guarantee app stability)

1. **Closed part set + mandatory fallback.** Every part carries `fallback_text`.
   A client that does not know a part's `type` renders `fallback_text`. New part
   types therefore degrade gracefully on old clients, and unknown agent output
   degrades to plain text on every client. Content is never lost, screens are
   never blank.
2. **Versioned envelope.** Every response and event stream declares
   `schema_version` (semver). Additive changes bump minor; clients render what
   they know and fall back for the rest. Breaking changes (avoid) bump major and
   require a capability handshake.
3. **Adapters live in the gateway.** Per-agent extraction (marker dictionaries,
   or native protocols later) is a gateway implementation detail. The wire
   format never names an agent-specific construct.

## Envelope

```json
{
  "schema_version": "1.0.0",
  "capabilities": { "parts": true, "assets": true, "image_upload": true },
  "data": { }
}
```

`capabilities` is also exposed per pane (agent kind detection may vary):

```json
{ "pane_id": "wM:p1", "agent": "claude-code", "parts": "dictionary",
  "image_input": "file-path" }
```

## Part

A pane transcript normalizes to an ordered list of parts.

Common fields: `type`, `fallback_text`, optional `range` (source row span, so a
client can correlate with the raw terminal view).

| type | payload | notes |
|---|---|---|
| `text` | `markdown: string` | prose; markdown-ish, render best-effort |
| `tool-block` | `tool, input, result: string[], status: ok\|error\|running, truncated` | collapsible in UI |
| `diff` | `file?, hunks: string[]` | red/green view |
| `todo` | `items: [{text, done}]` | checklist |
| `table` | `rows: string[][]` | best-effort cells |
| `status` | `text, spinner: bool` | transient agent state line |
| `prompt` | `text` | the input line content |
| `asset-ref` | `asset_id` | inline reference to an Asset (image, file) |

Example:

```json
{ "type": "tool-block", "tool": "Bash",
  "input": "cargo test",
  "result": ["test result: ok. 60 passed"],
  "status": "ok",
  "fallback_text": "⏺ Bash(cargo test)\n  ⎿ test result: ok. 60 passed" }
```

## Asset

Anything the agent produced that exists as a file and the user may want to see.

```json
{ "id": "as_9f2c…", "path": "/Users/…/report.md", "name": "report.md",
  "kind": "markdown", "mime": "text/markdown", "size": 6180,
  "modified_unix_ms": 1785100000000,
  "origin": { "session_id": "default", "pane_id": "wM:p1", "workspace_id": "wM" },
  "previewable": true }
```

`kind` is sniffed (same discipline as uploads): `image | markdown | text | pdf |
binary`. `previewable` tells the client whether `GET /api/assets/{id}/content`
will stream something renderable.

### Endpoints (additive)

- `GET /api/sessions/{sid}/assets?since=&limit=` — recent artifacts, newest
  first. Fed by the herdr `worktree.*` events the gateway already subscribes
  to; falls back to an mtime scan of workspace roots.
- `GET /api/assets/{id}/content` — streams the file. Read-only. Path must
  canonicalize inside a session workspace root (no symlink escape), size-capped,
  kind re-sniffed on read. 404 outside roots, 415 for kinds with no preview.
- `GET /api/sessions/{sid}/panes/{pid}/parts?lines=` — the normalized
  transcript. The raw ANSI endpoints remain forever (fallback path).
- SSE additions: `asset.created`, `parts.updated` events on the existing stream.

## Rollout slices

1. **v1.0 — assets first** (no dictionaries needed): asset listing + content
   endpoints + `asset-ref` inline detection for paths printed in output. App
   ships the Artifacts entry + markdown/image/text viewers.
2. **v1.1 — parts for Claude + Qoder** via marker dictionaries (prototype
   already validated: 85% / 64% typed coverage, tool blocks reconstruct).
3. **v1.2 — remaining mainstream dictionaries** (Herdr's agent-detection
   manifests are the ecosystem-maintained pattern source to mirror).
4. **v2 — native protocol adapters** (opencode server, codex app-server) feed
   the same parts; approvals arrive as a new part type behind a minor bump.

## Non-goals (v1)

- No write operations through the asset API.
- No agent-specific types on the wire, ever.
- No attempt to normalize full-screen TUI apps (nvim etc.) — those stay raw
  terminal, which the model's fallback already handles.
