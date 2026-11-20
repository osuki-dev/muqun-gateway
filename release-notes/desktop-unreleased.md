# Muqun Desktop preview

## Changes and fixes

- Desktop Settings → Computers lists paired computers and connected devices. Pair another Gateway by address or display this computer’s QR and pending verification code. Desktop keeps a selected remote computer when it is offline.
- Home starts a session in the chosen project and immediately shows its first message. Late session updates retain the active conversation. The composer keeps attachments with their draft, separates queued input from delivered messages, and keeps Send in the input while Stop sits beside the delivery mode.
- Conversation and file images open in Desktop’s viewer with wheel zoom and drag to pan. Plugin images can use the same viewer. Sub-agent details open in a shared window and stay out of recent top-level sessions.
- Terminal, browser and plugin panes share window controls. Linux browser tabs show the page title and favicon, support closing a tab, and use the packaged WebKitGTK helper for page rendering and common selection actions.
- Plugin settings show searchable cards with descriptions, keywords and source links from the package metadata. Bun resolves plugin sources and updates display progress. The host and SDK provide native content, notifications, image previews and selection actions to installed plugins. The distribution includes the existing Review Center plugin. Fizzy remains independently installed and updated.
- Desktop checks its own stable or preview channel from Settings → About. The installer displays the verified SHA-256, installs the platform package and provisions a missing local Gateway. Packaged Desktop can recover missing Gateway setup from Diagnostics or run local-only.

## Compatibility and installation

Linux x86_64 and Apple Silicon macOS are supported. The Linux embedded browser needs WebKitGTK 4.1. The macOS archive contains a Developer ID signed, notarized and stapled application. Existing users can install the verified preview with:

```sh
curl -fsSL https://muqun.dev/desktop.sh | sh -s -- --channel preview
```

Close Desktop before replacing it and reopen it afterward. Existing settings, pairing and conversations are retained. Installing the Desktop preview does not require a mobile App update. Terminal sessions can use tmux or Herdr protocol 17. Remote features depend on the paired Gateway and agent capabilities. A separately updated Gateway requires its own restart; active agent work should finish first.
