# Muqun Desktop

A native desktop client (Rust, [Blinc](https://github.com/project-blinc/Blinc) UI) for the coding
agents on this computer. It runs the gateway's agents plane in-process — no HTTP, no server, no
Chromium — and shows every agent's sessions in one window.

This is the **P0 slice**: it is not part of the current gateway release and is not published.

## Run

```sh
cargo run -p muqun-desktop --release      # release builds are the ones to look at
RUST_LOG=muqun_desktop=debug cargo run -p muqun-desktop --release
```

Debug builds work but are slow with a few hundred sessions listed; judge speed on `--release`.

`SIGTERM` / `SIGINT` close the window cleanly and exit with status 0 (the last window size is
saved first). The first `build_ui` logs `first frame` at info level.

Linux needs a GPU stack wgpu can use (Vulkan) and, for the fonts the UI looks for, Noto Sans CJK
and JetBrains Mono Nerd Font (see `src/platform/fonts.rs`; missing fonts fall back to the system
default).

## Environment

* `MUQUN_WINDOW_TITLE` -- the window's title, so builds from several worktrees can run side by
  side and input scripts or compositor rules can pick one (also what the QA harness uses to keep
  its window from taking focus).
* `MUQUN_QA_*` -- the timeline QA harness (`src/ui/timeline/qa.rs`), off unless set.
* `MUQUN_DEBUG_LIST=1` -- show the timeline list's window and offset.

## Files

| What | Linux | macOS | Windows |
|---|---|---|---|
| Config (`config.json`) | `~/.config/muqun-desktop/` | `~/Library/Application Support/muqun-desktop/` | `%APPDATA%\muqun-desktop\` |
| State (store, uploads) | `~/.local/share/muqun-desktop/` | same as config | `%LOCALAPPDATA%\muqun-desktop\` |

These are deliberately **not** the gateway's directories: a gateway service owns its state
directory through a lock, and a second owner would push it off the phones it serves.
Settings → About shows both paths and opens the config folder.

`config.json` is written atomically (temporary file, flush, rename), 500 ms after the last
change and once more at exit. Agent blocks use the gateway's own `*Config` types and key names.
Besides agents it remembers: window size, sidebar width, the agent filter, the sidebar grouping
(time / agent / folder), the language, the colour scheme, and the last folder per agent
(`general.last_cwd`).

## OpenCode ownership rule

There is one OpenCode service per user. When a Muqun gateway service is running on this computer
(its `gateway.lock` is held) the desktop only **adopts** a running OpenCode and never starts one;
with no gateway it follows `opencode.autostart`. DeepSeek is only ever adopted (`autostart` is
forced off). Codex joins the shared daemon; the ACP agents (Pi, FX, Qoder, Claude, Antigravity,
Cursor) use private child processes; T3 needs pairing in the desktop (Settings → Agents → T3).

## What P0 has — and only P0 has

* Home, a session list grouped by time / agent / folder with agent filter chips, the Agents panel.
* New session (Ctrl/Cmd+N): agent, then folder through a combobox — the agent's known projects
  ("Recent"; a folder that no longer exists is greyed and says so), the folders under whatever is
  typed (listed by the core after a 150 ms pause, never on the UI thread), "Browse…" for the
  system picker, a typed path used as typed. `~` is expanded when the session is created. Enter
  creates with the defaults; with text typed, Enter first commits that text.
* A session: a windowed, streaming timeline (Markdown with highlighted code, tool calls, diffs,
  todos, approvals, forms; long replies are cut into several list rows), composer (Enter sends, Shift+Enter newline),
  approval cards (Approve / Deny), Stop button, **Esc stops a running turn**.
* Settings (⚙ or Ctrl/Cmd+,): **Agents** (enable switch and config fields per agent, T3 pairing,
  sign-in hints in the gateway CLI's wording), **Appearance** (Light / Dark / System, live), **General** (language zh-CN / en, live; default folder; editor command
  for later; "close to tray" placeholder, off and disabled), **About**.
  Agent changes are saved at once and take effect when the desktop next starts — the page says so.
* Themes: **Ctrl/Cmd+Shift+T** opens the themes panel (installed, gallery, import a
  `.muqun-theme`; applied live and remembered). Six themes are built in
  (`assets/themes/README.md`).
* Not here yet: search, rename/delete/fork, model/mode
  switching, attachments, `@file`, `/commands`, tray, notifications, command palette.

## Baseline (Linux, 2026-11-02)

Release build, Hyprland (Wayland), 5120×2880 @2×, window 1266×1396 logical, this machine's real
agent data (8 usable agents, ~320 listed sessions), `muqun-gateway` not running.

| Measure | Value |
|---|---|
| Cold start → first `build_ui` (`first frame`, process start to first build; 5 runs) | 163, 169, 169, 171, 171 ms |
| Binary size (release, stripped, thin LTO) | 47.5 MB (45.3 MiB) |
| RSS after 30 s idle | 256 MB |
| Idle frames/s, 10 s, nothing touched, no text field focused | 3.2/s at t+31…41 s (agent session events from this machine's live sessions still arriving); 1/s by t+130 s; **0** in quiet 10 s windows (t+220…240 s) |
| Frames/s, composer focused (caret blink), 10 s | 2.8/s |

Frames are `wl_surface.commit` calls counted from `WAYLAND_DEBUG=1`. The architecture's gates are
<1 s cold start (met), <150 MB idle RSS (**not met**: 256 MB, mostly wgpu and the session lists),
<40 MB package (not met as a bare binary). Debug builds with this many sessions keep the UI
thread busy for ~30 s while the first lists are built; release does not.

## Colours

No colour literals outside `src/theme/tokens.rs` (the `.muqun-theme` → Blinc token mapping) and
`src/theme/agents.rs` (the ten agent brand colours, exported as `--agent-<id>` with lightness
adapting to the scheme, hue never); views and `styles.css` use `var(--token)`.
`desktop/scripts/lint-theme-tokens.sh` enforces it (exit 1 on a hit).

## Known limits (Blinc 0.5.1 fork)

* **Window position and maximized state are not saved.** `WindowedContext` exposes neither (the
  `Moved` event is not forwarded to the app, and there is no maximized query). Size is saved and
  restored; `window.x`, `window.y`, `window.maximized` in `config.json` are honoured when present.
  Wayland never reports position to clients anyway. Needs a small fork patch.
* The combobox list's text is painted outside the list box when its rows overflow it (text is not
  clipped in the foreground layer), so the chooser offers at most 8 rows and shortens long paths.
* Blinc derives the typed character from the key plus the Shift *modifier* state
  (`windowed.rs`), not from winit's own text for the key. Real keyboards (Shift held) and IMEs
  are fine; a virtual keyboard that sends an uppercase keysym without a Shift modifier (plain
  `wtype 'Hello'`) types lowercase, and Caps Lock or non-US layouts that produce shifted
  characters without Shift are affected. Needs a fork patch.

## Later

Window icon (Blinc's `WindowConfig` has no field), packaging and signing (dmg / msi / AppImage),
tray and close-to-tray, notifications, command palette, session search / rename / delete /
fork, model and mode switching, attachments, the theme editor and theme art on Home beyond Cover
Courier's, reduce-motion switch, hot-apply of agent settings, saving window position and
maximized state (fork patch), three-platform CI runs.

## Icons

The App's glyphs (`app/assets/glyphs`) are the single icon source. After they
change, run `desktop/scripts/sync-glyphs.sh` (set `MUQUN_APP_DIR` if the App is
not the sibling `app/` checkout). It refreshes `assets/glyphs/`, adds
`brand-opencode.svg`, and regenerates `src/ui/glyphs.rs`. Use
`ui::glyphs::icon(Glyph::X, size).tint(color)`, and `agent_mark(kind)` for brands.
