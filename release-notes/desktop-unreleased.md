# Muqun Desktop preview

This is an early Desktop preview, published separately from Muqun Gateway. It does not replace the stable Gateway release.

## Changes

- Delegated task details use readable line spacing and a bounded scroll area, so a single long instruction cannot expand across the whole conversation. Full instructions remain selectable and copyable.

- Compact composer footers keep delivery controls on one line in narrow split panes; controls and bottom spacing remain consistent when focus changes. Icon-only controls use matching circular hit areas instead of narrow ovals.

- Upgrade bundled SQLCipher to 4.20.0 / SQLite 3.54.1 through rusqlite 0.42, addressing RUSTSEC-2026-0502 (crafted database/WAL memory disclosure). Existing encryption remains enabled.
- CI validation and Desktop builds use Rust 1.99.1 consistently; Rust 1.100 support remains pending an async-stream compatibility fix.

- Split-pane composer controls keep their positions on hover. Returning from the timeline to the composer restores paste focus without an extra click. The default launch icon uses the full-resolution asset with rounded corners.

- Inline image markers and attachment chips now delete together. Removed images stay removed when an upload finishes late, and image uploads finish in their original split pane.

- Hover activates split panes without requiring a click. Clicking text in an already active pane no longer refocuses the composer and clears the selected word.

- Command, shortcuts, pairing and welcome dialogs share a single entrance/exit motion and reduced-motion fallback. Pane action menus reserve room for keyboard shortcuts and translate directional swap actions.


- Native desktop panels, theme-aware message rendering, and a compact composer with separate controls below the input.
- All-computers navigation routes existing sessions to their owning computer. The aggregate start page asks which computer to use instead of reporting that no agents are installed.
- Model catalogs stay with the session that requested them when switching split panes. Unavailable catalogs can be requested again from the model control.
- Enter sends and Shift+Enter inserts a newline by default. Custom shortcuts remain supported.
- Git and context controls are available below the session composer.
- Sidebar search shares a row with the recent-session filter. Focusing search expands it across the row; an active query remains visible until cleared.
- Running command rows keep their position while output arrives and commands finish. Activity folds together after the reply ends, reducing jumps between tool summaries.
- Split-pane composers share the same controls and footer sizing when focus moves between sessions.
- In-session search highlights matching message text, code and command output; matched folded groups open when navigating results.
- On Omarchy Linux, Appearance offers an optional system-palette theme. It reads Omarchy's colours without changing system files and refreshes when the palette changes.

- Message-level fork actions now require an explicit agent capability; whole-session fork support no longer exposes an unsupported “Fork from here” button or menu item.

- Context details open in a compact, scrollable popover above their own session button. Composer menus use session-specific anchors so split panes no longer open menus over another conversation.

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
