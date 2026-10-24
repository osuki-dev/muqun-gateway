# Unreleased

## Changes and fixes

- **Muqun tool activity remains visible in agent conversations.** Pending,
  running, completed and failed Muqun MCP calls now reach the App's tool UI
  across OpenCode, DeepSeek, T3 and Codex. Catalog discovery and pending
  `execute` calls remain visible too. Task completion markers and internal
  instructions retain their existing sanitation.

- **Optional Home task collection for Muqun 4.0.** Gateways advertise
  `home_task_summary_v1` and `home_task_collector_v1`. An App can subscribe to
  bounded task snapshots and changes on its existing encrypted WebSocket, or
  explicitly grant another paired Gateway read access for that App. Grants
  include task summaries and the owner's default folder, never full goals,
  transcripts or command authority. Revocation and device removal stop access.
  Older Apps keep their existing paths. The new grant table migrates automatically
  on Gateway startup; installing this version requires the usual Gateway restart.
  Full paired-device acceptance is still pending.

- **A terminal pane's skills run from the App's `$`, like an agent
  session's.** `$` is the App's skills entry for every agent; the App lists
  a pane's skills and names one, and how the agent's prompt takes it is the
  gateway's job. The pane shortcuts answer
  (`GET /api/sessions/{id}/panes/{pane}/shortcuts`) gains `skills` --
  `[{name, description, source}]`, `source` `user` or `workspace` -- and
  `file_mentions`, whether `@` file mentions mean anything to the pane's
  agent. The new `POST /api/sessions/{id}/agents/{target}/skill`
  (`{skill, args?}`) runs a listed skill: the gateway types the agent's own
  line and submits it exactly as `.../send` does, once. A name the list does
  not carry is `404 skill_not_found`, an agent the gateway delivers no skill
  to `409 skills_unsupported`, a malformed name or multi-line argument `400
  invalid_skill` / `invalid_args`. Claude Code reads `~/.claude/skills` (or
  `$CLAUDE_CONFIG_DIR/skills`) and `.claude/skills`; Codex reads
  `~/.agents/skills`, `$CODEX_HOME/skills` (default `~/.codex/skills`),
  `.agents/skills` and `.codex/skills`; Qoder reads `~/.qoder/skills`,
  `.qoder/skills` and `.agents/skills`. Repository directories are read in the
  pane's directory and each parent up to its checkout root. OpenCode, whose
  skills are loaded by the model rather than typed, other agents and plain
  shells list none. Slash commands are unchanged. Announced as `pane_skills`;
  the fields and the route are additive and older Apps ignore them.

- **`@` in a Codex session lists files before anything is typed.** A bare
  `@` (`GET /api/agent-sessions/{asid}/files` with an empty `query`) answered
  no files for Codex, whose fuzzy search matches nothing for an empty query.
  It now answers the shallowest files of the session's directory, as an
  OpenCode session does; a typed query still goes to Codex's own search.

- **Listing worktrees no longer fails past 128 checkouts.** A repository with
  more than 128 worktrees answered `GET …/git/worktrees` with `502 git_failed`
  (the App's "Could not list worktrees"), and every attempt logged
  `worktree inventory exceeded its entry limit`. The inventory now lists up to
  512 checkouts, main checkout first, and says when it cut: the envelope's
  `data` gains `truncated` and `total` (entries Git reported). Listing is also
  faster: each checkout is verified by reading its `.git` file and the
  repository's back-pointer instead of running git twice per checkout. A
  `worktree` selection on `git/status`, `git/diff` and lazygit is verified the
  same way on its own, so any checkout of the repository can be selected.
  The new fields are optional; no App update is needed.

- **Captured terminal history is off by default.** The Gateway no longer folds
  what it reads of panes Herdr keeps no scrollback for (Claude Code and other
  alternate-screen programs), in memory or in SQLite checkpoints: it only ever
  kept the stretches it happened to watch. Panes are now answered exactly as
  Herdr gives them, and the `/history` and `/history/segments` routes answer an
  empty `not_captured` page. Checkpoints already in the store are kept, not
  served. Set `"history": {"capture": true}` in `config.json` and restart to
  restore the previous behavior; `GET /api/history/storage` reports `capture`.
  No App update is needed.

- **Terminal agent transcripts are removed again.** The
  `GET /api/agents/transcripts/{agent}/{session_id}` and `.../head` routes,
  the `agent_transcripts_v1` capability, the `agent_transcripts` field of
  `/health` and `/api/discovery`, and the `agent_session` field on pane agent
  entries in `/agents` and `/snapshot`, all added in 0.14.1-preview.4 and
  preview.5, are gone because no client reads them: the App dropped its
  read-only Claude Code conversation view. The Gateway no longer opens Claude
  Code's session files. No configuration change is needed.

- **Codex has a "Waiting to send" inbox.** A prompt sent to a busy Codex
  session with `delivery: "queue"` now waits in a Gateway-owned queue instead
  of being refused, and Codex declares `features.inbox: true`. When the running
  turn completes normally, the Gateway starts the next waiting prompt as a new
  turn, one per turn and in order. After an interrupted or failed turn nothing
  is sent automatically; the queue waits until you send again, tap "send now"
  (`POST …/inbox/{id}/steer`, which steers the running turn or starts one) or
  cancel. Every queue change arrives as `agent.inbox.changed` with the same
  replay and resync behavior as OpenCode. A prompt Codex refused stays queued;
  one whose delivery outcome is unknown is removed, never resent, and the
  session gets a `codex_queued_delivery_outcome_unknown` resync. The queue is
  held in memory and does not survive a Gateway restart or a reconnect to Codex.
  An unknown or already delivered inbox item now answers
  `404 inbox_item_not_found`.

- **Queued prompts carry their attachments.** Inbox items list their files in
  `payload.files` (`uri`, `name`, `mime` when known), and for both OpenCode and
  Codex each item with files gains the same `attachments` and `image_assets`
  fields a transcript user message has, so the App can show image thumbnails
  for a prompt that is still waiting.
- Codex sessions no longer drop to read-only history when output streams
  quickly. A fast answer arrives as hundreds of small text updates at once, and
  the Gateway closed its Codex connection whenever more than 16 of them were
  waiting, so the App showed the live session as saved history in the middle of
  a turn until it reconnected. Consecutive updates to the same text are now
  combined while they wait, without losing, reordering or skipping any text or
  approvals. No App update or configuration change is needed.

- Add an explicit Gateway preview channel for the installer and protected updater.
  Stable remains the default; preview automatically selects the newest complete
  `X.Y.Z-preview.N` release. Existing protected updaters that predate channels can
  transition through a verified preview bootstrap helper without first publishing
  a new stable release. The selected preview must contain bootstrap support;
  preview.1 predates it. Preview uses the existing service and pairings; it does not
  introduce a second instance or a new Herdr/App compatibility requirement.
  Installing an update retains the existing lifecycle checks and restart behavior.
  Returning to an older stable binary is still refused because binary rollback
  alone cannot undo database migrations. Fresh preview installs require Python 3.
  Publication uses a manually invoked CI workflow and never changes GitHub latest.

- Codex subagent activity records no longer manufacture null `prompt`,
  `operation`, `model` and `reasoningEffort` arguments. Activities carry their
  actual operation and supplied metadata; child-session identity is preserved.
  Activity records do not include a reply in the upstream protocol. Actual
  collaboration-tool replies still reach the App, and the complete conversation
  remains available by explicitly opening the child session.

- OpenCode event streams now retry when the upstream accepts a connection but
  never returns response headers. A five-second handshake deadline enters the
  existing reconnect backoff; established streams keep their unlimited lifetime.
  No App update or configuration change is needed for this fix.

- Refresh the locked Rust dependencies for libc, serde_json, sha2, tokio,
  tokio-util, uuid and toml, including toml's supporting datetime crate. Keep
  reqwest on 0.12.28 to preserve the existing TLS provider and certificate-root
  behavior; the 0.13 migration needs separate compatibility validation.

- **`GET /api/discovery` and `GET /api/capabilities` require a credential.**
  They answered anyone on the network with the computer's id, label,
  version, generation, every capability and the agent configuration (agent
  endpoints and versions blanked). Like `/health`, they now take a paired
  device, a linked gateway's token, or the admin token (the `manage` TUI);
  no credential is `401`. No flow read them unauthenticated: the app sends
  its bearer, a linked gateway its token, `manage` the admin token.

- Large request bodies are bounded in number as well as size: two bodies
  over 25 MiB (uploads) and eight of the rest (prompts, commands, tasks) may
  be in memory at once, process-wide; past that the answer is `503 busy`
  before any byte is read. A sealed body is read only for a paired device
  id that holds a transport key, and fully authenticated once opened.

- **Screen recordings can be attached.** `POST /api/uploads` accepts MP4 and
  QuickTime video (the same `ftyp` brands the asset preview plays) up to
  64 MiB; every other type keeps its 25 MiB. `GET /api/discovery` carries
  both limits as `uploads.maxBytes` and `uploads.maxVideoBytes`. An upload is
  buffered whole while it is sniffed and written, so a video in flight costs
  up to 64 MiB of memory (about 1.4 times that for a sealed one while its
  envelope is opened).

- Session-to-agent routes survive a restart (store namespace
  `agent_session_routes`): the first open of each session after a restart no
  longer probes every attached agent in turn, which took up to twelve
  seconds with four. The store gains a migration, so a binary from before it
  refuses the migrated database until the backup is restored, as with every
  store migration.

- Twelve pairing requests per ten minutes (was six), so an owner pairing
  several devices again after turning encryption on is not stopped by
  "too many pairing requests". The eight wrong-code lockout per code is
  unchanged.

- **The device chooses where a task runs.** `POST /api/tasks` took an agent
  that is both a terminal agent and a session agent as a terminal pane only,
  so the app opened such sessions directly and the Home card could show only
  a transient local row. `delivery: "session" | "pane"` (capability
  `task_ledger_delivery`) routes the task as said -- a native agent session
  with its `asid`, or a pane with its `pane_id` -- and each `/api/discovery`
  agent entry says what it can do as `ledgerDelivery`; a delivery the agent
  cannot perform is `400 invalid_request`. Absent, nothing changes.

- A phone whose clock is off by more than five minutes now reads
  `403 clock_skew` with `error.server_time_ms` instead of the generic
  `invalid_envelope`, so the app can say by how much. Only an envelope that
  authenticated is told so.

- The earlier-history route answers `409 history_cursor_lost` when the
  gateway holds no cursor for the session (a restart, an eviction, a history
  that changed), where it used to answer an empty batch that read as the end
  of history; the app re-reads the snapshot and pulls on. The mirror keeps
  5,000 rows per session (was 1,500), so the earlier pages just pulled are
  not the first rows trimmed by the next upsert.

- OpenCode sessions page back too: `POST /api/agent-sessions/{asid}/timeline/earlier`
  (`features.timelineEarlier: true`) delivers the 80 messages below the
  oldest one shown, read as the newest `held + 80` (OpenCode's message route
  has no cursor) and bounded to 2,000 messages and 8 MiB per read.

- **Agents are told about the linked computers up front, and to prefer
  `exec.run`.** A task the owner opens on their phone now carries, in the
  agent's instructions (the MCP server instructions, Claude Code's appended
  system prompt, Codex's `developer_instructions`, OpenCode's session
  instruction), one line naming every linked computer with its reachability
  and the computers the owner named -- names and reachability only, never a
  URL, an id, a token or a key -- so the agent reaches for `exec.run` /
  `tasks.delegate` with `machine` without a `fleet.describe` first. The
  routing sentence now says to call `exec.run` with `machine` first for a
  command on another computer (visible, recorded, interruptible, approved
  there) and to fall back to its own SSH only when that computer is not
  linked or `exec.run` cannot do it, saying so. A task another computer sent
  over is told nothing (one hop). The home-directory fence on a received
  command is unchanged: it applies to another gateway's command, never to
  the owner's app.

- `fleet.list` machines carry `state`: `online`, `offline`, `unknown` (not
  checked in the last minute) or `unauthorized`. A linked computer whose
  token was revoked read as plainly offline before; it is there, refusing,
  and linking again is the remedy. `online` is unchanged (`false` for it).

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

Other capability declarations remain properties of the build and
stay unconditional, `agent_spawn` included: spawning runs on tmux as well as
Herdr, so a tmux-only gateway still announces it. tmux sessions are unaffected
in every respect except that they no longer claim a feature they cannot perform.

## T3 Code 0.1.0

T3 Code shipped stable **v0.1.0** on 2026-10-15, which makes Orchestrator V2
(protocol 2) the stable, V2-only server. The gateway's V2 path already matches
its wire; the pin moved to v0.1.0 and the one new event type
(`thread.pull-request-link-updated`) is accepted. What a V2 server cannot do
through this gateway, as before: **revert and the VCS diff are not available
for T3 sessions** (`features.revert` is false on protocol 2), and V1 threads
the server migrated show their message text only. Update every machine that
runs `t3 serve` (upstream pingdotgg/t3code#14871): a 0.0.x server keeps
protocol 1 and everything documented for it.

## App compatibility

- **An older App with this gateway.** Unchanged, except on a machine with no
  Herdr 0.9.0+ session at all, where `agent_collaboration` now correctly goes
  missing. That App then explains that the Gateway needs updating, which is the
  wrong upgrade to name but a better outcome than a task that silently goes
  nowhere. Apps that read the new per-session list name the right one.
- **A newer App with an older gateway.** Unchanged. `backends[].capabilities` is
  absent, and the App falls back to the top-level list plus the backend `kind`
  and `version` that `/health` has always carried.
- **Codex inbox.** Apps that predate it ignore Codex's new `inbox: true` and
  keep steering follow-ups as before. Newer Apps on an older Gateway see
  `inbox: false` and keep the steer-only path. Showing queued Codex prompts,
  and thumbnails on any queued prompt, needs an App that reads the inbox for
  Codex and the new item fields.
- **Transcripts, against 0.14.1-preview.4 or preview.5.** An App that asked
  for a terminal agent's transcript sees `agent_transcripts_v1` absent and
  keeps showing the terminal; pane agent entries no longer carry
  `agent_session`. Nothing a stable release served was removed.
- Apart from that, no capability, field, or endpoint was removed or renamed.

## Upgrade

Back up the Gateway store before upgrading, then restart the gateway. Startup
applies the pending store migrations, including session routing, fleet transport
pins and durable transport replay admission. An older binary refuses a database
whose migration version it cannot read; rollback requires a matching database
backup, not just replacing the executable. Do not delete the store to downgrade.

Devices paired before transport encryption was enabled must pair again when
`transport_encryption` is `required`. Update Apps and Gateway peers and verify
transport revision 2 before claiming its replay protections for a connection;
legacy revision 1 remains supported during migration.

Agents started from the phone or by an MCP caller run without their approval
prompts by default. Preserve prompts with `agent_launch_args` and
`agent_trust_folders` as described above.

## Validation

The October 18 dependency and event-stream candidate passed local Linux checks:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --offline --all-targets -- -D warnings`
- `cargo test --locked`: 1,810 passed, zero failed; 41 optional environment-specific or expensive tests ignored.
- `cargo build --release --locked --offline`
- `cargo audit`: 341 dependencies scanned, no advisories reported.

The new local TCP regression covers a stalled event-stream handshake, successful
reconnection and an established stream surviving beyond the handshake deadline.
These results do not establish macOS or paired-agent acceptance.

**Not verified here:** a real paired App-to-Gateway-to-agent delivery check.
Capability announcement was exercised against a live tmux session over HTTP; no
Herdr was installed on the machine the change was written on, so the Herdr side
of the switch is covered by unit tests and not by a live run.


## Home stream recovery and conversation WebSocket reads

- Add capability-gated conversation snapshot, delta and earlier-page requests to the authenticated App WebSocket, reusing the HTTP projection and filtering logic. Peer collectors cannot read private conversations.
- Replay an individual source overview after a revision gap without restarting task subscriptions.
- Isolate failed session providers and workspaces in opted-in overview subscriptions; keep healthy sections available. Older Apps keep conservative fallback behavior.
- Bound and cancel in-flight work on disconnect, drain replaced Home workers, and preserve the existing encrypted transport and authorization checks.

Restart the Gateway and update the App to enable these optional capabilities. Existing HTTP clients remain compatible.
