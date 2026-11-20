# Muqun Desktop preview

## Changes and fixes

- Installed Desktop now checks its own release channel on startup and every six hours while running. A notification opens the update page. You choose when to download and restart. To get this update from preview.27, open Settings → About.

- Search the model picker on Home and in sessions by model name, ID, or provider. Model and reasoning effort selections stay visible, including when working in split panes.
- The sidebar now uses the active theme's surface and artwork. On macOS the main workspace lets the desktop show through, and Reduce transparency restores an opaque background.
- Installing Bun or Herdr gives a newly downloaded binary time for its first startup. Herdr now downloads from its official release location.

## Compatibility and installation

Linux x86_64 packages target the Ubuntu 24.04 glibc 2.39 baseline. macOS packages target Apple Silicon. The Linux embedded browser needs WebKitGTK 4.1. The macOS archive contains a Developer ID signed, notarized and stapled application. Existing users can install the verified preview with:

```sh
curl -fsSL https://muqun.dev/desktop.sh | sh -s -- --channel preview
```

When updating from Settings, Desktop stays open until you choose to restart. Existing settings, pairing and conversations are retained. Installing the Desktop preview does not require a mobile App update. When you approve Gateway setup on a fresh computer, Desktop starts its built-in terminal in the background and connects Gateway to it. Existing tmux and Herdr installations remain supported, including Herdr protocol 17. Remote features depend on the paired Gateway and agent capabilities. A separately updated Gateway requires its own restart; active agent work should finish first.
