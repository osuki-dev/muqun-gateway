# Codebase context

## Purpose

This repository implements Muqun's local terminal gateway. It exposes one
authenticated HTTP/SSE contract over one or more terminal backends. The
repository, package, and binary are `muqun-gateway`. Distribution and lifecycle
are standalone; Herdr remains a backend, not the host of a Gateway plugin.
The retired `herdr.gateway` identifier remains only for migration compatibility.

## Shape of the codebase

- `src/main.rs`: composition root only -- module declarations, the legacy
  crate-root re-exports, `AppState`, and `fn main`. Domain code lives in the
  module tree below.
- `src/cli/`: CLI arguments, subcommands, and dispatch.
- `src/test_support.rs`: shared test fixtures and fakes, compiled only for
  test builds.
- `src/agents/`: agents plane.
  - `domain/`: neutral agent entities: sessions, timeline, permissions, forms.
  - `ports/`: `AgentPort` (`ports/agent.rs`) and `SessionMirrorPort`.
  - `adapters/`: OpenCode, DeepSeek, Codex (`adapters/codex/`, private app-server
    stdio or verified shared-daemon Unix WebSocket; wire reference in `docs/codex-protocol.md`), T3 Code (`adapters/t3/`,
    Effect RPC over a WebSocket; wire reference in `docs/t3-protocol.md`), and the in-memory
    mirror adapter. Each adapter's event stream is folded into domain events
    by a module under `manager/` (`deepseek.rs`, `t3.rs`, `codex.rs`).
  - `manager.rs`, `runtime.rs`, `use_cases/`: agent supervision and the
    session/prompt/interaction services. `runtime/t3.rs` holds T3's config
    block, its pairing-token-to-bearer credential (persisted by
    `platform/store.rs` as `t3-credential.json`, 0600), and its attach and
    discovery. `runtime/codex.rs` adopts a verified existing Codex daemon connection
    or owns a private fallback child and its lifecycle;
    `adapters/codex/native_sessions.rs` retains accepted native session IDs scoped
    to the verified Codex home in the shared SQLite store. Selected unloaded
    sessions can request official same-ID resume after restart; this evidence
    grants neither runtime ownership nor permission to replay a prompt.
    service agents default on for Codex and OpenCode, with the master
    `agent_runtime.enabled` switch independent of terminal-agent dispatch.
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
  - `history.rs`, `history_memory.rs`, `history_sqlite.rs`, `history_routes.rs`:
    captured-history pagination use case and async repository port, the memory
    adapter, the opt-in SQLite checkpoint adapter, and authenticated HTTP
    boundary. Captured historical prefix only, separately from native ranges and
    the mutable viewport; bounded ephemeral snapshots. The additive
    `/history/segments` contract (`pane_captured_history_segments_v1`) pages
    retained segments in either storage mode; legacy `/history` remains one
    prefix. Storage is selected at
    composition from `config.history.storage` (default `memory`); `sqlite` keeps
    checkpoints in the shared store's `history` namespace, with verified native
    identities, replacement recovery and bounded retention. Operational and
    recovery constraints are in `docs/captured-history.md`.
    Segment boundaries are metadata, never fabricated rows or cross-segment
    deduplication. Each shape shares 5,000 rows/2 MiB/64 segments; global
    retention is 48 shapes/24 MiB/7 days, with separate 60-second snapshots.
    SQLite migrates old prefixes and recovers verified checkpoints before fresh
    writes can overwrite them; storage stats count shapes, not segments.
    App negotiation and its 2,000-row history window fix remain deferred.
  - `login_env.rs`: the `PATH` and `LC_CTYPE` a backend actually needs,
    recovered from a login shell. An init system starts the gateway with neither,
    and the tmux adapter cannot spawn tmux without the first or parse its output
    without the second. Read at startup and written into the unit file.
- `src/connectivity/`: pairing identity, transports, and push.
  - `authority.rs`: pure pairing-code and credential authority; owns hashed
    device records (with their `DeviceKind`: `app`, or `gateway` for another of
    the owner's computers), token verification, install replacement, and
    last-seen policy.
  - `transport.rs`, `gateway_listener.rs`: the AES-GCM envelope and the listener
    bind behind the Tailscale-friendly address policy.
  - `routes.rs`: pairing and push-token registration routes.
  - `push.rs`: Expo notification payloads and the device records that receive
    them.
    `push_token_owner_binding_v1` declares paired-App-owned authenticated
    registrations; legacy unowned records are suppressed. Older Apps can skip
    registration for seven days from their cache: reopening is not sufficient.
    On each paired App, notifications OFF (wait for unregistration), then ON
    clears that cache and performs authenticated registration. Future App work
    must negotiate a stable registration revision, not re-register on every
    Gateway build or restart; seamless upgrade acceptance remains pending.
- `src/tasks/`: the work the gateway tracks. Domain types and the state
  machine (`domain.rs`), the `TaskStore` port with its SQLite adapter, the
  `TaskService` (delegate, wait, read, cancel, list, the task tree), command
  runs in a visible pane (`run_command.rs`), the startup/60 s reconcile of
  tasks nobody waits on and the follower that settles delegations from agent
  events as they arrive (`driver.rs`), change events for `/api/ws`, and
  `/api/tasks*` (including `POST /api/tasks`, a device opening a delegation).
  A delegation to a terminal agent (`codex`, `claude`, ...) is started through
  the spawn route's own `launch_agent` and bound to its pane
  (`binding.kind == "pane"` with `agent_id`); `pane_agent.rs` maps that pane
  agent's backend state onto the task. A delegation of either kind is `done`
  only on an explicit signal -- the agent's `tasks.complete` MCP call, the
  `MUQUN_TASK_DONE` marker (`pane_agent::marker`, read off the pane or
  the session's last message by `service::turn_end`), or the device's
  `POST /api/tasks/{id}/complete`; a stop without one is `idle_unconfirmed`.
  The goal goes to the agent alone: how to finish is the MCP server's
  `instructions` and the `tasks.complete` description, never conversation
  text. `transcript.rs` cuts the consumed marker (and the instruction older
  gateways sent) from the served transcript of a task's session.
  An agent that needs the user says it is blocked (`tasks.complete` with
  `outcome: blocked`, or `MUQUN_TASK_BLOCKED`): the task is `waiting`, and
  stays so through idles until the agent works on a later turn or finishes
  (`TaskService::blocked_at`/`settle`, `pane_agent::judge`).
  An open task's `progress` line (what it is doing now) and the agent
  session names behind an open task's `TaskView.title` live in
  `TaskService` memory only, fed by agent events and pane polls; progress is
  published at most every 2 s per task. An ended session delegation stores
  its session's last name (`session_title`). A device's default task folder
  (`default_cwd`, named from `~`) is served by `GET /api/tasks/defaults` and
  a `defaults` frame on `subscribe_tasks` (capability `task_defaults`); the
  blocked outcome, the waiting line, `title` and the worded `progress` are
  `task_outcome`.
  Every task carries `parent_task_id` and `origin`; `children` counts are
  computed when a view is made, never stored. The driver's pass deletes
  tasks that ended over 30 days ago (`TASK_RETENTION`) with their events;
  the MCP sweep deletes token rows revoked or expired over 7 days ago.
- `src/fleet/`: this machine's capability manifest (agents, terminal backends,
  probed toolchains, task counts) behind `GET /api/fleet/self` and the
  `fleet.*` MCP tools, which also list the linked computers from what this
  gateway last heard, at once (`FleetService::describe`/`list` over
  `PeerService::reachability`); a stale peer is checked and its manifest
  fetched in the background, one refresh per peer at a time
  (`refresh_later`).
  The owner's other gateways (`peers.rs`, `peers_sqlite.rs`, `/api/peers*`
  and `PUT /api/fleet/settings` in `peer_routes.rs`, `muqun-gateway peers`):
  a peer is linked by pairing with it as a phone does, as a device of
  `kind: gateway`, and its token, device id and transport key live in the
  shared store's `peers` table. `peer_client.rs` speaks to it through its
  device API, sealed when it issued a transport key. On the receiving side a
  gateway token reaches only `require_device_or_peer` routes (the task
  routes except `complete`, `subscribe_tasks`, `GET /api/fleet/self`,
  its own `DELETE /api/pairings/{id}`); `require_device` refuses it with
  `peer_forbidden`. `fleet.accept_delegations` is the accept switch.
  `exec.run(machine)` uses `POST /api/tasks` with `kind: command` to run a
  short command on a peer without starting an agent. It retains a command
  mirror and returns bounded output through `tasks.wait`; the receiver
  restricts the working directory to its home and the sender enforces one hop.
  A peer command keeps a single background observer after the create request
  returns its handle, so its exit reaches waiters and result notices promptly;
  the driver restores that observer after a restart without resending the command.
  A `peers` row is keyed by this gateway's own id; the `server_id` the
  peer gave is display data, and a second link giving one already held is
  `409 peer_conflict`, never a merge. An owner-given `server_id` on
  `POST /api/peers` must match the claim answer (`409
  peer_identity_mismatch`). A gateway-kind login record's `server_id` is
  set only by the owner's mint (`POST /api/pair/pending`), never by the
  claimant's `POST /api/pair/request`, since unlinking revokes by it. On the other side a gateway enrolment
  has no install id, so a claim can never replace or revoke another
  device; a link that fails after the claim revokes what it enrolled, with
  that device's own token. A name another peer holds is refused
  (`409 peer_name_taken`), never silently replaced.
  After rolling a gateway back below the build that added device kinds,
  revoke its gateway devices: an older build reads them as app devices.
- `src/mcp/`: the gateway as an MCP server for local agents: `/mcp` on the
  existing listener (loopback, proxy-header refusal, assistant token in
  `auth.rs`), the eight tools in `tools.rs` (which file what they start under
  the calling task through `lineage.rs`), assistant tokens (`tokens.rs`,
  `tokens_sqlite.rs`, pane-bound issue and sweep in `issue.rs`,
  `/api/mcp/tokens*` in `token_routes.rs`), per-host client config in
  `hosts.rs`, and the server handed at launch to the agents the gateway
  starts (`inject.rs`: Claude Code's `--mcp-config`, Codex's `-c`, OpenCode's
  runtime API; no user file is written). Off with `mcp.enabled: false`;
  contract in `docs/mcp.md`. `McpConfig` denies unknown fields: a config.json
  that sets `mcp.inject_into_agents` does not load in a build from before
  it, so rolling back to such a binary means removing that key first.
- `src/platform/`: shared infrastructure.
  - `store/`: the shared SQLite store (`sqlite.rs`, one worker thread,
    migrations per domain) and its optional SQLCipher key file (`db_key.rs`);
    selected by `store.encryption`.
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
  - `manage.rs`: crossterm management views (overview, terminals, devices,
    settings). Settings is one row per setting (`Setting`: value, one-line
    explanation, config key; Enter toggles, opens a chooser or edits), the
    footer one navigation line plus the view's own keys; default-cancel scope
    confirmations, selected-row scrolling and cell-safe, `…`-cut rows.
  - `lifecycle.rs`: shared CLI/TUI start/stop/restart/status ownership and
    transition checks; serializes commands separately from the runtime state lock.
  - `update.rs`: explicit stable-only official GitHub release checks and self-update;
    requires the complete four-platform binary/checksum matrix, verifies bounded
    HTTPS downloads, SHA256/native platform/version, then atomically replaces with
    exact-image rollback. Retains one lifecycle controller across downtime; no
    automatic update polling, downgrades or history database.
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
- `docs/mcp.md`: the MCP server, assistant tokens, host setup and the Tasks API.
- `docs/content-model.md`, `docs/agent-api.md`: the two wire contracts shared
  with the app team -- the versioned content envelope and part set, and the
  agent-session field names. Contract tests in `platform/parts.rs` and
  `agents/domain/` enforce them.
- `install.sh`, `release.sh`, `.github/workflows/release.yml`: standalone
  installation and releases. The plugin manifest/fetch step are retired;
  import compatibility and historical fixtures remain.
  Existing installs delegate to the identified protected updater without shell
  overwrite or a second restart; older installs are refused with manual migration
  guidance. Fresh installs pin one stable tag for HTTPS binary/checksum downloads
  in private staging and verify SHA256 (integrity, not signing). Native
  ELF/Mach-O architecture is checked before staged execution; a time/output-bounded
  `--version` probe must match the pinned tag before publication. Fresh installs
  require curl and sha256sum/shasum; macOS also requires preinstalled system Perl
  for direct `link()` atomic no-clobber publication and fails closed without it.
  No automatic dependency or global installation is attempted. Numbered stages report
  OK/FAILED/UNCONFIRMED; supported TTYs use real curl progress, with plain
  NO_COLOR/dumb/non-TTY output and narrow-terminal-friendly stage labels.

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

### Pane-bound tasks

A delegation to a terminal agent (`claude`, `codex`, ...) opened by `POST
/api/tasks` or `tasks.delegate` is bound to a pane: `TaskService::delegate_to_pane`
makes the pane through the spawn route's `place_agent`, binds it (`binding.kind
== "pane"` with `agent_id` and `shell_pid`) and answers at once as `dispatching`.
`start_terminal_agent` then starts the agent with its launch arguments and, once
it is ready, types the goal (`type_agent_prompt`) where it could not go on
the command line; `goal_accepted` presses Enter again if the goal sits
unsent in the input box, and fails the task if it never leaves. `pane_agent::follow`
(resumed by `driver.rs` after a restart) reads the pane every 2 s: an agent
starting or working is `working`, one blocked on a menu is `waiting`, one idle
on two polls after it was seen busy (or after 30 s) is `idle_unconfirmed`
(polled every 12 s, backing off to 60 s while nothing changes), and a pane
that closed or lost its agent is `failed`.
`done` needs an explicit signal: a `MUQUN_TASK_DONE: <summary>` below the
echo of the goal in the pane's recent output (`marker_after`), or the agent's
`tasks.complete` call; an idle agent is never finished. A
`MUQUN_TASK_BLOCKED: <reason>` line (or `tasks.complete` with outcome
`blocked`) makes it `waiting` until the user's answer sets it working. Pane, launch and prompt
delivery live in `src/agents/session_routes.rs`; the prompt wording, the marker
and the `judge` table in `src/tasks/pane_agent.rs`.

A finished delegation follows its still-live binding into another turn. A new
user timeline item reopens an agent session; a pane must show an idle-to-busy
boundary and retain its recorded shell and foreground job identity. Reopening
keeps the task ID and prior summary, increments `reopen_count`, and records
`reopened_ms`. Old completion markers are excluded from the new turn. Cancelled
tasks and missing or reused bindings stay finished. Peer mirrors follow the
same generation, including when a whole turn finished between observations.
A terminal agent's first tool call can precede that observation. Its still-live
finished assignment remains the parent for a command it starts, so child history
and the peer-origin one-hop restriction do not depend on polling timing. A tool
call alone does not reopen the assignment.

### Linked computers

A peer is a `gateway` device on the other side: this gateway paired with it
as a phone would and holds an ordinary device token for it, which there
reaches only the peer routes (`require_device_or_peer`). Nothing pings a
peer on a timer: `PeerService::checked` pings one whose last answer is over
60 s old when something needs it (`GET /api/peers`, `fleet.*`), and it is
reachable while that answer is under 2 minutes old; a 401 forgets it. Its
compact manifest is cached in its row for 60 s (`manifest_of`). `fleet.*`
never waits on a peer except `fleet.describe(machine: <peer>)` on a peer
not checked lately or without a manifest; a manifest is asked for at most
once a minute, answered or not. The
computers the user `@`-named when opening a task (`POST /api/tasks`
`mentions`, checked against the peer names) are stored on the task
(`Task::mentions`) and reach the agent only through `tasks.read` of its own
task, never through its conversation.

### Agent upstream contract baselines

OpenCode's upstream baseline is **2.0.27**, released October 12, 2026. The
contract was checked against published source and mocked protocol replies;
this is not live macOS/Linux platform certification. Discovery authenticates
`GET /api/info`, requires a valid live PID and a known 2.x version, and matches
any registered PID/version. Supervision also tracks the registration identity
and reattaches when the instance changes. Credential-bearing debug output is
redacted. Explicit `OPENCODE_URL` / `HERDR_GATEWAY_OPENCODE_URL` endpoints are
attach-only: failed identity, authentication or version checks must not start a
local fallback service. Older and future major versions are refused.

Gateway absorbs upstream mappings without requiring an existing App update.
The legacy session-list/children arrays remain; the additive page endpoint is
gated by `agent_session_pages_v1` and per-agent `sessionPagination` (OpenCode
and Codex). It preserves provider page order without automatically following
pages. The current App's fifty-session request does not become full pagination.
See [Agent API](docs/agent-api.md#get-apiagent-sessionspage) for the contract.

DeepSeek's **rc.2** mapping reads `session/projections.asOfSeq` as the
authoritative history cut and retains it across backward pages
(`DeepseekClient::read_history`): the opening read keeps the newest pages
within `INITIAL_HISTORY_RETAINED_BYTES` (2 MiB) and says `hasMore` rather
than failing for the size of a history, and the driver keeps a per-session
cursor (`through_seq`, the oldest seq delivered, carried tool results) that
`load_earlier_timeline` continues from, one batch within
`MAX_HISTORY_RETAINED_BYTES` (8 MiB) at a time, until seq 0. The
end-of-run refetch of the newest messages never moves that cursor back up. File references
use `fileReferences/list` with the live session as `agentId`; workspace file
list/stat use `workspaceFileScopeId`, not a directory substituted for session
scope. Model switching reads the returned `selected` model; readiness requires
a matching `server-response`/`rpcId`, successful result and model-catalog shape,
not merely HTTP 200. No upstream package replacement is required. Existing HTTP
and HTTP-backed reverse-proxy paths are preserved; **0.2.1-alpha.2** native
HTTPS Origin/Cookie handling and optional asynchronous questions with late
answers remain unsupported. Do not generalize that limitation to every HTTPS
reverse proxy or to ordinary blocking forms.

Codex targets stable **rust-v0.162.1**. Its default-enabled service adapter runs
a verified existing daemon connection on Unix, falling back to one private
`codex app-server --listen stdio://` child from the existing `PATH`
and CLI authentication/configuration; Gateway never installs or logs in for the
owner. `codex.enabled: false` disables it; `agent_runtime.enabled: false` disables
all service agents without changing terminal Codex routes or their permission
flags. Primary preference is `opencode`, `deepseek`, `t3`, `codex`. Model/effort
selection is ephemeral Gateway-local next-turn state, pending until authoritative
normalization confirms application; it is not a fabricated pre-turn upstream
update. Unsupported optional features remain false. See
[Codex protocol reference](docs/codex-protocol.md) for mappings and limits.

T3 negotiates protocol **1 or 2** from the public descriptor, with missing meaning
1 and unknown/malformed values unsupported. V2 uses a pinned nightly-source
baseline, not a claim of live certification. Revert is advertised only for known
protocol 1, never V2 or unknown. See [T3 protocol reference](docs/t3-protocol.md).
Real Codex and paired App/macOS checks for this phase are deferred by user
instruction; source/fixture evidence does not replace the required acceptance.

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
legacy environments share standalone ownership. Explicit
`MUQUN_GATEWAY_CONFIG_DIR`/`MUQUN_GATEWAY_STATE_DIR` overrides take precedence;
service units pin the resolved paths so service and ordinary runs share identity.

Lifecycle uses a live state-lock owner, verified as a gateway process, not a
PID-file value or a port scan, for running status and direct signals. A separate
`state/lifecycle/gateway.lock` serializes lifecycle commands. Detached startup
waits for the child's authenticated local manager endpoint and reports/reaps
failed starts. Supervised actions go through the installed user unit/LaunchAgent;
custom user systemd units must use process-only shutdown, and system units are
left to their operator. A failed stop never starts a replacement. Gateway
autostart registers the service; backend autostart independently opts terminal
servers into one startup attempt. Neither removal nor gateway shutdown terminates
terminal sessions. Disabling gateway autostart preserves whether it was running.
On Linux, safety and identity checks follow daemon-reload and inspect effective
systemd properties, including drop-ins. `busctl` supplies structured argv and
environment values that `systemctl show` renders lossily. Unpinned older units
also require the inherited manager environment; unsupported environment files,
custom stop hooks, missing inspection tools, or ambiguous ownership fail closed.
TUI clipping/wrapping measures display cells using POSIX `wcwidth` under a private
thread-local UTF-8 locale, without changing the multithreaded process's global locale.

The public listener is normally localhost behind Tailscale Serve HTTPS or a
Tailscale IPv4 address. HTTPS is preferred. Application transport encryption is
`required` by default and binds a per-device AES-GCM key to each newly paired
device, so its bearer token is not sufficient by itself. `disabled` is an
explicit token-only compatibility mode. In `required` mode every device is
held to it: a record paired while the setting was `disabled` holds no
transport key, and its bearer-only requests are refused with
`426 device_repair_required` by `require_device`/`require_device_or_peer`
(so by every device route) until it pairs again. Switching to `required`
therefore means pairing every earlier device again; switching to `disabled`
keeps serving sealed devices on their token.

HTTP calls and manual HTTP code pairing remain supported. HTTP inside a
verified SSH or Tailscale tunnel differs from plain HTTP on an unprotected
LAN/WAN link; a hostname or address range alone is not evidence of protection.
Without the QR's pairing key, the manual claim sends `code` and `request_id`
without an application encryption envelope. A response key derived from those
values cannot protect credentials from passive capture of both legs on an
unprotected link. Use the QR key, a verified protected tunnel, or HTTPS where
available. A gateway linking to another (`POST /api/peers`) gets that key
from the mint: `POST /api/pair/pending` answers `pairing_key` in `required`
mode, the phone hands it over with the code, and `peer_client::claim` seals
the claim as the phone's QR claim is sealed and takes only an answer sealed
under that key and bound to the claim's nonce (`claim_unsealed`,
`claim_not_opened`). Without the key (an older app, `muqun-gateway peers
link`), the code-sealed answer is kept, but an answer in the clear from a
gateway that said `required` of itself is refused.

Agents the gateway launches for remote dispatch (`POST …/tasks`, `POST …/spawn`,
and a terminal-agent delegation through `POST /api/tasks` or `tasks.delegate`)
have nobody at the terminal to answer a prompt. They start with their own
bypass flags (`codex --dangerously-bypass-approvals-and-sandbox`, with
`-c projects=…` trusting the task directory and its checkout; `claude
--dangerously-skip-permissions`, with the directory marked trusted in
`~/.claude.json` beforehand). Folder trust, for both, is the owner's choice:
on by default, switched off with `"agent_trust_folders": false`, and only
for directories strictly inside their home. The launch arguments in effect
are logged once at startup and shown in `manage`'s Settings view. Agents
start with these flags, replaceable per kind with `agent_launch_args`
in `config.json` (an empty list disables them), and with `agent_env` (e.g. a
proxy the gateway's own environment lacks) set on the pane they start in:
tmux `-e` (3.2+ for a session, 3.0+ for a window or split), Herdr's pane
`env`, or, when the backend cannot set a pane's environment, an `env K=V …`
prefix on the launch line tmux types. Herdr's `agent.start` takes no env, so a
worktree pane Herdr made itself does not get `agent_env` (the task's steps say
so). `-e` and the prefix put values on a command line (visible in the local
process list). `agent_env` values are never logged; `Debug` redacts them.
On tmux, the spawn route's prompt and a terminal delegation's goal reach
Claude Code and Codex as the launch line's last argument (`-- '<goal>'`),
so the goal is on that command line too, quoted by `shell_word` to read
back unchanged in sh, bash, zsh and fish; a goal with a backslash or a
control character other than a newline, other agents, and Herdr have it
typed once the agent is up.

Delegated agents run as the gateway's OS user with the default bypass flags
above. Folder selection is not a sandbox: `agent_launch_args` and
`agent_trust_folders` are owner controls, not isolation from same-user processes
or shell access.

`fleet.accept_delegations` (default `true`) is whether linked computers may
open tasks here; the phone's switch writes it (`PUT /api/fleet/settings`),
changing only that key in `config.json`.

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
- The historical plugin ID (`herdr.gateway`) still identifies migration paths;
  legacy `herdr` response metadata remains unchanged for older Apps.

### Encrypted transport revision 2 and durable request admission

`transport_versions: [1, 2]` in authenticated health/discovery answers advertises
revision 2. The transport header remains `1` (an encryption marker); paired keys
and the HKDF key family do not change. Revision 2 authenticates
`muqun-transport-v2\n<timestamp_ms>\n<existing AAD>`. Existing AAD is method plus
path/query, with the initiating request nonce appended for replies. Responses
use the authenticated request revision. SSE/WebSocket record keys and counters
remain unchanged; their opening request uses the negotiated envelope.

Clients verify an authenticated, nonce-bound v2 read before durably pinning v2
and sending an application operation. A plain `transport_version_required`
refusal may suggest a v2 read but cannot establish a trusted pin. Lost replies
can be recovered with read-only negotiation; never retry an ambiguous mutation
or downgrade a pin. `fleet::Peer.transport_min_version` persists outbound choice.

`connectivity::replay_store` stores canonical nonce bytes and the minimum inbound
revision per credential in SQLite, atomically before dispatch. A successfully
admitted v2 request pins its credential across restart. Admission rechecks
freshness on the SQLite worker inside the transaction, not only at decryption;
invalid metadata changes neither the nonce ledger nor the version pin. Global
v2 pruning advances a durable monotonic cutoff: v2 timestamps below it remain
invalid after clock rollback or restart. Accepted legacy v1 identities remain
until upgrade; unauthenticated timestamps are never grounds for eviction.
Admission refuses at 262,144 outstanding identities per credential instead of
evicting protection. A full legacy ledger returns `transport_version_required`
so a read-only v2 negotiation can release old history without re-pairing.

Legacy v1 still works during migration, including pairing bootstrap. Durable
identities prevent replay of requests this Gateway has accepted, but cannot
authenticate an unseen v1 request's freshness or recover pre-upgrade history.
Do not describe legacy v1 as equivalent to v2. Update App and Gateway peers and
verify v2 before claiming remediation for that connection. Back up the store
before updating: the fleet migration and appended `transport` migration #2
mean an older binary alone is not a valid rollback of the migrated database;
store migration checks reject schemas newer than that binary supports. Recovery
requires a database backup compatible with the exact binary version, not deleting
the database. Replay history cleared by pre-fix builds cannot be reconstructed;
this change does not retroactively repair that loss.

Outbound MCP forwarding resolves context separately from best-effort local task
filing. Missing, ambiguous, failed, timed-out or truncated evidence and broken
ancestry return `lineage_unresolved` before remote work. Peer ancestry returns
`delegation_loop`. Local operations retain their behavior. Shared provider
credentials and same-user shell access are not a session sandbox; this is a
conservative workflow boundary rather than hostile OS isolation.
