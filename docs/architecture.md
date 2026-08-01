# Gateway architecture

The gateway follows a pragmatic Clean/Hexagonal boundary around the terminal
workspace backend. The mobile HTTP contract is the stable outside contract;
Herdr and tmux are replaceable infrastructure, and a single gateway instance
may mount several backend sessions concurrently.

## Layers

```text
Muqun / HTTP / SSE
        |
        v
application workflows and route orchestration (main.rs)
        |
        v
TerminalBackend port + backend-neutral model (src/backend/model.rs)
        |
        +-------------------+
        v                   v
Herdr adapter(s)        tmux adapter(s)
JSON line protocol      argv-only tmux CLI
```

- `src/backend/model.rs` is the reusable domain boundary. It owns opaque IDs,
  workspaces, tabs, panes, output requests, commands, and the
  `TerminalBackend` port.
- `src/backend/herdr.rs` translates protocol 17 JSON requests and responses.
- `src/backend/tmux.rs` translates tmux format output and commands. It invokes
  tmux directly, never through a shell.
- `src/backend/compat.rs` maps backend-neutral values into the existing
  Herdr-shaped HTTP envelope. This is why existing app builds can use tmux.
- `src/main.rs` is currently both the inbound HTTP adapter and application
  workflow layer. New backend behavior should enter through `TerminalBackend`,
  not through backend conditionals scattered in feature modules.
- A configured session is the composition root for one backend adapter. Route
  handlers select it by `sessionId`; pairing, authentication, devices, SSE, and
  the HTTP listener belong to the gateway process and are shared.

## Use cases

The use cases are actions meaningful to the app, independent of the terminal
implementation:

- inspect topology: snapshot and list/get workspace, tab, pane, and agent;
- inspect terminal state: read visible/recent output, compose structured parts,
  detect approvals, and find files relative to pane working directories;
- control topology: create, focus, rename, close, and split;
- control a terminal: send text, keys, prompts, and interrupts;
- run agent workflows: start an agent, create a task/worktree, spawn beside or
  in a new tab, and observe lifecycle state;
- publish changes: convert backend activity into the existing SSE and push
  notification vocabulary.

Herdr subscriptions and tmux polling are adapter strategies, not separate use
cases. tmux currently polls topology and selected-pane output because that gives
the same observable app contract without depending on a long-lived tmux control
client.

## Reuse and compatibility rules

Reusable across backends:

- authentication, pairing, device revocation, HTTP routing, OpenAPI, and SSE;
- approval detection, agent catalog/state inference, composer, structured
  parts, scrollback cache, shortcuts, tasks, uploads, and asset discovery;
- all mobile request and response shapes.

Backend-specific:

- transport and availability checks;
- native ID validation and parsing;
- topology mutation commands;
- pane capture and input injection;
- event acquisition (Herdr subscription versus tmux polling).

An old config with no `backend` field must deserialize as Herdr and serialize
without gaining that field. The API keeps the legacy `herdr` metadata object
and response envelope for the first configured session; the additive `backend`
and `backends` metadata plus the `multiple_terminal_backends` capability expose
the new abstraction to newer clients. Backend order is therefore compatibility
state, not presentation-only state.

The Manager is a gateway UI. It edits the shared backend registry and lifecycle
configuration; it does not belong to Herdr or tmux and never owns or terminates
their terminal sessions. Configuration changes require an explicit gateway
restart.

Observation is side-effect free. In particular, the legacy `pane/zoom` request
that released app builds send when mounting their viewport is acknowledged at
the HTTP compatibility edge and is not part of `TerminalBackend`; neither
adapter may change the user's native layout merely because Muqun connected.

## Identity migration

An already paired Herdr plugin identity is authoritative during
`import-herdr-plugin`. Import merges standalone backend sessions and device
records into it, creates backups before replacing standalone files, and writes
a marker that makes later plugin actions resolve the standalone config. The
source plugin state is retained for rollback. Import refuses to proceed while
the source gateway is listening, preventing two identities from racing on one
address.

## Transport security

Device bearer tokens authorize full terminal control and must be treated like
SSH credentials. HTTPS is the preferred transport. HTTP to a Tailscale address
is still protected on the network by WireGuard, subject to tailnet ACLs; HTTP
on an ordinary LAN is unencrypted and unsafe against observers. The protocol
does not add bespoke payload encryption because TLS/WireGuard already provide
authenticated transport, while application crypto would not independently
solve bearer-token replay or endpoint compromise.

## Adding another backend

Implement `TerminalBackend`, validate native identifiers before invoking the
transport, translate failures into `BackendError`, and add contract tests for
topology, capture, input, and lifecycle commands. Do not add native response
fields to HTTP handlers; extend the backend-neutral model and compatibility
mapper only when the app contract genuinely needs new information.
