# Desktop windows and plugins

Desktop owns window placement, chrome, focus and lifecycle. Plugins supply content
and declare supported surfaces. `plugin/window.rs` defines `WindowKind`; the
host's transitions live in `ui/windows.rs`.

| Window kind | Manifest surface | Behaviour |
| --- | --- | --- |
| Tile | `tile` | Participates in the workspace; supports dwindle, scrolling and floating layouts, plus popping a single tile above its neighbours. |
| Page | `page` | Uses the main content area. Maximizing an existing tile retains its layout/geometry; Restore returns to that tile. Maximizing a dock restores the dock. |
| Dock | `drawer` | Resizable right-side panel alongside the workspace. Opening it does not discard other tiles. `drawer` remains the manifest spelling for compatibility. |

Toolbar, command and modal extension slots are not additional window kinds.
Application/OS fullscreen is independent from maximizing a content window.
The host mounts the shared surface controls; plugin renderers should not draw
another copy. Unsupported destinations are filtered and rejected before mutation.
A full workspace never silently replaces a focused tile when opening a plugin.

## Files

The built-in `files` plugin uses the same Tile/Page/Dock lifecycle. A preview opens
as a floating tile, so dragging, resizing and maximizing use the workspace's
existing machinery. It currently has one preview instance; another file replaces
that preview's content.

In Settings → General, enable **Open files directly in editor** and choose
**Neovim**. Clicking text/code then opens nvim in the embedded terminal directly,
without first opening a preview. Images retain the built-in preview. The custom
terminal editor command retains its arguments. The existing Open in editor action
remains available when the preview default is preferred.

Native plugins can provide alternate file-view/edit content using `DesktopPlugin`.
File-extension association with arbitrary external plugin IDs is not implemented
by this change; the configurable direct-open route currently targets editors.

## External plugins

The left sidebar's **Plugins** entry opens the installed list. Each plugin exposes
buttons for its declared window types. External extensions appear before built-ins.
The host applies the selected theme, window chrome and motion to their content.

External manifests can set `"icon": "assets/icon.png"` (PNG, JPEG or WebP), or
keep an existing built-in icon name. Images must resolve inside the plugin
package, including symlink targets; absolute paths and parent traversal are
rejected. Discovery accepts at most 1 MiB and 1024×1024 decoded pixels, caches a
thumbnail up to 256×256, and uses it consistently in Settings, split headers and
the plugin picker. Missing, malformed or oversized images fall back to a native
vector icon. Restart Desktop after changing a package icon.

Directional controls use the theme's `home.arrow` asset with its declared
`iconDirections.home.arrow`, rotated to match the action. System themes and
packs missing that asset or a declared direction retain the native arrow, so
an older pack is never rotated by guesswork. Dropdown, search navigation and
collapse controls share this path and honour Reduce Motion.

For image-icon acceptance, set `MUQUN_SCENE_PLUGIN_DIR` to a temporary package
and run `plugin-image-settings`, `plugin-image-tile` or `plugin-image-picker`.
These scenes register the package only in their isolated process; they do not
install it or alter the user's plugin directories.

`desktop/examples/plugins/workbench-notes` is a minimal Markdown extension.
Copy its directory under the platform config/data directory's
`muqun-desktop/plugins/` and restart Desktop. Development also discovers
`desktop/assets/plugins`. Marketplace/download installation is not implemented.

Bun command scripts run on a worker thread. Duplicate invocation while busy is
suppressed, stdout/stderr are drained concurrently and bounded, and the process
has a deadline. Results are applied on the UI thread. A slow request must not
block terminal input. Plugin scripts execute with the local user's privileges;
this is an extension mechanism, not a sandbox.

## Pull Requests & Issues

`desktop/assets/plugins/review-center` is a real file-based plugin. It uses Bun
and the existing GitHub CLI login, resolves the active workspace repository, and
reads up to 30 open PRs and 30 open issues. Refresh is explicit. A failed section
does not hide the other section. Titles/labels are escaped and links require HTTPS.
It displays authors, draft/review state, labels and detail links. It does not write
comments, merge PRs or create issues. This version supports GitHub via `gh`;
Gitea/GitLab adapters are not yet included.

## Agent usage

Settings → Agents shows per-agent loaded-session count, reported input/output
Tokens, reported estimated cost and coverage counts. Values come from session
snapshots on the selected computer. Repeated snapshots are not added again.
Missing usage stays unknown rather than becoming zero. These are not account
quota or billing totals; separate cache/reasoning counters are not included in
the input/output numbers. No prices are inferred from model names.
