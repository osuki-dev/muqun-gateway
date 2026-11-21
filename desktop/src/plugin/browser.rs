//! User-navigable web tabs. Each tab owns an isolated browser-mode WebView.
use super::{
    loader, webview_preview::Surface, DesktopPlugin, PluginCommand, PluginSlot, PluginViewContext,
};
use crate::config::Language;
use crate::ui::{copy::tr_ref, icons, UiState};
use reqwest::Url;
use rune::{col, el, icon, row, text, El, Icon, TextEdit, Tx};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, TextInput};
use std::process::Command;

pub(crate) const ID: &str = "browser";
pub(super) const GLOBE: Icon = Icon {
    name: "browser-globe",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18"/></g>"#,
};
const BACK: Icon = Icon {
    name: "browser-back",
    view: 24.0,
    body: r#"<path d="m13 5-7 7 7 7M6 12h14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
const FORWARD: Icon = Icon {
    name: "browser-forward",
    view: 24.0,
    body: r#"<path d="m11 5 7 7-7 7M18 12H4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

#[derive(Default)]
pub(crate) struct Browser {
    tabs: Vec<Tab>,
    selected: usize,
    next_id: u64,
    pub(crate) address: TextEdit,
}

#[derive(Default)]
struct Tab {
    id: u64,
    location: String,
    draft: TextEdit,
    error: Option<String>,
    page: Option<Surface>,
}

impl Browser {
    fn select(&mut self, index: usize) {
        if self.tabs.get(index).is_none() || index == self.selected {
            return;
        }
        self.address.focused = false;
        std::mem::swap(&mut self.tabs[self.selected].draft, &mut self.address);
        self.selected = index;
        std::mem::swap(&mut self.tabs[index].draft, &mut self.address);
    }

    fn add(&mut self) {
        self.next_id += 1;
        self.tabs.push(Tab {
            id: self.next_id,
            ..Tab::default()
        });
        self.select(self.tabs.len() - 1);
    }

    fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if index == self.selected && self.tabs.len() > 1 {
            self.select(if index == 0 { 1 } else { index - 1 });
        }
        self.tabs.remove(index);
        if index < self.selected {
            self.selected -= 1;
        }
        if self.tabs.is_empty() {
            self.address = TextEdit::default();
        }
    }

    fn navigate(&mut self, lang: Language, tx: &mut Tx) {
        let Some(tab) = self.tabs.get_mut(self.selected) else {
            return;
        };
        let Some(url) = address_url(&self.address.text) else {
            tab.error = Some(tr_ref(lang, "Enter a valid HTTP or HTTPS address.").into());
            return;
        };
        let address = url.to_string();
        if let Some(page) = &tab.page {
            page.navigate(&address);
        } else {
            let Some(helper) = loader::webview_helper() else {
                tab.error = Some(tr_ref(lang, "WebView is unavailable.").into());
                return;
            };
            match Surface::browser(Command::new(helper), address.clone()) {
                Ok(page) => tab.page = Some(page),
                Err(error) => {
                    tab.error = Some(error);
                    return;
                }
            }
        }
        tab.location = address.clone();
        self.address = TextEdit::new(address);
        tab.error = None;
        if let Some(page) = &tab.page {
            page.focus(tx);
        }
    }

    fn poll(&mut self, opened: bool) -> bool {
        let mut changed = false;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let Some(page) = &tab.page else { continue };
            let visible = opened && self.selected == index;
            page.set_visible(visible);
            if visible && self.address.focused {
                page.set_focused(false);
            }
            if !page.sync() {
                continue;
            }
            changed = true;
            let location = page.navigation().url;
            if location != tab.location && !location.is_empty() {
                let draft = if self.selected == index {
                    &mut self.address
                } else {
                    &mut tab.draft
                };
                if draft.text == tab.location || !draft.focused {
                    *draft = TextEdit::new(&location);
                }
                tab.location = location;
            }
        }
        changed
    }
}

fn address_url(input: &str) -> Option<Url> {
    let value = input.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let url = if value.contains("://") {
        Url::parse(value).ok()?
    } else {
        // An explicit port on a host is useful for local development.
        if value.starts_with(['/', '\\', '?', '#']) {
            return None;
        }
        Url::parse(&format!("https://{value}")).ok()?
    };
    (matches!(url.scheme(), "https" | "http")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

pub(super) fn new_tab(s: &mut UiState) {
    if !s.browser.tabs.is_empty() {
        s.browser.add();
    }
    s.switch_plugin_surface(ID, PluginSlot::Drawer);
    s.draft.focused = false;
}

fn shortcut(s: &mut UiState, event: &rune::KeyEvent, tx: &mut Tx) -> bool {
    if !event.mods.primary() || event.mods.alt || event.mods.shift {
        return false;
    }
    let rune::Key::Command(letter) = event.key else {
        return false;
    };
    match letter.to_ascii_lowercase() {
        'l' => {
            tx.focus("browser-address", false);
            s.browser
                .address
                .set_selection(0, s.browser.address.text.len());
        }
        't' => {
            s.browser.add();
            tx.focus("browser-address", false);
        }
        'r' => {
            let Some(page) = s
                .browser
                .tabs
                .get(s.browser.selected)
                .and_then(|tab| tab.page.as_ref())
            else {
                return false;
            };
            page.reload();
        }
        _ => return false,
    }
    true
}

pub(crate) fn tabs(s: &UiState, k: &Kit, in_dock: bool) -> El {
    let mut strip = row().center_y().gap(3.0);
    for (index, tab) in s.browser.tabs.iter().enumerate() {
        let nav = tab
            .page
            .as_ref()
            .map(Surface::navigation)
            .unwrap_or_default();
        let current = !tab.location.is_empty() && nav.url == tab.location;
        let favicon = current.then_some(nav.favicon).flatten();
        let title = if current && !nav.title.is_empty() {
            nav.title
        } else {
            Url::parse(&tab.location)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| tr_ref(s.cfg.general.language, "Browser").to_owned())
        };
        let active = (!in_dock || s.selected_dock_tab() == Some(ID)) && s.browser.selected == index;
        strip = strip.child(
            Button::new(title.clone())
                .help(title.clone())
                .label_with(move |font, ink| {
                    row()
                        .center_y()
                        .gap(4.0)
                        .child(favicon.as_ref().map_or_else(
                            || icon(&GLOBE, 14.0).color(ink),
                            |image| el().w(14.0).h(14.0).pixels(image),
                        ))
                        .child(text(title).font(font).color(ink).max_w(170.0).truncate())
                })
                .variant(if active {
                    ButtonVariant::Outline
                } else {
                    ButtonVariant::GhostMuted
                })
                .on_press(move |s: &mut UiState| {
                    if in_dock {
                        s.open_plugin_drawer(ID);
                    }
                    s.browser.select(index);
                })
                .build(k)
                .key(format!("browser-tab-{}", tab.id)),
        );
        if in_dock || s.browser.tabs.len() > 1 {
            let close = tr_ref(s.cfg.general.language, "Close tab");
            strip = strip.child(
                IconButton::new(k.glyph.close)
                    .label(close)
                    .help(close)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.browser.close(index);
                        if in_dock && s.browser.tabs.is_empty() {
                            s.close_plugin_dock_tab(ID);
                        }
                    })
                    .build(k)
                    .key(format!("browser-close-{}", tab.id)),
            );
        }
    }
    strip
}

pub(crate) struct BrowserPlugin;
impl DesktopPlugin for BrowserPlugin {
    fn id(&self) -> &'static str {
        ID
    }
    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Browser").into()
    }
    fn icon(&self) -> &'static Icon {
        &GLOBE
    }
    fn can_move_view(&self, _: &str) -> bool {
        true
    }
    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            id: "browser.new",
            title: "New browser tab".into(),
            shortcut: None,
        }]
    }
    fn on_command(&self, s: &mut UiState, command: &str) -> bool {
        if command != "browser.new" {
            return false;
        }
        new_tab(s);
        true
    }
    fn on_view_action(&self, s: &mut UiState, _instance: &str, action: &str) -> bool {
        if action != "open" {
            return false;
        }
        if s.browser.tabs.is_empty() {
            s.browser.add();
        }
        true
    }
    fn poll(&self, s: &mut UiState, _: &mut Tx) -> bool {
        let in_page = s.active_plugin_page.as_deref() == Some(ID);
        let in_dock = s.selected_dock_tab() == Some(ID) && s.inspect.is_none();
        let tile = s.tile_of_plugin(ID);
        let has_browser = in_page || tile.is_some() || s.dock_tabs.iter().any(|id| id == ID);
        if !has_browser && !s.browser.tabs.is_empty() {
            s.browser = Browser::default();
            return true;
        }
        let in_tile = tile.is_some_and(|i| {
            s.mode == crate::ui::Mode::Session && s.zoomed_tile.is_none_or(|zoom| zoom == i)
        });
        s.browser
            .poll(in_page || (s.active_plugin_page.is_none() && (in_dock || in_tile)))
    }
    fn render_tile(&self, ctx: &PluginViewContext<'_>, _: &mut Vec<El>) -> Option<El> {
        let s = ctx.s;
        let k = ctx.k;
        let lang = s.cfg.general.language;
        let tab = s.browser.tabs.get(s.browser.selected)?;
        let nav = tab
            .page
            .as_ref()
            .map(Surface::navigation)
            .unwrap_or_default();
        let control =
            |glyph: &'static Icon, label: &'static str, available: bool, op: fn(&Surface)| {
                IconButton::new(glyph)
                    .label(tr_ref(lang, label))
                    .help(tr_ref(lang, label))
                    .disabled(!available)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        if let Some(page) = s
                            .browser
                            .tabs
                            .get(s.browser.selected)
                            .and_then(|t| t.page.as_ref())
                        {
                            op(page)
                        }
                    })
                    .build(k)
            };
        let mut bar = row()
            .w_full()
            .center_y()
            .gap(4.0)
            .padding(6.0)
            .child(control(&BACK, "Back", nav.back, Surface::back))
            .child(control(&FORWARD, "Forward", nav.forward, Surface::forward))
            .child(control(
                &icons::REFRESH,
                "Reload",
                tab.page.is_some(),
                Surface::reload,
            ))
            .child(
                TextInput::new(&s.browser.address)
                    .label(tr_ref(lang, "Web address"))
                    .placeholder(tr_ref(lang, "Enter a URL"))
                    .on_key(shortcut)
                    .on_edit(|s: &mut UiState| &mut s.browser.address)
                    .on_submit(|s: &mut UiState, tx| s.browser.navigate(s.cfg.general.language, tx))
                    .build(k)
                    .key("browser-address")
                    .grow(1.0)
                    .min_w(0.0),
            );
        if ctx.surface != PluginSlot::Drawer {
            let label = tr_ref(lang, "New browser tab");
            bar = bar.child(
                IconButton::new(&icons::PLUS)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .label(label)
                    .help(label)
                    .on_press_tx(|s: &mut UiState, tx| {
                        s.browser.add();
                        tx.focus("browser-address", false);
                    })
                    .build(k)
                    .key("browser-new"),
            );
        }
        let mut body = col().w_full().grow(1.0).min_h(0.0).child(bar);
        if ctx.surface != PluginSlot::Drawer && s.browser.tabs.len() > 1 {
            body = body.child(rune::el().scroll_x().h(34.0).child(tabs(s, k, false)));
        }
        if let Some(error) = &tab.error {
            body = body.child(text(error).color(k.c.text_base).padding(8.0));
        }
        if let Some(page) = &tab.page {
            body = body.child(page.view_with_keys(k, ctx.scale, Some(shortcut)));
        }
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_bar_accepts_web_hosts_but_not_local_files_script_or_credentials() {
        assert_eq!(
            address_url("example.org/path").unwrap().as_str(),
            "https://example.org/path"
        );
        assert_eq!(
            address_url(" http://localhost:8080/ ").unwrap().port(),
            Some(8080)
        );
        for input in [
            "file:///etc/passwd",
            "/etc/passwd",
            "javascript:alert(1)",
            "https://u:p@example.org",
        ] {
            assert!(
                address_url(input).is_none(),
                "unexpected navigation: {input}"
            );
        }
    }
    #[test]
    fn switching_and_closing_tabs_preserves_other_address_drafts() {
        let mut browser = Browser::default();
        browser.add();
        browser.address = TextEdit::new("first.test");
        browser.add();
        browser.address = TextEdit::new("second.test");
        browser.select(0);
        assert_eq!(browser.address.text, "first.test");
        browser.close(0);
        assert_eq!(browser.address.text, "second.test");
        assert_eq!(browser.selected, 0);
    }
    #[test]
    fn moving_browser_with_two_tabs_between_dock_tile_and_page_preserves_drafts() {
        let (tx, _) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        state.plugins.register(std::sync::Arc::new(BrowserPlugin));
        state.open_terminal();
        new_tab(&mut state);
        state.browser.address = TextEdit::new("first.example");
        new_tab(&mut state);
        state.browser.address = TextEdit::new("second.example");
        for slot in [PluginSlot::Tile, PluginSlot::Page, PluginSlot::Drawer] {
            state.switch_plugin_surface(ID, slot);
            BrowserPlugin.poll(&mut state, &mut Tx::default());
            assert_eq!(state.browser.tabs.len(), 2);
            assert_eq!(state.browser.address.text, "second.example");
        }
        state.browser.select(0);
        assert_eq!(state.browser.address.text, "first.example");
        state.close_plugin_drawer();
        BrowserPlugin.poll(&mut state, &mut Tx::default());
        assert!(state.browser.tabs.is_empty());
    }
}
