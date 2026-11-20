# Muqun Desktop preview

## Changes and fixes

- Desktop Settings → Computers lists paired computers and connected devices. Pair another Gateway by address or display this computer’s QR and pending verification code. Desktop keeps a selected remote computer when it is offline.
- Home starts a session in the chosen project and immediately shows its first message. Late session updates retain the active conversation. The composer keeps attachments with their draft, separates queued input from delivered messages, and keeps Send in the input while Stop sits beside the delivery mode.
- Conversation and file images open in Desktop’s viewer with wheel zoom and drag to pan. Plugin images can use the same viewer. Sub-agent details open in a shared window and stay out of recent top-level sessions.
- Terminal, browser and plugin panes share window controls. Linux browser tabs show the page title and favicon, support closing a tab, and use the packaged WebKitGTK helper for page rendering and common selection actions.
- Plugin settings show searchable cards with descriptions, keywords and source links from the package metadata. Bun resolves plugin sources and updates display progress. The host and SDK provide native content, notifications, image previews and selection actions to installed plugins. The distribution includes the existing Review Center plugin. Fizzy remains independently installed and updated.
- Desktop checks its stable or preview channel from Settings → About. Choose Download & install to fetch the package, verify it and install it in the existing location. Restart when ready.
- The one-line installer displays the verified SHA-256 and installs Desktop. On first launch, install a missing local Gateway from the matching channel to enter the workspace. Desktop shows download progress and supports retrying failed installations.

## Compatibility and installation

Linux x86_64 packages target the Ubuntu 24.04 glibc 2.39 baseline. macOS packages target Apple Silicon. The Linux embedded browser needs WebKitGTK 4.1. The macOS archive contains a Developer ID signed, notarized and stapled application. Existing users can install the verified preview with:

```sh
curl -fsSL https://muqun.dev/desktop.sh | sh -s -- --channel preview
```

When updating from Settings, Desktop stays open until you choose to restart. Existing settings, pairing and conversations are retained. Installing the Desktop preview does not require a mobile App update. When you approve Gateway setup on a fresh computer, Desktop starts its built-in terminal in the background and connects Gateway to it. Existing tmux and Herdr installations remain supported, including Herdr protocol 17. Remote features depend on the paired Gateway and agent capabilities. A separately updated Gateway requires its own restart; active agent work should finish first.
