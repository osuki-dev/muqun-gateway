# Muqun Desktop v0.1.0

Muqun Desktop is a native window for the coding agents on this computer: Codex, OpenCode, Claude Code, Cursor, Qoder, Pi, FX, Antigravity, DeepSeek Harness and T3 Code, every session of every agent in one place. It is written in Rust and drawn on the GPU with Rune; there is no webview and no browser engine inside.

When a Muqun Gateway service runs on the same computer, the desktop attaches to it with the local client token the Gateway publishes, so the phone and the desktop see the same sessions, approvals and terminals. Without a Gateway it runs the agents itself, in-process, with nothing listening on the network.

This is the first release. It is built from the Gateway repository (`desktop/`) and ships separately from the Gateway's own releases.

## Highlights

### Sessions

- **Home** starts a session: pick an agent and a folder (recent projects, folders under what you type, the system picker), a model, a mode and optionally a worktree, then write the first prompt.
- **The sidebar** lists every session, searchable, grouped by time, agent or folder, with agent filter chips. Sessions can be pinned, renamed, forked, deleted or moved to the background; projects can be pinned, hidden and renamed.
- **The timeline streams** and draws each kind of agent output as its own card: Markdown with highlighted code, tool calls in the shapes the real agents send them, inline diffs, to-do lists, images, sub-agent sessions (opened in a sheet), and a context meter. Approvals and questions are answered in place.
- **The composer** sends with Enter, takes attachments (pick, paste an image, drop files; large images and videos are shrunk to what the agent accepts), `@` files and computers, `/` commands and skills, and switches model, effort, mode and branch. Prompts sent while the agent works wait in a queue above it.
- **Split view** shows up to four sessions side by side (Ctrl/Cmd+\ splits, Ctrl/Cmd+W closes a tile, Ctrl/Cmd+1–4 focus one), each with its own timeline. The layout is restored on the next start.
- **The inspector** shows the open session's queue, git status and worktrees, sub-agents, saved permissions and background shells, and the working-tree diff with staged revert.
- **Inbox** collects every waiting approval and question across sessions; **Tasks** lists delegated tasks and lets you create, cancel and complete them.

### New in the desktop

- **Command palette.** Ctrl/Cmd+K opens a frosted launcher: fuzzy-match any session (title, agent, folder; most recent first) or action — a new session with a given agent in a given folder, the split, the inspector, any page or Settings tab, a theme pack, light/dark, dictation. Arrow keys and Enter, matched letters highlighted, recent commands first.
- **Voice dictation.** The mic button or Ctrl/Cmd+Shift+Space records; a waveform scrolls while you speak and, in realtime mode, the words appear as the service hears them. Done puts the text in the draft — it is never sent on its own — and Escape cancels. Settings → Voice takes an OpenAI-compatible `/v1/audio/transcriptions` endpoint (after recording) or an OpenAI-Realtime-compatible WebSocket (realtime), a model, a language and an API key, which is kept in the system keyring.
- **File viewer.** Files from the timeline open in an overlay: highlighted text and code with line numbers, rendered Markdown with its source a click away, images fit to the window or zoomed. Video, audio, PDF and other files show their details and an **Open with system app** button. Every file offers **Save as…** and **Reveal in folder**. `muqun-desktop path/to/file` opens a file directly.
- **Terminals.** The Herdr (and tmux) workspaces of the attached Gateway: every tab and pane with its agent and status, a live view of a pane's screen in the theme's colours, a key row (Esc, Tab, arrows, Ctrl+C, Backspace, Enter) and a field that types into the pane. The desktop never focuses, closes or rearranges your panes.
- **Notifications.** An approval, a question or a finished turn in a session you are not looking at raises a system notification titled with the session; clicking it brings the window up on that session. The rules and wording are the Gateway's own agent pushes: one per request, a turn counts as finished after 30 seconds idle, at most once every two minutes per session, and never for a sub-agent's turn. Settings → General → Notifications turns them off.
- **Status icon.** A tray icon whose tooltip and menu list the sessions that need you, with Open Muqun and Quit. With Settings → General → **Keep running in the tray** on (it is off by default), closing the window leaves Muqun running there.
- **One window.** Launching Muqun again brings the running window to the front (and opens the file it was given) instead of starting a second copy.

### Look and language

- **Themes.** The App's theme packs drive colours, glass materials and artwork in light and dark. Six packs are built in; more install from the gallery or import from a `.muqun-theme` file or link, and apply live. Reduce motion and reduce transparency are respected everywhere.
- **Languages.** English, 简体中文 and 繁體中文 (Taiwan), switchable live in Settings → General.

## Install on Linux

From a checkout of the Gateway repository:

```sh
desktop/scripts/install-local.sh            # builds --release, installs for this user (no sudo)
desktop/scripts/uninstall-local.sh          # removes it again
```

The script installs `~/.local/bin/muqun-desktop`, the launcher entry `~/.local/share/applications/muqun-desktop.desktop` and icons under `~/.local/share/icons/hicolor/`. Run it again to update; a running Muqun keeps the old binary until you reopen it. `--no-build` installs an existing `target/release` build, and `PREFIX=/some/dir` installs somewhere other than `~/.local`.

Uninstalling leaves your settings and session state in place (`~/.config/muqun-desktop`, `~/.local/share/muqun-desktop`); delete those folders by hand to start over.

## Requirements

- **A GPU stack wgpu can use**: Vulkan, or OpenGL through Mesa.
- **Fonts**: Noto Sans CJK and JetBrains Mono (Nerd Font) are used when installed; otherwise the system's fonts stand in.
- **Audio** for dictation: ALSA, which PipeWire and PulseAudio provide. Building from source needs the ALSA headers (`libasound2-dev` on Debian and Ubuntu).
- **A session D-Bus** for notifications, the status icon, the keyring and Reveal in folder. The status icon needs a tray host (KDE, waybar and most bars; GNOME needs an extension). The API key for dictation is stored through the Secret Service (GNOME Keyring, KWallet, KeePassXC); without one it is kept in a file only you can read.
- **Terminals** need the Muqun Gateway service running on the same computer.

## Known limits

- **One computer only.** The desktop shows this computer's agents. Linked computers, which the App can reach, are not available yet.
- **Linux is the tested platform.** macOS and Windows build but have not been tested as releases; there is no installer, bundle or code signing for them, notifications are Linux-only, there is no status icon on macOS, and the single-window behaviour is Unix-only.
- **The status icon carries no badge.** The number of sessions that need you is in the icon's tooltip and menu, not on the icon itself.
- Window position and the maximized state are not restored (Wayland never tells an app where its window is); the size is.

## Upgrading

- **The launcher entry is now `muqun-desktop.desktop`.** An earlier install script wrote `muqun.desktop` with `muqun` icons; `install-local.sh` removes those when it installs the new entry, so you will not see Muqun twice in your launcher. The application id (Wayland `app_id`, X11 class, icon name) is `muqun-desktop` everywhere.
- Settings in `config.json` from earlier builds are kept; new settings start at their defaults (notifications on, keep running in the tray off).
