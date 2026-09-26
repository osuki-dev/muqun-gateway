# Native Windows support

Status: in progress. Windows is not a release target, and `install.sh` still
refuses it. This document records what upstream supports, the scope we are
aiming for, and the order the work lands in.

## Scope

An **agent-only** gateway: the Muqun App pairs with a Windows machine and
drives OpenCode 2 through the agent engine. No tmux (there is none on Windows)
and, at first, no Herdr. Terminal features stay macOS and Linux only, and the
gateway says so through capabilities rather than failing when the App tries.

## Upstream support (checked 2026-09-26)

| | Native Windows | Notes |
|---|---|---|
| OpenCode 2 | Binaries for x64, x64-baseline and arm64 ([download](https://opencode.ai/download), npm `@opencode/cli`) | The [v2 docs](https://opencode.ai/v2/docs/) say "Windows package managers are not supported" and recommend WSL. Scoop, Chocolatey and npm `opencode-ai` still ship 1.x, which the gateway refuses (`MIN_OPENCODE_MAJOR = 2`). Install with `npm i -g @opencode/cli`. `opencode service start` and `serve --service` exist in the 2.0.18 source and register `~/.local/state/opencode/service.json` on every OS, which is where `discovery.rs` already looks. **Not yet verified on a real Windows machine.** |
| Herdr | Generally available since 0.8.0 ([docs](https://herdr.dev/docs/windows-beta/)) | x86_64 only (arm64 runs it emulated). IPC is a named pipe, not a Unix socket; plugin support is a preview. Out of scope for now. |
| Rust (`*-pc-windows-msvc`) | Tier 1 | Windows 10 / Server 2016 or newer since Rust 1.78. |
| Tailscale | Supported | Windows 10 / Server 2016 or newer. `tailscale serve` needs an elevated terminal. |

Neither OpenCode nor Herdr states a minimum Windows version, so the floor comes
from Rust and Tailscale. **Target: Windows 10 22H2 and Windows 11, x64.**
arm64 is best effort later.

## Phases

### 0. Build and CI (this branch)

- `.github/workflows/windows.yml` runs fmt, clippy and tests on
  `windows-latest` for every PR.
- Tests that need Unix APIs (symlinks, Unix sockets, `ExitStatusExt`, a shell)
  are `#[cfg(unix)]`.
- `state_lock` takes a real lock on Windows (`File::try_lock`, i.e.
  `LockFileEx`). It used to take none, so two gateways on one state directory
  could overwrite each other's paired devices.

### 1. Agent-only gateway (all platforms)

- `setup --backend none` writes a config with no terminal sessions.
- `/health` and `/api/meta` answer with zero sessions instead of
  `502 backend_unavailable`, so the App can still read capabilities.
- A new capability (for example `agent_only`) and trimming terminal-only
  capabilities when there is no session, so a new App can explain what needs
  macOS/Linux and an old App does not open a terminal that is not there.
  Coordinated with the App; verified against a real paired App.

### 2. Windows runtime

| Today (Unix) | Windows |
|---|---|
| `start` detaches with `setsid` | `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP \| CREATE_NO_WINDOW` |
| `kill -0` / `kill` | `OpenProcess` / `TerminateProcess` (`windows-sys`) or `tasklist` / `taskkill` |
| `lsof` / `ss` port-to-pid | `GetExtendedTcpTable` or `netstat -ano`, or rely on the pid file |
| LaunchAgent / systemd user unit | `schtasks /SC ONLOGON /RL LIMITED` (runs as the user, never elevated) |
| `opencode` on `PATH` | Honour `PATHEXT` (`opencode.exe`, npm's `opencode.cmd`) |
| login-shell environment probe | Skip |
| `tailscale` on `PATH` | Fall back to `C:\Program Files\Tailscale\tailscale.exe` |
| config and state dirs | `%APPDATA%\muqun-gateway`; consider `%LOCALAPPDATA%` for state |

Plus `gateway.ps1` (`irm https://muqun.dev/gateway.ps1 | iex`), a Windows
target in `release.yml`, and release notes stating the supported versions.

### 3. Optional

Herdr over its named pipe; arm64 builds.

## Open questions

- Does OpenCode 2's `service start`, `/api/info` and event stream work on
  native Windows? This is the largest risk and needs a real machine before
  phase 2.
- OpenCode's shell tool on Windows depends on Git Bash
  (`OPENCODE_GIT_BASH_PATH`); agent behaviour may differ from macOS and Linux.
- Whether the Tailscale installer puts `tailscale.exe` on `PATH`.
