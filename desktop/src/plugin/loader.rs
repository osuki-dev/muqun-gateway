//! Plugin discovery and external plugin runtime loader.
//!
//! Scans configured plugin search paths for folders containing `plugin.json`,
//! parses their manifests, and instantiates [`ExternalPlugin`] which implements
//! [`DesktopPlugin`].

use std::path::{Path, PathBuf};

use rune::{col, el, icon, row, text, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Markdown};

use crate::config::Language;
use crate::plugin::manifest::{PluginCategory, PluginManifest};
use crate::plugin::{DesktopPlugin, PluginCommand, PluginViewContext};
use crate::ui::copy::tr_ref;
use crate::ui::icons;
use crate::ui::terminal_panel;
use crate::ui::UiState;

/// An external file-based plugin loaded from a `plugin.json` manifest.
pub struct ExternalPlugin {
    id: &'static str,
    manifest: PluginManifest,
    dir: PathBuf,
    commands: Vec<PluginCommand>,
}

impl ExternalPlugin {
    /// Construct an external plugin instance from a parsed manifest and directory.
    pub fn new(manifest: PluginManifest, dir: PathBuf) -> Self {
        let id: &'static str = Box::leak(manifest.id.clone().into_boxed_str());
        let commands = manifest
            .commands
            .iter()
            .map(|c| {
                let cid: &'static str = Box::leak(c.id.clone().into_boxed_str());
                let shortcut: Option<&'static str> = c
                    .shortcut
                    .as_ref()
                    .map(|s| &*Box::leak(s.clone().into_boxed_str()));
                PluginCommand {
                    id: cid,
                    title: c.title.clone(),
                    shortcut,
                }
            })
            .collect();

        Self {
            id,
            manifest,
            dir,
            commands,
        }
    }

    /// Load an external plugin from a directory containing `plugin.json`.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        let manifest_path = dir.join("plugin.json");
        let manifest = PluginManifest::from_file(&manifest_path)?;
        if manifest.id.trim().is_empty() {
            anyhow::bail!("Plugin manifest in {} has an empty id", dir.display());
        }
        Ok(Self::new(manifest, dir.to_path_buf()))
    }

    /// Access the underlying manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Access the directory where the plugin is located.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Attempt to load markdown content from configured tile entry or README.md.
    fn load_markdown_content(&self) -> Option<String> {
        if let Some(tile) = &self.manifest.tile {
            if tile.kind == "markdown"
                || tile
                    .entry
                    .as_ref()
                    .is_some_and(|e| e.ends_with(".md") || e.ends_with(".markdown"))
            {
                if let Some(entry) = &tile.entry {
                    let entry_path = if Path::new(entry).is_absolute() {
                        PathBuf::from(entry)
                    } else {
                        self.dir.join(entry)
                    };
                    return match std::fs::read_to_string(&entry_path) {
                        Ok(content) => Some(content),
                        Err(err) => Some(format!(
                            "# {}\n\n*Failed to read markdown file `{}`: {}*",
                            tile.title,
                            entry_path.display(),
                            err
                        )),
                    };
                } else {
                    return Some(format!("# {}\n\n{}", tile.title, self.manifest.description));
                }
            }
        }

        let readme = self.dir.join("README.md");
        if readme.is_file() {
            if let Ok(content) = std::fs::read_to_string(&readme) {
                return Some(content);
            }
        }

        None
    }
}

impl DesktopPlugin for ExternalPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self, _lang: Language) -> String {
        self.manifest.name.clone()
    }

    fn icon(&self) -> &'static Icon {
        resolve_icon(self.manifest.icon.as_deref())
    }

    fn description(&self, _lang: Language) -> Option<String> {
        Some(self.manifest.description.clone())
    }

    fn category(&self) -> PluginCategory {
        self.manifest.category()
    }

    fn supports_tile(&self) -> bool {
        true
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let look = ctx.look;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id;
        let pid = self.id.to_string();

        let header_icon = icon(self.icon(), 14.0).color(k.c.text_accent);
        let title_text = text(&self.manifest.name)
            .font(k.ty.ui_medium)
            .color(k.c.text_base);
        let category_badge = text(self.category().label(lang))
            .font(k.ty.caption())
            .color(k.c.text_muted);
        let version_badge = text(format!("v{}", self.manifest.version))
            .font(k.ty.caption())
            .color(k.c.text_faint);

        let mut header = row()
            .w_full()
            .h(36.0)
            .px(12.0)
            .gap(8.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.45))
            .child(header_icon)
            .child(title_text)
            .child(category_badge)
            .child(version_badge)
            .child(el().grow(1.0));

        if let Some(author) = &self.manifest.author {
            header = header.child(
                text(format!("by {author}"))
                    .font(k.ty.caption())
                    .color(k.c.text_faint),
            );
        }

        let divider = el().w_full().h(1.0).fill(k.alpha(k.c.border_weak, 0.35));

        let md_content = self.load_markdown_content();

        let body = if let Some(content) = md_content {
            let mut col_inner = col().w_full().max_w(760.0).px(24.0).py(16.0).gap(12.0);

            if !self.manifest.commands.is_empty() {
                let mut cmd_bar = row()
                    .w_full()
                    .wrap()
                    .gap(8.0)
                    .center_y()
                    .px(12.0)
                    .py(8.0)
                    .fill(k.alpha(k.c.layer_01, 0.3))
                    .radius(k.radius.sm)
                    .border(1.0, k.alpha(k.c.border_weak, 0.3));

                for cmd in &self.manifest.commands {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let btn = Button::new(&cmd.title)
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) = s.plugins.get(&pid_clone) {
                                p.on_command(s, &cmd_id);
                            }
                        })
                        .build(k);
                    cmd_bar = cmd_bar.child(btn);
                }
                col_inner = col_inner.child(cmd_bar);
            }

            col_inner = col_inner.child(
                Markdown::new(content)
                    .selectable(true)
                    .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
                        crate::ui::open_markdown_link(s, url, tx);
                    })
                    .build(k),
            );

            col()
                .key(format!("ext-plugin-md-{tile_id}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(col().w_full().items_center().child(col_inner))
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
        } else {
            let mut content_col = col().w_full().max_w(720.0).px(24.0).py(20.0).gap(16.0);

            let desc_card = look.row_card(
                k,
                col()
                    .w_full()
                    .px(14.0)
                    .py(14.0)
                    .gap(6.0)
                    .child(
                        text(tr_ref(lang, "About"))
                            .font(k.ty.caption())
                            .color(k.c.text_muted),
                    )
                    .child(
                        text(&self.manifest.description)
                            .font(k.ty.body)
                            .color(k.c.text_base),
                    ),
                k.radius.md,
            );
            content_col = content_col.child(desc_card);

            if let Some(tile) = &self.manifest.tile {
                if tile.kind == "webview" {
                    let entry_str = tile.entry.as_deref().unwrap_or("about:blank");
                    let webview_card = look.row_card(
                        k,
                        col()
                            .w_full()
                            .px(14.0)
                            .py(14.0)
                            .gap(8.0)
                            .child(
                                row()
                                    .gap(6.0)
                                    .center_y()
                                    .child(icon(&icons::OPEN_EXTERNAL, 12.0).color(k.c.text_accent))
                                    .child(
                                        text(&tile.title).font(k.ty.ui_medium).color(k.c.text_base),
                                    ),
                            )
                            .child(
                                text(format!("Entry: {entry_str}"))
                                    .font(k.ty.mono)
                                    .color(k.c.text_muted),
                            )
                            .child(
                                Button::new(tr_ref(lang, "Open in Browser"))
                                    .variant(ButtonVariant::Outline)
                                    .control_size(ControlSize::Small)
                                    .on_press({
                                        let url = entry_str.to_string();
                                        move |_s: &mut UiState| {
                                            crate::platform::shell::open_url(&url);
                                        }
                                    })
                                    .build(k),
                            ),
                        k.radius.md,
                    );
                    content_col = content_col.child(webview_card);
                }
            }

            if !self.manifest.commands.is_empty() {
                let mut commands_col = col().w_full().gap(8.0).child(
                    text(tr_ref(lang, "Commands"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                );

                for cmd in &self.manifest.commands {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let mut cmd_row = row()
                        .w_full()
                        .h(36.0)
                        .px(10.0)
                        .center_y()
                        .gap(10.0)
                        .fill(k.alpha(k.c.layer_01, 0.3))
                        .radius(k.radius.sm)
                        .border(1.0, k.alpha(k.c.border_weak, 0.3))
                        .child(icon(&icons::KEYBOARD, 12.0).color(k.c.icon_muted))
                        .child(text(&cmd.title).font(k.ty.small).color(k.c.text_base));

                    if let Some(sc) = &cmd.shortcut {
                        cmd_row =
                            cmd_row.child(text(sc).font(k.ty.caption()).color(k.c.text_faint));
                    }

                    cmd_row = cmd_row.child(el().grow(1.0));

                    let run_btn = Button::new(tr_ref(lang, "Run"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) = s.plugins.get(&pid_clone) {
                                p.on_command(s, &cmd_id);
                            }
                        })
                        .build(k);

                    cmd_row = cmd_row.child(run_btn);
                    commands_col = commands_col.child(cmd_row);
                }

                content_col = content_col.child(commands_col);
            }

            col()
                .key(format!("ext-plugin-content-{tile_id}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .items_center()
                .child(content_col)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
        };

        Some(
            col()
                .key(format!("ext-plugin-tile-{}-{}", self.id, tile_id))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header)
                .child(divider)
                .child(body),
        )
    }

    fn commands(&self) -> Vec<PluginCommand> {
        self.commands.clone()
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        if let Some(cmd) = self.manifest.commands.iter().find(|c| c.id == command_id) {
            if let Some(action) = &cmd.action {
                if action.starts_with("http://") || action.starts_with("https://") {
                    crate::platform::shell::open_url(action);
                }
            }
            s.toast(
                format!("{}: {}", self.manifest.name, cmd.title),
                cmd.action.clone(),
            );
            true
        } else {
            false
        }
    }
}

/// Resolve icon string from manifest to static Rune Icon.
pub fn resolve_icon(name: Option<&str>) -> &'static Icon {
    match name.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("terminal") | Some("shell") | Some("console") => &terminal_panel::TERMINAL_WINDOW,
        Some("task") | Some("notes") | Some("todo") | Some("note") => &icons::TASK,
        Some("settings") | Some("config") | Some("preferences") => &icons::SETTINGS,
        Some("branch") | Some("diff") | Some("git") | Some("vcs") => &icons::BRANCH,
        Some("running") | Some("play") | Some("run") => &icons::RUNNING,
        Some("keyboard") | Some("command") | Some("shortcuts") => &icons::KEYBOARD,
        Some("agent") | Some("bot") | Some("ai") => &icons::AGENT,
        Some("info") | Some("information") | Some("help") => &icons::INFORMATION,
        Some("external") | Some("open_external") | Some("link") | Some("browser") => {
            &icons::OPEN_EXTERNAL
        }
        Some("controls") | Some("sliders") => &icons::CONTROLS,
        Some("appearance") | Some("theme") | Some("paint") => &icons::APPEARANCE,
        Some("storage") | Some("database") | Some("disk") => &icons::STORAGE,
        Some("refresh") | Some("reload") | Some("sync") => &icons::REFRESH,
        Some("grid") | Some("dashboard") | Some("table") => &icons::GRID,
        Some("fork") => &icons::FORK,
        Some("inbox") => &icons::INBOX,
        _ => &icons::GRID,
    }
}

/// Returns the standard directories scanned for external plugins.
pub fn default_plugin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(config_dir) = dirs::config_dir() {
        dirs.push(config_dir.join("muqun-desktop").join("plugins"));
    }
    if let Some(data_dir) = dirs::data_dir() {
        dirs.push(data_dir.join("muqun-desktop").join("plugins"));
    }
    #[cfg(target_os = "linux")]
    {
        dirs.push(PathBuf::from("/usr/share/muqun-desktop/plugins"));
        dirs.push(PathBuf::from("/usr/local/share/muqun-desktop/plugins"));
    }

    let local_assets = PathBuf::from("desktop/assets/plugins");
    if local_assets.is_dir() {
        dirs.push(local_assets);
    }
    let crate_assets = PathBuf::from("assets/plugins");
    if crate_assets.is_dir() && !dirs.contains(&crate_assets) {
        dirs.push(crate_assets);
    }

    dirs
}

/// Discover external plugins located in the specified search directories.
pub fn discover_plugins(plugin_dirs: &[PathBuf]) -> Vec<ExternalPlugin> {
    let mut plugins = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for base_dir in plugin_dirs {
        if !base_dir.is_dir() {
            continue;
        }

        // Direct directory check
        let direct_manifest = base_dir.join("plugin.json");
        if direct_manifest.is_file() {
            match ExternalPlugin::from_dir(base_dir) {
                Ok(plugin) => {
                    let id = plugin.manifest().id.clone();
                    if seen_ids.insert(id) {
                        plugins.push(plugin);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        path = %base_dir.display(),
                        error = %err,
                        "Failed to load external plugin"
                    );
                }
            }
            continue;
        }

        // Subdirectories check
        let read_dir = match std::fs::read_dir(base_dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };

        let mut entries: Vec<_> = read_dir.flatten().collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if manifest_path.is_file() {
                match ExternalPlugin::from_dir(&path) {
                    Ok(plugin) => {
                        let id = plugin.manifest().id.clone();
                        if seen_ids.insert(id) {
                            plugins.push(plugin);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "Failed to load external plugin"
                        );
                    }
                }
            }
        }
    }

    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_plugins_scans_and_filters() {
        let temp_dir =
            std::env::temp_dir().join(format!("muqun_test_plugins_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 1. Valid plugin A
        let p_a = temp_dir.join("plugin-a");
        std::fs::create_dir_all(&p_a).unwrap();
        std::fs::write(
            p_a.join("plugin.json"),
            r#"{
                "id": "plugin-a",
                "name": "Plugin A",
                "version": "1.0.0",
                "description": "First test plugin"
            }"#,
        )
        .unwrap();

        // 2. Valid plugin B with README
        let p_b = temp_dir.join("plugin-b");
        std::fs::create_dir_all(&p_b).unwrap();
        std::fs::write(
            p_b.join("plugin.json"),
            r#"{
                "id": "plugin-b",
                "name": "Plugin B",
                "version": "2.0.0",
                "description": "Second test plugin",
                "tile": {
                    "kind": "markdown",
                    "title": "B Guide",
                    "entry": "README.md"
                }
            }"#,
        )
        .unwrap();
        std::fs::write(p_b.join("README.md"), "# Plugin B Content\n\nHello world").unwrap();

        // 3. Invalid plugin (malformed JSON)
        let p_c = temp_dir.join("plugin-invalid");
        std::fs::create_dir_all(&p_c).unwrap();
        std::fs::write(p_c.join("plugin.json"), "{ invalid json").unwrap();

        // 4. Directory without plugin.json
        let p_d = temp_dir.join("not-a-plugin");
        std::fs::create_dir_all(&p_d).unwrap();

        let discovered = discover_plugins(std::slice::from_ref(&temp_dir));
        assert_eq!(discovered.len(), 2);

        let ids: Vec<_> = discovered.iter().map(|p| p.id()).collect();
        assert!(ids.contains(&"plugin-a"));
        assert!(ids.contains(&"plugin-b"));

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_plugins_deduplicates_by_id() {
        let root = std::env::temp_dir().join(format!("muqun_dedup_{}", uuid::Uuid::new_v4()));
        let dir1 = root.join("dir1");
        let dir2 = root.join("dir2");
        std::fs::create_dir_all(dir1.join("same-plugin")).unwrap();
        std::fs::create_dir_all(dir2.join("same-plugin")).unwrap();

        let manifest = r#"{
            "id": "duplicate-id",
            "name": "Same Plugin",
            "version": "1.0.0",
            "description": "Dedup test"
        }"#;

        std::fs::write(dir1.join("same-plugin").join("plugin.json"), manifest).unwrap();
        std::fs::write(dir2.join("same-plugin").join("plugin.json"), manifest).unwrap();

        let plugins = discover_plugins(&[dir1, dir2]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id(), "duplicate-id");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sample_plugin_loads_successfully() {
        let manifest_path =
            if Path::new("desktop/assets/plugins/sample-plugin/plugin.json").is_file() {
                PathBuf::from("desktop/assets/plugins/sample-plugin")
            } else {
                PathBuf::from("assets/plugins/sample-plugin")
            };

        let plugin = ExternalPlugin::from_dir(&manifest_path).expect("sample plugin must load");
        assert_eq!(plugin.id(), "sample-plugin");
        assert_eq!(plugin.title(Language::En), "Sample Workspace Guide");
        assert_eq!(plugin.category(), PluginCategory::Extensions);
        assert!(plugin.supports_tile());
        assert_eq!(plugin.commands().len(), 2);

        let md = plugin.load_markdown_content();
        assert!(md.is_some());
        assert!(md.unwrap().contains("Sample Desktop Plugin"));
    }

    #[test]
    fn external_plugin_trait_and_command_handling() {
        use crate::plugin::manifest::{PluginCommandManifest, PluginTileManifest};

        let manifest = PluginManifest {
            id: "ext-test".to_string(),
            name: "Ext Test".to_string(),
            version: "0.2.0".to_string(),
            description: "Test description".to_string(),
            author: Some("Tester".to_string()),
            category: Some(PluginCategory::Tools),
            icon: Some("terminal".to_string()),
            commands: vec![PluginCommandManifest {
                id: "ext.run".to_string(),
                title: "Run Command".to_string(),
                shortcut: Some("Ctrl+R".to_string()),
                action: Some("Executed test".to_string()),
            }],
            tile: Some(PluginTileManifest {
                kind: "command".to_string(),
                title: "Command Tile".to_string(),
                entry: None,
            }),
        };

        let plugin = ExternalPlugin::new(manifest, PathBuf::from("/tmp"));
        assert_eq!(plugin.id(), "ext-test");
        assert_eq!(plugin.title(Language::En), "Ext Test");
        assert_eq!(plugin.category(), PluginCategory::Tools);
        assert_eq!(
            plugin.description(Language::En).as_deref(),
            Some("Test description")
        );
        assert!(plugin.supports_tile());

        let cmds = plugin.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].id, "ext.run");
        assert_eq!(cmds[0].shortcut, Some("Ctrl+R"));

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        assert!(plugin.on_command(&mut s, "ext.run"));
        assert!(!s.toasts.is_empty());
        assert!(!plugin.on_command(&mut s, "unknown.command"));
    }
}
