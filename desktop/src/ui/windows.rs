//! Host-owned plugin window lifecycle. Tile layout is independent from window placement.
use super::{tiles, Mode, Tile, TileContent, UiState, MAX_TILES};
use crate::plugin::WindowKind;

impl UiState {
    pub fn close_plugin_windows(&mut self, id: &str) {
        if self.active_plugin_page.as_deref() == Some(id) {
            self.active_plugin_page = None;
            self.plugin_page_origin = None;
        }
        if self.active_plugin_drawer.as_deref() == Some(id) {
            self.close_plugin_drawer();
        }
        while let Some(i) = self.tile_of_plugin(id) {
            self.close_tile(i);
        }
    }

    /// Open a plugin in the full-screen Page slot (`PluginSlot::Page`).
    pub fn open_plugin_page(&mut self, plugin_id: &str) {
        if self
            .plugins
            .get_enabled(plugin_id, &self.cfg.plugins_disabled)
            .is_none()
        {
            return;
        }
        self.active_plugin_page = Some(plugin_id.to_string());
        if plugin_id == "sysinfo" {
            self.refresh_runtimes();
        } else if plugin_id == "terminal" {
            if self.term.session.is_none() && self.term.tabs.is_empty() {
                self.start_terminal();
            }
            self.focus_terminal();
        } else if plugin_id == "quicknotes" {
            self.quick_notes_edit.focused = true;
            self.draft.focused = false;
            self.term.sink.focused = false;
        }
    }

    /// Close the active full-screen plugin page, returning to the underlying workspace view.
    pub fn close_plugin_page(&mut self) {
        let id = self.active_plugin_page.take();
        let origin = self.plugin_page_origin.take();
        if let (Some(id), Some(origin)) = (id, origin) {
            match origin {
                crate::plugin::PluginSlot::Drawer => self.open_plugin_drawer(&id),
                crate::plugin::PluginSlot::Tile => {
                    if let Some(i) = self.tile_of_plugin(&id) {
                        self.focus_tile(i);
                    }
                }
                _ => {}
            }
        }
    }

    /// Open a plugin in the right-hand Drawer slot (`PluginSlot::Drawer`).
    pub fn open_plugin_drawer(&mut self, plugin_id: &str) {
        if plugin_id == "terminal" {
            self.active_plugin_drawer = None;
            self.open_terminal();
            return;
        }
        if self
            .plugins
            .get_enabled(plugin_id, &self.cfg.plugins_disabled)
            .is_none()
        {
            return;
        }
        if self.term.open {
            self.close_terminal();
        }
        self.active_plugin_drawer = Some(plugin_id.to_string());
        self.right_open = true;
        self.right.set_open(true);
        if plugin_id == "sysinfo" {
            self.refresh_runtimes();
        } else if plugin_id == "quicknotes" {
            self.quick_notes_edit.focused = true;
            self.draft.focused = false;
        }
    }

    /// Close the active right-hand plugin drawer.
    pub fn close_plugin_drawer(&mut self) {
        if self.active_plugin_drawer.take().is_some() {
            self.right_open = false;
            self.right.set_open(false);
        }
    }

    /// Find the tile index currently hosting `plugin_id`, if any.
    pub fn tile_of_plugin(&self, plugin_id: &str) -> Option<usize> {
        if plugin_id == "terminal" {
            return self.tile_of_terminal();
        }
        self.tiles.iter().position(|t| match &t.content {
            TileContent::Custom { kind, .. } => kind == plugin_id,
            TileContent::Diff(_) => plugin_id == "diff",
            _ => false,
        })
    }

    /// Open a plugin in a split-view Tile (`PluginSlot::Tile`).
    pub fn open_plugin_tile(&mut self, plugin_id: &str) {
        if plugin_id == "terminal" {
            self.active_plugin_page = None;
            self.open_terminal_tile();
            return;
        }
        if self
            .plugins
            .get_enabled(plugin_id, &self.cfg.plugins_disabled)
            .is_none()
        {
            return;
        }
        self.active_plugin_page = None;
        if self.active_plugin_drawer.as_deref() == Some(plugin_id) {
            self.close_plugin_drawer();
        }
        if let Some(i) = self.tile_of_plugin(plugin_id) {
            self.mode = Mode::Session;
            self.focus_tile(i);
            return;
        }
        self.close_terminal();
        self.right_open = false;
        self.right.set_open(false);

        if self.tiles.is_empty() {
            let current = self.selected.clone();
            self.tiles.push(Tile {
                content: match &current {
                    Some(asid) => TileContent::Session(asid.clone()),
                    None => TileContent::Empty,
                },
                asid: current,
                ..Tile::default()
            });
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }

        let at = match self.tiles.iter().position(|t| t.is_empty()) {
            Some(empty) => empty,
            None if self.tiles.len() < MAX_TILES => self.add_tile(),
            None => {
                self.toast(
                    "Workspace is full",
                    Some("Close a tile or open this plugin as a page or dock.".into()),
                );
                return;
            }
        };
        self.mode = Mode::Session;
        tiles::fill_plugin_tile(self, at, plugin_id);
    }

    /// Seamlessly switch `plugin_id` between `Tile` ⇄ `Page` ⇄ `Drawer`.
    pub fn switch_plugin_surface(&mut self, plugin_id: &str, target: crate::plugin::PluginSlot) {
        if WindowKind::from_slot(target).is_none() {
            return;
        }
        if !self
            .plugins
            .get_enabled(plugin_id, &self.cfg.plugins_disabled)
            .is_some_and(|plugin| plugin.supports_surface(target))
        {
            return;
        }
        use crate::plugin::PluginSlot;
        if target == PluginSlot::Tile
            && self.tile_of_plugin(plugin_id).is_none()
            && self.tiles.len() >= MAX_TILES
            && self.tiles.iter().all(|tile| !tile.is_empty())
        {
            self.toast(
                "Workspace is full",
                Some("Close a tile or open this plugin as a page or dock.".into()),
            );
            return;
        }
        match target {
            PluginSlot::Page => {
                if self.active_plugin_page.as_deref() == Some(plugin_id) {
                    return;
                }
                self.plugin_page_origin = if self.active_plugin_drawer.as_deref() == Some(plugin_id)
                    || (plugin_id == "terminal" && self.term.open)
                {
                    Some(PluginSlot::Drawer)
                } else if self.tile_of_plugin(plugin_id).is_some() {
                    Some(PluginSlot::Tile)
                } else {
                    None
                };
                if self.active_plugin_drawer.as_deref() == Some(plugin_id) {
                    self.close_plugin_drawer();
                }
                if plugin_id == "terminal" && self.term.open {
                    self.close_terminal();
                }
                self.open_plugin_page(plugin_id);
            }
            PluginSlot::Drawer => {
                self.plugin_page_origin = None;
                self.active_plugin_page = None;
                if let Some(i) = self.tile_of_plugin(plugin_id) {
                    if self.in_split() {
                        self.close_tile(i);
                    } else {
                        self.tiles.clear();
                    }
                }
                self.open_plugin_drawer(plugin_id);
            }
            PluginSlot::Tile => {
                self.plugin_page_origin = None;
                self.active_plugin_page = None;
                if self.active_plugin_drawer.as_deref() == Some(plugin_id) {
                    self.close_plugin_drawer();
                }
                self.open_plugin_tile(plugin_id);
            }
            _ => {}
        }
    }
}
