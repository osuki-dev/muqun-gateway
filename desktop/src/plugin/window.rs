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

/// Panel placement glyphs share an outline; the divider identifies the actual layout.
impl WindowKind {
    pub(super) fn glyph(self) -> &'static rune::Icon {
        match self {
            Self::Tile => &crate::ui::icons::catalog::WINDOW_TILE,
            Self::Page => &crate::ui::icons::catalog::WINDOW_PAGE,
            Self::Dock => &crate::ui::icons::catalog::WINDOW_DOCK_RIGHT,
        }
    }

    pub(super) fn float_glyph() -> &'static rune::Icon {
        &crate::ui::icons::catalog::WINDOW_FLOAT
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
    Some(chrome(plugin.id(), ctx, body, layer))
}

fn sanitize_window_title(raw: &str) -> String {
    let first = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let cleaned = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() > 80 {
        let head: String = cleaned.chars().take(79).collect();
        format!("{head}…")
    } else {
        cleaned
    }
}

/// Shared leading slot of the right dock header. Each shell is a tab of its own.
pub(crate) fn dock_tabs(s: &crate::ui::UiState, k: &rune_kit::Kit) -> Option<El> {
    use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant};
    if s.dock_tabs.len() < 2 && s.selected_dock_tab() != Some(super::browser::ID) {
        return None;
    }
    let mut tabs = row().gap(4.0).center_y();
    for id in &s.dock_tabs {
        if id == super::browser::ID {
            tabs = tabs.child(super::browser::tabs(s, k, true));
            continue;
        }
        if id == "terminal" {
            tabs = tabs.child(crate::ui::terminal_panel::shell_tabs(s, k, true));
            continue;
        }
        let Some(plugin) = s.plugins.get_enabled(id, &s.cfg.plugins_disabled) else {
            continue;
        };
        let active = s.selected_dock_tab() == Some(id.as_str());
        let raw_title = plugin
            .window_title(s)
            .unwrap_or_else(|| plugin.title(s.cfg.general.language));
        let title = sanitize_window_title(&raw_title);
        let select_id = id.clone();
        let close_id = id.clone();
        let close = crate::ui::copy::tr_ref(s.cfg.general.language, "Close tab");
        let select = Button::new(title.clone())
            .variant(if active {
                ButtonVariant::Outline
            } else {
                ButtonVariant::GhostMuted
            })
            .control_size(ControlSize::Regular)
            .on_press(move |s: &mut crate::ui::UiState| s.open_plugin_drawer(&select_id))
            .build(k)
            .key(format!("dock-tab-{id}"));
        let dismiss = IconButton::new(k.glyph.close)
            .label(format!("{close} · {title}"))
            .help(close)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut crate::ui::UiState| s.close_plugin_dock_tab(&close_id))
            .build(k)
            .key(format!("dock-tab-close-{id}"));
        tabs = tabs.child(row().center_y().child(select).child(dismiss));
    }
    Some(
        el().key("dock-tabs")
            .scroll_x()
            .h(34.0)
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .child(tabs),
    )
}

/// Append workspace/terminal actions to the unified `...` window menu.
pub(crate) fn append_dock_action_items(
    mut menu: rune_kit::Menu,
    s: &crate::ui::UiState,
    terminal: bool,
) -> rune_kit::Menu {
    use crate::ui::{copy::tr_ref, keymap, terminal_panel::TERMINAL_WINDOW, UiState};
    use rune_kit::MenuItem;

    let lang = s.cfg.general.language;
    let active_comp = s.active_session_computer();
    let is_remote = UiState::is_remote_computer_id(&active_comp);
    if is_remote {
        let host = s.computer_name(&active_comp);
        menu = menu
            .item(
                MenuItem::new(format!("{} ({host})", tr_ref(lang, "New remote terminal")))
                    .icon(&TERMINAL_WINDOW)
                    .shortcut(keymap::label(keymap::Action::TermNewTab))
                    .on_select_tx(|s: &mut UiState, tx| {
                        s.menu = None;
                        if !s.term.open && !s.has_terminal_tile() {
                            s.open_terminal();
                        }
                        s.term_new_tab();
                        s.focus_terminal_tx(tx);
                    }),
            )
            .item(
                MenuItem::new(tr_ref(lang, "New local terminal"))
                    .icon(&TERMINAL_WINDOW)
                    .on_select_tx(|s: &mut UiState, tx| {
                        s.menu = None;
                        if !s.term.open && !s.has_terminal_tile() {
                            s.open_terminal();
                        }
                        s.term_new_local_tab();
                        s.focus_terminal_tx(tx);
                    }),
            );
    } else {
        menu = menu.item(
            MenuItem::new(tr_ref(lang, "New terminal tab"))
                .icon(&TERMINAL_WINDOW)
                .shortcut(keymap::label(keymap::Action::TermNewTab))
                .on_select_tx(|s: &mut UiState, tx| {
                    s.menu = None;
                    if !s.term.open && !s.has_terminal_tile() {
                        s.open_terminal();
                    }
                    s.term_new_tab();
                    s.focus_terminal_tx(tx);
                }),
        );
    }
    menu = menu.item(
        MenuItem::new(tr_ref(lang, "Switch terminal to session folder"))
            .icon(&rune_kit::glyphs::FOLDER)
            .on_select_tx(|s: &mut UiState, tx| {
                s.menu = None;
                if !s.term.open && !s.has_terminal_tile() {
                    s.open_terminal();
                }
                s.term_sync_cwd();
                s.focus_terminal_tx(tx);
            }),
    );
    if s.plugins
        .get_enabled(super::browser::ID, &s.cfg.plugins_disabled)
        .is_some()
    {
        menu = menu.item(
            MenuItem::new(tr_ref(lang, "New browser tab"))
                .icon(&super::browser::GLOBE)
                .on_select_tx(|s: &mut UiState, tx| {
                    s.menu = None;
                    super::browser::new_tab(s);
                    tx.focus("browser-address", false);
                }),
        );
    }
    if terminal {
        menu = menu.separator().item(
            MenuItem::new(tr_ref(lang, "Restart terminal"))
                .icon(&crate::ui::icons::REFRESH)
                .on_select_tx(|s: &mut UiState, tx| {
                    s.menu = None;
                    s.restart_terminal();
                    s.focus_terminal_tx(tx);
                }),
        );
    }
    menu
}

pub fn chrome(id: &str, ctx: &PluginViewContext<'_>, body: El, layer: &mut Vec<El>) -> El {
    // Split tiles already have host chrome, including placement controls.
    if id == "terminal" || ctx.surface == PluginSlot::Tile {
        return body;
    }
    let mut header = row()
        .key(format!("window-header-{id}-{:?}", ctx.surface))
        .w_full()
        .h(rune_kit::shell::TOOLBAR_HEIGHT)
        .pl(10.0)
        .pr(6.0)
        .gap(6.0)
        .center_y();
    if let Some(tabs) = (ctx.surface == PluginSlot::Drawer)
        .then(|| dock_tabs(ctx.s, ctx.k))
        .flatten()
    {
        header = header.child(tabs);
    } else if let Some(plugin) = ctx.s.plugins.get(id) {
        let raw_title = plugin
            .window_title(ctx.s)
            .unwrap_or_else(|| plugin.title(ctx.s.cfg.general.language));
        header = header
            .child(
                plugin
                    .render_icon(ctx.s, 14.0, ctx.k.c.text_accent)
                    .a11y_hidden(),
            )
            .child(
                text(sanitize_window_title(&raw_title))
                    .font(ctx.k.ty.ui_medium)
                    .color(ctx.k.c.text_base)
                    .single_line()
                    .truncate()
                    .min_w(0.0)
                    .w(0.0)
                    .grow(1.0),
            );
    } else {
        header = header.child(el().grow(1.0));
    }
    let mut controls = row()
        .key(format!("window-controls-{id}-{:?}", ctx.surface))
        .gap(4.0)
        .center_y();
    if let Some(actions) = ctx
        .s
        .plugins
        .get(id)
        .and_then(|p| p.window_actions(ctx.s, ctx.k))
    {
        controls = controls.child(actions);
    }
    controls = controls.child(super::window_controls(
        ctx.s,
        ctx.k,
        ctx.s.cfg.general.language,
        id,
        ctx.surface,
        layer,
    ));
    header = header.child(controls);
    col()
        .key(format!("plugin-window-{id}-{:?}", ctx.surface))
        .w_full()
        .h(0.0)
        .grow(1.0)
        .min_h(0.0)
        .child(header)
        .child(
            el().w_full()
                .h(1.0)
                .fill(ctx.k.alpha(ctx.k.c.border_weak, 0.25)),
        )
        .child(body.h(0.0).grow(1.0).min_h(0.0))
}
