# Muqun Desktop preview

This is an early Desktop preview, published separately from Muqun Gateway. It does not replace the stable Gateway release.

## Changes

- Native desktop panels, theme-aware message rendering, and a compact composer with separate controls below the input.
- All-computers navigation routes existing sessions to their owning computer. The aggregate start page asks which computer to use instead of reporting that no agents are installed.
- Model catalogs stay with the session that requested them when switching split panes. Unavailable catalogs can be requested again from the model control.
- Enter sends and Shift+Enter inserts a newline by default. Custom shortcuts remain supported.
- Git and context controls are available below the session composer.
- Sidebar search shares a row with the recent-session filter. Focusing search expands it across the row; an active query remains visible until cleared.

## Platforms and compatibility

- Linux x86_64 with a graphical desktop; the CI installation smoke check uses X11 and software rendering.
- macOS Apple Silicon; public preview archives must be Developer ID signed, notarized, and stapled.
- Windows and Intel macOS packages are not included in this preview.
- Desktop and Gateway are separate executables. Headless servers need only Gateway. Remote Desktop connections require a paired Gateway with the agent-session APIs; optional features depend on the connected Gateway and agent capabilities.
- The App does not need an update to install Desktop. Herdr is optional for coding-agent sessions; terminal features using Herdr require Gateway's supported protocol 17 backend.

## Installation

Close the Desktop application before replacing it, then reopen it. Do not delete existing configuration or conversation data. No database migration is required by this Desktop preview. Updating the UI does not require restarting the shared agent backend.

## Preview limitations

This is not a stable release. Windows packaging and a complete cross-platform visual and interaction acceptance pass remain outside the current verified scope. Do not treat successful CI compilation as proof that every desktop workflow has been manually validated.
