//! Manifest schema and definitions for desktop plugins.
//!
//! Directory plugins use `plugin.json`; Bun packages use standard `package.json`
//! metadata with a `muqun` discovery marker. Both normalize to [`PluginManifest`]
//! without executing the plugin or recompiling the host.

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

impl PluginRenderKind {
    /// Short label for settings lists and badges.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Native { .. } => "Native",
            Self::TerminalTui { .. } => "Terminal TUI",
            Self::WebView { .. } => "Web",
            Self::BunScript { .. } => "Bun",
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
    pub const ALL: [Self; 4] = [Self::Core, Self::Tools, Self::Diagnostics, Self::Extensions];

    pub fn id(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Tools => "tools",
            Self::Diagnostics => "diagnostics",
            Self::Extensions => "extensions",
        }
    }

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

/// A browser-safe repository from standard package metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginRepository {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

impl PluginRepository {
    /// Open the package directory on GitHub; other forges keep their repository URL.
    pub fn source_url(&self) -> String {
        let Some(directory) = &self.directory else {
            return self.url.clone();
        };
        let Ok(mut url) = reqwest::Url::parse(&self.url) else {
            return self.url.clone();
        };
        if url.host_str() == Some("github.com") {
            if let Ok(mut path) = url.path_segments_mut() {
                path.pop_if_empty().extend(["tree", "HEAD"]);
                path.extend(directory.split('/'));
            }
        }
        url.into()
    }
}

fn deserialize_keywords<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
        .map(str::to_owned)
        .collect())
}

fn deserialize_category<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PluginCategory>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

fn deserialize_repository<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PluginRepository>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let raw = value.as_str().or_else(|| value.get("url")?.as_str());
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let shorthand = raw.strip_prefix("github:").unwrap_or(raw);
    let source = if !shorthand.contains(':') && !shorthand.starts_with('/') {
        let parts: Vec<_> = shorthand.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || matches!(*part, "." | "..")
                    || !part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            })
        {
            return Ok(None);
        }
        format!("https://github.com/{shorthand}")
    } else {
        raw.strip_prefix("git+").unwrap_or(raw).to_string()
    };
    let Ok(mut url) = reqwest::Url::parse(&source) else {
        return Ok(None);
    };
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Ok(None);
    }
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).to_string();
    url.set_path(&path);
    let directory = value
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|directory| {
            !directory.is_empty()
                && !directory.contains('\\')
                && !directory.chars().any(char::is_control)
                && directory
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | ".."))
        })
        .map(str::to_owned);
    Ok(Some(PluginRepository {
        url: url.into(),
        directory,
    }))
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

/// Host metadata shared by legacy directory plugins and Bun packages.
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
    #[serde(
        default,
        deserialize_with = "deserialize_keywords",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub keywords: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_repository",
        skip_serializing_if = "Option::is_none"
    )]
    pub repository: Option<PluginRepository>,
    #[serde(default)]
    /// Built-in icon name, or a package-relative PNG/JPEG/WebP path.
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<PluginSlot>,
    #[serde(skip)]
    pub composer_draft: bool,
    #[serde(skip)]
    pub notification_entry: Option<String>,
    #[serde(default)]
    pub commands: Vec<PluginCommandManifest>,
    #[serde(default)]
    pub tile: Option<PluginTileManifest>,
}

impl PluginManifest {
    /// Read a Bun native-view package that opts into Desktop discovery.
    pub fn from_package(value: serde_json::Value) -> anyhow::Result<Option<Self>> {
        if value.get("muqun").is_none() {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Package {
            name: String,
            version: String,
            main: String,
            #[serde(default)]
            description: String,
            #[serde(default, deserialize_with = "deserialize_keywords")]
            keywords: Vec<String>,
            #[serde(default, deserialize_with = "deserialize_repository")]
            repository: Option<PluginRepository>,
            muqun: NativeView,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NativeView {
            #[serde(default)]
            requires: Vec<String>,
            #[serde(default)]
            permissions: Vec<String>,
            display_name: Option<String>,
            notification_entry: Option<String>,
            icon: Option<String>,
            #[serde(default, deserialize_with = "deserialize_category")]
            category: Option<PluginCategory>,
            #[serde(default)]
            surfaces: Vec<PluginSlot>,
        }
        let package: Package = serde_json::from_value(value)?;
        anyhow::ensure!(
            !package.version.trim().is_empty(),
            "Missing package version"
        );
        anyhow::ensure!(!package.main.trim().is_empty(), "Missing package entry");
        if let Some(path) = &package.muqun.notification_entry {
            anyhow::ensure!(
                !path.is_empty()
                    && std::path::Path::new(path)
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_))),
                "Notification entry must be a package-relative path"
            );
        }
        for capability in &package.muqun.requires {
            anyhow::ensure!(
                matches!(
                    capability.as_str(),
                    "native-view" | "forms" | "terminal-entry"
                ) || (matches!(std::env::consts::OS, "linux" | "macos")
                    && capability == "webview-entry"),
                "This plugin requires a newer Desktop capability: {capability}"
            );
        }
        let title = package
            .muqun
            .display_name
            .unwrap_or_else(|| package.name.clone());
        Ok(Some(Self {
            notification_entry: package.muqun.notification_entry,
            composer_draft: package
                .muqun
                .permissions
                .iter()
                .any(|p| p == "composer.draft.append"),
            id: package.name,
            name: title.clone(),
            version: package.version,
            description: package.description,
            keywords: package.keywords,
            repository: package.repository,
            entry: Some(package.main),
            runtime: Some("bun".into()),
            surfaces: package.muqun.surfaces,
            icon: package.muqun.icon,
            category: package.muqun.category.or_else(default_category),
            tile: Some(PluginTileManifest {
                title,
                kind: "native-view".into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

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
    fn package_repository_links_normalize_standard_forms_and_keep_monorepo_directory() {
        for (repository, url, directory) in [
            (
                serde_json::json!("npm/cli"),
                "https://github.com/npm/cli",
                None,
            ),
            (
                serde_json::json!("github:npm/cli"),
                "https://github.com/npm/cli",
                None,
            ),
            (
                serde_json::json!("git+https://github.com/npm/cli.git"),
                "https://github.com/npm/cli",
                None,
            ),
            (
                serde_json::json!({"type":"git","url":"https://git.example.org/team/project.git","directory":"packages/desktop-plugin"}),
                "https://git.example.org/team/project",
                Some("packages/desktop-plugin"),
            ),
            (
                serde_json::json!({"url":"https://gitlab.example.org/team/project.git","directory":"../outside"}),
                "https://gitlab.example.org/team/project",
                None,
            ),
        ] {
            let manifest = PluginManifest::from_package(serde_json::json!({
                "name":"plugin", "version":"1", "main":"main.ts", "muqun":{},
                "repository":repository,
            }))
            .unwrap()
            .unwrap();
            let source = manifest.repository.unwrap();
            assert_eq!(source.url, url);
            assert_eq!(source.directory.as_deref(), directory);
            assert_eq!(source.source_url(), url);
        }
        let manifest = PluginManifest::from_package(serde_json::json!({
            "name":"monorepo-plugin", "version":"1", "main":"main.ts", "muqun":{},
            "repository":{"url":"git+https://github.com/npm/cli.git","directory":"packages/my plugin#1"},
        })).unwrap().unwrap();
        assert_eq!(
            manifest.repository.unwrap().source_url(),
            "https://github.com/npm/cli/tree/HEAD/packages/my%20plugin%231"
        );
    }

    #[test]
    fn unsafe_optional_metadata_never_becomes_a_source_action_or_breaks_old_plugins() {
        for repository in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!({"url":false}),
            serde_json::json!("javascript:alert(1)"),
            serde_json::json!("file:///etc/passwd"),
            serde_json::json!("git+ssh://git@github.com/team/project.git"),
            serde_json::json!("https://token@git.example.org/team/project"),
            serde_json::json!("../../local"),
        ] {
            let manifest = PluginManifest::from_package(serde_json::json!({
                "name":"old-plugin", "version":"1", "main":"main.ts",
                "muqun":{"category":"future-category"},
                "keywords":[" boards ", 42, ""], "repository":repository,
            }))
            .unwrap()
            .unwrap();
            assert!(manifest.repository.is_none());
            assert_eq!(manifest.category(), PluginCategory::Extensions);
            assert_eq!(manifest.keywords, ["boards"]);
            assert_eq!(manifest.entry.as_deref(), Some("main.ts"));
        }
        let legacy = PluginManifest::parse(
            r#"{"id":"legacy","name":"Legacy","version":"1","description":""}"#,
        )
        .unwrap();
        assert!(legacy.repository.is_none());
        assert!(legacy.keywords.is_empty());
        assert_eq!(legacy.category(), PluginCategory::Extensions);
    }

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
            keywords: vec![],
            repository: None,
            icon: Some("settings".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![PluginSlot::Tile, PluginSlot::Page],
            composer_draft: false,
            notification_entry: None,
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
