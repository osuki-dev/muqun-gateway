# Muqun Gateway

The program that lets [Muqun](https://muqun.dev) reach a terminal on your
own computer. It runs on your machine, talks to tmux or to Herdr, and answers
your phone directly — there is no account and no server of ours in between.

**Get the app: [muqun.dev](https://muqun.dev)**

macOS and Linux. Windows is not supported yet.

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

It shows a QR code. Scan it in Muqun, then type back the short code your
computer displays. That is the whole of pairing.

No camera on the phone? Type the gateway's address into the app instead, and
finish with the same short code.

Keys in the manager:

| key | what it does |
| --- | --- |
| `p` | show the pairing QR again, to add another device |
| `x` | revoke a paired device — its token stops working immediately |
| `u` | change the address the app connects to |
| `a` | detect that address again |
| `h` / `m` | add a Herdr or tmux backend |
| `f` | choose the default session |
| `d` | remove a backend |

Backend and address changes take effect when the gateway restarts, and never
close your terminal sessions.

## Run it

```sh
muqun-gateway start     # start in the background
muqun-gateway status    # whether it is running, and where it listens
muqun-gateway stop      # stop it
```

`start` keeps running after you close the terminal, but **not** after the
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

With a service installed, `muqun-gateway stop` stops the process but the service
starts it again — that is what it is for. `service uninstall` is how you stop it
for good.

### The agent engine

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
worse than saying so. `GET /api/agent-engine` reports the same facts to the
app, including whether the engine was `adopted` or `spawned`.

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

## Reaching it from outside your network

Put both devices on [Tailscale](https://tailscale.com) and point the gateway at
your tailnet address. That keeps the gateway off the public internet and needs
no port forwarding. Use Tailscale Serve, not Funnel.

## Update

Re-run the install command. It replaces the binary in place, and your paired
phones stay paired.

## License

MIT. See [LICENSE](LICENSE).
