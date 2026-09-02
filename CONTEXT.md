# Codebase context

## Purpose

This repository implements Muqun's local terminal gateway. It exposes one
authenticated HTTP/SSE contract over one or more terminal backends. The
repository, package, and binary are `muqun-gateway`; the Herdr plugin
identifier stays `herdr.gateway` because it names the Herdr integration, not
the gateway itself (see the last line of this file).

## Shape of the codebase

- `src/main.rs`: CLI, configuration and identity lifecycle, HTTP routes,
  pairing/devices, SSE orchestration, Manager UI, and process lifecycle.
- `src/backend/model.rs`: backend-neutral entities, commands, errors, and the
  `TerminalBackend` port. The port is the test surface for synchronous terminal
  use cases, including topology, terminal I/O, agents, and optional worktrees.
- `src/backend/herdr.rs`: Herdr protocol-17 socket adapter and subscription.
- `src/backend/tmux.rs`: argv-only tmux adapter and polling event source.
- `src/backend/registry.rs`: static adapter registry for construction,
  availability checks, endpoint rendering, and setup defaults.
- `src/backend/compat.rs`: maps neutral models into the legacy Herdr-shaped API.
- `src/authority.rs`: pure pairing-code and credential authority; owns hashed
  device records, token verification, install replacement, and last-seen policy.
- `src/scrollback.rs`: bounded scrollback retention and the application policy
  for observing frames and serving row-bounded reads. Callers do not assemble
  cache keys or compare output byte lengths.
- `src/login_env.rs`: the `PATH` and `LC_CTYPE` a backend actually needs,
  recovered from a login shell. An init system starts the gateway with neither,
  and the tmux adapter cannot spawn tmux without the first or parse its output
  without the second. Read at startup and written into the unit file.
- `docs/architecture.md`: Clean/Hexagonal boundaries and compatibility rules.
- `herdr-plugin.toml`, `install.sh`, `scripts/`: plugin packaging and releases.

There are no nested `CONTEXT.md` files at present.

## Domain model and data flow

The stable domain concepts are sessions, workspaces, tabs, panes, agents,
terminal output, and backend commands. A configured session selects one
`TerminalBackend`; multiple Herdr and tmux sessions can coexist. HTTP handlers
authenticate a device, resolve `sessionId`, invoke the backend port, then pass
neutral results through the compatibility mapper. Synchronous use cases do not
branch on backend kind. `TerminalBackend::activity_stream` normalizes Herdr
native events and tmux topology polling into the same internal vocabulary for
SSE and push.

The gateway owns pairing identity, device tokens, network listener, backend
registry, Manager, SSE fan-out, and lifecycle. A backend owns only interaction
with its terminal system. The first session is compatibility-sensitive because
older Muqun UI currently opens the first `/api/sessions` item.

HTTP extracts credentials and maps errors; `authority.rs` decides whether a
credential is a paired device or the narrow local-manager identity. Device
records persist only token hashes. File I/O remains an outbound concern in the
composition root, so credential policy is testable without HTTP or disk.

## Engineering conventions

- Keep backend branching in adapter construction or adapter modules, not route
  handlers and application workflows.
- Treat backend IDs as opaque untrusted input. Validate them before forming any
  native command. tmux must be invoked with argv, never a shell command string.
- Preserve the legacy `herdr` metadata and response envelope until the app
  compatibility floor explicitly changes.
- Setup and imports must be idempotent and preserve pairing identity. Write
  secrets atomically with `0600` files under `0700` directories.
- Errors returned to clients are bounded and generic; detailed backend errors
  belong in local logs. Avoid logging bearer or admin tokens.
- Prefer small neutral model additions over copying a use case per backend.
- Add a shipped backend once in `backend/registry.rs`; route and Manager code
  must not grow backend-kind presentation or construction branches.
- Optional backend capabilities return `BackendError::Unsupported`; application
  workflows choose a fallback without inspecting backend kind.

## Configuration and deployment

Standalone config defaults to `~/.config/muqun-gateway`; mutable state defaults
to `~/.local/share/muqun-gateway`. An install that predates the muqun-gateway
rename is migrated once, automatically, the first time either directory is
resolved: if the new name is absent and the old `herdr-gateway` one is
present, it is renamed in place (an atomic same-filesystem `rename`, so there
is no partially-migrated state) and never looked at again. The Herdr plugin
supplies its own directories through environment variables until
`import-herdr-plugin` writes the migration marker. After that, direct CLI and
plugin actions share standalone ownership.

The public listener is normally localhost behind Tailscale Serve HTTPS or a
Tailscale IPv4 address. HTTPS is preferred. Application transport encryption is
`required` by default and binds a per-device AES-GCM key to each newly paired
device, so its bearer token is not sufficient by itself. `disabled` is an
explicit token-only compatibility mode. Existing device records retain their
pairing mode; changing the gateway setting governs newly paired devices.

## Verification

Run `cargo fmt --check`, `cargo test --offline`, `cargo clippy --offline --all-targets
-- -D warnings`, and `cargo build --release --offline`. The ignored Herdr and
tmux contracts use isolated sockets and should be run when adapter behavior
changes. Live Herdr integration checks should remain read-only; write checks
belong on isolated adapter fixtures. The tmux contract test now also asserts
that disjoint absolute ranges tile a pane exactly; it needs a real tmux
server, so it stays behind `--ignored`.

Important quirks:

- A gateway restart is required after backend registry edits.
- Removing a backend never terminates the corresponding terminal sessions.
- The current Muqun data layer is multi-session capable, but its ordinary UI
  automatically selects the first session and does not yet expose a picker.
- The Herdr plugin ID (`herdr.gateway`) and the legacy `herdr` response
  metadata are the Herdr-integration surface, not the gateway's own name --
  they do not follow the gateway when it is renamed.
