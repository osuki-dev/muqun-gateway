# Herdr Gateway

Rust Herdr plugin and token-protected mobile gateway for Muqun.

Herdr Gateway exposes a small HTTP API for reading Herdr workspaces, tabs, panes,
agents, pane output, and sending pane input. It talks to the local Herdr socket
API and is intended to be reached from trusted devices over a private network
such as Tailscale.

## Install

One command that checks your system, installs the plugin, and on a **first
install** also configures it, starts it, and opens the pairing QR (macOS and
Linux; Windows is not supported yet):

```sh
curl -fsSL https://raw.githubusercontent.com/BANG88/herdr-gateway/main/install.sh | sh
```

Herdr Gateway requires Herdr 0.7.5 or newer. The installer checks this before
Herdr reads the plugin manifest; on an older version it stops with the exact
`herdr update --handoff` command instead of exposing an unrelated TOML parse
error. Herdr 0.7.5 installs plugins globally for the current user, so the plugin
can be installed from any shell. Its setup, start, and pairing-QR actions still
need a live herdr session; if none is reachable, the script installs the plugin
and prints the manual commands to finish.

It needs [Herdr](https://herdr.dev). Install downloads a prebuilt, statically
linked binary for your platform, so no Rust toolchain is required -- it is only
a fallback if no release binary matches your OS/arch.

If you prefer to do it by hand, install directly with Herdr's plugin installer:

```sh
herdr plugin install BANG88/herdr-gateway
```

then configure once, start, and open the manager panel to view the QR code,
approve pairing requests, and start or stop the gateway:

```sh
herdr plugin action invoke herdr.gateway.setup   # first time only -- mints server id + token
herdr plugin action invoke herdr.gateway.start
herdr plugin pane open --plugin herdr.gateway --entrypoint manage
```

In the manager panel, press `u` to edit the public gateway URL. Press `a` to
auto-detect it again. Saving the URL updates both the gateway config and the
pairing QR code. Once a device is paired, the panel shows the paired-device list
instead of the QR; press `p` to bring the QR back and pair another device. Press
`x` to select and revoke a paired device; its token is invalidated immediately.

Stop the gateway:

```sh
herdr plugin action invoke herdr.gateway.stop
```

## Update

Herdr has no separate update step, and **you do not need to uninstall first** --
reinstalling a GitHub-managed plugin replaces its checkout in place. Re-run the
same one command:

```sh
curl -fsSL https://raw.githubusercontent.com/BANG88/herdr-gateway/main/install.sh | sh
```

On a re-run the script updates the plugin and **reloads the binary for you**
(stop + start). It does **not** re-run setup, because setup mints a fresh server
id and token -- skipping it keeps every device you have already paired.
Installation may run from any shell; keep a herdr session running so the reload
actions can attach.

Or reinstall directly (then restart it yourself, see below):

```sh
herdr plugin install BANG88/herdr-gateway
```

Pin a specific version with `--ref`:

```sh
herdr plugin install BANG88/herdr-gateway --ref v0.3.0
```

After updating, restart the gateway so the new build takes over:

```sh
herdr plugin action invoke herdr.gateway.stop
herdr plugin action invoke herdr.gateway.start
```

> Working from a local checkout (`herdr plugin link .`)? Herdr refuses to install
> over a local link, so update it in place with `git pull && cargo build
> --release` instead, or `herdr plugin unlink herdr.gateway` first to switch to
> the GitHub-managed build.

## Development

Use `plugin link` while working from a local checkout:

```sh
cargo test
cargo build --release
herdr plugin link .
herdr plugin action invoke herdr.gateway.setup
herdr plugin action invoke herdr.gateway.start
herdr plugin pane open --plugin herdr.gateway --entrypoint manage
```

If you previously installed the GitHub-managed plugin, uninstall it before
linking a local checkout:

```sh
herdr plugin uninstall herdr.gateway
```

## Pairing

The manager panel displays a QR code for Muqun. The QR code contains only the
gateway URL and server ID:

```text
muqun://pair?u=<gateway_url>&s=<server_id>
```

It does not contain the bearer token.

The gateway URL is configurable. During `setup`, Herdr Gateway chooses a default
URL in this order:

1. `https://<tailscale-magic-name>` when Tailscale is running and Tailscale Serve appears to be forwarding the gateway port.
2. `http://<tailscale-ip>:23847` when Tailscale is running.
3. `http://127.0.0.1:23847` as a local fallback.

For automatic setup, the listener is restricted to the matching interface:
localhost for Tailscale Serve HTTPS, the detected Tailscale IPv4 address for
direct tailnet access, or localhost for the fallback. An explicit
`--public-url` keeps the listener on all interfaces because that mode is an
intentional user override.

You can override it from the manager panel with `u`. When running the binary
directly from a local checkout, `setup --public-url <url>` is also supported.

The port defaults to 23847 and is set with `setup --port <port>`. That default
sits outside the common service range and outside the Linux and macOS ephemeral
ranges, so it rarely collides with anything else on the machine. `start`,
`stop`, and the manager panel all read the port back from the config, and
`stop` only signals processes that really are the gateway.

For Tailscale HTTPS, configure Tailscale Serve to forward HTTPS traffic to the
local gateway port, then set the URL to the MagicDNS HTTPS name:

```text
https://<machine>.<tailnet>.ts.net
```

HTTPS through Tailscale Serve is recommended. Direct HTTP over a Tailscale IP
is supported for private tailnets, but its safety still depends on your
Tailscale ACLs and the security of every device in that tailnet.

Pairing flow:

1. Muqun scans the QR code.
2. Muqun sends a pairing request with a request ID and device name.
3. The manager panel hides the QR code and shows the device name plus a short confirmation code.
4. Enter the confirmation code in Muqun.
5. Muqun claims the pairing request and receives a token minted for that device.

Pending pairing state is held in gateway process memory and is not written to
disk.

## Devices and revocation

Every successful pairing mints a token belonging to that one device, stored
hashed in `devices.json`. Revoking a device cuts off only that device; the
others keep working.

There are two kinds of credential, and they are not interchangeable:

- **Device tokens** are the only thing that authorises a control route. They
  exist on the paired device and are stored hashed on the server.
- **The admin token** in `pairing.json` belongs to the local manager panel and
  authorises only pending-pairing reads and device revocation. It is never
  handed out to a device. Because it sits in plaintext on disk, it deliberately
  cannot reach the routes that run commands on the host.

```sh
herdr-gateway devices             # list paired devices and when each was last seen
herdr-gateway revoke <device_id>  # revoke one device
herdr-gateway revoke --all        # revoke every device
```

The same thing is available over the API, so Muqun can show and revoke devices
from its settings screen:

```text
GET    /api/pairings
DELETE /api/pairings/:deviceId
```

## API

OpenAPI documentation is available from the running gateway:

```text
GET /docs
GET /openapi.json
```

Control routes require a paired device's bearer token:

```http
Authorization: Bearer <device_token>
```

`POST /api/pair/request` and `POST /api/pair/claim` are unauthenticated, since
that is how a device gets its token. `GET /api/pair/pending` takes the admin
token instead of a device token.

Routes:

- `GET /health`
- `POST /api/pair/request`
- `POST /api/pair/claim`
- `GET /api/pair/pending`
- `GET /api/pairings`
- `DELETE /api/pairings/:deviceId`
- `POST /api/devices/push-token`
- `DELETE /api/devices/push-token`
- `GET /api/sessions`
- `GET /api/sessions/default/events`
  (optional `?types=` filter; optional `?stream_pane=<paneId>` inlines that
  pane's current output into its `pane.updated` events as `data.output`, so a
  client paints on arrival with no follow-up read; the gateway's own
  `asset.created` events ride the same stream)
- `GET /api/sessions/default/assets?since=&limit=&path=`
- `GET /api/assets/:assetId/content`
- `GET /api/sessions/default/snapshot`
- `GET /api/sessions/default/workspaces`
- `POST /api/sessions/default/workspaces`
- `POST /api/sessions/default/workspaces/:workspaceId/focus`
- `PATCH /api/sessions/default/workspaces/:workspaceId`
- `DELETE /api/sessions/default/workspaces/:workspaceId`
- `GET /api/sessions/default/tabs`
- `POST /api/sessions/default/tabs`
- `POST /api/sessions/default/tabs/:tabId/focus`
- `PATCH /api/sessions/default/tabs/:tabId`
- `DELETE /api/sessions/default/tabs/:tabId`
- `GET /api/sessions/default/panes`
- `GET /api/sessions/default/panes/:paneId`
- `POST /api/sessions/default/panes/:paneId/focus`
- `PATCH /api/sessions/default/panes/:paneId`
- `DELETE /api/sessions/default/panes/:paneId`
- `POST /api/sessions/default/panes/:paneId/split`
- `GET /api/sessions/default/agents`
- `GET /api/sessions/default/agents/:target`
- `POST /api/sessions/default/agents/:target/focus`
- `POST /api/sessions/default/agents/:target/send`
- `GET /api/sessions/default/panes/:paneId/output?source=recent-unwrapped&lines=200`
- `GET /api/sessions/default/panes/:paneId/parts?lines=400`
- `POST /api/sessions/default/panes/:paneId/send-text`
- `POST /api/sessions/default/panes/:paneId/send-keys`
- `GET /api/agents/catalog`
- `POST /api/sessions/default/tasks`
- `POST /api/uploads`

`POST /api/uploads` takes a `multipart/form-data` body with a single `file`
field and stores it under `uploads/` in the gateway's state directory. It
answers with the absolute path, the sanitised original name, the size, and the
detected MIME type, so the app can send that path to an agent as ordinary text
and the agent reads the file straight off the host.

This first version accepts images only: png, jpeg, gif, webp, and heic. The
type is decided by sniffing the content, so everything else is refused with
`415`, executables and scripts included, and the client's filename is only
echoed back and never reaches the filesystem. The body is capped at 25 MiB, and
stored uploads are deleted 48 hours after they were written.

### Starting a task

`POST /api/sessions/default/tasks` is how the app starts a new piece of work
rather than joining one already running: pick a repo, name a branch, choose an
agent, and send the first prompt.

```json
{
  "repo_path": "/Users/dev/code/muqun",
  "branch_name": "task/594",
  "agent": "claude",
  "prompt": "Read AGENTS.md and start on card 594.",
  "workspace_label": "card 594"
}
```

With `branch_name`, the task gets its own git worktree; without it the task runs
in the repo as it stands. Herdr does the work: `worktree.create` makes the
checkout and its workspace, tab and root pane in one call, and `agent.start`
launches the agent and waits for it to become interactive before the prompt is
sent. Asking twice for the same branch reuses the existing checkout instead of
making a second one, so a retry from a phone that lost its connection converges.

The answer names where the work is, and what happened:

```json
{
  "workspace_id": "wD", "pane_id": "wD:p1",
  "worktree_path": "/Users/dev/code/muqun-task-594",
  "branch": "task/594", "agent": "claude",
  "reused_worktree": false, "agent_started": true, "prompt_submitted": true,
  "steps": [ { "step": "worktree", "status": "ok", "detail": { } } ]
}
```

A run that got part of the way answers `207` with the same body plus the failed
step, because "it failed" does not tell the user whether they now have a
checkout, and the wrong guess makes them either abandon real work or create a
second copy of it. Nothing is rolled back at that point: a pane sitting in a
fresh checkout is usable. The one case that is rolled back is a checkout this
request created that could not be given a workspace, which is useless on its own.

Two things are the gateway's own, not Herdr's. `repo_path` has to be, or be
inside, a workspace this session already has open -- anything else is `403`, and
a symlink out of one resolves to the outside path and fails there too.
`branch_name` is held to letters, digits, dot, underscore, dash and slash, with
`..`, a leading dash, and dot-leading segments refused, so the one field that
reaches `git` as an identifier can only ever be a ref and never an argument.

`GET /api/agents/catalog` feeds the agent picker: every kind the gateway offers,
the executable it maps to, and whether that executable is on this host's `PATH`.
Herdr resolves a kind to its canonical executable itself, so `available: false`
is a hint rather than a veto. Add `agent_commands` to `config.json` for a host
whose binary is named something else:

```json
{ "agent_commands": { "claude": "claude-canary" } }
```

### Assets

`GET /api/sessions/default/assets` lists the files a session's workspaces
produced recently, newest first, and `GET /api/assets/:assetId/content` streams
one of them back. Both speak the unified content model (`docs/content-model.md`)
and answer in its versioned envelope:

```json
{
  "schema_version": "1.4.0",
  "capabilities": { "parts": true, "assets": true, "image_upload": true,
    "composer": true },
  "data": { "assets": [ { "id": "as_…", "path": "/Users/…/report.md",
    "name": "report.md", "kind": "markdown", "mime": "text/markdown; charset=utf-8",
    "size": 6180, "modified_unix_ms": 1785100000000,
    "origin": { "session_id": "default", "workspace_id": "wM", "pane_id": "wM:p1",
      "root": "/Users/…" }, "previewable": true } ] }
}
```

`since` is Unix milliseconds, the same unit as `modified_unix_ms`, and returns
only files modified strictly after it. `limit` runs from 1 to 200 and defaults
to 50. `path=<absolute path>` resolves one exact file instead, for a path the
user tapped in terminal output; it takes precedence over `since` and `limit`,
and answers with that one asset or with none.

Two feeds keep the list current. Herdr's `worktree.*` events, which the gateway
already subscribes to, carry the checkout's root path and its workspace but no
file information -- protocol 17 has no per-file event -- so each one is taken as
a "this root just changed" trigger and the files come from scanning exactly that
root at that moment. Newly seen files are announced on the events stream as
`asset.created`, carrying one asset in the same envelope, and obeying the same
`types=` allow-list under that name. Everything else, a cold start included,
comes from an mtime scan of the session's workspace roots, which are the panes'
working directories. The scan is shallow and budgeted, and skips dot
directories, dependency directories, and build output.

`kind` is sniffed from the file's own bytes -- `image`, `markdown`, `text`,
`pdf`, or `binary` -- and `previewable` is false only for `binary`. The content
endpoint sniffs again on read, so a file that changed under a stale listing is
still served as what it now is. A path must canonicalize to a regular file
inside a workspace root the session currently has: a symlink pointing out of the
root, a traversal, and an unknown id all answer `404`, and no answer
distinguishes them. Assets over 10 MiB answer `413`, and a binary asset answers
`415` with its metadata and no body. Nothing about this API writes.

### Parts

`GET /api/sessions/default/panes/:paneId/parts` answers the same pane text the
output endpoint serves, normalized into the content model's ordered parts and
wrapped in the same envelope:

```json
{
  "schema_version": "1.1.0",
  "capabilities": { "parts": true, "assets": true, "image_upload": true },
  "data": {
    "pane_id": "wM:p1", "source": "recent-unwrapped", "lines": 400,
    "pane": { "pane_id": "wM:p1", "agent": "claude", "parts": "dictionary",
      "dictionary": "claude", "native": null, "image_input": "file-path" },
    "parts": [ { "type": "tool-block", "tool": "Bash", "input": "cargo test",
      "result": ["test result: ok. 71 passed"], "status": "ok",
      "truncated": false, "range": { "start": 12, "end": 13 },
      "fallback_text": "⏺ Bash(cargo test)\n  ⎿  test result: ok. 71 passed" } ]
  }
}
```

Which agent is in the pane decides how it is read, and `data.pane.parts` says
which of three sources answered. `native`: the agent runs a protocol of its own
and the gateway was pointed at it, so exit codes, patches, checklists and pending
permissions arrive as data. `dictionary`: the pane's text read through a marker
table -- Claude Code, Qoder CLI, Codex and opencode today (`docs/content-model.md`
documents the glyph tables and the rules). `text`: no table covers this pane, so
everything degraded to prose, which is an answer and not an error.

The one native adapter today is opencode's server API. Point the gateway at it
with `HERDR_GATEWAY_OPENCODE_URL=http://127.0.0.1:<port>` (the port `opencode
serve` printed, or the one the TUI's own server is on) and opencode panes whose
workspace that server knows are read through the protocol instead of the screen.
It is configuration rather than discovery on purpose: nothing here scans the host
for ports. Every step of the read may fail -- no endpoint, nothing listening, no
session in this pane's workspace -- and every failure falls back to the marker
dictionary, so an adapter can add structure to a pane and can never take one
away.

A native read also carries approvals in the transcript, as the `approval` part
type v1.4 added to the closed set, and the approval endpoints answer such a pane
from the protocol: the reply names the agent's own request id, so no keystrokes
are sent and a menu that moved between the read and the answer cannot be answered
by accident.

Every part carries `fallback_text`, the source lines verbatim, and every
non-blank line of the read lands in exactly one part, in order, with a `range`
that does not overlap its neighbours -- on the native path too, where the adapter
renders the transcript the spans index into, which is the parts' `fallback_text`
joined by newlines. So a part type a client does not know still renders, and a
source that drifts when an agent upgrades loses structure and cannot lose
content. The raw output endpoint is unchanged and stays the fallback
path forever.

The gateway registers Muqun Expo push tokens and watches Herdr agent lifecycle
events in the background. It sends a notification when an agent becomes
blocked, and when an agent transitions from working to idle. The title names the
server and the body names the agent (e.g. `Agent blocked · <server>` /
`<agent> needs your input.`) -- only the user-set server label and agent name,
never terminal output or prompts. Duplicate status events are ignored; tapping a
notification opens the matching server in Muqun.

## Compatibility and API versions

Herdr Gateway 0.5.0 requires Herdr 0.7.5 or newer and socket protocol 17.
Earlier Herdr releases are intentionally unsupported; update Herdr and restart
the running session before starting this Gateway release.

The Gateway API has its own Semantic Version (`apiVersion`), independent from
the Gateway binary version and the installed Herdr version. `/health` and the
authenticated `/api/meta` endpoint return the API version, capability names,
Gateway version, Herdr version, Herdr protocol, and the supported Herdr protocol
range. Muqun should feature-detect capabilities and treat missing version fields
as a legacy Gateway instead of rejecting the server.

Breaking Gateway API changes increment the API major version. Herdr
compatibility follows Herdr's socket protocol number rather than Herdr's package
version. Calendar dates may be appended as release build metadata, but are not
used to decide compatibility. Existing unversioned routes remain available for
older Muqun releases.

Pairing confirmation codes expire after five minutes and are consumed atomically
after the first successful claim. A consumed or expired code cannot be reused,
and eight failed attempts invalidate the pending code.

Security defaults:

- No raw Herdr API proxy is exposed.
- Control routes require a paired device's bearer token.
- The admin token cannot reach a control route, only the pending-pairing read.
- QR pairing does not expose any token.
- Each device gets its own token and can be revoked on its own.
- Token verification uses a constant-time hash comparison against every candidate.
- Device names are rejected if they contain control characters, so a pairing request cannot forge the manager panel with terminal escapes.
- `config.json`, `pairing.json`, and `devices.json` are written with `0600` permissions.
- `stop` only signals processes whose name matches the gateway.
- Pairing requests are rate-limited and confirmation codes permit only eight attempts.
- API responses disable caching and hide local backend error details.
- Push registrations are capped and can be removed when notifications are disabled.
- Pane output reads are capped at 1000 lines.
- Pane and agent text sends are capped at 64 KiB.
- Pane key sends are capped at 32 keys per request.
- Uploads are typed by magic number, never by filename: only image formats are
  accepted, and Mach-O and ELF binaries, Windows `MZ` images, and `#!` scripts
  are refused outright.
- Upload names are generated by the gateway (`uuid.<sniffed extension>`), so a
  client filename cannot traverse or overwrite anything; uploads land in their
  own `uploads/` directory with `0700` on the directory and `0600` on files.
- Upload bodies are capped at 25 MiB by the framework's body limit, so an
  oversized request is refused while it streams rather than after it is read.
- Uploads are swept at startup and every hour, and deleted after 48 hours.

Prefer Tailscale Serve HTTPS whenever it is available. Direct HTTP over a
Tailscale IP remains supported for user-managed private networks, but HTTP does
not provide transport encryption by itself; the security of that connection
depends on the tailnet and its access controls.

## Publishing

Herdr discovers community plugins from public GitHub repositories tagged with
the `herdr-plugin` topic. The repository should contain `herdr-plugin.toml` at
the repository root, or in the subdirectory passed to `herdr plugin install`.

## License

MIT
