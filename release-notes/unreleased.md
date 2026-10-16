# Unreleased

## Changes and fixes

- **`GET /api/agent-sessions/{asid}/descendants`**: a session's whole subtree
  in one answer, where the app walked `/children` once per descendant (fifty
  requests to open one workbench). Same query, authorisation and error
  semantics as `/children`; `data.data` holds the sessions, newest first, at
  most 8 levels deep and 500 sessions, with `data.truncated: true` when a
  bound cut the walk short. The envelope announces it as
  `capabilities.session_descendants`.

- **A long DeepSeek session opens again, and pages back.** Opening a DeepSeek
  session walked its whole history fifty messages at a time and refused the
  session (`upstream history too large`) once the records passed 1 MiB, so a
  session with a large tool output could not be opened at all. The walk now
  keeps what fits its budget (2 MiB on open) and stops there, and DeepSeek
  implements `POST /api/agent-sessions/{asid}/timeline/earlier`
  (`features.timelineEarlier: true`): each pull delivers the pages below the
  oldest row shown, under the same frozen cut, within 8 MiB, until the start
  of the session. A tool result separated from its call by a page boundary is
  joined onto the call when that page arrives.

- **A linking gateway can no longer say whose login it is.**
  `POST /api/pair/request` with `kind: gateway` accepted a `server_id` and
  kept it on the login record, which is what `DELETE /api/peers/{id}` revokes
  by: a remote naming another computer's id would have had that computer's
  login here revoked instead of its own. The field is now ignored there; a
  gateway login's `server_id` comes only from the owner's mint
  (`POST /api/pair/pending`), and a login made from a self-requested code has
  none (revoke it from the device list). `POST /api/peers` additionally takes
  an optional `server_id` the owner knows the other computer by, which its
  claim answer must match (`409 peer_identity_mismatch`).

- **Linking two computers no longer sends the pairing code in the clear.**
  `POST /api/pair/pending` answers `pairing_key` (the pairing transport key the
  QR carries) on a `required` gateway; the app passes it to `POST /api/peers`
  as `pairing_key`, and the linking gateway seals its claim exactly as a phone
  that scanned the QR does, taking only an answer sealed under that key and
  bound to its claim. Before, the claim carried `request_id` and `code` in
  plain JSON and the answer was sealed under a key derived from both, which a
  listener on an `http://` link could open; a plaintext answer was accepted
  too. A link without the key (an older app, `muqun-gateway peers link`) keeps
  the code-sealed answer but refuses one in the clear from a gateway that said
  `required` of itself (`peer_refused`, reason `claim_unsealed`).

- **`transport_encryption: required` now applies to every device.** A device
  paired while the setting was `disabled` has no transport key. It was still
  served in cleartext on its bearer token alone, indefinitely, on a gateway
  configured `required`; only an envelope from such a device drew the
  `426 device_repair_required` answer. The shared device authentication now
  gives that answer to every bearer-only request from such a record, for apps
  and linked gateways alike and on every device route (SSE, the socket,
  captured history and push registration included). **Every device paired
  before encryption was turned on must pair again.** `disabled` mode, the
  admin token and `dev_unauthenticated` are unchanged.

- **Agents the gateway starts now skip their own prompts by default.** Codex
  and Claude Code started from the phone (`POST …/agents/spawn`,
  `POST …/tasks`, `POST /api/tasks`) or by an MCP caller (`tasks.delegate`)
  run with `--dangerously-bypass-approvals-and-sandbox` and
  `--dangerously-skip-permissions`, and the task directory is marked trusted
  for them (Codex's `-c projects=…`, Claude Code's `~/.claude.json`; only
  directories inside your home). Before, they stopped at prompts nobody was
  there to answer. To keep the prompts, set
  `"agent_launch_args": { "codex": [], "claude": [] }` and
  `"agent_trust_folders": false` in `config.json`. The launch arguments in
  effect are logged at startup and shown in `manage` → Settings.

- `agent_collaboration` is announced only where it works. It was in the static
  capability list, so a gateway attached to nothing but tmux told every app that
  agent collaboration was available; the refusal then arrived after the reader
  had written the task. It is now earned per session: the session's backend has
  to be a connected Herdr 0.9.0 or newer, which is the release that puts an
  opaque agent instance id on the wire. tmux has no instance identity to bind an
  assignment to, and a pane id is not a substitute.

- `/health` gained `backends[].capabilities`, a per-session capability list, and
  the same key on the primary `backend` object. It is always present, empty
  included, so an app can tell "this gateway answers per session" from "this
  gateway predates the field". The top-level `capabilities` array still carries
  `agent_collaboration` when *any* configured session qualifies -- the weaker
  claim, kept for apps that only read that list.

Nothing else changed. Every other capability is a property of the build and
stays unconditional, `agent_spawn` included: spawning runs on tmux as well as
Herdr, so a tmux-only gateway still announces it. tmux sessions are unaffected
in every respect except that they no longer claim a feature they cannot perform.

## App compatibility

- **An older App with this gateway.** Unchanged, except on a machine with no
  Herdr 0.9.0+ session at all, where `agent_collaboration` now correctly goes
  missing. That App then explains that the Gateway needs updating, which is the
  wrong upgrade to name but a better outcome than a task that silently goes
  nowhere. Apps that read the new per-session list name the right one.
- **A newer App with an older gateway.** Unchanged. `backends[].capabilities` is
  absent, and the App falls back to the top-level list plus the backend `kind`
  and `version` that `/health` has always carried.
- No capability, field, or endpoint was removed or renamed.

## Upgrade

Restart the gateway. No configuration or migration is required, but read the
first change above: agents started from the phone or by an MCP caller now run
without their approval prompts unless `config.json` says otherwise.

## Validation

Record the checks actually run before this is released.

**Not verified here:** a real paired App-to-Gateway-to-agent delivery check.
Capability announcement was exercised against a live tmux session over HTTP; no
Herdr was installed on the machine the change was written on, so the Herdr side
of the switch is covered by unit tests and not by a live run.
