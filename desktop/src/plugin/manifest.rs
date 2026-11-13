//! Manifest schema and definitions for desktop plugins.
//!
//! External plugins define a `plugin.json` file complying with [`PluginManifest`].
//! This enables declarative discovery, command contribution, and custom tile
//! rendering without modifying or recompiling core Rust code.

use serde::{Deserialize, Serialize};

use crate::config::Language;
use crate::ui::copy::tr_ref;

/// Standard display surface slots where a plugin can be mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginSlot {
    /// Full-screen workspace page in the main card.
    #[serde(alias = "Page", alias = "page", alias = "fullscreen")]
    Page,
    /// Split-view tile in the dwindle layout.
    #[serde(alias = "Tile", alias = "tile", alias = "split")]
    #[default]
    Tile,
    /// Right-side inspector drawer panel.
    #[serde(alias = "Drawer", alias = "drawer", alias = "panel")]
    Drawer,
    /// Floating modal overlay dialog.
    #[serde(alias = "Modal", alias = "modal", alias = "dialog")]
    Modal,
    /// Header toolbar action button.
    #[serde(alias = "Toolbar", alias = "toolbar", alias = "header")]
    Toolbar,
}

/// Alias for [`PluginSlot`].
pub type PluginSurface = PluginSlot;

impl PluginSlot {
    pub fn label(self, lang: Language) -> &'static str {
        match self {
            Self::Page => tr_ref(lang, "Page"),
            Self::Tile => tr_ref(lang, "Tile"),
            Self::Drawer => tr_ref(lang, "Drawer"),
            Self::Modal => tr_ref(lang, "Modal"),
            Self::Toolbar => tr_ref(lang, "Toolbar"),
        }
    }
}

/// Standard rendering modes supported by the desktop plugin host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginRenderKind {
    /// Rust native or declarative schema (`markdown`, `command`, `schema`, etc.) rendered via Rune GPU.
    #[serde(
        alias = "native",
        alias = "markdown",
        alias = "command",
        alias = "schema"
    )]
    Native {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        entry: Option<String>,
    },
    /// Embedded terminal TUI (`lazygit`, `btop`, `yazi`, `bun run ...`, etc.).
    #[serde(alias = "terminal", alias = "tui", alias = "terminal_tui")]
    TerminalTui {
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Web / URL entry with rich card preview and one-click browser launch.
    #[serde(alias = "webview", alias = "web")]
    WebView { entry: String },
    /// External TypeScript/JavaScript plugin executed via the `bun` runtime.
    #[serde(alias = "bun", alias = "bun_script", alias = "script")]
    BunScript { entry: String },
}

impl Default for PluginRenderKind {
    fn default() -> Self {
        Self::Native {
            kind: "native".to_string(),
            entry: None,
        }
    }
}

/// Category of the plugin for filtering and presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginCategory {
    #[serde(alias = "Core", alias = "core")]
    Core,
    #[serde(alias = "Tools", alias = "tools")]
    Tools,
    #[serde(alias = "Diagnostics", alias = "diagnostics")]
    Diagnostics,
    #[serde(alias = "Extensions", alias = "extensions")]
    #[default]
    Extensions,
}

impl PluginCategory {
    pub fn label(self, lang: Language) -> &'static str {
        match self {
            Self::Core => tr_ref(lang, "Core"),
            Self::Tools => tr_ref(lang, "Tools"),
            Self::Diagnostics => tr_ref(lang, "Diagnostics"),
            Self::Extensions => tr_ref(lang, "Extensions"),
        }
    }
}

fn default_category() -> Option<PluginCategory> {
    Some(PluginCategory::Extensions)
}

/// Declarative command contributed by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginCommandManifest {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub shortcut: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

/// Tile view configuration for plugins that support split view panes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginTileManifest {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The declarative manifest read from `plugin.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default = "default_category")]
    pub category: Option<PluginCategory>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<PluginSlot>,
    #[serde(default)]
    pub commands: Vec<PluginCommandManifest>,
    #[serde(default)]
    pub tile: Option<PluginTileManifest>,
}

impl PluginManifest {
    /// Parse a plugin manifest from a JSON string.
    pub fn parse(json_str: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_str)
    }

    /// Read and parse a plugin manifest from a file.
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let manifest = serde_json::from_str(&content)?;
        Ok(manifest)
    }

    /// Category for grouping, defaulting to `PluginCategory::Extensions`.
    pub fn category(&self) -> PluginCategory {
        self.category.unwrap_or(PluginCategory::Extensions)
    }

    /// Resolved display slots supported by this plugin.
    /// Defaults to `[Tile, Page, Drawer]` when `surfaces` is not explicitly specified.
    pub fn supported_surfaces(&self) -> Vec<PluginSlot> {
        if !self.surfaces.is_empty() {
            return self.surfaces.clone();
        }
        vec![PluginSlot::Tile, PluginSlot::Page, PluginSlot::Drawer]
    }

    /// Resolve the primary [`PluginRenderKind`] from `runtime`, `entry`, and `tile`.
    pub fn render_kind(&self) -> PluginRenderKind {
        let is_script_path = |p: &str| {
            let lower = p.to_ascii_lowercase();
            lower.ends_with(".ts")
                || lower.ends_with(".js")
                || lower.ends_with(".mjs")
                || lower.ends_with(".tsx")
                || lower.ends_with(".jsx")
        };

        if self
            .runtime
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case("bun"))
        {
            let entry = self
                .entry
                .clone()
                .or_else(|| self.tile.as_ref().and_then(|t| t.entry.clone()))
                .unwrap_or_else(|| "index.ts".to_string());
            return PluginRenderKind::BunScript { entry };
        }

        if let Some(entry) = &self.entry {
            if is_script_path(entry) {
                return PluginRenderKind::BunScript {
                    entry: entry.clone(),
                };
            }
        }

        if let Some(tile) = &self.tile {
            let kind_lower = tile.kind.to_ascii_lowercase();
            if kind_lower == "bun" || kind_lower == "bun_script" || kind_lower == "script" {
                let entry = tile
                    .entry
                    .clone()
                    .or_else(|| self.entry.clone())
                    .unwrap_or_else(|| "index.ts".to_string());
                return PluginRenderKind::BunScript { entry };
            }
            if let Some(entry) = &tile.entry {
                if is_script_path(entry) {
                    return PluginRenderKind::BunScript {
                        entry: entry.clone(),
                    };
                }
            }
            if kind_lower == "terminal" || kind_lower == "tui" || kind_lower == "terminal_tui" {
                let command = tile.command.clone().unwrap_or_else(|| {
                    tile.entry
                        .as_ref()
                        .map(|e| {
                            e.split_whitespace()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
                return PluginRenderKind::TerminalTui {
                    command,
                    cwd: tile.cwd.clone(),
                };
            }
            if kind_lower == "webview" || kind_lower == "web" {
                return PluginRenderKind::WebView {
                    entry: tile
                        .entry
                        .clone()
                        .unwrap_or_else(|| "about:blank".to_string()),
                };
            }
            return PluginRenderKind::Native {
                kind: tile.kind.clone(),
                entry: tile.entry.clone(),
            };
        }

        PluginRenderKind::Native {
            kind: "native".to_string(),
            entry: self.entry.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_manifest() {
        let json = r#"{
            "id": "my-plugin",
            "name": "My Plugin",
            "version": "1.2.0",
            "description": "A test plugin description",
            "author": "Alice",
            "category": "tools",
            "icon": "task",
            "commands": [
                {
                    "id": "cmd.test",
                    "title": "Run Test",
                    "shortcut": "Ctrl+Shift+T",
                    "action": "execute"
                }
            ],
            "tile": {
                "kind": "markdown",
                "title": "My Tile",
                "entry": "README.md"
            }
        }"#;

        let manifest = PluginManifest::parse(json).expect("valid manifest");
        assert_eq!(manifest.id, "my-plugin");
        assert_eq!(manifest.name, "My Plugin");
        assert_eq!(manifest.version, "1.2.0");
        assert_eq!(manifest.description, "A test plugin description");
        assert_eq!(manifest.author.as_deref(), Some("Alice"));
        assert_eq!(manifest.category, Some(PluginCategory::Tools));
        assert_eq!(manifest.category(), PluginCategory::Tools);
        assert_eq!(manifest.icon.as_deref(), Some("task"));
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].id, "cmd.test");
        assert_eq!(
            manifest.commands[0].shortcut.as_deref(),
            Some("Ctrl+Shift+T")
        );
        assert_eq!(manifest.commands[0].action.as_deref(), Some("execute"));

        let tile = manifest.tile.expect("tile present");
        assert_eq!(tile.kind, "markdown");
        assert_eq!(tile.title, "My Tile");
        assert_eq!(tile.entry.as_deref(), Some("README.md"));
    }

    #[test]
    fn parse_minimal_manifest_defaults_category() {
        let json = r#"{
            "id": "minimal",
            "name": "Minimal Plugin",
            "version": "0.1.0",
            "description": "Minimal description"
        }"#;

        let manifest = PluginManifest::parse(json).expect("valid minimal manifest");
        assert_eq!(manifest.id, "minimal");
        assert_eq!(manifest.name, "Minimal Plugin");
        assert_eq!(manifest.author, None);
        assert_eq!(manifest.category, Some(PluginCategory::Extensions));
        assert_eq!(manifest.category(), PluginCategory::Extensions);
        assert_eq!(manifest.icon, None);
        assert!(manifest.commands.is_empty());
        assert_eq!(manifest.tile, None);
    }

    #[test]
    fn category_aliases_and_case() {
        for (raw, expected) in [
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"core"}"#,
                PluginCategory::Core,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"Core"}"#,
                PluginCategory::Core,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"tools"}"#,
                PluginCategory::Tools,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"Tools"}"#,
                PluginCategory::Tools,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"diagnostics"}"#,
                PluginCategory::Diagnostics,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"Diagnostics"}"#,
                PluginCategory::Diagnostics,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"extensions"}"#,
                PluginCategory::Extensions,
            ),
            (
                r#"{"id":"a","name":"b","version":"1","description":"c","category":"Extensions"}"#,
                PluginCategory::Extensions,
            ),
        ] {
            let m = PluginManifest::parse(raw).unwrap();
            assert_eq!(m.category(), expected);
        }
    }

    #[test]
    fn manifest_roundtrip_serialization() {
        let manifest = PluginManifest {
            id: "roundtrip".to_string(),
            name: "Roundtrip Plugin".to_string(),
            version: "1.0.0".to_string(),
            description: "Testing roundtrip serialization".to_string(),
            author: Some("Bob".to_string()),
            category: Some(PluginCategory::Diagnostics),
            icon: Some("settings".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![PluginSlot::Tile, PluginSlot::Page],
            commands: vec![PluginCommandManifest {
                id: "rt.cmd".to_string(),
                title: "RT Command".to_string(),
                shortcut: None,
                action: None,
                script: None,
            }],
            tile: Some(PluginTileManifest {
                kind: "webview".to_string(),
                title: "RT Webview".to_string(),
                entry: Some("https://example.com".to_string()),
                command: None,
                cwd: None,
            }),
        };

        let serialized = serde_json::to_string(&manifest).expect("serialize");
        let deserialized: PluginManifest = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(manifest, deserialized);
        assert_eq!(
            manifest.render_kind(),
            PluginRenderKind::WebView {
                entry: "https://example.com".to_string()
            }
        );
    }

    #[test]
    fn multi_surface_and_render_kind_resolution() {
        let bun_json = r#"{
            "id": "bun-ext",
            "name": "Bun Extension",
            "version": "1.0.0",
            "description": "Bun script plugin",
            "runtime": "bun",
            "entry": "index.ts",
            "surfaces": ["page", "tile", "drawer", "modal", "toolbar"]
        }"#;
        let bun_manifest = PluginManifest::parse(bun_json).unwrap();
        assert_eq!(
            bun_manifest.supported_surfaces(),
            vec![
                PluginSlot::Page,
                PluginSlot::Tile,
                PluginSlot::Drawer,
                PluginSlot::Modal,
                PluginSlot::Toolbar,
            ]
        );
        assert_eq!(
            bun_manifest.render_kind(),
            PluginRenderKind::BunScript {
                entry: "index.ts".to_string()
            }
        );

        let tui_json = r#"{
            "id": "lazygit-ext",
            "name": "Lazygit TUI",
            "version": "1.0.0",
            "description": "Embedded lazygit TUI",
            "tile": {
                "kind": "terminal_tui",
                "title": "Lazygit",
                "command": ["lazygit", "--use-config-dir"],
                "cwd": "/tmp"
            }
        }"#;
        let tui_manifest = PluginManifest::parse(tui_json).unwrap();
        assert_eq!(
            tui_manifest.render_kind(),
            PluginRenderKind::TerminalTui {
                command: vec!["lazygit".to_string(), "--use-config-dir".to_string()],
                cwd: Some("/tmp".to_string()),
            }
        );
    }

    #[test]
    fn invalid_manifest_fails() {
        let bad_json = r#"{"name": "No ID Plugin"}"#;
        assert!(PluginManifest::parse(bad_json).is_err());
    }
}
