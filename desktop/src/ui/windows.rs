//! Host-owned plugin window lifecycle. Tile layout is independent from window placement.
use super::{tiles, Mode, Tile, TileContent, UiState, MAX_TILES};
use crate::plugin::WindowKind;

impl UiState {
    pub(crate) fn mount_plugin_view(&mut self, id: &str, instance: &str) {
        if self
            .moving_plugin_view
            .as_ref()
            .is_some_and(|(plugin, _)| plugin == id)
        {
            return;
        }
        if let Some(plugin) = self.plugins.get_enabled(id, &self.cfg.plugins_disabled) {
            plugin.on_view_action(self, instance, "open");
        }
    }

    pub(crate) fn release_plugin_view(&mut self, id: &str, instance: &str) {
        if self
            .moving_plugin_view
            .as_ref()
            .is_some_and(|(plugin, source)| plugin == id && source == instance)
        {
            return;
        }
        if let Some(plugin) = self.plugins.get(id) {
            plugin.close_view(instance);
            if id == super::parts::detail::ID {
                self.detail = None;
                self.retain_timelines();
            }
        }
        self.plugin_fields
            .remove(&(id.to_owned(), instance.to_owned()));
    }

    pub(crate) fn release_plugin_page(&mut self) {
        if let Some(id) = self.active_plugin_page.take() {
            self.release_plugin_view(&id, "page");
        }
    }

    pub fn close_plugin_windows(&mut self, id: &str) {
        if self.selected_dock_tab() != Some(id) {
            self.remove_dock_tab(id, false);
        }
        if id == "terminal" && self.term.open {
            self.close_terminal();
        }
        if matches!(
            (id, self.mode),
            ("tasks", Mode::Tasks) | ("inbox", Mode::Inbox) | ("mao", Mode::Mao)
        ) {
            self.go(if self.selected.is_some() {
                Mode::Session
            } else {
                Mode::Home
            });
        }
        if id == "inspector" && self.active_plugin_drawer.is_none() && self.right_open {
            self.close_plugin_drawer();
        }
        if self.active_plugin_page.as_deref() == Some(id) {
            self.release_plugin_page();
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
        if self.active_plugin_page.as_deref() != Some(plugin_id) {
            self.release_plugin_page();
        }
        let opening = self.active_plugin_page.as_deref() != Some(plugin_id);
        self.active_plugin_page = Some(plugin_id.to_string());
        if opening {
            self.mount_plugin_view(plugin_id, "page");
        }
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
        if let (Some(id), Some(origin)) = (self.active_plugin_page.clone(), self.plugin_page_origin)
        {
            if self
                .plugins
                .get(&id)
                .is_some_and(|plugin| plugin.can_move_view("page"))
            {
                self.switch_plugin_surface(&id, origin);
                return;
            }
        }
        let id = self.active_plugin_page.take();
        if id.is_none() && self.mode == Mode::Mao {
            self.go(if self.selected.is_some() {
                Mode::Session
            } else {
                Mode::Home
            });
            return;
        }
        if let Some(id) = id.as_deref() {
            self.release_plugin_view(id, "page");
        }
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

    pub(crate) fn selected_dock_tab(&self) -> Option<&str> {
        if self.term.open {
            Some("terminal")
        } else if self.right_open {
            Some(self.active_plugin_drawer.as_deref().unwrap_or("inspector"))
        } else {
            None
        }
    }

    pub(crate) fn right_panel_open(&self) -> bool {
        self.selected_dock_tab().is_some()
            || (self.inspect.is_some() && self.inspect_pane.is_open())
    }

    pub(crate) fn toggle_right_panel(&mut self) {
        if self.right_panel_open() {
            self.hidden_dock_tab = self.selected_dock_tab().map(str::to_owned);
            if let Some(tab) = self.hidden_dock_tab.clone() {
                self.remember_dock_tab(&tab);
            }
            self.term.open = false;
            self.term.pane.set_open(false);
            self.term.sink.focused = false;
            self.right_open = false;
            self.right.set_open(false);
            self.inspect_pane.set_open(false);
            self.active_plugin_drawer = None;
            self.quick_notes_edit.focused = false;
            self.draft.focused = self.mode == Mode::Session;
            return;
        }

        if self.inspect.is_some() {
            self.inspect_pane.set_open(true);
            if self.hidden_dock_tab.is_none() {
                return;
            }
        }
        let default = if crate::plugin::is_enabled("inspector", &self.cfg.plugins_disabled) {
            "inspector"
        } else {
            "terminal"
        };
        let tab = self
            .hidden_dock_tab
            .take()
            .or_else(|| self.dock_tabs.last().cloned())
            .unwrap_or_else(|| default.to_owned());
        self.open_plugin_drawer(&tab);
    }

    pub(crate) fn remember_dock_tab(&mut self, id: &str) {
        if !self.dock_tabs.iter().any(|tab| tab == id) {
            self.dock_tabs.push(id.to_owned());
        }
    }

    pub(crate) fn remove_dock_tab(&mut self, id: &str, restore: bool) {
        let Some(index) = self.dock_tabs.iter().position(|tab| tab == id) else {
            return;
        };
        if self.hidden_dock_tab.as_deref() == Some(id) {
            self.hidden_dock_tab = None;
        }
        self.release_plugin_view(id, "drawer");
        self.dock_tabs.remove(index);
        if restore {
            if let Some(next) = self.dock_tabs.get(index.saturating_sub(1)).cloned() {
                self.carry_dock_width(id, &next);
                self.open_plugin_drawer(&next);
            }
        }
    }

    pub(super) fn carry_dock_width(&mut self, previous: &str, next: &str) {
        match (previous == "terminal", next == "terminal") {
            (true, false) => self.right.set_width(self.term.pane.width()),
            (false, true) => self.term.pane.set_width(self.right.width()),
            _ => {}
        }
    }

    /// Open or select one registered plugin in the shared right dock.
    pub fn open_plugin_drawer(&mut self, plugin_id: &str) {
        if !self
            .plugins
            .get_enabled(plugin_id, &self.cfg.plugins_disabled)
            .is_some_and(|plugin| plugin.supports_surface(crate::plugin::PluginSlot::Drawer))
        {
            return;
        }
        self.hidden_dock_tab = None;
        if plugin_id == "terminal" {
            self.open_terminal();
            return;
        }
        if let Some(previous) = self.selected_dock_tab().map(str::to_owned) {
            self.carry_dock_width(&previous, plugin_id);
            self.remember_dock_tab(&previous);
        }
        let opening = !self.dock_tabs.iter().any(|tab| tab == plugin_id);
        self.remember_dock_tab(plugin_id);
        if opening {
            self.mount_plugin_view(plugin_id, "drawer");
        }
        if self.term.open {
            self.term.open = false;
            self.term.pane.set_open(false);
            self.term.sink.focused = false;
        }
        self.quick_notes_edit.focused = plugin_id == "quicknotes";
        self.active_plugin_drawer =
            (plugin_id != "inspector" || self.dock_tabs.len() > 1).then(|| plugin_id.to_owned());
        self.right_open = true;
        self.right.set_open(true);
        if plugin_id == "inspector" {
            self.refresh_inspector();
        } else if plugin_id == "sysinfo" {
            self.refresh_runtimes();
        } else if plugin_id == "quicknotes" {
            self.draft.focused = false;
        }
    }

    pub(crate) fn close_plugin_dock_tab(&mut self, id: &str) {
        if self.selected_dock_tab() == Some(id) {
            self.close_plugin_drawer();
        } else {
            self.remove_dock_tab(id, false);
        }
    }

    pub fn close_plugin_drawer(&mut self) {
        let closing = self
            .active_plugin_drawer
            .take()
            .unwrap_or_else(|| "inspector".into());
        self.quick_notes_edit.focused = false;
        self.right_open = false;
        self.right.set_open(false);
        self.draft.focused = true;
        self.remove_dock_tab(&closing, true);
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
            self.release_plugin_page();
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
        self.release_plugin_page();
        if self.selected_dock_tab() == Some(plugin_id) {
            self.close_plugin_drawer();
        }
        self.remove_dock_tab(plugin_id, false);
        if let Some(i) = self.tile_of_plugin(plugin_id) {
            self.mode = Mode::Session;
            self.focus_tile(i);
            return;
        }
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
        let tile_instance = |s: &Self| {
            s.tile_of_plugin(plugin_id)
                .and_then(|i| match &s.tiles[i].content {
                    TileContent::Custom { id, .. } => Some(id.clone()),
                    _ => None,
                })
        };
        let source = if self.active_plugin_page.as_deref() == Some(plugin_id) {
            Some("page".to_owned())
        } else if self.selected_dock_tab() == Some(plugin_id) {
            Some("drawer".to_owned())
        } else {
            tile_instance(self).or_else(|| {
                self.dock_tabs
                    .iter()
                    .any(|id| id == plugin_id)
                    .then(|| "drawer".into())
            })
        };
        let plugin = self.plugins.get(plugin_id);
        self.moving_plugin_view = source
            .filter(|instance| plugin.as_ref().is_some_and(|p| p.can_move_view(instance)))
            .map(|instance| (plugin_id.to_owned(), instance));
        self.place_plugin_surface(plugin_id, target);
        if let Some((_, from)) = self.moving_plugin_view.take() {
            let to = match target {
                PluginSlot::Drawer => Some("drawer".to_owned()),
                PluginSlot::Page => Some("page".to_owned()),
                PluginSlot::Tile => tile_instance(self),
                _ => None,
            };
            if let (Some(plugin), Some(to)) = (plugin, to) {
                if to != from {
                    plugin.close_view(&to);
                    self.plugin_fields
                        .remove(&(plugin_id.to_owned(), to.clone()));
                    plugin.move_view(&from, &to);
                    if let Some(fields) = self.plugin_fields.remove(&(plugin_id.to_owned(), from)) {
                        self.plugin_fields
                            .insert((plugin_id.to_owned(), to), fields);
                    }
                }
            }
        }
    }

    fn place_plugin_surface(&mut self, plugin_id: &str, target: crate::plugin::PluginSlot) {
        use crate::plugin::PluginSlot;
        if target != PluginSlot::Drawer && self.selected_dock_tab() != Some(plugin_id) {
            self.remove_dock_tab(plugin_id, false);
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
                self.release_plugin_page();
                if plugin_id == "mao" && self.mode == Mode::Mao {
                    self.mode = if self.selected.is_some() {
                        Mode::Session
                    } else {
                        Mode::Home
                    };
                }
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
                self.release_plugin_page();
                if plugin_id == "mao" && self.mode == Mode::Mao {
                    self.mode = if self.selected.is_some() {
                        Mode::Session
                    } else {
                        Mode::Home
                    };
                }
                if self.active_plugin_drawer.as_deref() == Some(plugin_id) {
                    self.close_plugin_drawer();
                }
                self.open_plugin_tile(plugin_id);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::UiState;
    #[test]
    fn dock_tabs_keep_shell_and_plugin_state_across_selection_close_and_disable() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_terminal();
        s.ensure_term_tabs_seeded(true);
        let shell_id = s.term.tabs[0].id;
        s.term.pane.set_width(676.0);
        s.open_plugin_drawer("quicknotes");
        assert_eq!(s.right.width(), 676.0);
        s.right.set_width(734.0);
        s.quick_notes_edit.text = "unfinished".into();
        s.open_plugin_drawer("sysinfo");
        s.open_plugin_drawer("terminal");
        assert_eq!(s.dock_tabs, ["terminal", "quicknotes", "sysinfo"]);
        assert_eq!(s.term.pane.width(), 734.0);
        assert_eq!(s.term.tabs[s.term.active_tab].id, shell_id);
        assert!(s.term.sink.focused);
        s.close_plugin_dock_tab("sysinfo");
        assert_eq!(s.selected_dock_tab(), Some("terminal"));
        assert_eq!(s.dock_tabs, ["terminal", "quicknotes"]);
        s.open_plugin_drawer("sysinfo");
        s.open_plugin_drawer("terminal");
        s.edit(crate::config::ConfigEdit::PluginEnabled {
            plugin: "sysinfo".into(),
            on: false,
        });
        assert_eq!(s.dock_tabs, ["terminal", "quicknotes"]);
        s.open_plugin_drawer("quicknotes");
        assert_eq!(s.quick_notes_edit.text, "unfinished");
        assert!(!s.term.sink.focused);
        s.close_plugin_dock_tab("quicknotes");
        assert_eq!(s.selected_dock_tab(), Some("terminal"));
        assert_eq!(s.term.tabs[s.term.active_tab].id, shell_id);
        s.close_terminal();
        assert!(s.dock_tabs.is_empty());
        assert!(s.selected_dock_tab().is_none());
    }

    #[test]
    fn hiding_dock_restores_selected_plugin_or_existing_shell() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_terminal();
        s.ensure_term_tabs_seeded(true);
        let shell = s.term.tabs[s.term.active_tab].id;
        s.open_plugin_drawer("quicknotes");
        s.quick_notes_edit.text = "draft".into();
        s.open_plugin_drawer("sysinfo");
        s.open_plugin_drawer("quicknotes");
        s.toggle_right_panel();
        assert!(!s.right_panel_open());
        assert_eq!(s.dock_tabs, ["terminal", "quicknotes", "sysinfo"]);
        s.toggle_right_panel();
        assert_eq!(s.selected_dock_tab(), Some("quicknotes"));
        assert_eq!(s.quick_notes_edit.text, "draft");
        s.open_plugin_drawer("terminal");
        s.toggle_right_panel();
        assert!(!s.right_panel_open());
        s.toggle_right_panel();
        assert_eq!(s.selected_dock_tab(), Some("terminal"));
        assert_eq!(s.term.tabs[s.term.active_tab].id, shell);
    }

    #[test]
    fn moving_a_live_view_preserves_it_until_its_window_closes() {
        use crate::plugin::{DesktopPlugin, PluginSlot, PluginViewContext};

        #[derive(Default)]
        struct LivePanel(std::sync::Mutex<Option<String>>);
        impl DesktopPlugin for LivePanel {
            fn id(&self) -> &'static str {
                "live-test"
            }
            fn title(&self, _: crate::config::Language) -> String {
                self.id().into()
            }
            fn icon(&self) -> &'static rune::Icon {
                &crate::ui::icons::OPEN_EXTERNAL
            }
            fn render_tile(
                &self,
                _: &PluginViewContext<'_>,
                _: &mut Vec<rune::El>,
            ) -> Option<rune::El> {
                None
            }
            fn on_view_action(&self, _: &mut UiState, instance: &str, _: &str) -> bool {
                // A second mount would lose the live session.
                assert!(self.0.lock().unwrap().replace(instance.into()).is_none());
                true
            }
            fn can_move_view(&self, instance: &str) -> bool {
                self.0.lock().unwrap().as_deref() == Some(instance)
            }
            fn move_view(&self, from: &str, to: &str) {
                assert_eq!(
                    self.0.lock().unwrap().replace(to.into()).as_deref(),
                    Some(from)
                );
            }
            fn close_view(&self, instance: &str) {
                let mut live = self.0.lock().unwrap();
                if live.as_deref() == Some(instance) {
                    live.take();
                }
            }
        }
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("workspace-session".into());
        let live = std::sync::Arc::new(LivePanel::default());
        let id = live.id();
        s.plugins.register(live.clone());
        s.open_terminal();
        s.open_plugin_drawer(id);
        s.switch_plugin_surface(id, PluginSlot::Tile);
        let i = s.tile_of_plugin(id).unwrap();
        s.toggle_tile_pop_at(i);
        assert!(s.tiles[i].popped);
        s.switch_plugin_surface(id, PluginSlot::Page);
        assert!(live.can_move_view("page"));
        s.close_plugin_page();
        assert_eq!(s.tile_of_plugin(id), Some(i));
        s.switch_plugin_surface(id, PluginSlot::Drawer);
        assert!(live.can_move_view("drawer"));
        s.close_plugin_drawer();
        assert!(live.0.lock().unwrap().is_none());
        assert_eq!(s.selected_dock_tab(), Some("terminal"));
    }

    #[test]
    fn disabling_builtins_closes_current_surfaces_and_reenable_works_immediately() {
        use crate::config::ConfigEdit;
        use crate::ui::Mode;
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        for (plugin, mode) in [("tasks", Mode::Tasks), ("inbox", Mode::Inbox)] {
            s.go(mode);
            assert_eq!(s.mode, mode);
            s.edit(ConfigEdit::PluginEnabled {
                plugin: plugin.into(),
                on: false,
            });
            assert_eq!(s.mode, Mode::Home);
            s.go(mode);
            assert_eq!(s.mode, Mode::Home);
            s.edit(ConfigEdit::PluginEnabled {
                plugin: plugin.into(),
                on: true,
            });
            s.go(mode);
            assert_eq!(s.mode, mode);
        }
        s.toggle_inspector();
        assert!(s.right_open);
        s.edit(ConfigEdit::PluginEnabled {
            plugin: "inspector".into(),
            on: false,
        });
        assert!(!s.right_open);
        s.toggle_inspector();
        assert!(!s.right_open);
        s.edit(ConfigEdit::PluginEnabled {
            plugin: "inspector".into(),
            on: true,
        });
        s.toggle_inspector();
        assert!(s.right_open);
    }

    #[test]
    fn closing_implicit_inspector_closes_pane_and_restores_draft_focus() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.right_open = true;
        s.right.set_open(true);
        s.draft.focused = false;
        assert!(s.active_plugin_drawer.is_none());
        s.close_plugin_drawer();
        assert!(!s.right_open);
        assert!(!s.right.is_open());
        assert!(s.draft.focused);
        s.toggle_inspector();
        assert!(s.right_open);
        s.close_plugin_drawer();
        assert!(!s.right_open);
    }
}
