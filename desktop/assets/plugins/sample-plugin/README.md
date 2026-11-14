# Sample Desktop Plugin

Welcome to the **Muqun Desktop Plugin System**!

This sample plugin demonstrates how you can create custom extensions for Muqun Desktop **without writing a single line of Rust code**. Plugins are purely file-based and declarative.

---

## Plugin Structure

Every plugin lives in its own subdirectory containing a `plugin.json` manifest:

```text
~/.config/muqun-desktop/plugins/my-custom-plugin/
├── plugin.json
├── README.md
└── assets/ (optional)
```

---

## Manifest Reference (`plugin.json`)

| Field | Type | Description |
|---|---|---|
| `id` | `string` | Unique identifier (e.g., `"sample-plugin"`). |
| `name` | `string` | Display name of the plugin. |
| `version` | `string` | Semantic version string (e.g., `"1.0.0"`). |
| `description` | `string` | Brief description of what this plugin provides. |
| `author` | `string?` | Author name or organization. |
| `category` | `string?` | Grouping category: `"core"`, `"tools"`, `"diagnostics"`, or `"extensions"`. Defaults to `"extensions"`. |
| `icon` | `string?` | Icon identifier: `"task"`, `"terminal"`, `"branch"`, `"keyboard"`, `"settings"`, `"agent"`, `"info"`, etc. |
| `commands` | `array` | List of commands contributed by this plugin. |
| `tile` | `object?` | Split-view tile configuration. |

### Commands Schema

```json
{
  "id": "myplugin.greet",
  "title": "Send Greeting",
  "shortcut": "Ctrl+Shift+G",
  "action": "Hello from custom plugin!"
}
```

- When `action` is a URL (starting with `http://` or `https://`), clicking or running the command opens it in the system browser.
- Any action text is displayed as an interactive desktop toast notification.

### Tile View Schema

```json
{
  "kind": "markdown",
  "title": "Workspace Guide",
  "entry": "README.md"
}
```

Supported `kind` values:
- `"markdown"`: Renders the markdown file specified in `entry` (or falls back to `README.md`) using native Rune UI typography and styling.
- `"webview"`: Shows an entry card with quick launch into the browser.
- `"command"`: Displays a dedicated command palette panel with executable buttons and shortcuts.

---

## Installing Plugins

Place your plugin folder into:
- **Linux:** `~/.config/muqun-desktop/plugins/<plugin-id>/`
- **macOS:** `~/Library/Application Support/muqun-desktop/plugins/<plugin-id>/`
- **Windows:** `%APPDATA%\muqun-desktop\plugins\<plugin-id>\`

Muqun Desktop automatically discovers and activates your plugin on startup!
