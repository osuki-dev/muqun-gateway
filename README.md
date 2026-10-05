# Muqun Gateway

The program that lets [Muqun](https://muqun.dev) reach a terminal on your
own computer. It runs on your machine, talks to tmux or to Herdr, and answers
your phone directly — there is no account and no server of ours in between.

**Get the app: [muqun.dev](https://muqun.dev)**

macOS and Linux. Windows is not supported yet.

## Architecture

Two planes are implemented in this binary:

- **Terminal control plane** — sessions, workspaces, tabs, panes, output,
  scrollback and SSE, behind the `TerminalBackend` port; Herdr and tmux
  adapters implement it.
- **Agents plane** — agent sessions, models, prompts, tasks,
  permissions, approvals and event history, behind the `AgentPort`
  port; the OpenCode, DeepSeek and T3 Code adapters implement it.

`/api/discovery` also reports an **SSH plane**, but that one is the app's own
transport, not a gateway subsystem: the phone can open an SSH connection to a
host and tunnel the gateway's loopback port, so the same HTTP API answers on
the far side. The gateway implements no SSH client or server itself.

```text
 Muqun app (phone)
        │
        ├── SSH host / tunnel (app-side: the phone is the SSH client; the
        │   gateway answers the same HTTP API on its loopback port)
        │
        │   HTTPS + SSE — device token, optional AES-GCM transport envelope
        ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ muqun-gateway — one binary, one user                                     │
│                                                                          │
│  src/main.rs · src/cli/ — composition root and commands                  │
│  src/platform/server.rs — startup + middleware: locale · security        │
│    headers · known hosts · encrypted transport · compression · routing   │
│                                                                          │
│  the two planes this binary implements                                   │
│                                                                          │
│   ┌────────────────────────────┐  ┌────────────────────────────────┐     │
│   │ Terminal plane             │  │ Agents plane                   │     │
│   │ sessions · workspaces ·    │  │ agent sessions · models ·      │     │
│   │ tabs · panes · output ·    │  │ prompts · tasks · permissions ·│     │
│   │ SSE · scrollback           │  │ approvals · event history      │     │
│   │ port: TerminalBackend      │  │ port: AgentPort                │     │
│   └─────────────┬──────────────┘  └───────────────┬────────────────┘     │
│                 │                                 │                      │
│        Herdr adapter · tmux adapter   OpenCode · DeepSeek · T3 Code      │
│                                       adapters                           │
│                                                                          │
│  routes: platform (health, discovery, openapi) · connectivity (pairing,  │
│    push registration) · terminal · agents                                │
│  shared: config · store (identity, devices, secrets) · http (envelope,   │
│    auth, validation) · metadata · uploads · assets · i18n (one catalog   │
│    per language) · git · parts · discovery · openapi · manage ·          │
│    lifecycle · service · state_lock                                      │
└──────────────────────────────────────────────────────────────────────────┘
        │
        ▼
 Herdr unix socket · tmux argv · OpenCode / DeepSeek HTTP · T3 WebSocket
```

A request travels: the phone authenticates with its device token (and, when
paired with transport encryption, the body arrives sealed); `platform::server`
resolves the locale, checks the host, decrypts and routes; the route's plane
calls its port; the adapter talks to the terminal or agent; the answer
returns through the compatibility mapper in the versioned content envelope,
with SSE events sealed one by one.

## Install

One command. It checks the machine, installs the gateway, and on a first
install also configures it, starts it, and shows you the pairing QR:

```sh
curl -fsSL https://muqun.dev/gateway.sh | sh
```

It downloads a prebuilt, statically linked binary for your platform, so no Rust
toolchain is needed.

tmux is all it needs — Herdr is optional. The installer configures whichever
backends are actually present: tmux, Herdr, or both. Herdr-only installs also use
the standalone gateway so it can start independently at login/boot.
Re-running it is safe: it never duplicates a backend, never loses a paired
device, and never flips a default an earlier install chose.

The installer downloads the [agent command catalog](https://agent-commands.muqun.dev/)
once for terminal slash-command suggestions. It does not run a refresh service.
To refresh the local copy later, run:

```sh
muqun-gateway commands update
```

If the catalog site is unavailable during installation, command suggestions
remain empty until the command above succeeds. OpenCode's live command API
remains the source for native OpenCode sessions.

With Herdr, version 0.7.5 or newer is required.

## Pair your phone

Open the manager on your computer:

```sh
muqun-gateway manage
```

Open **Devices** (`3`), then press Enter (first device) or `p` (new device)
to see the QR code. Scan it in Muqun, then type back the short code your
computer displays. That is the whole of pairing.

No camera on the phone? Type the gateway's address into the app instead, and
finish with the same short code.

The manager has four views. `1`–`4`, Tab/Shift-Tab, or Left/Right switches
views; Up/Down or `j`/`k` selects a row and Enter performs the visible primary
action. PageUp/PageDown scrolls. Esc goes back from a view or cancels a dialog;
at Overview, Esc closes the manager. `q` closes from any main view without
stopping Gateway. Confirmations default to **cancel**: arrows/Tab choose,
Enter accepts the selected answer. The footer shows full-word, view-specific
actions. Long lists keep the selected item visible after a resize.

| view | keys |
| --- | --- |
| Overview (`1`) | `s` start, `t` stop, `r` restart the gateway |
| Terminals (`2`) | `h` / `m` add Herdr / tmux; `f` make the selected backend default; `d` remove it; `b` toggle its backend autostart |
| Devices (`3`) | `p` show pairing QR; `x` revoke the selected device after confirmation; `r` return to the device list |
| Settings (`4`) | `u` edit URL; `a` detect URL; `e` toggle transport encryption; `g` enable/disable gateway autostart after confirmation |

Enter starts/stops Gateway in Overview, makes the selected terminal backend
the default in Terminals, pairs the first device or reviews revocation in
Devices, and changes the selected setting in Settings. The `>` selection marker
is separate from the explicit `[default]` backend label. Empty lists explain
the next step. Gateway stop, backend removal, and device revocation explain
their scope before you confirm; none closes terminal tasks.

Runtime/connectivity/backend panels use the terminal's default background,
with bold/reversed active tabs and rows, restrained status accents, and a
bounded content width. Compact terminals use a simpler layout; CJK and combining
characters are clipped/wrapped by display cells. Color is never the only status cue.

On a small terminal the QR is replaced with address pairing instructions;
resize to show the complete QR. No clipped QR is presented as usable.

Backend and address changes take effect when the gateway restarts, and never
close your terminal sessions.

## Run it

```sh
muqun-gateway start     # start in the background, or through the installed user service
muqun-gateway status    # whether it is running, and where it listens
muqun-gateway stop      # stop it
muqun-gateway restart   # restart through the same owner
```

Without a user service, `start` keeps running after you close the terminal, but **not** after the
machine restarts. To have it come back on its own:

```sh
muqun-gateway service install     # start at login, and restart if it stops
muqun-gateway service status      # whether an init system is managing it
muqun-gateway service uninstall   # stop doing that; pairings are untouched
```

The installer offers this and will not do it unless you say yes.

It registers the gateway with **your own user account** — a LaunchAgent in
`~/Library/LaunchAgents` on macOS, a systemd user unit in
`~/.config/systemd/user` on Linux. There is no administrator password, nothing
is written outside your home directory, and nothing runs as root. That is not
caution for its own sake: the gateway's job is to drive *your* tmux server, and
a root daemon cannot see that socket at all.

On Linux it also runs `loginctl enable-linger`, without which the service is
torn down when you log out and the phone can only reach the machine while
somebody is signed in. If your host refuses it, the install still succeeds and
says so.

With a service installed, `start`, `stop`, and `restart` use systemd/launchd
instead of signalling its process behind the supervisor's back. `stop` leaves
autostart registered, but does not immediately respawn the gateway; it can start
again at the next login/boot. `service uninstall` removes that registration and
keeps a running gateway running as an ordinary background process. If it was
already stopped, it stays stopped.

On Linux, lifecycle actions reload and inspect the **effective** systemd unit,
including drop-ins. They require process-only shutdown (`KillMode=process` or
`none`) and matching executable, config, and state ownership. Inspection uses
`systemctl` and systemd's `busctl`; if inspection fails, or an environment file
or custom stop hook makes safety ambiguous, no stop/restart/removal is attempted.
Older units without explicit directory pins also have their inherited user-manager
environment checked. Repair an incompatible unit/drop-in before retrying.

Lifecycle commands serialize per state directory, wait for a local authenticated
management response on startup, and use the state lock's live owner rather than
trusting a stale PID file or killing anything found on a port. Failure is reported
without starting a competing replacement. Terminal servers and their tasks are
not stopped. Custom systemd gateway units are controlled only as the user, and
must exec the gateway as their MainPID and use `KillMode=process` (or `none`);
system-wide units require the operator's
own systemctl command.

### Retired Herdr plugin

Gateway is distributed only as the standalone binary/installer. The old
`herdr.gateway` plugin manifest and plugin build/fetch script are no longer
shipped. **Herdr remains a supported terminal backend** (0.7.5+); legacy App
metadata and pairing formats are unchanged.

For a prior plugin install, stop the legacy Gateway process and disable/remove
the Gateway plugin in Herdr, not Herdr itself. Then run:

```sh
muqun-gateway import-herdr-plugin
muqun-gateway start
```

The installer retains its guarded `import-herdr-plugin --if-present` migration.
Import preserves the server identity and paired-device records, merges compatible
configuration, and retains source files and backups. It refuses to import while
a gateway owns the target state or the old gateway is running. Do not re-pair or
delete plugin state to perform the migration.

Old `HERDR_PLUGIN_CONFIG_DIR` / `HERDR_PLUGIN_STATE_DIR` environments remain
supported until the import marker redirects ownership to standalone storage.
Explicit `MUQUN_GATEWAY_CONFIG_DIR` / `MUQUN_GATEWAY_STATE_DIR` overrides take
precedence, and newly installed units pin the resolved directories along with
HOME, PATH, and LC_CTYPE. Lifecycle commands refuse to control a same-label unit
that points to another installation.

### Agents at a glance

```sh
muqun-gateway agent                  # each agent's status and the next step (--json for the raw list)
muqun-gateway agent setup t3         # store a T3 Code credential and enable T3
muqun-gateway agent setup deepseek   # point the gateway at a running DeepSeek Harness
```

`agent` probes the agents from your shell the way the running gateway does,
prints one line per agent (`id  status  version  endpoint  → next step`) and
whether the gateway is running, and exits non-zero while an enabled agent is not
usable. It reports reachability only: it never attaches, so it shows `reachable`
where the gateway may be `connected`. A T3 bearer the server refuses (expired or
revoked) shows as `unconfigured`. `setup t3` checks that a T3 server answers
(`--url`, else `t3.url`, else `http://127.0.0.1:3773`), mints a bearer with
`t3 auth session issue` when `t3` is on `PATH` (`--base-dir` is passed through) or
exchanges a `--token <code>` from `t3 pair` (`--token` wins over the local `t3`,
and `--base-dir` is ignored with it; a non-loopback `--url` always needs
`--token`), stores it in `t3-credential.json`,
and sets `t3.enabled`/`t3.url` in `config.json`. It prints how long the bearer
lasts: what T3 granted for a pairing code, or 30 days (unless T3 caps it) for
an issued one. The gateway cannot renew it; re-run `agent setup t3` when it
expires (`agent` then shows T3 as `unconfigured`). `setup deepseek` probes
`--endpoint`, `deepseek.endpoint`, `DEEPSEEK_HARNESS_URL`/`DSH_URL`, then ports
3080 and 19387 (with `deepseek.token`/`secret`, else the `DEEPSEEK_HARNESS_*` /
`DSH_*` token and secret from the environment), and sets
`deepseek.enabled`/`deepseek.endpoint`. An explicit endpoint still takes the
token and secret from the environment when config.json has none. Both offer to restart the gateway's
user service when the change needs one (`--yes` skips the question). `agent setup
opencode` only shows the OpenCode binary the gateway would use: OpenCode needs
no setup.

### The OpenCode agent

The gateway's agent features require OpenCode 2 with `GET /api/info`. While the
gateway runs, it adopts a registered service or runs `opencode service start`.
OpenCode starts the background server and loads
the environment saved by `opencode service set env`; the gateway does not copy
credentials or manage a second service.

**It runs `opencode` as your `PATH` resolves it**, or the file you name:

```json
{ "opencode": { "autostart": true, "binary": "/absolute/path/to/opencode" } }
```

in `config.json`. Set `autostart` to `false` to have it only ever adopt a
service you start yourself. The gateway does not go looking in an install
directory of its own — where OpenCode lives differs per OS and per install, and
guessing would run a different binary than your shell does.

That is worth knowing because **the two ways of running the gateway do not
share a `PATH`**. `muqun-gateway start` inherits the shell you typed it in,
version managers and all; `service install` runs under your init system with
its own environment. The same machine can resolve `opencode` to two different
files depending on which you used. So the gateway logs the file it resolved,
and its version, every time it starts or adopts one:

```
INFO no OpenCode service found, starting one binary=/home/you/.opencode/bin/opencode version="opencode v2.0.14"
INFO adopted the running OpenCode service url=http://127.0.0.1:49374 version="2.0.14" binary=/home/you/.opencode/bin/opencode
```

If what it finds is older than 2.0 it refuses it — started or adopted — with
one line saying which file, which version, and that `opencode.binary` is how to
point it elsewhere. OpenCode 1 is a different API, and half-working with it is
worse than saying so. `GET /api/agent-status` reports the same facts to the
app, including whether the agent was `adopted` or `spawned`.

### T3 Code

The gateway can also drive a [T3 Code](https://t3.codes) server (`t3 serve`),
which runs Claude Code, Codex and other agents behind one API. It is off
until you turn it on in `config.json`, and the gateway only ever tries the URL
you give it (T3's default `http://127.0.0.1:3773` if you give none):

```json
{ "t3": { "enabled": true, "url": "http://127.0.0.1:3773", "pairing_token": "…" } }
```

`muqun-gateway agent setup t3` does all of this for you. By hand: get the
pairing token by running `t3 pair` on the T3 host; it prints a pairing
link ending in `#token=…` and the token itself. The gateway exchanges the token
once for a long-lived credential, keeps that in `t3-credential.json` in its
state directory (readable by you only), and never needs the token again.
`t3.runtime_mode` sets what new threads may do without asking
(`full-access` by default, or `approval-required`, `auto-accept-edits`,
`auto`). The full reference is in `docs/agent-api.md` under "Configuring T3
Code".

If the configured listen IP changes or is unavailable, startup reports the
address and asks you to update `listen` in the gateway's `config.json`, then
restart. It never switches addresses automatically. If the pairing URL contains
the old IP too, update that URL through `muqun-gateway manage`.

### Optional terminal startup

The installer separately asks whether configured terminal backends should start
with the gateway. Only an explicit **yes** enables this; Enter, no terminal, and
existing configurations retain their settings (off for a new install).

```sh
muqun-gateway backend list
muqun-gateway backend autostart tmux on
muqun-gateway backend autostart herdr on
muqun-gateway backend autostart herdr off
```

Use the IDs from `backend list`. On each gateway start, opted-in backends get
one background startup attempt. Running servers are reused. A stopped tmux
backend gets a detached `muqun` shell session; Herdr starts headless using its
configured default or named session. Custom Herdr socket paths whose session
store cannot be identified must still be started manually. Herdr may restore
its own persisted workspace state; Muqun does not submit agent prompts or answer
trust dialogs.

A startup failure is logged once and does not block the gateway or create a
restart loop. Start the backend manually, or restart the gateway to retry.
Disabling the option affects future starts, not existing terminal processes.
Reinstall the service after upgrading to apply the child-process lifetime rules:
gateway restarts must not kill persistent terminal servers. On macOS this is
login startup; on Linux boot startup requires the user service and lingering.

### Modifier chords on tmux

Keys such as `shift+enter` (newline without sending), `ctrl+enter`, `ctrl+up`
or `alt+x` reach a tmux pane only when tmux 3.2+ has extended keys on. The
gateway reads this setting and never changes it; add it to `~/.tmux.conf`:

```sh
set -s extended-keys on
```

Even then, tmux encodes a chord only for a program that asked for it
(modifyOtherKeys; tmux does not act on the kitty keyboard request). Where the
program has not, and the chord would arrive as a different key -- `ctrl+enter`
as a plain Enter, which submits -- `send-keys` answers `400 key_unsupported`
instead of sending it. Discovery reports what each backend can deliver as
`planes.terminal.backends[].keyboard`, and a pane's shortcuts response
carries `keyboard.extended` -- whether chords reach that pane right now -- so
the app can dim a chord before it is pressed rather than after. The agents'
newline keys avoid the problem altogether: Claude Code gets backslash-Enter,
Codex `ctrl+j`, and opencode, which asks for extended keys, `shift+enter`.

Herdr delivers the chords natively. Its key names stop short of Home, End,
PageUp, PageDown, Insert and Delete, so the gateway types those as the bytes a
keyboard sends (`ESC [1~`, `ESC [1;5H` for `ctrl+home`, ...).

### Recording pane reads

Set `MUQUN_SCROLLBACK_TRACE_DIR` to a directory and the gateway appends every
pane read it serves or samples -- buffered or passed through -- to
`<dir>/<session>_<pane>.jsonl`, one JSON line per read: time, source, format,
whether the gateway was keeping its own history for the pane, whether the pane
owns its screen, and the rows. It is off by default and is for replaying a real
pane through the scrollback model in a test; it writes terminal contents to
disk, so switch it on for a session (a systemd drop-in with
`Environment=MUQUN_SCROLLBACK_TRACE_DIR=…`) and off again.

## Reaching it from outside your network

Put both devices on [Tailscale](https://tailscale.com) and point the gateway at
your tailnet address. That keeps the gateway off the public internet and needs
no port forwarding. Use Tailscale Serve, not Funnel.

## Update

Re-run the install command. It replaces the binary in place, and your paired
phones stay paired.

## License

MIT. See [LICENSE](LICENSE).
