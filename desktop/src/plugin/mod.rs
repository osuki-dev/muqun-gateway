//! Desktop plugin architecture.
//!
//! Provides an extensible trait [`DesktopPlugin`] and registry [`PluginRegistry`]
//! allowing internal modules (e.g. terminal, diff, system diagnostics, notes)
//! and user extensions to seamlessly plug into desktop split tiles, full-screen
//! pages, right-hand drawers, commands, and workspace panels.

use std::collections::HashMap;
use std::sync::Arc;

use muqun_gateway::agents::AgentSessionId;
use muqun_gateway::platform::runtimes::{RuntimeKind, RuntimeSource};
use rune::{col, el, icon, row, text, El, Icon, ScrollSpec, Tx};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, ScrollBar, TextArea};

use crate::config::Language;
use crate::model::commands::Command;
use crate::theme::color_scheme::ColorScheme;
use crate::theme::look::Look;
use crate::ui::copy::tr_ref;
use crate::ui::icons;
use crate::ui::terminal_panel;
use crate::ui::{Mode, ToastLevel, UiState};

pub mod loader;
pub mod manifest;

pub use loader::{discover_plugins, BunScriptResponse, BunToastResponse, ExternalPlugin};
pub use manifest::{
    PluginCategory, PluginCommandManifest, PluginManifest, PluginRenderKind, PluginSlot,
    PluginSurface, PluginTileManifest,
};

/// A command contributed by a plugin.
#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub id: &'static str,
    pub title: String,
    pub shortcut: Option<&'static str>,
}

/// Rendering context passed to a plugin surface view.
pub struct PluginViewContext<'a> {
    pub s: &'a UiState,
    pub k: &'a Kit,
    pub look: &'a Look,
    pub tile_id: &'a str,
    pub scale: f32,
    pub surface: PluginSlot,
}

impl<'a> PluginViewContext<'a> {
    pub fn new(
        s: &'a UiState,
        k: &'a Kit,
        look: &'a Look,
        tile_id: &'a str,
        scale: f32,
        surface: PluginSlot,
    ) -> Self {
        Self {
            s,
            k,
            look,
            tile_id,
            scale,
            surface,
        }
    }
}

/// Unified Surface Switcher bar rendered on plugin headers so users can seamlessly
/// switch any plugin between **Tile (Split View) ⇄ Page (Full Screen) ⇄ Drawer (Right Panel)**.
pub fn surface_switcher(k: &Kit, lang: Language, plugin_id: &str, active: PluginSlot) -> El {
    let mut bar = row()
        .key(format!("surface-switcher-{plugin_id}-{active:?}"))
        .gap(4.0)
        .center_y();

    for (slot, label_en) in [
        (PluginSlot::Tile, "Tile"),
        (PluginSlot::Page, "Page"),
        (PluginSlot::Drawer, "Drawer"),
    ] {
        let pid = plugin_id.to_string();
        let is_active = active == slot;
        let btn = Button::new(tr_ref(lang, label_en))
            .variant(if is_active {
                ButtonVariant::Contrast
            } else {
                ButtonVariant::Ghost
            })
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                s.switch_plugin_surface(&pid, slot);
            })
            .build(k)
            .key(format!("surf-btn-{plugin_id}-{slot:?}"));
        bar = bar.child(btn);
    }

    if matches!(active, PluginSlot::Page | PluginSlot::Drawer) {
        let close_btn = Button::new(tr_ref(lang, "Close"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| match active {
                PluginSlot::Page => s.close_plugin_page(),
                PluginSlot::Drawer => s.close_plugin_drawer(),
                _ => {}
            })
            .build(k)
            .key(format!("surf-close-{plugin_id}-{active:?}"));
        bar = bar.child(close_btn);
    }

    bar
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

    /// Display surfaces supported by this plugin.
    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Tile, PluginSlot::Page, PluginSlot::Drawer]
    }

    /// Whether this plugin supports mounting in `slot`.
    fn supports_surface(&self, slot: PluginSlot) -> bool {
        self.surfaces().contains(&slot)
    }

    /// Render mode used by this plugin.
    fn render_kind(&self) -> PluginRenderKind {
        PluginRenderKind::default()
    }

    /// Whether this plugin can be instantiated as a split view tile.
    fn supports_tile(&self) -> bool {
        self.supports_surface(PluginSlot::Tile)
    }

    /// Render this plugin inside a tile panel.
    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El>;

    /// Render this plugin in any surface (`Tile`, `Page`, `Drawer`, `Modal`).
    fn render_surface(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        self.render_tile(ctx, layer)
    }

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
        r.register(Arc::new(DiffPlugin));
        r.register(Arc::new(InspectorPlugin));
        r.register(Arc::new(TasksPlugin));
        r.register(Arc::new(InboxPlugin));
        r.register(Arc::new(SettingsPlugin));
        r.register(Arc::new(SysInfoPlugin));
        r.register(Arc::new(QuickNotesPlugin));

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

    /// Retrieve all plugins that support a given [`PluginSlot`].
    pub fn plugins_for_surface(&self, slot: PluginSlot) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter_map(|id| self.plugins.get(id))
            .filter(|p| p.supports_surface(slot))
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

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![
            PluginSlot::Tile,
            PluginSlot::Drawer,
            PluginSlot::Page,
            PluginSlot::Toolbar,
        ]
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        let scheme = if ctx.k.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        let lang = ctx.s.cfg.general.language;
        let body = terminal_panel::tile_body(ctx.s, ctx.k, scheme, ctx.scale, layer);
        if matches!(ctx.surface, PluginSlot::Page | PluginSlot::Drawer) {
            let header = row()
                .w_full()
                .h(36.0)
                .px(12.0)
                .gap(8.0)
                .center_y()
                .fill(ctx.k.alpha(ctx.k.c.layer_01, 0.45))
                .child(icon(&terminal_panel::TERMINAL_WINDOW, 14.0).color(ctx.k.c.text_accent))
                .child(
                    text(tr_ref(lang, "Terminal"))
                        .font(ctx.k.ty.ui_medium)
                        .color(ctx.k.c.text_base),
                )
                .child(el().grow(1.0))
                .child(surface_switcher(ctx.k, lang, self.id(), ctx.surface));
            return Some(
                col()
                    .key(format!("term-plugin-{:?}-{}", ctx.surface, ctx.tile_id))
                    .grow(1.0)
                    .w_full()
                    .min_h(0.0)
                    .child(header)
                    .child(body),
            );
        }
        Some(body)
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
            PluginCommand {
                id: "terminal.toggle_drawer",
                title: "Toggle Terminal Drawer".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "terminal.open_split",
                title: "Open Terminal in Split Tile".to_string(),
                shortcut: None,
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
            "terminal.toggle_drawer" => {
                s.toggle_terminal();
                true
            }
            "terminal.open_split" => {
                s.open_terminal_tile();
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
                "Gateway connectivity, agent health, managed runtimes (Bun & Herdr), and workspace metrics",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Diagnostics
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Tile, PluginSlot::Page, PluginSlot::Drawer]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![
            PluginCommand {
                id: "sysinfo.refresh_runtimes",
                title: "Refresh Runtime Status (Bun & Herdr)".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "sysinfo.install_bun",
                title: "Install / Update Managed Bun Runtime".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "sysinfo.install_herdr",
                title: "Install / Update Managed Herdr Runtime".to_string(),
                shortcut: None,
            },
        ]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "sysinfo.refresh_runtimes" => {
                s.refresh_runtimes();
                s.notify(
                    ToastLevel::Info,
                    "Refreshed runtime status",
                    Some("Probed Bun and Herdr runtimes".to_string()),
                );
                true
            }
            "sysinfo.install_bun" => {
                s.install_managed_runtime(RuntimeKind::Bun);
                true
            }
            "sysinfo.install_herdr" => {
                s.install_managed_runtime(RuntimeKind::Herdr);
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id;

        let header_row = row()
            .w_full()
            .h(36.0)
            .px(12.0)
            .gap(8.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.45))
            .child(icon(&icons::INFORMATION, 14.0).color(k.c.text_accent))
            .child(
                text(tr_ref(lang, "System & Diagnostics"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(el().grow(1.0))
            .child(surface_switcher(k, lang, self.id(), ctx.surface));

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

        // Managed Runtimes (Bun & Herdr) section
        let runtimes_section = {
            let mut rt_col = col()
                .key(format!("sysinfo-runtimes-{tile_id}"))
                .w_full()
                .gap(8.0)
                .px(12.0)
                .py(12.0)
                .fill(k.alpha(k.c.layer_01, 0.3))
                .radius(k.radius.md)
                .border(1.0, k.alpha(k.c.border_weak, 0.35));

            let rt_header = row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(icon(&icons::STORAGE, 13.0).color(k.c.text_accent))
                .child(
                    text(tr_ref(lang, "Managed Runtimes (Bun & Herdr)"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                )
                .child(el().grow(1.0))
                .child(
                    Button::new(tr_ref(lang, "Refresh"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(|s: &mut UiState| {
                            s.refresh_runtimes();
                        })
                        .build(k)
                        .key(format!("sysinfo-rt-refresh-{tile_id}")),
                );
            rt_col = rt_col.child(rt_header);

            for status in &s.runtime_statuses {
                let kind = status.kind;
                let is_installing = s.runtime_installing.contains(&kind);
                let (source_label, badge_variant) = match &status.source {
                    Some(RuntimeSource::System(_)) => ("System", BadgeVariant::Accent),
                    Some(RuntimeSource::Managed(_)) => ("Managed", BadgeVariant::Accent),
                    Some(RuntimeSource::Configured(_)) => ("Configured", BadgeVariant::Accent),
                    None => ("Not Installed", BadgeVariant::Subtle),
                };

                let version_text = status
                    .version
                    .as_deref()
                    .map(|v| format!("v{v}"))
                    .unwrap_or_else(|| tr_ref(lang, "unavailable").to_string());
                let path_text = status
                    .binary_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| {
                        format!(
                            "managed path: {}",
                            status.managed_dir.join("current").display()
                        )
                    });

                let action_label = if is_installing {
                    tr_ref(lang, "Installing…")
                } else if status.available {
                    tr_ref(lang, "Update Managed Runtime")
                } else {
                    tr_ref(lang, "Install Managed Runtime")
                };

                let install_btn = Button::new(action_label)
                    .variant(if status.available {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Outline
                    })
                    .control_size(ControlSize::Small)
                    .disabled(is_installing)
                    .on_press(move |s: &mut UiState| {
                        s.install_managed_runtime(kind);
                    })
                    .build(k)
                    .key(format!("sysinfo-rt-install-{}-{tile_id}", kind.id()));

                let rt_card = col()
                    .key(format!("sysinfo-rt-card-{}-{tile_id}", kind.id()))
                    .w_full()
                    .gap(4.0)
                    .px(10.0)
                    .py(8.0)
                    .fill(k.alpha(k.c.layer_01, 0.25))
                    .radius(k.radius.sm)
                    .border(1.0, k.alpha(k.c.border_weak, 0.25))
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(
                                text(kind.display_name())
                                    .font(k.ty.ui_medium)
                                    .color(k.c.text_base),
                            )
                            .child(
                                Badge::new(tr_ref(lang, source_label))
                                    .variant(badge_variant)
                                    .build(k),
                            )
                            .child(text(version_text).font(k.ty.mono).color(k.c.text_muted))
                            .child(el().grow(1.0))
                            .child(install_btn),
                    )
                    .child(
                        text(path_text)
                            .font(k.ty.caption())
                            .color(k.c.text_faint)
                            .truncate(),
                    );

                rt_col = rt_col.child(rt_card);
            }

            rt_col
        };

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

        let body = col()
            .key(format!("sysinfo-body-{tile_id}"))
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .px(16.0)
            .py(16.0)
            .gap(12.0)
            .child(metrics_row)
            .child(runtimes_section)
            .child(sys_badge)
            .child(active_agents_list)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .scroll_bar(k);

        Some(
            col()
                .key(format!("sysinfo-tile-{tile_id}"))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header_row)
                .child(body),
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

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Tile, PluginSlot::Drawer, PluginSlot::Page]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![
            PluginCommand {
                id: "quicknotes.insert_composer",
                title: "Insert Scratchpad Notes into Composer".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "quicknotes.clear",
                title: "Clear Scratchpad Notes".to_string(),
                shortcut: None,
            },
        ]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "quicknotes.insert_composer" => {
                s.insert_quick_notes_into_composer();
                true
            }
            "quicknotes.clear" => {
                s.clear_quick_notes();
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id.to_string();

        let has_notes = !s.quick_notes_edit.text.trim().is_empty();

        let header_row = row()
            .w_full()
            .h(36.0)
            .px(10.0)
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
            .child(surface_switcher(k, lang, self.id(), ctx.surface));

        let actions_bar = row()
            .w_full()
            .px(10.0)
            .py(6.0)
            .gap(6.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.25))
            .child(
                Button::new(tr_ref(lang, "Insert into Composer"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .disabled(!has_notes)
                    .on_press(|s: &mut UiState| {
                        s.insert_quick_notes_into_composer();
                    })
                    .build(k)
                    .key(format!("notes-insert-{tile_id}")),
            )
            .child(
                Button::new(tr_ref(lang, "Copy"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .disabled(!has_notes)
                    .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
                        let content = s.quick_notes_edit.text.clone();
                        if !content.is_empty() {
                            tx.copy(content);
                            s.notify(
                                ToastLevel::Success,
                                tr_ref(s.cfg.general.language, "Copied scratchpad notes"),
                                None,
                            );
                        }
                    })
                    .build(k)
                    .key(format!("notes-copy-{tile_id}")),
            )
            .child(el().grow(1.0))
            .child(
                Button::new(tr_ref(lang, "Clear"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .disabled(!has_notes)
                    .on_press({
                        let tid = tile_id.clone();
                        move |s: &mut UiState| {
                            s.clear_quick_notes();
                            s.quick_notes.remove(&tid);
                        }
                    })
                    .build(k)
                    .key(format!("notes-clear-{tile_id}")),
            );

        let editor = TextArea::new(&s.quick_notes_edit)
            .placeholder(tr_ref(
                lang,
                "Type or paste snippets, commands, prompts, or thoughts here…",
            ))
            .label(tr_ref(lang, "Scratchpad"))
            .width(None)
            .rows(10)
            .on_edit(|s: &mut UiState| &mut s.quick_notes_edit)
            .on_change({
                let tid = tile_id.clone();
                move |s: &mut UiState, _: &mut Tx| {
                    let current = s.quick_notes_edit.text.clone();
                    s.quick_notes.insert(tid.clone(), current.clone());
                    s.quick_notes.insert("default".to_string(), current);
                }
            })
            .build(k)
            .key(format!("notes-textarea-{tile_id}"))
            .w_full();

        let note_body = col()
            .grow(1.0)
            .w_full()
            .min_h(0.0)
            .px(12.0)
            .py(10.0)
            .gap(8.0)
            .child(editor);

        Some(
            col()
                .key(format!("quicknotes-tile-{tile_id}"))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header_row)
                .child(actions_bar)
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

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Tile, PluginSlot::Drawer, PluginSlot::Page]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![
            PluginCommand {
                id: "diff.open_view",
                title: "Open Full Working Tree Diff View".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "diff.open_inspector",
                title: "Open Git Inspector Drawer".to_string(),
                shortcut: None,
            },
            PluginCommand {
                id: "diff.refresh",
                title: "Refresh Working Tree Diff".to_string(),
                shortcut: None,
            },
        ]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "diff.open_view" => {
                if let Some(asid) = s.selected.clone() {
                    s.send(Command::OpenDiff {
                        asid: AgentSessionId(asid),
                        file: None,
                    });
                }
                s.close_plugin_page();
                s.mode = Mode::Session;
                s.show_diff = true;
                true
            }
            "diff.open_inspector" => {
                s.close_plugin_drawer();
                s.open_git_inspector();
                true
            }
            "diff.refresh" => {
                if let Some(asid) = s.selected.clone() {
                    s.send(Command::RefreshDiff(AgentSessionId(asid)));
                }
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id;

        let header_row = row()
            .w_full()
            .h(36.0)
            .px(10.0)
            .gap(8.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.45))
            .child(icon(&icons::BRANCH, 13.0).color(k.c.text_accent))
            .child(
                text(tr_ref(lang, "Working Tree Changes"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(el().grow(1.0))
            .child(surface_switcher(k, lang, self.id(), ctx.surface));

        let actions_bar = row()
            .w_full()
            .px(10.0)
            .py(6.0)
            .gap(6.0)
            .center_y()
            .fill(k.alpha(k.c.layer_01, 0.25))
            .child(
                Button::new(tr_ref(lang, "Full Diff View"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        if let Some(asid) = s.selected.clone() {
                            s.send(Command::OpenDiff {
                                asid: AgentSessionId(asid),
                                file: None,
                            });
                        }
                        s.close_plugin_page();
                        s.mode = Mode::Session;
                        s.show_diff = true;
                    })
                    .build(k)
                    .key(format!("diff-full-view-{tile_id}")),
            )
            .child(
                Button::new(tr_ref(lang, "Open Git Inspector"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        s.close_plugin_drawer();
                        s.open_git_inspector();
                    })
                    .build(k)
                    .key(format!("diff-open-inspector-{tile_id}")),
            )
            .child(el().grow(1.0))
            .child(
                Button::new(tr_ref(lang, "Refresh"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        if let Some(asid) = s.selected.clone() {
                            s.send(Command::RefreshDiff(AgentSessionId(asid)));
                        }
                    })
                    .build(k)
                    .key(format!("diff-refresh-{tile_id}")),
            );

        let content = if s.diff_stat.files == 0 && s.diff_stat.paths.is_empty() {
            col()
                .grow(1.0)
                .w_full()
                .center()
                .gap(8.0)
                .px(16.0)
                .child(
                    text(tr_ref(lang, "Working tree is clean"))
                        .font(k.ty.body_medium)
                        .color(k.c.text_base),
                )
                .child(
                    text(tr_ref(
                        lang,
                        "Modified files in the active session will appear here with line stats",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_muted),
                )
        } else {
            let mut file_list = col()
                .key(format!("diff-files-list-{tile_id}"))
                .w_full()
                .gap(6.0)
                .px(12.0)
                .py(10.0);

            let summary_badge = row()
                .w_full()
                .px(10.0)
                .py(8.0)
                .gap(8.0)
                .center_y()
                .fill(k.alpha(k.c.layer_01, 0.35))
                .radius(k.radius.sm)
                .border(1.0, k.alpha(k.c.border_weak, 0.35))
                .child(
                    text(format!(
                        "{} {}",
                        s.diff_stat.files.max(s.diff_stat.paths.len()),
                        tr_ref(lang, "changed file(s)")
                    ))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
                )
                .child(el().grow(1.0))
                .child(
                    text(format!("+{}", s.diff_stat.added))
                        .font(k.ty.mono)
                        .color(k.c.success.fg),
                )
                .child(
                    text(format!("−{}", s.diff_stat.removed))
                        .font(k.ty.mono)
                        .color(k.c.danger.fg),
                );
            file_list = file_list.child(summary_badge);

            for (idx, file) in s.diff_stat.paths.iter().enumerate() {
                let path_str = file.path.clone();
                let asid = s.selected.clone().unwrap_or_default();
                let file_row = row()
                    .key(format!("diff-plugin-file-{idx}-{tile_id}"))
                    .w_full()
                    .h(32.0)
                    .px(10.0)
                    .gap(8.0)
                    .center_y()
                    .fill(k.alpha(k.c.layer_01, 0.22))
                    .radius(k.radius.sm)
                    .border(1.0, k.alpha(k.c.border_weak, 0.25))
                    .child(
                        text(file.path.clone())
                            .font(k.ty.mono)
                            .color(k.c.text_base)
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0)
                            .truncate(),
                    )
                    .child(
                        text(format!("+{} −{}", file.added, file.removed))
                            .font(k.ty.caption())
                            .color(k.c.text_muted),
                    )
                    .child(
                        Button::new(tr_ref(lang, "View"))
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| {
                                if !asid.is_empty() {
                                    s.send(Command::OpenDiff {
                                        asid: AgentSessionId(asid.clone()),
                                        file: Some(path_str.clone()),
                                    });
                                }
                                s.close_plugin_page();
                                s.mode = Mode::Session;
                                s.show_diff = true;
                            })
                            .build(k)
                            .key(format!("diff-plugin-open-{idx}-{tile_id}")),
                    );
                file_list = file_list.child(file_row);
            }

            file_list
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k)
        };

        Some(
            col()
                .key(format!("diff-tile-{tile_id}"))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(header_row)
                .child(actions_bar)
                .child(content),
        )
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 5: Workspace Inspector Plugin (Queue, Git, Subagents, Saved, Shells)
// ---------------------------------------------------------------------------

pub struct InspectorPlugin;

impl DesktopPlugin for InspectorPlugin {
    fn id(&self) -> &'static str {
        "inspector"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Inspector").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &rune_kit::shell::INSPECTOR
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Session queue, Git commits & worktrees, sub-agents, permissions, and background shells",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Drawer, PluginSlot::Tile, PluginSlot::Page]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![
            PluginCommand {
                id: "inspector.toggle",
                title: "Toggle Inspector".to_string(),
                shortcut: Some("Ctrl+Alt+B"),
            },
            PluginCommand {
                id: "inspector.git",
                title: "Open Git Inspector".to_string(),
                shortcut: Some("Ctrl+Alt+G"),
            },
        ]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "inspector.toggle" => {
                s.toggle_inspector();
                true
            }
            "inspector.git" => {
                s.open_git_inspector();
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::inspector::view(ctx.s, ctx.k))
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 6: Tasks Plugin (Delegations & Worktree Orchestration)
// ---------------------------------------------------------------------------

pub struct TasksPlugin;

impl DesktopPlugin for TasksPlugin {
    fn id(&self) -> &'static str {
        "tasks"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Tasks").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::TASK
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Delegated agent tasks, remote fleet commands, and worktree orchestration",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Page, PluginSlot::Tile, PluginSlot::Drawer]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            id: "tasks.open",
            title: "Open Tasks".to_string(),
            shortcut: Some("Ctrl+Shift+T"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "tasks.open" => {
                s.go(Mode::Tasks);
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::tasks::view(ctx.s, ctx.k, layer))
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 7: Inbox Plugin (Permissions, Questions & Approvals)
// ---------------------------------------------------------------------------

pub struct InboxPlugin;

impl DesktopPlugin for InboxPlugin {
    fn id(&self) -> &'static str {
        "inbox"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Inbox").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::INBOX
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Pending tool approvals, agent questions, and paused delegations across sessions",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Page, PluginSlot::Tile, PluginSlot::Drawer]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            id: "inbox.open",
            title: "Open Inbox".to_string(),
            shortcut: Some("Ctrl+Shift+I"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "inbox.open" => {
                s.go(Mode::Inbox);
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, _layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::inbox::view(ctx.s, ctx.k))
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 8: Settings Plugin
// ---------------------------------------------------------------------------

pub struct SettingsPlugin;

impl DesktopPlugin for SettingsPlugin {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Settings").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::SETTINGS
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "General preferences, appearance & themes, agents, shortcuts, and storage",
            )
            .to_string(),
        )
    }

    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        vec![PluginSlot::Page, PluginSlot::Tile]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            id: "settings.open",
            title: "Open Settings".to_string(),
            shortcut: Some("mod+comma"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "settings.open" => {
                s.go(Mode::Settings);
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::settings::view(ctx.s, ctx.k, layer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::inbox::Update;
    use crate::ui::keymap::{esc_step, escape, EscStep};
    use muqun_gateway::agents::AgentSessionInfo;
    use muqun_gateway::platform::runtimes::RuntimeStatus;
    use rune::{Env, Key, KeyEvent, Mods, Runtime, TextEdit};
    use std::path::PathBuf;

    fn sample_session(asid: &str) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid,
            "agent_id": "claude",
            "title": "Test Session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap()
    }

    #[test]
    fn multi_surface_switching_and_esc_closes_page_and_drawer() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![sample_session("s1")];
        s.open_session("s1");
        assert_eq!(s.mode, Mode::Session);
        assert!(s.active_plugin_page.is_none());
        assert!(s.active_plugin_drawer.is_none());

        // 1. Open sysinfo in Full-Screen Page
        s.open_plugin_page("sysinfo");
        assert_eq!(s.active_plugin_page.as_deref(), Some("sysinfo"));
        assert_eq!(esc_step(&s), Some(EscStep::Page));

        // Esc closes the plugin page and returns to the active session
        assert!(escape(&mut s));
        assert!(s.active_plugin_page.is_none());
        assert_eq!(s.mode, Mode::Session);
        assert_eq!(s.selected.as_deref(), Some("s1"));

        // 2. Open quicknotes in Right Drawer
        s.open_plugin_drawer("quicknotes");
        assert_eq!(s.active_plugin_drawer.as_deref(), Some("quicknotes"));
        assert!(s.right_open);
        assert_eq!(esc_step(&s), Some(EscStep::Pane));

        // Esc closes the plugin drawer
        assert!(escape(&mut s));
        assert!(s.active_plugin_drawer.is_none());
        assert!(!s.right_open);

        // 3. Switch seamlessly: Page -> Tile -> Drawer -> Page
        s.switch_plugin_surface("sysinfo", PluginSlot::Page);
        assert_eq!(s.active_plugin_page.as_deref(), Some("sysinfo"));

        s.switch_plugin_surface("sysinfo", PluginSlot::Tile);
        assert!(s.active_plugin_page.is_none());
        assert!(s.in_split());
        assert!(s.tile_of_plugin("sysinfo").is_some());

        s.switch_plugin_surface("sysinfo", PluginSlot::Drawer);
        assert!(!s.in_split());
        assert_eq!(s.active_plugin_drawer.as_deref(), Some("sysinfo"));
        assert!(s.right_open);

        s.switch_plugin_surface("sysinfo", PluginSlot::Page);
        assert!(s.active_plugin_drawer.is_none());
        assert!(!s.right_open);
        assert_eq!(s.active_plugin_page.as_deref(), Some("sysinfo"));
    }

    #[test]
    fn quicknotes_interactive_editing_and_sending_to_composer() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![sample_session("s1")];
        s.open_session("s1");
        s.draft = TextEdit::new("Initial context:");

        // Edit scratchpad notes and insert into composer
        s.quick_notes_edit = TextEdit::new("Check `src/platform/runtimes.rs` for Bun support");
        let notes_plugin = s.plugins.get("quicknotes").expect("quicknotes registered");
        assert!(notes_plugin.on_command(&mut s, "quicknotes.insert_composer"));

        assert!(s.draft.text.contains("Initial context:"));
        assert!(s
            .draft
            .text
            .contains("Check `src/platform/runtimes.rs` for Bun support"));
        assert!(s.draft.focused);
        assert!(!s.toasts.is_empty());

        // Render quicknotes across surfaces
        let k = Kit::neutral(s.dark);
        let look = s.look();
        let mut layer = Vec::new();
        for surface in [PluginSlot::Tile, PluginSlot::Drawer, PluginSlot::Page] {
            let ctx = PluginViewContext::new(&s, &k, &look, "test-notes", 1.0, surface);
            assert!(notes_plugin.render_surface(&ctx, &mut layer).is_some());
        }

        // Clear notes via command
        assert!(notes_plugin.on_command(&mut s, "quicknotes.clear"));
        assert!(s.quick_notes_edit.text.is_empty());
    }

    #[test]
    fn sysinfo_plugin_displays_runtime_probing_and_handles_managed_install() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.runtime_statuses = vec![
            RuntimeStatus {
                kind: RuntimeKind::Bun,
                available: true,
                source: Some(RuntimeSource::System(PathBuf::from("/usr/local/bin/bun"))),
                binary_path: Some(PathBuf::from("/usr/local/bin/bun")),
                version: Some("1.2.4".to_string()),
                managed_dir: PathBuf::from("/tmp/muqun/runtimes/bun"),
            },
            RuntimeStatus {
                kind: RuntimeKind::Herdr,
                available: false,
                source: None,
                binary_path: None,
                version: None,
                managed_dir: PathBuf::from("/tmp/muqun/runtimes/herdr"),
            },
        ];

        let sysinfo = s.plugins.get("sysinfo").expect("sysinfo registered");
        let k = Kit::neutral(s.dark);
        let look = s.look();
        let mut layer = Vec::new();
        let ctx = PluginViewContext::new(&s, &k, &look, "sysinfo-1", 1.0, PluginSlot::Page);
        assert!(sysinfo.render_surface(&ctx, &mut layer).is_some());

        // Trigger managed Herdr install command
        while rx.try_recv().is_ok() {}
        assert!(sysinfo.on_command(&mut s, "sysinfo.install_herdr"));
        assert!(s.runtime_installing.contains(&RuntimeKind::Herdr));
        let sent_cmd = rx
            .try_recv()
            .expect("expected InstallManagedRuntime command");
        assert!(matches!(
            sent_cmd,
            Command::InstallManagedRuntime(RuntimeKind::Herdr)
        ));

        // Simulate completion update -> clears installing flag, updates status, shows success toast
        s.toasts.clear();
        let installed_status = RuntimeStatus {
            kind: RuntimeKind::Herdr,
            available: true,
            source: Some(RuntimeSource::Managed(PathBuf::from(
                "/tmp/muqun/runtimes/herdr/current/herdr",
            ))),
            binary_path: Some(PathBuf::from("/tmp/muqun/runtimes/herdr/current/herdr")),
            version: Some("0.15.0".to_string()),
            managed_dir: PathBuf::from("/tmp/muqun/runtimes/herdr"),
        };
        s.apply_updates(vec![Update::RuntimeInstalled {
            kind: RuntimeKind::Herdr,
            result: Ok(installed_status),
        }]);
        assert!(!s.runtime_installing.contains(&RuntimeKind::Herdr));
        assert_eq!(s.toasts.len(), 1);
        assert_eq!(s.toasts[0].level, ToastLevel::Success);
        assert!(s.toasts[0].title.contains("Herdr"));
    }

    #[test]
    fn diff_and_terminal_builtin_plugins_dogfood_multi_surface_and_commands() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![sample_session("s1")];
        s.open_session("s1");
        s.diff_stat.files = 2;
        s.diff_stat.added = 18;
        s.diff_stat.removed = 4;
        s.diff_stat.paths = vec![
            crate::model::types::DiffFile {
                path: "desktop/src/plugin/mod.rs".to_string(),
                added: 12,
                removed: 2,
                patch: String::new(),
            },
            crate::model::types::DiffFile {
                path: "desktop/src/ui/shell.rs".to_string(),
                added: 6,
                removed: 2,
                patch: String::new(),
            },
        ];

        let diff_plugin = s.plugins.get("diff").expect("diff registered");
        let k = Kit::neutral(s.dark);
        let look = s.look();
        let mut layer = Vec::new();
        let ctx = PluginViewContext::new(&s, &k, &look, "diff-1", 1.0, PluginSlot::Tile);
        assert!(diff_plugin.render_surface(&ctx, &mut layer).is_some());

        assert!(diff_plugin.on_command(&mut s, "diff.open_view"));
        assert!(s.show_diff);
        assert!(diff_plugin.on_command(&mut s, "diff.open_inspector"));
        assert!(s.right_open);

        let term_plugin = s.plugins.get("terminal").expect("terminal registered");
        assert!(term_plugin.supports_surface(PluginSlot::Tile));
        assert!(term_plugin.supports_surface(PluginSlot::Drawer));
        assert!(term_plugin.supports_surface(PluginSlot::Page));
    }

    #[test]
    fn plugin_commands_execute_from_keymap_and_command_palette() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.splash = None;

        // Register a test plugin with a custom shortcut `Ctrl+Shift+Y`
        let manifest = PluginManifest {
            id: "shortcut-test".to_string(),
            name: "Shortcut Test Plugin".to_string(),
            version: "1.0.0".to_string(),
            description: "Tests keymap and palette integration".to_string(),
            author: None,
            category: Some(PluginCategory::Tools),
            icon: Some("keyboard".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![PluginSlot::Page, PluginSlot::Tile, PluginSlot::Drawer],
            commands: vec![PluginCommandManifest {
                id: "shortcut.trigger".to_string(),
                title: "Trigger Shortcut Action".to_string(),
                shortcut: Some("Ctrl+Shift+Y".to_string()),
                action: Some("Triggered via shortcut!".to_string()),
                script: None,
            }],
            tile: None,
        };
        s.plugins.register(Arc::new(ExternalPlugin::new(
            manifest,
            PathBuf::from("/tmp"),
        )));

        // 1. Verify command palette lists plugin surface openers and plugin commands
        let all_entries = crate::ui::palette::entries(&s);
        assert!(all_entries
            .iter()
            .any(|e| e.label.contains("Open Shortcut Test Plugin (Full Screen)")));
        assert!(all_entries
            .iter()
            .any(|e| e.label.contains("Open System & Diagnostics (Full Screen)")));
        assert!(all_entries.iter().any(|e| e
            .label
            .contains("Shortcut Test Plugin: Trigger Shortcut Action")));

        // 2. Dispatch `Ctrl+Shift+Y` through keymap window dispatcher
        let mut rt = Runtime::new(s, crate::ui::view, (1280.0, 800.0), Env::new(true, 1.0));
        rt.on_key = Some(crate::ui::on_key);
        rt.frame(0.0);

        rt.state.toasts.clear();
        rt.key(KeyEvent {
            key: Key::Command('y'),
            mods: Mods {
                ctrl: true,
                shift: true,
                cmd: !crate::platform::is_macos(),
                alt: false,
                logo: false,
            },
        });
        rt.frame(1.0 / 60.0);

        assert_eq!(rt.state.toasts.len(), 1);
        assert!(rt.state.toasts[0]
            .title
            .contains("Shortcut Test Plugin: Trigger Shortcut Action"));
    }
}
