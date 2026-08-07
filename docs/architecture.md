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
  workspaces, tabs, panes, agents, worktrees, output requests, commands, and the
  `TerminalBackend` port.
- `src/backend/herdr.rs` translates Herdr's JSON socket requests and responses
  (protocol 17 and newer; the README's "Compatibility and API versions" explains
  why there is no upper bound).
- `src/backend/tmux.rs` translates tmux format output and commands. It invokes
  tmux directly, never through a shell.
- `src/backend/registry.rs` is the static composition registry for the adapters
  shipped in this binary. It owns construction, availability probes, setup
  defaults, and endpoint presentation; it is not a dynamic plugin loader.
- `src/backend/compat.rs` maps backend-neutral values into the existing
  Herdr-shaped HTTP envelope. This is why existing app builds can use tmux.
- `src/authority.rs` owns the pure pairing and credential rules. HTTP supplies
  bearer tokens and maps failures; disk persistence remains outside it.
- `src/main.rs` is currently both the inbound HTTP adapter and application
  workflow layer. Synchronous terminal use cases enter through
  `TerminalBackend`; activity publication consumes the same port while each
  adapter owns native subscription versus polling.
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

Herdr subscriptions and tmux polling are adapter strategies behind
`TerminalBackend::activity_stream`, not separate use cases. tmux polls topology
because that gives the same observable app contract without depending on a
long-lived tmux control client. Selected-pane output polling is a shared
application policy for both adapters.

## Reuse and compatibility rules

Reusable across backends:

- authentication, pairing, device revocation, HTTP routing, OpenAPI, and SSE;
- approval detection, agent catalog/state inference, composer, structured
  parts, scrollback cache, shortcuts, tasks, uploads, and asset discovery;
- all mobile request and response shapes.

Terminal reads have one application policy in `scrollback.rs`. Direct reads,
structured-parts reads, and sampled event frames identify the same
session/pane/source/format stream; the store owns observation, deduplication,
row-bounded serving, and memory limits. Route handlers never inspect its keys
or decide history depth from UTF-8 byte length.

Backend-specific:

- transport and availability checks;
- native ID validation and parsing;
- topology mutation commands;
- pane capture and input injection;
- event acquisition (Herdr subscription versus tmux polling).

`TerminalBackend` is a real seam: both Herdr and tmux satisfy the same isolated
read/write contract. A capability one adapter lacks, such as native worktree
orchestration in tmux, reports `Unsupported`; the use case may then apply a
backend-neutral fallback without branching on adapter kind.

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

`transport_encryption` is a gateway-level policy for newly paired devices. Its
default, `required`, protects pairing and API bodies with AES-256-GCM. A QR
bootstrap secret protects the confirmation-code exchange; only after the code
is consumed does the gateway mint a distinct transport key for that device.
HKDF derives direction-separated keys, request method/path are authenticated as
AAD, timestamps bound acceptance, and successful request nonces enter a replay
cache. A bearer token alone therefore cannot use an encrypted device record.

`disabled` is an explicit compatibility mode. Its QR carries no bootstrap key,
new devices receive no transport key, and their bearer token is sufficient for
API access. Existing device records retain the mode in which they paired.

Application encryption hides credentials and payloads, but route names, query
strings, sizes, timing, device id, and availability remain visible. It has no
forward secrecy and cannot protect a compromised endpoint. HTTPS remains
preferred; Tailscale WireGuard and ACLs remain valuable network boundaries.

## Adding another backend

Implement `TerminalBackend`, validate native identifiers before invoking the
transport, translate failures into `BackendError`, and add contract tests for
topology, capture, input, and lifecycle commands. Do not add native response
fields to HTTP handlers; extend the backend-neutral model and compatibility
mapper only when the app contract genuinely needs new information.
