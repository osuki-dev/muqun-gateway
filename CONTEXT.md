# Codebase context

## Purpose

This repository implements Muqun's local terminal gateway. It exposes one
authenticated HTTP/SSE contract over one or more terminal backends. The current
binary and Herdr plugin identifiers remain `herdr-gateway` / `herdr.gateway` for
compatibility; the intended repository name is `muqun-gateway`.

## Shape of the codebase

- `src/main.rs`: CLI, configuration and identity lifecycle, HTTP routes,
  pairing/devices, SSE orchestration, Manager UI, and process lifecycle.
- `src/backend/model.rs`: backend-neutral entities, commands, errors, and the
  `TerminalBackend` port.
- `src/backend/herdr.rs`: Herdr protocol-17 socket adapter and subscription.
- `src/backend/tmux.rs`: argv-only tmux adapter and polling event source.
- `src/backend/compat.rs`: maps neutral models into the legacy Herdr-shaped API.
- `src/scrollback.rs`: bounded scrollback retention and output cursors.
- `docs/architecture.md`: Clean/Hexagonal boundaries and compatibility rules.
- `herdr-plugin.toml`, `install.sh`, `scripts/`: plugin packaging and releases.

There are no nested `CONTEXT.md` files at present.

## Domain model and data flow

The stable domain concepts are sessions, workspaces, tabs, panes, agents,
terminal output, and backend commands. A configured session selects one
`TerminalBackend`; multiple Herdr and tmux sessions can coexist. HTTP handlers
authenticate a device, resolve `sessionId`, invoke the backend port, then pass
neutral results through the compatibility mapper. Herdr pushes native events;
tmux polling produces the same internal event vocabulary for SSE and push.

The gateway owns pairing identity, device tokens, network listener, backend
registry, Manager, SSE fan-out, and lifecycle. A backend owns only interaction
with its terminal system. The first session is compatibility-sensitive because
older Muqun UI currently opens the first `/api/sessions` item.

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

## Configuration and deployment

Standalone config defaults to `~/.config/herdr-gateway`; mutable state defaults
to `~/.local/state/herdr-gateway`. The Herdr plugin supplies its own directories
through environment variables until `import-herdr-plugin` writes the migration
marker. After that, direct CLI and plugin actions share standalone ownership.

The public listener is normally localhost behind Tailscale Serve HTTPS or a
Tailscale IPv4 address. HTTPS is preferred. Tailscale HTTP is protected by its
WireGuard transport and ACLs; ordinary LAN HTTP is unencrypted. Device tokens
grant terminal control and must be handled like SSH credentials.

## Verification

Run `cargo fmt --check`, `cargo test --offline`, `cargo clippy --offline --all-targets
-- -D warnings`, and `cargo build --release --offline`. The ignored real-tmux
contract uses an isolated socket and should be run when tmux behavior changes.
Herdr integration checks should remain read-only against a user's live socket;
write API checks belong on an isolated tmux socket.

Important quirks:

- A gateway restart is required after backend registry edits.
- Removing a backend never terminates the corresponding terminal sessions.
- The current Muqun data layer is multi-session capable, but its ordinary UI
  automatically selects the first session and does not yet expose a picker.
- The plugin ID and legacy metadata names must survive the repository rename.
