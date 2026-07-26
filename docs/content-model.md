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
  "schema_version": "1.3.0",
  "capabilities": { "parts": true, "assets": true, "image_upload": true,
                    "composer": true },
  "data": { }
}
```

One version covers the whole content model, assets and parts alike: a client
reads it once. 1.0.0 shipped assets with `parts: false`; 1.1.0 added the parts
endpoint and flipped that flag, and changed nothing about the 1.0.0 payloads.
1.2.0 adds the Codex and opencode dictionaries: more panes answer
`parts: "dictionary"` where they used to answer `parts: "text"`, and again no
payload changed shape. 1.3.0 adds the composer capabilities — a `composer`
object on the pane descriptor and the file search endpoint — and again changes
no existing payload: a 1.2.0 client reads a 1.3.0 response unchanged and simply
does not see the new field. The gateway's own `apiVersion` stays 1.4.0 — no
route, and no field of any route, moved.

`capabilities` is also exposed per pane (agent kind detection may vary):

```json
{ "pane_id": "wM:p1", "agent": "claude-code", "parts": "dictionary",
  "image_input": "file-path",
  "composer": {
    "version": 1, "table": "claude", "captured_from": "claude 2.1.220",
    "file_mentions": true,
    "slash_commands": [
      { "name": "/compact", "description": "Free up context by summarizing…",
        "args_hint": "[instructions]", "source": "builtin" },
      { "name": "/ship-release", "description": "Cut and publish a release",
        "args_hint": null, "source": "workspace" }
    ]
  } }
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

### Marker dictionaries (v1.1; Codex and opencode added in v1.2)

`src/parts.rs` is one block state machine plus a per-agent glyph table. Adding an
agent is a new table, never a new part type and never a change to this document's
wire format.

| | block | call | result | quote | prompt | status |
|---|---|---|---|---|---|---|
| Claude Code | `⏺` | — | `⎿` | — | `❯` | `✻ ✽ ✳ ✢ ∗ ·` |
| Qoder CLI | `▪ ●` | — | `└` | `│` | — | — |
| Codex CLI | `• ⚠` | — | `└` | `│` | `›` | `◦` |
| opencode | — | `→` | `↳` | `+` | `┃` | — |

A glyph only marks when it opens the line and is followed by a space, so a box
rule or a bullet inside somebody's output is not read as structure. What the
machine does with them:

- A **block** line whose remainder parses as `Tool(input)` is a `tool-block`;
  anything else on a block line ("Update Todos", a sentence) is `text`. The name
  must be one identifier immediately followed by `(`, which is what keeps
  `Background command "…" completed (exit code 0)` out of the tool set.
- Agents that write `Ran cargo test` instead of `Bash(cargo test)` list those
  head words in the dictionary (Codex: `Ran`, `Explored`). A verb head is only
  promoted to a tool when the block actually produced a result group, so a
  sentence that opens with the same word stays prose.
- A **call** glyph is a block opener that is a tool call by construction, for
  agents that give tool calls a glyph of their own (opencode's `→`): the first
  word is the tool and the rest is the input. Such an agent prints the outcome
  on the call line, so a call block with no result group is `ok` rather than
  `running`.
- A **prompt** is normally one marker line with its wrapping indented under it.
  opencode instead draws a gutter — the marker repeated on every row of the
  message — which the dictionary says so, and the run becomes one `prompt`.
- **Result** groups under a block are the tool's `result`. `status` is read off
  the first result line (`Error…`, `✗`) because the terminal never carried an
  exit code; a block with no result yet is `running`.
- A result group that is mostly **checkboxes** (`☐ ☑ ☒`) becomes a `todo`, and
  one that is mostly **climbing line numbers** becomes a `diff` beside its block.
  Requiring the numbers to climb, and to be followed by gutter padding rather
  than by a word, is what keeps `13 warnings` and `2026-07-21` out of diffs.
- An **ellipsis** anywhere in a block sets `truncated`: the agent said this is
  not all of it.
- A **quote** line is reasoning drawn as a tree; it degrades to `text` rather
  than inventing a type outside the closed set.
- Everything else merges into `text`.

Two invariants are tested rather than intended. Every part carries
`fallback_text`, the source lines verbatim; and every non-blank source line lands
in exactly one part's `fallback_text`, in order. A dictionary that drifts when an
agent upgrades therefore loses structure and cannot lose content. The fixture
snapshots in `tests/fixtures/` pin each dictionary's output so that drift shows
up as a reviewable diff; re-pin with `UPDATE_PART_SNAPSHOTS=1 cargo test`.

## Composer capabilities (v1.3)

The app's composer offers slash commands and `@` file mentions without knowing
any agent by name, because the pane's descriptor says what this agent takes.
`src/composer.rs` is one versioned table per agent kind, keyed the same way the
dictionaries are, and `composer` is **absent entirely** for an agent with no
table — a client can tell "this gateway knows nothing about this agent" from
"this agent understands no slash commands", which a `null` would not allow.

| table | read off | commands |
|---|---|---|
| `claude` | Claude Code 2.1.220, the shipped binary's own command definitions | 34 |
| `codex` | `SlashCommand` at `rust-v0.145.0`, cross-checked against the installed binary | 44 |
| `opencode` | opencode 1.18.0, the palette entries that register a slash name | 28 |
| `qoder` | Qoder CLI 1.1.5, the shipped binary's own command definitions | 37 |

Nothing in those tables is remembered or inferred: no agent prints its slash
commands to `--help`, so each was read out of the program actually installed on
the machine. Debug-only, removed and platform-specific commands are left out —
the table is what a phone should offer on a tap, not the agent's full surface.
The tables are snapshot-pinned in `tests/fixtures/*-commands.json` (re-pin with
`UPDATE_COMMAND_SNAPSHOTS=1 cargo test`), and a drift check fails if a table is
keyed on a name no Herdr detection manifest reports, exactly as the dictionaries
are checked. The same tables answer `GET .../shortcuts`: two lists of one
agent's commands that could disagree would be one list too many.

**Workspace commands** (`source: "workspace"`) are read from the pane's own
workspace root, read-only: a skill directory holding a `SKILL.md` whose front
matter names and describes it (`.claude/skills`, `.agents/skills`,
`.codex/skills`, `.opencode/skills`, `.qoder/skills`) and a directory of one
markdown file per command (`.claude/commands`, `.opencode/command`,
`.qoder/commands`). Which directories are looked at is the agent's own list, so
a Codex pane is not offered a Claude skill it cannot run. Discovery is under the
same fence as the assets API — every directory and every file canonicalizes
inside the root, so a symlinked skill directory pointing at another checkout is
skipped — and it is capped at 64 commands and 64 KiB per file. A workspace
command shadows a builtin of the same name: the file is what the agent actually
reads.

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
  transcript, read from the pane's own `recent-unwrapped` text (the dictionaries
  key off line starts, so an unwrapped read is the only sensible source).
  `data.pane` carries the per-pane capability descriptor above, with
  `parts: "text"` when no dictionary covers the pane. The raw ANSI endpoints
  remain forever (fallback path).
- `GET /api/sessions/{sid}/panes/{pid}/files?query=&limit=` — fuzzy path search
  for `@` mentions. Paths only: a relative path, its name, and a `kind` guessed
  from the name, never contents and never an absolute path. The only directory
  searched is the pane's own cwd as Herdr reports it, canonicalized, which is
  the same fence the asset API is gated on; symlinks are not followed, and dot,
  dependency and build directories are skipped, so nothing outside the root can
  be named. Limit defaults to 20 and caps at 50. A pane with no workspace — the
  filesystem root, the home directory, a pane Herdr does not list — answers with
  an empty list and `root: null` rather than an error, so this cannot be used to
  probe the host.
- SSE additions: `asset.created`, `parts.updated` events on the existing stream.

## Rollout slices

1. **v1.0 — assets first** (no dictionaries needed): asset listing + content
   endpoints + `asset-ref` inline detection for paths printed in output. App
   ships the Artifacts entry + markdown/image/text viewers.
2. **v1.1 — parts for Claude + Qoder** via marker dictionaries. Gateway side
   shipped: dictionaries, the parts endpoint, and fixture snapshots pinned per
   agent version. App side renders the parts with the fallback path one tap away.
3. **v1.2 — mainstream dictionaries.** Codex and opencode shipped, each pinned
   to a fixture captured from a live pane. Herdr's agent-detection manifests
   (`~/.local/state/herdr/agent-detection/`) are the ecosystem-maintained agent
   list this tracks, and a test fails if a dictionary is keyed on a name no
   manifest reports; it also prints the manifests with no dictionary yet.
   Gemini CLI, Copilot CLI and Cursor Agent are the remaining ones, and are
   waiting on captured output — the manifests carry state-detection patterns,
   not transcript glyphs, so a table cannot be written from them alone.
4. **v1.3 — composer capabilities.** A slash-command table per agent kind, the
   workspace's own skills and commands discovered under the asset fence, and
   the `@` file search endpoint. Additive: the pane descriptor gains a field and
   one endpoint appears, and no existing payload changed shape.
5. **v2 — native protocol adapters** (opencode server, codex app-server) feed
   the same parts; approvals arrive as a new part type behind a minor bump.

## Non-goals (v1)

- No write operations through the asset API.
- No agent-specific types on the wire, ever.
- No attempt to normalize full-screen TUI apps (nvim etc.) — those stay raw
  terminal, which the model's fallback already handles.
