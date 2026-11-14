//! Window capabilities are distinct from non-window extension slots (toolbar, modal, command).
use super::{DesktopPlugin, PluginSlot, PluginViewContext};
use rune::{col, el, row, text, El};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    Tile,
    Page,
    Dock,
}
impl WindowKind {
    pub const ALL: [Self; 3] = [Self::Tile, Self::Page, Self::Dock];
    pub fn slot(self) -> PluginSlot {
        match self {
            Self::Tile => PluginSlot::Tile,
            Self::Page => PluginSlot::Page,
            Self::Dock => PluginSlot::Drawer,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Tile => "Tile",
            Self::Page => "Page",
            Self::Dock => "Dock",
        }
    }
    pub fn from_slot(slot: PluginSlot) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.slot() == slot)
    }
}

/// Every window is framed by the host. Plugins return their content only.
pub fn render(
    plugin: &dyn DesktopPlugin,
    ctx: &PluginViewContext<'_>,
    layer: &mut Vec<El>,
) -> Option<El> {
    WindowKind::from_slot(ctx.surface)?;
    if !plugin.supports_surface(ctx.surface) {
        return None;
    }
    let body = plugin.render_surface(ctx, layer)?;
    Some(chrome(plugin.id(), ctx, body))
}

pub fn chrome(id: &str, ctx: &PluginViewContext<'_>, body: El) -> El {
    let mut header = row()
        .key(format!("window-header-{id}-{:?}", ctx.surface))
        .w_full()
        .min_h(36.0)
        .px(10.0)
        .py(4.0)
        .gap(8.0)
        .center_y()
        .flex_wrap();
    if ctx.surface != PluginSlot::Tile {
        if let Some(plugin) = ctx.s.plugins.get(id) {
            header = header.child(
                text(plugin.title(ctx.s.cfg.general.language))
                    .font(ctx.k.ty.ui_medium)
                    .color(ctx.k.c.text_base),
            );
        }
    }
    header = header.child(el().grow(1.0)).child(super::surface_switcher(
        ctx.s,
        ctx.k,
        ctx.s.cfg.general.language,
        id,
        ctx.surface,
    ));
    col()
        .key(format!("plugin-window-{id}-{:?}", ctx.surface))
        .w_full()
        .h(0.0)
        .grow(1.0)
        .min_h(0.0)
        .child(header)
        .child(body.h(0.0).grow(1.0).min_h(0.0))
}
