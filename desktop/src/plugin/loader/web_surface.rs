//! Host-owned, local WebViews mounted in ordinary plugin surface instances.
use super::*;
use crate::ui::{Mode, TileContent};

pub(super) fn local_panel_url(address: &str) -> bool {
    reqwest::Url::parse(address).is_ok_and(|url| {
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_some()
    })
}

impl ExternalPlugin {
    pub(super) fn clear_web_instance(&self, instance: &str) {
        self.web_surfaces.lock().unwrap().remove(instance);
    }

    pub(super) fn open_web_url(
        &self,
        s: &UiState,
        instance: &str,
        url: &str,
        owned_service: bool,
        target: Option<crate::plugin::draft::Target>,
    ) -> Result<(), String> {
        if !local_panel_url(url) {
            return Err("Expected a local HTTP panel on 127.0.0.1".into());
        }
        let helper = webview_helper().ok_or("The embedded WebView helper is unavailable")?;
        let surface = crate::plugin::webview_preview::Surface::new(
            ProcessCommand::new(helper),
            url.to_owned(),
            crate::plugin::context::web_context(s, owned_service),
            (owned_service && self.manifest.composer_draft).then_some(target),
        )?;
        self.web_surfaces
            .lock()
            .unwrap()
            .insert(instance.to_owned(), surface);
        Ok(())
    }

    pub(super) fn poll_web_surfaces(&self, s: &mut UiState) -> bool {
        let mut changed = false;
        for (instance, surface) in self.web_surfaces.lock().unwrap().iter() {
            let visible = match instance.as_str() {
                "drawer" => s.selected_dock_tab() == Some(self.id),
                "page" => s.active_plugin_page.as_deref() == Some(self.id),
                _ => s.active_plugin_page.is_none()
                    && s.mode == Mode::Session
                    && s.tiles.iter().enumerate().any(|(index, tile)| {
                        s.zoomed_tile.is_none_or(|zoom| zoom == index)
                            && matches!(&tile.content, TileContent::Custom { id, .. } if id == instance)
                    }),
            };
            surface.refresh_context(s);
            surface.set_visible(visible);
            changed |= surface.apply_pending_draft(s, visible);
            changed |= surface.sync();
        }
        changed
    }
}
