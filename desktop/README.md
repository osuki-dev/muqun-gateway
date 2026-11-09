# Muqun Desktop

A native desktop client for the coding agents on this computer, written in Rust on
[Rune](https://github.com/ryuhzk/rune) (our fork; GPU-drawn UI, no webview, no Chromium). Every
agent's sessions in one window: Codex, OpenCode, Claude Code, Cursor, Qoder, Pi, FX, Antigravity,
DeepSeek and T3.

When a Muqun gateway service runs on this computer the desktop attaches to it (the token in its
`local-client.json`), so the phone and the desktop see the same sessions. Without one it runs the
gateway's agents plane in-process — no HTTP server, nothing listening.

## Install (Linux)

```sh
desktop/scripts/install-local.sh          # builds --release, installs for this user (no sudo)
desktop/scripts/uninstall-local.sh        # removes it again; settings and state stay
```

`install-local.sh` puts the binary at `~/.local/bin/muqun-desktop`, the launcher entry at
`~/.local/share/applications/muqun-desktop.desktop` and the icons under
`~/.local/share/icons/hicolor/<size>/apps/muqun-desktop.png`. It is idempotent: run it again to
update (a running Muqun keeps its old binary until it is reopened). `--no-build` installs
`target/release` as it is; `PREFIX=/some/dir` installs somewhere else (`$PREFIX/bin`,
`$PREFIX/share`). It also removes the `muqun.desktop` entry an older version of the script wrote.

The launcher entry and icons come from `desktop/packaging/linux/`. The icons are generated from
`assets/icons/app-icon.png` by `scripts/gen-linux-icons.sh` (ImageMagick) and committed, so
installing needs no tools beyond `cargo`. The app id — Wayland `app_id`, X11 class, desktop file
and icon name — is `muqun-desktop` everywhere (`src/branding.rs`).

macOS and Windows build and run (`cargo run -p muqun-desktop --release`) but have no installer or
bundle yet.

Linux needs a GPU stack wgpu can use (Vulkan, or GL through Mesa) and a session D-Bus for
notifications, file dialogs and "Reveal in folder". For the fonts the UI looks for — Noto Sans CJK
and JetBrains Mono Nerd Font (`src/platform/fonts.rs`) — missing ones fall back to the system's.

## Run

```sh
muqun-desktop                 # or Muqun from the app launcher
muqun-desktop path/to/file    # open a file in the viewer
cargo run -p muqun-desktop --release
RUST_LOG=muqun_desktop=debug cargo run -p muqun-desktop --release
```

**One instance.** A second launch hands its request to the running window — focus it (with the
launcher's activation token, which a Wayland compositor needs before it lets a window take focus)
and open the file it was given — and exits before it starts any agent. Instances are keyed by
window title, so `MUQUN_WINDOW_TITLE` builds from different worktrees still run side by side, and
Rune's tooling flags (`--scene`, `--snap`, …) always run on their own. Unix only; on Windows every
launch is its own (`src/platform/instance.rs`).

`SIGTERM` / `SIGINT` close the window cleanly and exit 0 after saving the config.

## What it does

* **Home** — start a session: agent, folder (recent projects, folders under what you type,
  Browse…, a typed path as typed), model, mode, worktree, then the first prompt. Recent and
  pinned sessions.
* **Sidebar** — every session, searchable, grouped by time, agent or folder, with agent filter
  chips; pin, rename, fork, delete, move to background; projects can be pinned, hidden and named.
* **Session** — a streaming timeline (Markdown with highlighted code, tool calls, inline diffs,
  todos, images), approvals and questions answered in place, Stop. The composer sends with Enter
  (Shift+Enter for a newline), takes attachments (pick, paste an image, drop files; images and
  videos are shrunk to what the agent accepts), `@` files and computers, `/` commands and skills,
  and switches model, effort, mode and branch. Queued prompts wait above it.
* **Command palette** — Ctrl/Cmd+K: a frosted launcher that fuzzy-matches every session (title,
  agent, folder; most recent first) and action — new session by agent and folder, split,
  inspector, every page and Settings tab, theme packs, light/dark, dictation — with ↑/↓, Enter,
  matched letters lit and your recent commands first.
* **Status icon** — Linux (StatusNotifierItem) and Windows: the tooltip and menu list the sessions
  that need you; Open Muqun, a session, or Quit. Settings → General → Keep running in the tray
  makes closing the window leave Muqun running there (off by default).
* **Dictation** — the mic button (or Ctrl/Cmd+Shift+Space) records; a waveform scrolls while
  you speak and, in realtime mode, the words fade in as the service hears them. Done
  transcribes into the draft — never sent on its own; Escape cancels. Settings → Voice: after
  recording (an OpenAI-compatible `/v1/audio/transcriptions`) or realtime (an
  OpenAI-Realtime-compatible WebSocket, PCM16 mono 24 kHz), URL, model, language, and an API
  key kept in the system keyring (Secret Service on Linux; a 0600 file when there is none).
  Building on Linux needs the ALSA headers (`libasound2-dev`) for the microphone.
* **Inspector** — the open session's queue, git and worktrees, subagents, saved permissions and
  background shells. The working-tree diff, with staged revert.
* **Terminals** — the Herdr (and tmux) workspaces of the gateway this desktop is attached to:
  every tab and pane with its agent and status, a live view of a pane's screen (colours follow
  the theme), a key row (Esc, Tab, arrows, Ctrl+C, Backspace, Enter) and a field that types into
  the pane, as the App's terminal workspace does. It never focuses, closes or rearranges a pane.
  Needs the gateway service (the in-process hub has no terminal plane).
* **Inbox** — every waiting approval and question across sessions. **Tasks** — delegated tasks,
  new ones, cancel and complete.
* **File viewer** — highlighted text and code with line numbers, rendered Markdown (source a
  click away), images fit to the window or zoomed; video, audio, PDF and anything else show their
  metadata and **Open with system app**. **Save as…** and **Reveal in folder** for every file.
* **Notifications** — an approval, a question or a finished turn in a session you are not looking
  at raises a system notification titled with the session; clicking it brings the window up on
  that session. Rules and words are the gateway's native agent pushes (one per request; a turn
  counts as finished after 30 s idle, at most once per 2 min; never for a subagent's turn).
  Settings → General → Notifications turns them off. Linux (freedesktop notifications) only for
  now; macOS and Windows build with a no-op.
* **Settings** — Agents (enable, every config field, T3 pairing, sign-in hints), Appearance
  (light / dark / system, density, reduce motion, reduce transparency), General (language
  English / 简体中文, default folder, editor for "Open in", notifications), Themes (installed,
  gallery, import a `.muqun-theme` file or link; applied live), About.
* **Themes** — the App's theme packs: colours, materials and artwork follow the active pack in
  light and dark. Six are built in (`assets/themes/README.md`).

Keys: one table, `src/ui/keymap.rs`, drives dispatch, the shortcuts sheet (Ctrl/Cmd+/, or `?`
when not typing), the palette's hints, button tooltips and Settings → Shortcuts. Ctrl/Cmd+K
opens the command palette, Ctrl/Cmd+P jumps to a session, Ctrl/Cmd+B toggles the sidebar.
Escape closes the innermost popup, then leaves a page for the last session. In the terminal
every key is the shell's; the app's Ctrl chords reach the window with Shift added.

### Opening a file from code

`UiState::open_file(target)` (`src/ui/viewer.rs`) is the one entry point. `target` is a
`PathBuf` (`~` is expanded) or a `model::viewer::FileRef::Asset { asset, directory }` for a file
the gateway presented (`files.present`, a timeline image or file link) with the session's folder.
It resolves the file on this computer, reads text off the UI thread, and says so plainly when the
file is on another computer.

## Files

| What | Linux | macOS | Windows |
|---|---|---|---|
| Config (`config.json`) | `~/.config/muqun-desktop/` | `~/Library/Application Support/muqun-desktop/` | `%APPDATA%\muqun-desktop\` |
| State (store, uploads) | `~/.local/share/muqun-desktop/` | same as config | `%LOCALAPPDATA%\muqun-desktop\` |

These are deliberately **not** the gateway's directories: a gateway service owns its state
directory through a lock, and a second owner would push it off the phones it serves. Settings →
About shows both paths.

`config.json` is written atomically (temporary file, flush, rename), 500 ms after the last change
and once more at exit. Agent blocks use the gateway's own `*Config` types and key names. Besides
agents it remembers the window size, pane widths, sidebar grouping and filters, project choices,
pinned sessions, unsent drafts, the last folder / model / mode per agent, language, appearance,
theme and the notification switch.

## OpenCode ownership rule

There is one OpenCode service per user. When a Muqun gateway service is running on this computer
(its `gateway.lock` is held) the desktop only **adopts** a running OpenCode and never starts one;
with no gateway it follows `opencode.autostart`. DeepSeek is only ever adopted. Codex joins the
shared daemon; the ACP agents (Pi, FX, Qoder, Claude, Antigravity, Cursor) use private child
processes; T3 needs pairing in the desktop (Settings → Agents → T3).

## Daily development

The desktop is the `desktop/` member of the gateway workspace.

The terminal panel's emulator (`libghostty-vt`) is Zig built from source: its build script clones
Ghostty and needs **Zig 0.15.2** on `PATH` (with mise: `mise exec zig@0.15.2 -- cargo build …`;
the first build also needs network for the clone).

```sh
cargo build -p muqun-desktop             # type-check fast
cargo run --release -p muqun-desktop     # judge speed on release; debug is slow with 300+ sessions
cargo test -p muqun-desktop

# before every commit (Desktop CI runs the same):
cargo fmt --all -- --check
cargo clippy -p muqun-desktop --all-targets -- -D warnings
cargo test -p muqun-desktop --locked
desktop/scripts/lint-theme-tokens.sh     # no colour literals outside theme/{tokens,agents,pack}.rs
grep -rn 'cfg(target_os' desktop/src | grep -v 'desktop/src/platform/'   # must be empty
```

**Look at it without a screen.** Named demo states live in `src/ui/scenes.rs` (the only place
demo data may live); Rune renders one headless:

```sh
./target/debug/muqun-desktop --scene session --snap out.png --size 1440x900 --scale 1 --frames 30
./target/debug/muqun-desktop --list-scenes
```

`--theme light|dark`, `--script` for scripted input and `--tree` (dump the element tree) are
Rune's; see its README. Add a scene for every new state you build.

**Rune** comes from our fork, pinned by git rev in `desktop/Cargo.toml` (`patches/rune/` lists
what the fork adds over upstream). To iterate on Rune itself, override the pin with a local
checkout: see the comment at the end of the root `Cargo.toml`.

Where things go:

| You want to… | Edit |
|---|---|
| change how a page looks | `src/ui/<page>.rs` (`home`, `sessions`, `timeline`, `composer`, `inspector`, `inbox`, `tasks`, `settings`, `viewer`) |
| add a command the UI sends to the core | `model/commands.rs` → handle in `model/projection.rs` |
| react to a new gateway event | `model/projection.rs` (filter) → `ui/mod.rs` `apply_updates` |
| talk to the gateway | `hub/` (via `SessionHub` only: `local.rs` in-process, `remote.rs` the local service) |
| platform-specific behaviour | `src/platform/*.rs` (the only place `cfg(target_os)` is allowed) |
| colours, materials, artwork | theme tokens and pack slots (`src/theme/`); never literals in UI code |
| a new icon | `assets/glyphs/` via `scripts/sync-glyphs.sh` (the App is the source) |
| strings | `ui::copy::tr(language, en, zh)` |
| the launcher entry or icons | `packaging/linux/`, `scripts/gen-linux-icons.sh` |

The UI never runs on the tokio thread: the core pushes `Update`s into the inbox
(`model/inbox.rs`), and the main thread folds a batch per frame. Kill every instance you start.

## Environment

* `MUQUN_WINDOW_TITLE` — the window's title, so builds from several worktrees can run side by
  side (it also keys the single-instance socket).
* `MUQUN_LOCAL_GATEWAY` — the gateway credential file to attach with (default: the gateway
  service's own `local-client.json`).
* `MUQUN_GATEWAY_STATE_DIR` — where to look for a running gateway's lock.
* `MUQUN_THEME_GALLERY` — another theme gallery (an `https://` base ending in `/`), for testing.
* `MUQUN_QA_*` — composer and session-switch QA harnesses, off unless set.

## Known limits

* Window position and maximized state are not restored (Wayland never reports position to
  clients; size is saved).
* System notifications are Linux-only so far (macOS and Windows build with a no-op), and single
  instance is Unix-only (on Windows every launch is its own).
* No status icon on macOS yet; no installer for macOS or Windows, no code signing.
