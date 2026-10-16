# Unreleased

## Changes and fixes

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
