//! Blinc views. Everything here runs on the main thread; it reads the
//! [`Bindings`](crate::model::view::Bindings) and sends
//! [`Command`](crate::model::commands::Command)s, nothing else.

pub mod agents;
pub mod composer;
pub mod errors;
pub mod glyphs;
pub mod home;
pub mod icons;
pub mod markdown;
pub mod motion;
pub mod parts;
pub mod sessions;
pub mod settings;
pub mod shell;
pub mod strings;
pub mod text;
pub mod themes;
pub mod timeline;

use std::sync::{Arc, Mutex};

use blinc_app::windowed::WindowedContext;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_area::SharedTextAreaState;
use blinc_theme::{ColorScheme, SystemSchemeWatcher, ThemeState};

use crate::config::{ConfigEdit, Scheme};
use crate::model::commands::{self, Command, CommandSender};
use crate::model::drain::Drain;
use crate::model::projection::ConfigStore;
use crate::model::view::Bindings;
use strings::Strings;

/// The app's own stylesheet. Colours only through theme variables.
pub const STYLES: &str = include_str!("styles.css");

/// Where the desktop keeps its files, for the About tab.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: std::path::PathBuf,
    pub state_dir: std::path::PathBuf,
}

/// Everything a view needs, cheap to clone into event handlers.
#[derive(Clone)]
pub struct App {
    pub commands: CommandSender,
    pub drain: Drain,
    pub bindings: Bindings,
    pub strings: Strings,
    pub config: Arc<ConfigStore>,
    pub composer: SharedTextAreaState,
    pub scheme_watcher: Arc<Mutex<Option<SystemSchemeWatcher>>>,
    pub paths: Arc<Paths>,
}

impl App {
    pub fn send(&self, command: Command) {
        commands::send(&self.commands, command);
    }

    pub fn open_session(&self, asid: &muqun_gateway::agents::AgentSessionId) {
        sessions::reveal(self, &asid.0);
        self.drain.select(&asid.0);
        self.send(Command::OpenSession(asid.clone()));
    }

    /// The agent a new session starts with: the last one used when it is
    /// still usable, else the first usable in the gateway's order.
    pub fn default_agent(&self) -> Option<String> {
        let agents = self.bindings.agents.get();
        let usable: Vec<String> = agents::by_standing(&agents)
            .into_iter()
            .filter(|(_, s)| *s == agents::Standing::Usable)
            .map(|(a, _)| a.id)
            .collect();
        let last = self.config.get().general.last_agent;
        last.filter(|a| usable.contains(a))
            .or_else(|| usable.first().cloned())
    }

    /// New session (Ctrl/Cmd+N, the sidebar's `+`): the start page, with the
    /// caret in its composer and every default filled in.
    pub fn open_draft(&self) {
        self.bindings.settings_open.signal().set(false);
        self.drain.go_home();
        composer::focus_input(self);
        self.bindings.inbox_open.signal().set(false);
    }

    /// The start page for a project (a project row's `+`; the sidebar calls it).
    #[allow(dead_code)]
    pub fn open_draft_in(&self, cwd: &std::path::Path) {
        composer::start_in(cwd.to_string_lossy().into_owned());
        self.open_draft();
    }

    pub fn open_settings(&self) {
        self.bindings.inbox_open.signal().set(false);
        self.bindings.settings_open.signal().set(true);
    }

    pub fn close_settings(&self) {
        self.bindings.settings_open.signal().set(false);
    }

    /// Switch the colour scheme now and remember it. `System` follows the OS
    /// from here on; an explicit choice stops following.
    pub fn apply_scheme(&self, scheme: Scheme) {
        let mut watcher = self.scheme_watcher.lock().unwrap();
        if let Some(mut old) = watcher.take() {
            old.stop();
        }
        let theme = ThemeState::get();
        match scheme {
            Scheme::Light => theme.set_scheme(ColorScheme::Light),
            Scheme::Dark => theme.set_scheme(ColorScheme::Dark),
            Scheme::System => {
                theme.set_scheme(blinc_theme::detect_system_color_scheme());
                *watcher = Some(SystemSchemeWatcher::start());
            }
        }
        drop(watcher);
        // Rows bake theme colours into their text.
        timeline::theme_changed(self);
        themes::note_scheme(scheme);
        self.send(Command::Edit(ConfigEdit::Scheme(scheme)));
    }

    /// Switch the language now (every view reads it on its next build, and a
    /// full rebuild is asked for) and remember it.
    pub fn apply_language(&self, language: crate::config::Language) {
        strings::Strings::set_language(language);
        self.send(Command::Edit(ConfigEdit::Language(language)));
        blinc_core::BlincContextState::get().request_rebuild();
    }
}

/// `Primary+<key>` as this platform writes it: "Ctrl+X" or "⌘X"
/// (`platform/keys.rs`).
pub fn shortcut(key: &str) -> String {
    crate::platform::keys::label(&format!("Primary+{key}"))
}

const KEY_ENTER: u32 = 13;
const KEY_ESCAPE: u32 = 27;
const KEY_0: u32 = 48;
const KEY_K: u32 = 75;
const KEY_N: u32 = 78;
const KEY_COMMA: u32 = 188;
const KEY_T: u32 = 84;
const KEY_PERIOD: u32 = 190;

/// The window-wide shortcuts. Key-down reaches every key handler
/// (`renderer/events.rs` `broadcast_key_event`), so this one sees all keys
/// and only acts on its own chords.
fn on_key(app: &App, ctx: &blinc_layout::event_handler::EventContext) {
    let chord = ctx.ctrl || ctx.meta;
    let toggle_themes = ctx.key_code == KEY_T && chord && ctx.shift;
    if !toggle_themes && themes::is_open() && themes::on_key(app, ctx) {
        return;
    }
    let composer_focused = app.composer.lock().map(|s| s.is_focused()).unwrap_or(false);
    match ctx.key_code {
        KEY_N if chord => app.open_draft(),
        KEY_K if chord => sessions::focus_search(),
        // The themes panel, until Settings → Appearance mounts it.
        KEY_T if toggle_themes => themes::toggle(),
        KEY_0 if chord => app.drain.go_home(),
        KEY_COMMA if chord => {
            if app.bindings.settings_open.get() {
                app.close_settings();
            } else {
                app.open_settings();
            }
        }
        // Esc: close what is open, else stop the running turn. Not gated on
        // the composer having focus: the text area gives up focus on the same
        // Esc, and key-down reaches every handler in no fixed order, so by
        // the time this runs `composer_focused` may already be false.
        // Esc closes what is open, nearest first: a pill menu or the
        // `@` list, then Settings, else it stops the running turn.
        KEY_ESCAPE if composer::dismiss() => {}
        KEY_ESCAPE if app.bindings.settings_open.get() => app.close_settings(),
        KEY_ESCAPE => interrupt(app),
        KEY_PERIOD if chord => interrupt(app),
        // On the start page with the caret elsewhere, Enter goes back to the
        // composer (typing then starts the message).
        KEY_ENTER
            if !chord
                && !composer_focused
                && app.bindings.selected.get().is_none()
                && !app.bindings.settings_open.get() =>
        {
            composer::focus_input(app)
        }
        _ => {}
    }
}

/// Stop the running turn of the open session, if there is one.
fn interrupt(app: &App) {
    if let Some(info) = app.bindings.header.get() {
        if matches!(
            info.status,
            muqun_gateway::agents::AgentSessionStatus::Busy
                | muqun_gateway::agents::AgentSessionStatus::Retry
        ) {
            app.send(Command::Interrupt(info.asid.clone()));
        }
    }
}

/// Add blinc_cn's component stylesheet and then the app's, once. Both via
/// `add_css`, so both are kept in `css_sources` and re-parsed against the
/// new theme variables on a scheme switch (`WindowedContext::reparse_css`);
/// a component stylesheet added with `add_component_stylesheet` is not
/// re-added there and would be lost on the first switch.
pub fn install_css(ctx: &mut WindowedContext) {
    for css in blinc_cn::cn_bundle().css_sources {
        ctx.add_css(&css);
    }
    ctx.add_css(STYLES);
    ctx.add_css(themes::THEMES_CSS);
    sync_agent_colours(ctx);
}

/// The scheme whose `--agent-<id>` block was added last (0 none, 1 light,
/// 2 dark). A scheme change rebuilds the whole tree; each rebuild calls this,
/// and a block for the new scheme goes in after the old ones (later rules win).
static AGENT_COLOURS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn sync_agent_colours(ctx: &mut WindowedContext) {
    use std::sync::atomic::Ordering;
    let scheme = ThemeState::get().scheme();
    let tag = match scheme {
        ColorScheme::Light => 1,
        ColorScheme::Dark => 2,
    };
    if AGENT_COLOURS.swap(tag, Ordering::Relaxed) != tag {
        ctx.add_css(&crate::theme::agents::css(scheme));
    }
}

pub fn root(ctx: &WindowedContext, app: &App) -> Div {
    let keys = app.clone();
    div()
        .id("mq-root")
        .class("mq-root")
        .w(ctx.width)
        .h(ctx.height)
        .flex_col()
        .child(shell::top_bar(app))
        .child(themes::shell_body(shell::body(ctx, app)))
        .child(themes::overlay(
            app,
            ctx.width,
            (ctx.height - shell::TOP_BAR_HEIGHT).max(0.0),
        ))
        .on_key_down(move |ctx| on_key(&keys, ctx))
}
