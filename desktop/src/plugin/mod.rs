//! Desktop plugin architecture.
//!
//! Provides an extensible trait [`DesktopPlugin`] and registry [`PluginRegistry`]
//! allowing internal modules (e.g. terminal, diff, system diagnostics, notes)
//! and user extensions to seamlessly plug into desktop split tiles, full-screen
//! pages, right-hand drawers, commands, and workspace panels.

use std::collections::{BTreeSet, HashMap};
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

pub(crate) mod browser;
pub(crate) mod context;
pub(crate) mod draft;
pub mod loader;
pub mod manifest;
pub mod packages;
pub(crate) mod webview_preview;
pub mod window;
pub use window::WindowKind;

pub use loader::{discover_plugins, BunScriptResponse, BunToastResponse, ExternalPlugin};
pub use manifest::{
    PluginCategory, PluginCommandManifest, PluginManifest, PluginRenderKind, PluginRepository,
    PluginSlot, PluginSurface, PluginTileManifest,
};

/// Built-in plugins that stay on; their Settings toggle is disabled.
pub fn is_always_on(id: &str) -> bool {
    matches!(id, "terminal" | "settings")
}

/// Whether `id` is usable given the owner's disabled set.
pub fn is_enabled(id: &str, disabled: &BTreeSet<String>) -> bool {
    is_always_on(id) || !disabled.contains(id)
}

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

/// Append unified window placement items (`Tile` / `Floating window` / `Page` / `Dock`)
/// with checkmarks into any window's `...` menu.
pub fn append_surface_items(
    mut menu: rune_kit::Menu,
    s: &UiState,
    lang: Language,
    plugin_id: &str,
    active: PluginSlot,
    pop_shortcut: Option<String>,
) -> rune_kit::Menu {
    let is_popped = active == PluginSlot::Tile
        && s.tile_of_plugin(plugin_id)
            .and_then(|i| s.tiles.get(i))
            .is_some_and(|t| t.popped);
    for kind in WindowKind::ALL {
        let slot = kind.slot();
        if !s
            .plugins
            .get(plugin_id)
            .is_some_and(|plugin| plugin.supports_surface(slot))
        {
            continue;
        }
        let pid = plugin_id.to_string();
        let mut item = rune_kit::MenuItem::new(tr_ref(lang, kind.label()))
            .icon(kind.glyph())
            .checked(active == slot && (slot != PluginSlot::Tile || !is_popped))
            .on_select(move |s: &mut UiState| {
                s.menu = None;
                s.switch_plugin_surface(&pid, slot);
            });
        if slot == PluginSlot::Tile && is_popped {
            if let Some(sc) = pop_shortcut.as_deref().filter(|s| !s.is_empty()) {
                item = item.shortcut(sc);
            }
        }
        menu = menu.item(item);
        if slot == PluginSlot::Tile {
            let pid_float = plugin_id.to_string();
            let mut float_item = rune_kit::MenuItem::new(tr_ref(lang, "Floating window"))
                .icon(WindowKind::float_glyph())
                .checked(is_popped)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.open_plugin_float(&pid_float);
                });
            if !is_popped {
                if let Some(sc) = pop_shortcut.as_deref().filter(|s| !s.is_empty()) {
                    float_item = float_item.shortcut(sc);
                }
            }
            menu = menu.item(float_item);
        }
    }
    menu
}

/// Unified trailing window controls rendered on Drawer and Page headers:
/// an always-visible `...` menu (window placement + plugin/dock actions) followed
/// by the Close/Restore button. Matches Split Tile and Floating Window headers.
pub fn window_controls(
    s: &UiState,
    k: &Kit,
    lang: Language,
    plugin_id: &str,
    active: PluginSlot,
    layer: &mut Vec<El>,
) -> El {
    let menu_id = format!("window-{plugin_id}-{active:?}");
    let open = s.menu == Some(crate::ui::OpenMenu::WindowSurface(menu_id.clone()));
    if WindowKind::from_slot(active).is_none() {
        return el();
    }
    let label = tr_ref(lang, "Actions");
    let mut menu = rune_kit::Menu::new(menu_id.clone())
        .trigger(
            crate::ui::icons::header_button(&crate::ui::icons::MORE, label)
                .expanded(open)
                .on_press(move |s: &mut UiState| {
                    s.menu = (!open).then(|| crate::ui::OpenMenu::WindowSurface(menu_id.clone()));
                })
                .build(k)
                .key(format!("window-menu-{plugin_id}-{active:?}")),
        )
        .open(open)
        .min_width(230.0)
        .on_close(|s: &mut UiState| s.menu = None);

    menu = append_surface_items(menu, s, lang, plugin_id, active, None);

    if plugin_id != "terminal" {
        if let Some(plugin) = s.plugins.get(plugin_id) {
            let cmds = plugin.commands();
            if !cmds.is_empty() {
                menu = menu.separator();
                for cmd in cmds {
                    let cmd_id = cmd.id;
                    let p = plugin.clone();
                    menu = menu.item(
                        rune_kit::MenuItem::new(cmd.title)
                            .shortcut(cmd.shortcut.unwrap_or_default())
                            .on_select(move |s: &mut UiState| {
                                s.menu = None;
                                p.on_command(s, cmd_id);
                            }),
                    );
                }
            }
        }
    }

    if active == PluginSlot::Drawer || plugin_id == "terminal" {
        menu = menu.separator();
        menu = window::append_dock_action_items(menu, s, plugin_id == "terminal");
    }

    let mut bar = row()
        .key(format!("window-controls-bar-{plugin_id}-{active:?}"))
        .gap(4.0)
        .center_y()
        .child(menu.build(k, layer));

    if matches!(active, PluginSlot::Page | PluginSlot::Drawer) {
        let close_label = tr_ref(
            lang,
            if active == PluginSlot::Page && s.plugin_page_origin.is_some() {
                "Restore"
            } else {
                "Close"
            },
        );
        let terminal = plugin_id == "terminal";
        let close_btn = crate::ui::icons::header_button(k.glyph.close, close_label)
            .on_press(move |s: &mut UiState| match active {
                PluginSlot::Page => s.close_plugin_page(),
                PluginSlot::Drawer if terminal => s.close_terminal(),
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

    /// Optional title and contextual actions in the host window header.
    fn window_title(&self, _s: &UiState) -> Option<String> {
        None
    }
    fn window_actions(&self, _s: &UiState, _k: &Kit) -> Option<El> {
        None
    }

    /// Icon representing this plugin in toolbars and pickers.
    fn icon(&self) -> &'static Icon;

    /// Draw a vector icon by default; external packages may supply a bounded image.
    fn render_icon(&self, s: &UiState, size: f32, color: rune::Color) -> El {
        let _ = s;
        icon(self.icon(), size).color(color)
    }

    /// Optional description of what this plugin provides.
    fn description(&self, lang: Language) -> Option<String> {
        let _ = lang;
        None
    }

    /// Category for grouping in plugin lists and pickers.
    fn category(&self) -> PluginCategory {
        PluginCategory::Tools
    }

    /// Standard package keywords shown as tags and included in plugin search.
    fn keywords(&self) -> &[String] {
        &[]
    }

    /// Author-declared source repository; absent when missing or unsafe.
    fn repository(&self) -> Option<&PluginRepository> {
        None
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

    /// Apply a runtime enablement change without restarting the host.
    fn enabled_changed(&self, _enabled: bool) {}

    fn on_notification_action(&self, _s: &mut UiState, _action: &str) {}

    fn close_view(&self, _instance: &str) {}

    /// Move a mounted view without recreating its renderer or restarting its worker.
    fn can_move_view(&self, _instance: &str) -> bool {
        false
    }

    fn move_view(&self, _from: &str, _to: &str) {}

    fn open_terminal_entry(&self, _s: &mut UiState, _entry: &str) {}

    /// Fold background work into UI state without blocking input.
    fn poll(&self, _s: &mut UiState, _tx: &mut Tx) -> bool {
        false
    }

    /// Deliver a native component action to its owning surface.
    fn on_view_action(&self, _s: &mut UiState, _instance: &str, _action: &str) -> bool {
        false
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
    pub managed_versions: HashMap<String, String>,
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
        if loader::webview_helper().is_some() {
            r.register(Arc::new(browser::BrowserPlugin));
        }
        r.register(Arc::new(FilePlugin));
        r.register(Arc::new(DiffPlugin));
        r.register(Arc::new(GitPlugin));
        r.register(Arc::new(ActivityPlugin));
        r.register(Arc::new(TasksPlugin));
        r.register(Arc::new(MaoPlugin));
        r.register(Arc::new(InboxPlugin));
        r.register(Arc::new(SettingsPlugin));
        r.register(Arc::new(SysInfoPlugin));
        r.register(Arc::new(QuickNotesPlugin));

        let detail = crate::ui::parts::detail::SubagentWindow;
        // Contextual window opened by a conversation row, outside the plugin catalog.
        r.plugins.insert(detail.id().to_owned(), Arc::new(detail));

        let dirs = loader::default_plugin_dirs();
        for plugin in loader::discover_plugins(&dirs) {
            r.register(Arc::new(plugin));
        }

        if let Ok(entries) = packages::entries() {
            for (id, path) in entries {
                if r.get(&id).is_none() {
                    if let Ok(plugin) = ExternalPlugin::from_dir(&path) {
                        r.managed_versions
                            .insert(id, plugin.manifest().version.clone());
                        r.register(Arc::new(plugin));
                    }
                }
            }
        }
        r
    }

    pub fn reload_package(&mut self, id: &str) {
        if self.managed_versions.remove(id).is_some() {
            self.plugins.remove(id);
            self.order.retain(|old| old != id);
        }
        if let Ok(entries) = packages::entries() {
            if let Some((_, path)) = entries.into_iter().find(|(name, _)| name == id) {
                if let Ok(plugin) = ExternalPlugin::from_dir(&path) {
                    self.managed_versions
                        .insert(id.to_owned(), plugin.manifest().version.clone());
                    self.register(Arc::new(plugin));
                }
            }
        }
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

    /// Look up a plugin by id when it is enabled.
    pub fn get_enabled(
        &self,
        id: &str,
        disabled: &BTreeSet<String>,
    ) -> Option<Arc<dyn DesktopPlugin>> {
        if !is_enabled(id, disabled) {
            return None;
        }
        self.get(id)
    }

    /// Retrieve all plugins that support split view tiles.
    pub fn tile_plugins(&self) -> Vec<Arc<dyn DesktopPlugin>> {
        self.tile_plugins_enabled(&BTreeSet::new())
    }

    /// Tile-capable plugins, excluding those in `disabled`.
    pub fn tile_plugins_enabled(&self, disabled: &BTreeSet<String>) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter(|id| is_enabled(id, disabled))
            .filter_map(|id| self.plugins.get(id))
            .filter(|p| p.supports_tile())
            .cloned()
            .collect()
    }

    /// Retrieve all plugins that support a given [`PluginSlot`].
    pub fn plugins_for_surface(&self, slot: PluginSlot) -> Vec<Arc<dyn DesktopPlugin>> {
        self.plugins_for_surface_enabled(slot, &BTreeSet::new())
    }

    /// Surface-capable plugins, excluding those in `disabled`.
    pub fn plugins_for_surface_enabled(
        &self,
        slot: PluginSlot,
        disabled: &BTreeSet<String>,
    ) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter(|id| is_enabled(id, disabled))
            .filter_map(|id| self.plugins.get(id))
            .filter(|p| p.supports_surface(slot))
            .cloned()
            .collect()
    }

    /// Retrieve all registered plugins (including disabled).
    pub fn all(&self) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter_map(|id| self.plugins.get(id))
            .cloned()
            .collect()
    }

    /// Registered plugins that are currently enabled.
    pub fn all_enabled(&self, disabled: &BTreeSet<String>) -> Vec<Arc<dyn DesktopPlugin>> {
        self.order
            .iter()
            .filter(|id| is_enabled(id, disabled))
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
        let controls = Some(window_controls(
            ctx.s,
            ctx.k,
            ctx.s.cfg.general.language,
            "terminal",
            ctx.surface,
            layer,
        ));
        Some(terminal_panel::tile_body_with_controls(
            ctx.s, ctx.k, scheme, ctx.scale, layer, None, controls,
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
            "diff.open_git" | "diff.open_inspector" => {
                s.open_git_panel();
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
                Button::new(tr_ref(lang, "Open Git"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        s.open_git_panel();
                    })
                    .build(k)
                    .key(format!("diff-open-git-{tile_id}")),
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
                .child(actions_bar)
                .child(content),
        )
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 5: Developer Git Workbench Plugin
// ---------------------------------------------------------------------------

pub struct GitPlugin;

impl DesktopPlugin for GitPlugin {
    fn id(&self) -> &'static str {
        "git"
    }

    fn title(&self, _lang: Language) -> String {
        "Git".to_string()
    }

    fn icon(&self) -> &'static Icon {
        &icons::BRANCH
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Branches, staged & unstaged changes, commits, pull requests, and worktrees",
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
        vec![PluginCommand {
            id: "git.open",
            title: "Toggle Git Panel".to_string(),
            shortcut: Some("Ctrl+Alt+G"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "git.open" => {
                s.toggle_git_panel();
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::git_panel::view(ctx.s, ctx.k, layer))
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 6: Session Activity Plugin (Queue + Multi-tile + Tree)
// ---------------------------------------------------------------------------

pub struct ActivityPlugin;

impl DesktopPlugin for ActivityPlugin {
    fn id(&self) -> &'static str {
        "activity"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Activity").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &rune_kit::shell::INSPECTOR
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Waiting approvals, queued prompts across open tiles, and sub-agent session tree",
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
        vec![PluginCommand {
            id: "activity.toggle",
            title: "Toggle Activity Panel".to_string(),
            shortcut: Some("Ctrl+Alt+B"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "activity.toggle" => {
                s.toggle_activity_panel();
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::activity_panel::view(ctx.s, ctx.k, layer))
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
        let full_page = ctx.surface == PluginSlot::Page && ctx.s.active_plugin_page.is_none();
        Some(crate::ui::tasks::view_in_surface(
            ctx.s, ctx.k, layer, full_page,
        ))
    }
}

// ---------------------------------------------------------------------------
// Built-in Plugin 6b: MAO Orchestration Plugin
// ---------------------------------------------------------------------------

pub struct MaoPlugin;

impl DesktopPlugin for MaoPlugin {
    fn id(&self) -> &'static str {
        "mao"
    }

    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "MAO").to_string()
    }

    fn icon(&self) -> &'static Icon {
        &crate::ui::parts::glyphs::WORKFLOW
    }

    fn description(&self, lang: Language) -> Option<String> {
        Some(
            tr_ref(
                lang,
                "Multi-agent Coder ⇄ Reviewer orchestration, live status, deliverables & quality gates",
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

    fn can_move_view(&self, _from: &str) -> bool {
        true
    }

    fn window_title(&self, s: &UiState) -> Option<String> {
        Some(tr_ref(s.cfg.general.language, "MAO · Orchestration").to_string())
    }

    fn window_actions(&self, s: &UiState, k: &Kit) -> Option<El> {
        Some(crate::ui::orchestration::window_header_actions(s, k))
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            id: "mao.open",
            title: "Open MAO".to_string(),
            shortcut: Some("Ctrl+Shift+O"),
        }]
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        match command_id {
            "mao.open" => {
                s.go(Mode::Mao);
                true
            }
            _ => false,
        }
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::orchestration::window_view(
            ctx.s,
            ctx.k,
            ctx.surface,
            layer,
        ))
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
        let full_page = ctx.surface == PluginSlot::Page && ctx.s.active_plugin_page.is_none();
        Some(crate::ui::inbox::view_in_surface(ctx.s, ctx.k, full_page))
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
        let full_page = ctx.surface == PluginSlot::Page && ctx.s.active_plugin_page.is_none();
        Some(crate::ui::settings::view_in_surface(
            ctx.s, ctx.k, layer, full_page,
        ))
    }
}

/// The built-in preview follows exactly the same window contract as external plugins.
struct FilePlugin;
impl DesktopPlugin for FilePlugin {
    fn id(&self) -> &'static str {
        "files"
    }
    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Files").into()
    }
    fn icon(&self) -> &'static Icon {
        &icons::STORAGE
    }
    fn category(&self) -> PluginCategory {
        PluginCategory::Core
    }
    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        Some(crate::ui::viewer::content(ctx.s, ctx.k, ctx.scale, layer))
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
    fn maximizing_restores_the_tile_and_docking_keeps_other_tiles() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![sample_session("s1")];
        s.open_session("s1");
        s.open_plugin_tile("quicknotes");
        s.open_plugin_tile("sysinfo");
        let before = s.tiles.len();
        let at = s.tile_of_plugin("sysinfo").unwrap();
        s.tiles[at].floating_rect = Some([40.0, 60.0, 480.0, 360.0]);
        s.switch_plugin_surface("sysinfo", PluginSlot::Page);
        assert_eq!(s.tiles.len(), before);
        s.close_plugin_page();
        assert_eq!(s.focus, at);
        assert_eq!(s.tiles[at].floating_rect, Some([40.0, 60.0, 480.0, 360.0]));
        s.switch_plugin_surface("sysinfo", PluginSlot::Drawer);
        assert!(s.in_split());
        assert!(s.tile_of_plugin("quicknotes").is_some());
        assert_eq!(s.tiles.len(), before - 1);
        s.switch_plugin_surface("sysinfo", PluginSlot::Page);
        s.close_plugin_page();
        assert_eq!(s.active_plugin_drawer.as_deref(), Some("sysinfo"));
        assert!(s.right_open);
        // Unsupported destinations must not close the existing surface first.
        s.switch_plugin_surface("settings", PluginSlot::Drawer);
        assert_eq!(s.active_plugin_drawer.as_deref(), Some("sysinfo"));
        s.edit(crate::config::ConfigEdit::PluginEnabled {
            plugin: "sysinfo".into(),
            on: false,
        });
        assert!(s.active_plugin_drawer.is_none());
        assert!(s.tile_of_plugin("quicknotes").is_some());
        s.switch_plugin_surface("sysinfo", PluginSlot::Page);
        assert!(s.active_plugin_page.is_none());
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
    fn plugin_install_waits_for_an_explicit_runtime_install() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        while let Ok(startup) = rx.try_recv() {
            assert!(!matches!(
                startup,
                Command::InstallManagedRuntime(_) | Command::PluginPackage(..)
            ));
        }
        s.change_plugin_package(packages::Operation::Install("example".into()));
        assert!(!s.plugin_packages.busy());
        assert!(rx.try_recv().is_err());
        s.install_managed_runtime(RuntimeKind::Bun);
        s.install_managed_runtime(RuntimeKind::Bun);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::InstallManagedRuntime(RuntimeKind::Bun))
        ));
        assert!(rx.try_recv().is_err());
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
            event: uuid::Uuid::new_v4(),
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
        assert_eq!(
            s.active_tile_inline_panel(),
            Some(crate::ui::TileInlinePanel::Git)
        );

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
            keywords: vec![],
            repository: None,
            icon: Some("keyboard".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![PluginSlot::Page, PluginSlot::Tile, PluginSlot::Drawer],
            composer_draft: false,
            notification_entry: None,
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

    #[test]
    fn inbox_and_settings_in_tile_or_drawer_stay_below_host_header() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![sample_session("s1")];
        s.open_session("s1");
        s.splash = None;
        s.open_plugin_tile("settings");
        s.open_plugin_drawer("inbox");

        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        for _ in 0..10 {
            rt.frame(1.0 / 60.0);
        }
        let nodes = rt.elements();

        let inbox_header = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("window-header-inbox-Drawer"))
            .expect("inbox drawer window header");
        let inbox_body = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("inbox"))
            .expect("inbox body in drawer");
        assert!(
            inbox_body.rect[1] >= inbox_header.rect[1] + inbox_header.rect[3] - 1.0,
            "inbox drawer shifted over host header: header={:?} body={:?}",
            inbox_header.rect,
            inbox_body.rect
        );

        let settings_stage = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("settings-stage"))
            .expect("settings stage in tile");
        let tile_head = nodes
            .iter()
            .filter(|n| n.key.as_deref() == Some("tile-head"))
            .find(|n| {
                (n.rect[0] - settings_stage.rect[0]).abs() < 4.0
                    && (n.rect[2] - settings_stage.rect[2]).abs() < 4.0
            })
            .expect("settings tile header");
        assert!(
            settings_stage.rect[1] >= tile_head.rect[1] + tile_head.rect[3] - 1.0,
            "settings tile shifted over host header: header={:?} stage={:?}",
            tile_head.rect,
            settings_stage.rect
        );
    }
}
