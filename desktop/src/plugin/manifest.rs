//! Manifest schema and definitions for desktop plugins.
//!
//! External plugins define a `plugin.json` file complying with [`PluginManifest`].
//! This enables declarative discovery, command contribution, and custom tile
//! rendering without modifying or recompiling core Rust code.

use serde::{Deserialize, Serialize};

use crate::config::Language;
use crate::ui::copy::tr_ref;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandManifest {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub shortcut: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

/// Tile view configuration for plugins that support split view panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTileManifest {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub entry: Option<String>,
}

/// The declarative manifest read from `plugin.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
            commands: vec![PluginCommandManifest {
                id: "rt.cmd".to_string(),
                title: "RT Command".to_string(),
                shortcut: None,
                action: None,
            }],
            tile: Some(PluginTileManifest {
                kind: "webview".to_string(),
                title: "RT Webview".to_string(),
                entry: Some("https://example.com".to_string()),
            }),
        };

        let serialized = serde_json::to_string(&manifest).expect("serialize");
        let deserialized: PluginManifest = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(manifest, deserialized);
    }

    #[test]
    fn invalid_manifest_fails() {
        let bad_json = r#"{"name": "No ID Plugin"}"#;
        assert!(PluginManifest::parse(bad_json).is_err());
    }
}
