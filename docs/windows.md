# Native Windows support

Status, checked 2026-10-31: **not a release target.** `install.sh` refuses
Windows, `release.yml` and `preview.yml` build no Windows asset, and the README
says Windows is not supported. This branch makes the gateway compile on
Windows and keeps it compiling in CI. This document lists what still blocks a
usable Windows gateway and the order to fix it in.

Target: Windows 10 22H2 and Windows 11, x64. The floor comes from Rust
(`*-pc-windows-msvc` is Tier 1 on Windows 10 or newer) and Tailscale. arm64
can come later, on a best-effort basis.

## Why this is mostly an agent-only project

Most agents the gateway drives no longer need a terminal backend. They run as
native processes or local services: the Codex app-server, the OpenCode HTTP
service, the T3 server, the DeepSeek harness, and ACP agents over stdio. tmux
does not exist on Windows. Herdr has a Windows build, but the gateway's Herdr
transport is Unix-socket only. So the first useful Windows gateway is
**agent-only**: no terminal panes, and the agent features of the App
otherwise unchanged.

## Upstream availability on Windows

Checked on 2026-10-31 against each project's release assets and docs.

| Agent / tool | Native Windows today? | How the gateway would reach it | Source |
|---|---|---|---|
| OpenCode 2.2.5 | **Yes**: x64, x64-baseline, arm64 zips; npm `@opencode/cli` has `cli-windows-x64`/`-arm64` | `opencode service start` serves localhost HTTP (default port 28894 on Windows) and writes `%USERPROFILE%\.local\state\opencode\service.json`, the path `discovery.rs` already reads. **Verified in a local VM** (see below). | [releases v2.2.5](https://github.com/anomalyco/opencode/releases/tag/v2.2.5) |
| Codex CLI 0.166 | **Yes**: `codex-x86_64-pc-windows-msvc.exe`, arm64, npm `@openai/codex` | `app-server` over stdio works anywhere. `--listen unix://` also works on Windows (AF_UNIX via `uds_windows`, both sides unelevated), so adopting the shared daemon could be ported later. | [releases rust-v0.166.0](https://github.com/openai/codex/releases/tag/rust-v0.166.0) |
| T3 Code 0.1.6 | **Yes**: `t3-0.1.6-win32-{x64,arm64}.zip`, desktop exe | Local HTTP/WebSocket server. What `t3 service install` does on Windows is unverified. | [releases v0.1.6](https://github.com/pingdotgg/t3code/releases/tag/v0.1.6) |
| DeepSeek harness `@deepseek-ai/dsh` 0.2.1-rc.5 | **Yes**, through Node (no native binary) | ACP over stdio. Its shell tool calls `pwsh`, so it probably needs PowerShell 7 (unverified). | [npm](https://www.npmjs.com/package/@deepseek-ai/dsh) |
| Qoder CLI 1.1.81 | **Yes**, x64 only (arm64 "temporarily not supported") | `qodercli --acp` over stdio. npm installs a `.cmd` shim. | [docs](https://docs.qoder.com/cli/acp) |
| Pi 1.4.0 | **Yes**: `pi-windows-{x64,arm64}.zip` | `--mode rpc` over stdio. **Its bash tool needs Git Bash.** | [Windows docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/windows.md) |
| Claude Code through `claude-agent-acp` 0.99.1 | **Yes**: Claude Code is native (Windows 10 1809+, x64/arm64); the adapter is a Node script (Node ≥ 22) | ACP over stdio, spawned through `node` or npm's `.cmd` shim | [npm](https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp), [setup](https://code.claude.com/docs/en/setup) |
| Antigravity `agy` 1.4.12 | Binary **yes** (`cli_windows_x64.exe`, arm64). Whether `agy acp` works there is **unverified**. | ACP over stdio | [install.ps1](https://antigravity.google/cli/install.ps1) |
| fx (Vercel Labs) 0.0.16 | **No.** macOS and Linux only; Windows needs WSL. | n/a | [README](https://github.com/vercel-labs/fx) |
| Herdr 0.9.5 | **Yes**, x64 only (`herdr-windows-x86_64.zip`). The docs page is still `/windows-beta/` but says "generally available". | IPC over a **named pipe** (`\\.\pipe\` followed by the socket path), not a Unix socket. Not supported there: `terminal attach`, live handoff, foreground process groups. Plugins are a preview. Elevated servers reject unelevated clients. | [docs](https://herdr.dev/docs/windows-beta/), [release](https://github.com/herdrdev/herdr/releases/tag/v0.9.5) |
| tmux | **No** | n/a | |
| Tailscale | **Yes** | `tailscale serve` of a port needs no elevation; serving a path or socket needs an elevated caller | [CLI docs](https://tailscale.com/kb/1080/cli) |

## Current state of this repository

- **Compiles on Windows: yes, as of this branch.** On `main`, the first
  `windows-latest` run failed: clippy found **47 errors in the gateway binary
  and 113 in the test target** (117 distinct messages). By category:
  `std::os::unix`, `std::os::fd` or Unix sockets used without a `cfg` (43);
  callers of items already gated to Unix, such as the Codex daemon test
  helper, `update::apply`, `acquire_strict` and Herdr's RPC slots (20); `libc`
  signals, `kill`, `openat` and `O_*` flags (21); Unix-only extension methods
  such as `from_mode`, `as_bytes` and `custom_flags` (17); unused-variable and
  unused-mut lints that only fire off Unix (16). The **CI evidence** section
  below has the results after this
  branch.
- 59 source files carry `cfg(unix)` or `cfg(target_os)`; none carried
  `cfg(windows)` before this branch.
- What this branch changes, with Unix behavior unchanged:
  - `.github/workflows/windows.yml` runs fmt, then builds and uploads a
    release `.exe` as a run artifact (not a release), then runs clippy and
    tests.
  - `.gitattributes` keeps LF endings, because fixtures are compared byte for
    byte.
  - `state_lock` takes a real `LockFileEx` lock on Windows (`File::try_lock`),
    with the same retry budget as `flock`. `acquire_strict` is ported too.
  - `validate_relative_path` refuses `\` and `:` on Windows (`..\x`, `C:x`).
    Paths sent to the App and to git are always joined with `/`
    (`git::slash_path`).
  - Untracked-file line counts open reparse points rather than following
    them. Discard refuses a symlinked parent before removing; this is weaker
    than the Unix `openat`/`unlinkat` walk, and the code says so.
  - Windows stubs: self-update, the Herdr socket transport, POSIX signals and
    launchd liveness return "not supported" or `false` instead of failing to
    compile.
  - Tests that need symlinks, Unix sockets or `#!` scripts are `cfg(unix)`.

## Blockers by area

Effort: **S** is under a day, **M** is a few days, **L** is a week or more.
Phase numbers refer to the plan below.

| Area | What blocks Windows today | Where | Effort | Phase |
|---|---|---|---|---|
| Compile + CI | Fixed on this branch: Unix-only APIs used without a `cfg` | see above | S (done) | 0 |
| Binary lookup | `find_on_path` and `resolve_binary` join the bare name and never try `PATHEXT` (`.exe`, `.cmd`), so `opencode`, `codex`, `qodercli` and `claude-agent-acp` are not found on `PATH` (the VM run logs the adopted OpenCode service with `binary=unknown`). `login_env::lookup` splits `PATH` on `:`, which breaks on `C:\`. | `src/agents/tasks.rs` `find_on_path`; `src/agents/runtime.rs` `resolve_binary_in`; `src/terminal/login_env.rs` `lookup` | S | 1 |
| Spawning npm shims | npm agents install as `.cmd` shims, which `std::process::Command` runs through `cmd.exe` with its own quoting rules. Prefer resolving to `node <script>` or the native `.exe` inside the package. | ACP launcher `src/agents/adapters/acp/client.rs`; Pi `src/agents/adapters/pi/rpc.rs` | S–M | 1 |
| Child process control | ACP, Pi, OpenCode shell-kill and the Codex private app-server use process groups (`process_group(0)`, `killpg`), `SIGSTOP` and `SIGKILL`, `/proc` and `proc_pidinfo`. On Windows they degrade to killing only the direct child, so a turn's grandchildren can outlive a stop. Needs Job Objects (`windows-sys`): one job per agent, closed on stop. | `src/agents/adapters/acp/client.rs`, `src/agents/adapters/codex/process_tree.rs`, `src/agents/adapters/opencode/shell_kill.rs`, `src/agents/runtime/deepseek.rs`, `src/tasks/pane_agent.rs` | M | 1 |
| Process identity | `/proc/<pid>/stat`, `proc_pidinfo`, `lsof`/`ss` port-to-pid and boot-id checks prove a pid is the process it claims to be. Windows needs `OpenProcess` + `GetProcessTimes` (start time as the incarnation) or `GetExtendedTcpTable`. | `src/terminal/backend/process_identity.rs`, `src/platform/lifecycle.rs`, `src/platform/launchd.rs` `process_alive`, `src/terminal/supervision.rs` | M | 1–2 |
| Codex shared daemon | Adoption is `cfg(unix)` (`/tmp/codex-daemon-<uid>`, peer uid checks). On Windows the gateway falls back to a private stdio app-server. Upstream supports AF_UNIX on Windows, so adoption is portable later; the socket path and DACL checks differ. | `src/agents/adapters/codex/daemon.rs`, `src/agents/runtime/codex.rs` | M | later |
| OpenCode service | Works natively (verified below). **`opencode service start` launched from an SSH session dies when the session ends** (OpenSSH's job object), so the gateway has to start it from a process that outlives the login, or the user starts it from the desktop. | `src/agents/adapters/opencode/discovery.rs` | S | 1 |
| tmux backend | Does not exist on Windows. Leave it compiled out at runtime, and say so through capabilities. | `src/terminal/backend/tmux.rs`, `src/terminal/factory.rs` | S | 1 |
| Herdr backend | Herdr runs on Windows, but the gateway speaks to it over `UnixStream` and uses `peer_cred` for the peer pid. Windows needs a named-pipe transport (`tokio::net::windows::named_pipe`) and `GetNamedPipeServerProcessId`. Pane tty and size probes (`tty` module) also need a port. | `src/terminal/backend/herdr.rs`, `src/terminal/backend_startup.rs` | M–L | 1b (optional) |
| Agent-only mode | **Seen in the VM:** a fresh `setup` defaults to tmux and exits with "tmux backend selected, but no `tmux` was found", so there is no way to set up without a terminal backend. `setup --backend herdr` works around it: the gateway starts and serves agents, with the Herdr session marked unavailable. `setup` needs an agent-only choice (the default on Windows). `/health` and `/api/meta` with no terminal session must report zero sessions and say "agent-only" rather than `backend_unavailable`, and terminal-only capabilities must be trimmed. Needs a matching App change. | `src/platform/server.rs`, `src/platform/setup.rs` | M | 1 |
| State / config dirs | Already portable: `dirs::config_dir()` and `dirs::data_dir()` resolve to `%APPDATA%` (Roaming). Consider `%LOCALAPPDATA%` for state (the SQLCipher store, uploads, image caches), which should not roam. The DB key file relies on 0600 permissions, so Windows needs a user-only DACL or DPAPI. | `src/platform/store.rs`, `src/platform/store/db_key.rs` | S–M | 1 |
| Service install / supervision | `service install` writes a LaunchAgent or a systemd user unit and drives `launchctl`/`systemctl`. Windows: **Task Scheduler "at logon, run as the user, limited"** (`schtasks /SC ONLOGON /RL LIMITED`, or the COM API) is the closest match: no admin, and it runs in the user's session, so agents see the user's profile and credentials. A Windows Service would run as another account and needs admin. A tray app is nicer but is a separate product. | `src/platform/service.rs`, `src/platform/lifecycle.rs`, `src/cli/mod.rs` | M | 2 |
| Detach / `start` / `stop` | `start` detaches with `setsid`; `stop` signals the pid. Windows: `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP \| CREATE_NO_WINDOW`, and stop through a local control request or `TerminateProcess`. | `src/platform/lifecycle.rs` | S–M | 2 |
| Installer | `install.sh` exits on anything but Darwin and Linux. Needs an `install.ps1` twin (`irm …/gateway.ps1 \| iex`) that downloads the zip, checks SHA256, installs to `%LOCALAPPDATA%\Programs\muqun-gateway`, adds it to the user `PATH`, and runs `setup`/`pair`. | `install.sh`, website | M | 2 |
| Updater | `update` and `bootstrap-preview` replace the running binary in place and restart through launchd/systemd. **Windows cannot overwrite a running `.exe`**, but it can rename it. Rename `muqun-gateway.exe` to `.old`, move the new one in, restart through the scheduled task, and delete `.old` on the next start. Stubbed to "not supported" on this branch. | `src/platform/update.rs` (`apply_install`, `validate_destination`, `update_lock`), `target()` has no Windows entry | M | 2 |
| Release / preview assets | `release.yml` and `preview.yml` build four targets. Add `x86_64-pc-windows-msvc` as a `.zip` with a `.sha256`, and the `TARGETS` entry in `update.rs`. **Not done here on purpose.** | `.github/workflows/release.yml`, `.github/workflows/preview.yml` | S | 3 |
| `manage` TUI | crossterm works on Windows Terminal and conhost. The TUI's width locale (`WidthLocale`, `libc::locale_t`) is `cfg(unix)`. The update and service actions inherit the blockers above. The TUI needs no pty. | `src/platform/manage.rs` | S | 2 |
| Login-shell environment | `login_env::adopt` probes a POSIX login shell for `PATH` and locale. On Windows it prints "environment repaired from the login shell -- LC_CTYPE=C.UTF-8 … tmux" on every command (seen in the VM). Skip it on Windows: the scheduled task inherits the user environment. | `src/terminal/login_env.rs` | S | 1 |
| Paths in the API | Relative paths are validated and spelled with `/` (this branch). Still to do: absolute workspace roots and cwd slugs that contain `C:\…` (asset roots, `cwd` display, task repo roots) need a pass, plus a test on a drive-letter path. Case-insensitive path comparison for the "inside the workspace" fences. | `src/platform/assets.rs`, `src/platform/git.rs` `stays_inside`, `src/terminal/composer.rs` | M | 1 |
| Symlink fences | Several fences rely on `O_NOFOLLOW`, `openat` or a dev/ino identity. On Windows, open with `FILE_FLAG_OPEN_REPARSE_POINT` and compare the file index (`GetFileInformationByHandle`). Partly done here; `message_images` and uploads still use path checks only. | `src/agents/message_images/originals.rs`, `src/platform/uploads.rs` | M | 1 |
| Console / encoding | Logs are UTF-8; Windows PowerShell 5 decodes child output in the OEM code page (seen in the VM: localized errors arrive as `?`). The gateway never parses console text, so this is cosmetic. Set the console to UTF-8 in the installer, or log to a file. | — | S | 2 |
| Codesigning / SmartScreen | Not required to run. An unsigned `.exe` downloaded in a browser gets the SmartScreen "unknown publisher" prompt. `irm \| iex` downloads carry no Mark of the Web, so no prompt. Signing (Azure Trusted Signing) is optional polish. | — | S–M | 3 |
| LAN pairing / firewall | Binding `0.0.0.0:23847` makes Windows Defender Firewall show an "allow access" prompt on first start (it needs admin to allow, or a user can decline). Tailscale `serve` of a port needs no elevation. The installer should explain this, or add a rule once with consent. | `src/platform/server.rs` | S | 2 |

## Plan

### Phase 0: CI builds (this branch), S

The Windows job compiles, lints and tests on every PR, and uploads an unsigned
`muqun-gateway.exe` as a run artifact.

**What a user gets:** nothing yet. Contributors get a gateway that stays
buildable on Windows.

### Phase 1: agent-only gateway, manual start, M to L (about 1–2 weeks)

- PATHEXT-aware binary lookup and `.cmd`/`node` spawning for npm agents.
- Job Objects for agent process trees (stop kills the whole turn).
- Windows process identity (`OpenProcess` plus start time).
- An agent-only mode that reports zero terminal sessions honestly, and the
  matching App copy ("terminals need macOS or Linux").
- State in `%LOCALAPPDATA%`, the DB key protected with a user-only DACL.
- A drive-letter pass over workspace paths and fences.
- A real-machine smoke test: pair the App, run one low-effort turn each on
  OpenCode, Codex, Qoder and Claude Code (through `claude-agent-acp`).
- Optional **1b**: a named-pipe Herdr transport, so Herdr panes work on
  Windows (M to L; Herdr on Windows is x64 only, and `terminal attach` is
  unsupported there).

**What a user gets:** they download the `.exe` from a CI run, run
`muqun-gateway setup` and `muqun-gateway serve` in a terminal, pair the App,
and drive OpenCode, Codex, T3, Qoder, Pi (with Git Bash), Claude Code and
DeepSeek from the phone. No terminals, no auto-start, no self-update. fx is
not available on Windows.

### Phase 2: installer, updater, service, M (about 1 week)

- `install.ps1` (user scope, no admin).
- `service install`/`uninstall` as a logon scheduled task.
- `start`/`stop`/`restart` without signals.
- A rename-then-replace `update`.
- A first-start firewall explanation.
- The `manage` TUI on Windows.

**What a user gets:** a one-line install, a gateway that starts at login and
updates itself, and the same `manage` screen as on macOS and Linux.

### Phase 3: release and preview assets, S

- `x86_64-pc-windows-msvc` zip and checksum in `release.yml` and `preview.yml`.
- `TARGETS` in `update.rs`.
- README and website copy.
- Optional signing.

**What a user gets:** Windows on the stable and preview channels.

## CI evidence

| Run | Gateway binary | Test target |
|---|---|---|
| PR #36's pieces ported onto `main` as they were (b284ae0) | 47 errors | 113 errors |
| This branch | _see the PR_ | _see the PR_ |

## Local Windows VM (for maintainers)

A disposable-looking but **not disposable** Windows 11 VM can run on a Linux
workstation through [dockur/windows](https://github.com/dockur/windows). On
Omarchy, `omarchy-windows-vm` wraps it. Its disk image is someone's working
machine, so never recreate or reset the volume.

- Start: `docker compose -f ~/.config/windows/docker-compose.yml up -d`, then
  wait for `Windows started successfully` in `docker logs omarchy-windows`.
  The screen is at `http://127.0.0.1:8006`; RDP is on `127.0.0.1:3389`.
- `omarchy-windows-vm launch` needs that compose file. If the file is
  missing, the script prints "not configured" and starts nothing; restore the
  file rather than running `install` (install rewrites it).
- SSH (if OpenSSH Server was enabled in the guest): the guest sits behind
  QEMU's NAT inside the container. Reach it with
  `ProxyCommand docker exec -i omarchy-windows nc 172.30.0.2 22`, with your
  own key. The default shell is Windows PowerShell 5, so send multi-line
  scripts with `-EncodedCommand`.
- What works there today (2026-10-31), with no Rust toolchain in the VM:
  - OpenCode 2.2.5 via `npm i -g @opencode/cli`. `opencode service start`
    answers `GET /api/info` on `127.0.0.1:28894`, with basic auth (401
    without it), and writes `service.json` where the gateway looks. A service
    started from an SSH session stops when that session ends.
  - The gateway `.exe` from the Windows CI artifact runs on Windows 11
    (build 26200). `--help` works.
  - `setup` with no backend fails, because it defaults to tmux.
    `setup --backend herdr` writes the config and pairing identity.
  - `run` listens on `127.0.0.1:23847` and adopts the running OpenCode
    service (its event stream connects). The Herdr session is reported
    unavailable. The API answers, and requires a token (401 without one).
  - `agent list` shows OpenCode reachable, and Codex, Qoder and Pi not
    installed (none were installed in the VM).
  - Not tried yet: a paired App and a real agent turn.
- Building the gateway inside the VM needs the MSVC C++ build tools and Perl
  (for the vendored OpenSSL under SQLCipher). From the VM, `winget` hung
  updating its sources and the Build Tools bootstrapper downloaded at about
  13 KB/s, so neither got installed. The CI artifact is the quicker route.
