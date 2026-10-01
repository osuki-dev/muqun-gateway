# Codebase context

## Purpose

This repository implements Muqun's local terminal gateway. It exposes one
authenticated HTTP/SSE contract over one or more terminal backends. The
repository, package, and binary are `muqun-gateway`; the Herdr plugin
identifier stays `herdr.gateway` because it names the Herdr integration, not
the gateway itself (see the last line of this file).

## Shape of the codebase

- `src/main.rs`: composition root only -- module declarations, the legacy
  crate-root re-exports, `AppState`, and `fn main`. Domain code lives in the
  module tree below.
- `src/cli.rs`: CLI arguments, subcommands, and dispatch.
- `src/test_support.rs`: shared test fixtures and fakes, compiled only for
  test builds.
- `src/agents/`: agents plane.
  - `domain/`: neutral agent entities: sessions, timeline, permissions, forms.
  - `ports/`: `AgentPort` (`ports/agent.rs`) and `SessionMirrorPort`.
  - `adapters/`: OpenCode, DeepSeek, and the in-memory mirror adapter.
  - `manager.rs`, `runtime.rs`, `use_cases/`: agent supervision and the
    session/prompt/interaction services.
  - `routes.rs`: the OpenCode-style agent-session API (global and legacy paths).
  - `session_routes.rs`: terminal-multiplexer agent orchestration -- task
    dispatch, agent spawn, prompt delivery, approvals, and agent event history.
  - `tasks.rs`, `agent_events.rs`, `approvals.rs`: task step logs, the agent
    status transition ring, and approval classification.
- `src/terminal/`: terminal control plane.
  - `backend/model.rs`: backend-neutral entities, commands, errors, and the
    `TerminalBackend` port. The port is the test surface for synchronous terminal
    use cases, including topology, terminal I/O, agents, and optional worktrees.
  - `backend/herdr.rs`: Herdr protocol-17 socket adapter and subscription.
  - `backend/tmux.rs`: argv-only tmux adapter and polling event source.
  - `backend/registry.rs`: static adapter registry for construction,
    availability checks, endpoint rendering, and setup defaults.
  - `backend/compat.rs`: maps neutral models into the legacy Herdr-shaped API.
  - `routes.rs`: session/workspace/tab/pane HTTP routes, pane views
    (output/parts/files/context/git), and the terminal SSE hub.
  - `factory.rs`: the one call site that turns a session config into a live
    backend, including tmux wire-id translation.
  - `scrollback.rs`: bounded scrollback retention and the application policy
    for observing frames and serving row-bounded reads. Callers do not assemble
    cache keys or compare output byte lengths.
  - `login_env.rs`: the `PATH` and `LC_CTYPE` a backend actually needs,
    recovered from a login shell. An init system starts the gateway with neither,
    and the tmux adapter cannot spawn tmux without the first or parse its output
    without the second. Read at startup and written into the unit file.
- `src/connectivity/`: pairing identity, transports, and push.
  - `authority.rs`: pure pairing-code and credential authority; owns hashed
    device records, token verification, install replacement, and last-seen policy.
  - `transport.rs`, `gateway_listener.rs`: the AES-GCM envelope and the listener
    bind behind the Tailscale-friendly address policy.
  - `routes.rs`: pairing and push-token registration routes.
  - `push.rs`: Expo notification payloads and the device records that receive
    them.
- `src/platform/`: shared infrastructure.
  - `config.rs`: configuration types and the shared tuning constants.
  - `store.rs`: config/device/pairing/push-token persistence, PID files, and
    secret-file permissions.
  - `http.rs`: the versioned content envelope, API errors, device
    authentication, and request validators.
  - `server.rs`: HTTP startup, the middleware stack (encrypted transport,
    compression gate, security headers), and route composition via each
    domain's `mount()`.
  - `setup.rs`: setup, backend configuration, Herdr plugin import, and the
    service/background lifecycle.
  - `manage.rs`: the interactive terminal management UI.
  - `metadata.rs`: capabilities, health metadata, and session liveness ordering.
  - `uploads.rs`, `assets.rs`: phone uploads and the workspace asset index.
  - `git.rs`: read-only, bounded `git` for one pane's checkout -- the
    branch line, the changed-file list with totals, and one file's unified patch
    a page at a time -- behind the `pane_context` and `git_diff` capabilities.
    Fixed argument lists, one validated path after `--`, a timeout and an output
    cap, `--no-optional-locks` so the agent's index lock is never taken.
  - `i18n/`, `parts.rs`, `discovery.rs`, `openapi.rs`, `routes.rs`,
    `service.rs`, `state_lock.rs`: localization (one catalog file per
    language), pane-output parsing,
    capability discovery, the OpenAPI/Scalar spec, the platform routes, service
    unit files, and the single-owner state lock.
- `docs/content-model.md`, `docs/agent-api.md`: the two wire contracts shared
  with the app team -- the versioned content envelope and part set, and the
  agent-session field names. Contract tests in `platform/parts.rs` and
  `agents/domain/` enforce them.
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

Two planes are implemented here: the terminal control plane
(`TerminalBackend`) and the agents plane (`AgentPort`).
`/api/discovery` additionally reports an SSH plane, but that is the app's own
transport -- the phone can open an SSH connection and tunnel the gateway's
loopback port, and the same HTTP API answers on the far side -- not a gateway
subsystem; the gateway implements no SSH client or server.

The gateway owns pairing identity, device tokens, network listener, backend
registry, Manager, SSE fan-out, and lifecycle. A backend owns only interaction
with its terminal system. The first session is compatibility-sensitive because
older Muqun UI currently opens the first `/api/sessions` item.

HTTP extracts credentials and maps errors; `connectivity/authority.rs` decides
whether a credential is a paired device or the narrow local-manager identity.
Device records persist only token hashes. File I/O stays in
`platform/store.rs`, so credential policy is testable without HTTP or disk.

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
-- -D warnings`, and `cargo build --release --offline`. CI
(`.github/workflows/ci.yml`) runs the same formatting, clippy, test and audit
checks on every push and pull request; the audit job is what keeps a RustSec
advisory from landing unnoticed. The ignored Herdr and
tmux contracts use isolated sockets and should be run when adapter behavior
changes. Never mutate a user's active Herdr session in a test. Startup/delivery
changes additionally require real paired App checks on an explicitly isolated
QA session, following the trust/approval and proxy rules in `AGENTS.md`. The tmux contract test now also asserts
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
