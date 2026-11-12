//! Desktop plugin architecture.
//!
//! Provides an extensible trait [`DesktopPlugin`] and registry [`PluginRegistry`]
//! allowing internal modules (e.g. terminal, diff, system diagnostics, notes)
//! and user extensions to seamlessly plug into desktop split tiles, commands,
//! and workspace panels.

use std::collections::HashMap;
use std::sync::Arc;

use rune::{col, el, icon, row, text, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use crate::config::Language;
use crate::theme::color_scheme::ColorScheme;
use crate::theme::look::Look;
use crate::ui::copy::tr_ref;
use crate::ui::icons;
use crate::ui::terminal_panel;
use crate::ui::UiState;

pub mod loader;
pub mod manifest;

pub use loader::{discover_plugins, ExternalPlugin};
pub use manifest::{PluginCategory, PluginCommandManifest, PluginManifest, PluginTileManifest};

/// A command contributed by a plugin.
#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub id: &'static str,
    pub title: String,
    pub shortcut: Option<&'static str>,
}

/// Rendering context passed to a plugin tile view.
pub struct PluginViewContext<'a> {
    pub s: &'a UiState,
    pub k: &'a Kit,
    pub look: &'a Look,
    pub tile_id: &'a str,
    pub scale: f32,
}

/// Core interface for desktop extensions and internal modular panels.
pub trait DesktopPlugin: Send + Sync {
    /// Unique stable identifier (e.g., "terminal", "diff", "sysinfo", "quicknotes").
    fn id(&self) -> &'static str;

    /// Human-readable title in the specified language.
    fn title(&self, lang: Language) -> String;

    /// Icon representing this plugin in toolbars and pickers.
    fn icon(&self) -> &'static Icon;

    /// Optional description of what this plugin provides.
    fn description(&self, lang: Language) -> Option<String> {
        let _ = lang;
        None
    }

    /// Category for grouping in plugin lists and pickers.
    fn category(&self) -> PluginCategory {
        PluginCategory::Tools
    }

    /// Whether this plugin can be instantiated as a split view tile.
    fn supports_tile(&self) -> bool {
        true
    }

    /// Render this plugin inside a tile panel.
    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El>;

    /// Commands exposed to the command palette or keyboard shortcuts.
    fn commands(&self) -> Vec<PluginCommand> {
        Vec::new()
    }

    /// Handle invocation of a plugin command.
    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        let _ = (s, command_id);
        false
    }
}

/// Registry managing active desktop plugins.
#[derive(Clone, Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, Arc<dyn DesktopPlugin>>,
    order: Vec<String>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a registry populated with the default set of built-in plugins
    /// and any discovered external plugins.
    pub fn default_registry() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(TerminalPlugin));
        r.register(Arc::new(SysInfoPlugin));
        r.register(Arc::new(QuickNotesPlugin));
        r.register(Arc::new(DiffPlugin));

        let dirs = loader::default_plugin_dirs();
        for plugin in loader::discover_plugins(&dirs) {
            r.register(Arc::new(plugin));
        }

        r
    }

    /// Register a plugin instance.
    pub fn register(&mut self, plugin: Arc<dyn DesktopPlugin>) {
        let id = plugin.id().to_string();
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.plugins.insert(id, plugin);
    }

    /// Look up a plugin by its id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn DesktopPlugin>> {
        self.plugins.get(id).cloned()
    }

    /// Retrieve all plugins that support split view tiles.
    pub fn tile_plugins(&self) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter_map(|id| self.plugins.get(id))
            .filter(|p| p.supports_tile())
            .cloned()
            .collect()
    }

    /// Retrieve all registered plugins.
    pub fn all(&self) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter_map(|id| self.plugins.get(id))
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 1: Terminal Plugin
// ---------------------------------------------------------------------------

pub struct TerminalPlugin;

impl DesktopPlugin for TerminalPlugin {
    fn id(&self) -> &'static str {
        "terminal"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Terminal").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &terminal_panel::TERMINAL_WINDOW
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Embedded shell with tab management and persistent PTY session",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        let scheme = if ctx.k.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        Some(terminal_panel::tile_body(
            ctx.s, ctx.k, scheme, ctx.scale, layer,
        ))
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![
            PluginCommand {
                id: "terminal.new_tab",
                title: "New Terminal Tab".to_string(),
                shortcut: Some("Ctrl+Shift+T"),
            },
            PluginCommand {
                id: "terminal.close_tab",
                title: "Close Terminal Tab".to_string(),
                shortcut: Some("Ctrl+Shift+W"),
            },
        ]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "terminal.new_tab" => {
                s.term_new_tab();
                true
            }
            "terminal.close_tab" => {
                let tab = s.term.active_tab;
                s.term_close_tab(tab);
                true
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 2: System & Diagnostics Plugin
// ---------------------------------------------------------------------------

pub struct SysInfoPlugin;

impl DesktopPlugin for SysInfoPlugin {
    fn id(&self) -> &'static str {
        "sysinfo"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "System & Diagnostics").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::INFORMATION
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Gateway connectivity, agent health, and workspace metrics",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Diagnostics
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;

        let host_name = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "localhost".to_string());
        let os_type = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        let hub_status = if s.hub_label.is_empty() {
            tr_ref(lang, "Connecting...")
        } else {
            &s.hub_label
        };

        let stat_badge = |label: &str, value: String| {
            col()
                .gap(2.0)
                .px(10.0)
                .py(10.0)
                .grow(1.0)
                .fill(k.alpha(k.c.layer_01, 0.45))
                .radius(k.radius.md)
                .border(1.0, k.alpha(k.c.border_weak, 0.4))
                .child(text(label).font(k.ty.caption()).color(k.c.text_muted))
                .child(text(value).font(k.ty.ui_medium).color(k.c.text_base))
        };

        let metrics_row = row()
            .w_full()
            .gap(8.0)
            .child(stat_badge(
                tr_ref(lang, "Active Sessions"),
                s.sessions.len().to_string(),
            ))
            .child(stat_badge(
                tr_ref(lang, "Discovered Agents"),
                s.agents.len().to_string(),
            ))
            .child(stat_badge(
                tr_ref(lang, "Tracked Directories"),
                s.directories.len().to_string(),
            ));

        let sys_badge = col()
            .w_full()
            .gap(8.0)
            .px(12.0)
            .py(12.0)
            .fill(k.alpha(k.c.layer_01, 0.3))
            .radius(k.radius.md)
            .border(1.0, k.alpha(k.c.border_weak, 0.35))
            .child(
                row()
                    .gap(6.0)
                    .center_y()
                    .child(icon(&icons::SETTINGS, 13.0).color(k.c.text_accent))
                    .child(
                        text(tr_ref(lang, "Environment Details"))
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base),
                    ),
            )
            .child(
                row()
                    .gap(12.0)
                    .wrap()
                    .child(
                        text(format!("{}: {host_name}", tr_ref(lang, "Host")))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    )
                    .child(
                        text(format!("{}: {os_type} / {arch}", tr_ref(lang, "Platform")))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    )
                    .child(
                        text(format!("{}: {hub_status}", tr_ref(lang, "Gateway")))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    ),
            );

        let active_agents_list = {
            let mut col_agents = col().w_full().gap(6.0);
            col_agents = col_agents.child(
                text(tr_ref(lang, "Registered Agents"))
                    .font(k.ty.caption())
                    .color(k.c.text_muted),
            );
            if s.agents.is_empty() {
                col_agents = col_agents.child(
                    text(tr_ref(lang, "No agents discovered yet"))
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                );
            } else {
                for agent in &s.agents {
                    let name = &agent.name;
                    let id = &agent.id;
                    let ver = agent.version.as_deref().unwrap_or("unknown");
                    col_agents = col_agents.child(
                        row()
                            .w_full()
                            .h(28.0)
                            .px(8.0)
                            .center_y()
                            .gap(8.0)
                            .fill(k.alpha(k.c.layer_01, 0.2))
                            .radius(k.radius.sm)
                            .border(1.0, k.alpha(k.c.border_weak, 0.25))
                            .child(icon(&icons::AGENT, 12.0).color(k.c.icon_muted))
                            .child(text(name).font(k.ty.small).color(k.c.text_base))
                            .child(el().grow(1.0))
                            .child(
                                text(format!("id: {id} · v{ver}"))
                                    .font(k.ty.caption())
                                    .color(k.c.text_faint),
                            ),
                    );
                }
            }
            col_agents
        };

        Some(
            col()
                .key(format!("sysinfo-tile-{}", ctx.tile_id))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .px(16.0)
                .py(16.0)
                .gap(12.0)
                .child(metrics_row)
                .child(sys_badge)
                .child(active_agents_list),
        )
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 3: Quick Notes Plugin
// ---------------------------------------------------------------------------

pub struct QuickNotesPlugin;

impl DesktopPlugin for QuickNotesPlugin {
    fn id(&self) -> &'static str {
        "quicknotes"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Quick Notes").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::TASK
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Fast workspace scratchpad for drafting prompts, ideas, and commands",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Tools
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id.to_string();

        let current_text = s.quick_notes.get(&tile_id).cloned().unwrap_or_default();

        let header_row = row()
            .w_full()
            .h(32.0)
            .px(8.0)
            .gap(6.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.45))
            .child(icon(&icons::TASK, 13.0).color(k.c.text_accent))
            .child(
                text(tr_ref(lang, "Scratchpad"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(el().grow(1.0))
            .child(
                Button::new(tr_ref(lang, "Clear"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press({
                        let tid = tile_id.clone();
                        move |s: &mut UiState| {
                            s.quick_notes.remove(&tid);
                        }
                    })
                    .build(k)
                    .key(format!("notes-clear-{tile_id}")),
            );

        let note_body = if current_text.is_empty() {
            col()
                .grow(1.0)
                .w_full()
                .center()
                .gap(6.0)
                .child(
                    text(tr_ref(lang, "No notes written yet"))
                        .font(k.ty.body)
                        .color(k.c.text_muted),
                )
                .child(
                    text(tr_ref(
                        lang,
                        "Type or paste snippets, commands, or thoughts here",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_faint),
                )
        } else {
            col().grow(1.0).w_full().px(12.0).py(12.0).child(
                text(current_text)
                    .font(k.ty.mono)
                    .color(k.c.text_base)
                    .wrap(),
            )
        };

        Some(
            col()
                .key(format!("quicknotes-tile-{tile_id}"))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header_row)
                .child(note_body),
        )
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 4: Diff Inspector Plugin
// ---------------------------------------------------------------------------

pub struct DiffPlugin;

impl DesktopPlugin for DiffPlugin {
    fn id(&self) -> &'static str {
        "diff"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Diff Inspector").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::BRANCH
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(tr_ref(lang, "Workspace working tree status and git diff viewer").to_string())
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;

        let header_row = row()
            .w_full()
            .h(32.0)
            .px(8.0)
            .gap(6.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.45))
            .child(icon(&icons::BRANCH, 13.0).color(k.c.text_accent))
            .child(
                text(tr_ref(lang, "Working Tree Changes"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(el().grow(1.0))
            .child(
                Button::new(tr_ref(lang, "Open Inspector"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        s.open_git_inspector();
                    })
                    .build(k)
                    .key("diff-open-inspector"),
            );

        let content = col()
            .grow(1.0)
            .w_full()
            .center()
            .gap(8.0)
            .child(
                text(tr_ref(lang, "Git Working Tree"))
                    .font(k.ty.body_medium)
                    .color(k.c.text_base),
            )
            .child(
                text(tr_ref(
                    lang,
                    "Use the Git Inspector in the right panel or click Open Inspector above",
                ))
                .font(k.ty.caption())
                .color(k.c.text_muted),
            );

        Some(
            col()
                .key(format!("diff-tile-{}", ctx.tile_id))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header_row)
                .child(content),
        )
    }
}
