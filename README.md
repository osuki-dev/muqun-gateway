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
backends are actually present: tmux always, and Herdr too when it is on `PATH`.
Re-running it is safe: it never duplicates a backend, never loses a paired
device, and never flips a default an earlier install chose.

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

## Reaching it from outside your network

Put both devices on [Tailscale](https://tailscale.com) and point the gateway at
your tailnet address. That keeps the gateway off the public internet and needs
no port forwarding. Use Tailscale Serve, not Funnel.

## Update

Re-run the install command. It replaces the binary in place, and your paired
phones stay paired.

## License

MIT. See [LICENSE](LICENSE).
