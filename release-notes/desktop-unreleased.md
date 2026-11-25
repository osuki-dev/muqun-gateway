# Muqun Desktop preview

## Changes and fixes

- **Unified MCP Hub Settings (`Settings → MCP Hub`)**: Add, edit, enable/disable, live-probe, and remove external Streamable HTTP and stdio MCP servers hosted once in Muqun Gateway (`mcp_hub_v1`), with access scope (`Local only` / `Local + tasks` / `All`), isolation (`Shared` / `Per session`), masked secret preservation, discovered tool previews, and clear upgrade guidance on older Gateways.
- **Remote file preview & interactive remote terminal / `nvim`**: When viewing a remote computer's sessions, clicking files now opens them in Desktop's built-in viewer (text, diff, and images via Gateway's file-asset API) and supports opening files in a remote terminal (`nvim` / editor) over E2EE v2 `GET /api/pty/ws` (requires Gateway with `remote_pty_v1`).
- **Global window opacity, split-tile sidebar highlight, and session retention**: Added `Settings → Appearance → Window opacity` (`35%–100%` slider with Reset) applied uniformly across sidebar, panels, tiles, drawers, and terminal surfaces; split-view tiles keep all open sessions steadily highlighted in the sidebar without bouncing as the pointer moves across panes; closing a tile or refreshing sessions preserves active and recently closed sessions; and sidebar session filtering avoids redundant per-row lineage scans.
- **Model-led Multi-Agent Orchestration (MAO) in Desktop**: Configure a dynamic agent/model pool, per-session MAO overrides, automatic quota failover, optional `Auto code review` toggle (off by default for Lead-Model orchestration), and on-demand `Ask Advisor` directly from the composer and Settings.
- **Unified composer permission pill & queued image editing**: Permission modes and Auto-approve are consolidated into one labeled permission pill (`Ask` / `Auto-approve` / mode name) with one unified menu. Editing a queued message with image attachments preserves the real host upload path so thumbnails and re-sending work reliably.
- **Borderless animated GPU theme-aura focus & UI polish**: Replaced hard 1 px focus outlines across panels, split panes, composers, pills, and selectors with a borderless `rune-fx` animated theme-gradient aura that extracts vivid theme highlight stops (including light ink-primary themes such as `scene-voyager`). Fixed the Tasks computer-picker dropdown anchor so its menu opens directly beneath the trigger.
- **Single editor instance per workspace**: Opening multiple files from the same workspace reuses one editor tab/window (`nvim --listen` / `--server --remote-silent`, `vim --servername`, `hx`, `emacsclient`, and `--reuse-window` for VS Code/Cursor/etc.).
- **Synced Rune UI engine to `anomalyco/rune@46f334ef`**: Picks up 618 upstream fixes including native Wayland file drops (`#1808`/`#1866`/`#1872`), CJK `.ttc` fallback registration, >4 backdrop-blur slots (`#1800`), Wayland IME (`#1782`), and faster text shaping.
- Installed Desktop now checks its own release channel on startup and every six hours while running, and relaunches the verified package cleanly through Rune's normal quit hook. A notification opens the update page. You choose when to download and restart. To get this update from preview.27, open Settings → About.
- Search the model picker on Home and in sessions by model name, ID, or provider. Model and reasoning effort selections stay visible, including when working in split panes.
- The sidebar now uses the active theme's surface and artwork. On macOS the main workspace lets the desktop show through, and Reduce transparency restores an opaque background.
- Installing Bun or Herdr gives a newly downloaded binary time for its first startup. Herdr now downloads from its official release location.

## Compatibility and installation

Linux x86_64 packages target the Ubuntu 24.04 glibc 2.39 baseline. macOS packages target Apple Silicon. The Linux embedded browser needs WebKitGTK 4.1. The macOS archive contains a Developer ID signed, notarized and stapled application. Existing users can install the verified preview with:

```sh
curl -fsSL https://muqun.dev/desktop.sh | sh -s -- --channel preview
```

When updating from Settings, Desktop stays open until you choose to restart. Existing settings, pairing and conversations are retained. Installing the Desktop preview does not require a mobile App update. When you approve Gateway setup on a fresh computer, Desktop starts its built-in terminal in the background and connects Gateway to it. Existing tmux and Herdr installations remain supported, including Herdr protocol 17. Remote features depend on the paired Gateway and agent capabilities. A separately updated Gateway requires its own restart; active agent work should finish first.
