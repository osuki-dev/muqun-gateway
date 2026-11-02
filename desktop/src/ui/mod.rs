//! Blinc views. Everything here runs on the main thread; it reads the
//! [`Bindings`](crate::model::view::Bindings) and sends
//! [`Command`](crate::model::commands::Command)s, nothing else.

pub mod agents;
pub mod composer;
pub mod errors;
pub mod home;
pub mod icons;
pub mod new_session;
pub mod sessions;
pub mod settings;
pub mod shell;
pub mod strings;
pub mod text;
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
use crate::model::view::{Bindings, NewSessionDraft};
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

    /// Open the New-session page with the defaults filled in.
    pub fn open_draft(&self) {
        let agent = self
            .default_agent()
            .unwrap_or_else(|| crate::hub::OPENCODE.to_string());
        self.bindings.settings_open.signal().set(false);
        self.draft_for(agent);
    }

    /// The New-session page for `agent`: its last folder preselected, its
    /// known projects on the way. (The folder lives in the chooser's value,
    /// not in the draft, so choosing one does not rebuild the page.)
    pub fn draft_for(&self, agent: String) {
        let cwd = self.config.get().general.initial_cwd(&agent);
        self.bindings
            .chooser
            .signal()
            .set(cwd.to_string_lossy().into_owned());
        self.bindings.projects.signal().set(Arc::default());
        self.send(Command::LoadProjects {
            agent: agent.clone(),
        });
        self.bindings
            .draft
            .signal()
            .set(Some(NewSessionDraft { agent }));
    }

    /// Start a session for `agent` in `cwd` straight away.
    pub fn create(&self, agent: String, cwd: std::path::PathBuf) {
        self.bindings.draft.signal().set(None);
        self.send(Command::CreateSession { agent, cwd });
    }

    /// Create from the New-session page: the chooser's folder, `~` expanded
    /// first. A `~user` the gateway cannot expand is said, not guessed at.
    pub fn create_from_draft(&self) {
        let Some(draft) = self.bindings.draft.get() else {
            return;
        };
        let raw = self.bindings.chooser.get();
        match muqun_gateway::agents::directories::expand_home(raw.trim()) {
            Some(cwd) if !raw.trim().is_empty() => self.create(draft.agent, cwd),
            _ => self
                .bindings
                .error
                .signal()
                .set(Some(crate::model::inbox::UiError {
                    message: self.strings.folder_unusable(&raw),
                    detail: None,
                    unreachable: false,
                })),
        }
    }

    pub fn open_settings(&self) {
        self.bindings.draft.signal().set(None);
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

/// "Ctrl+X" or "⌘X".
pub fn shortcut(key: &str) -> String {
    format!("{}{key}", crate::platform::shell::primary_modifier())
}

const KEY_ENTER: u32 = 13;
const KEY_ESCAPE: u32 = 27;
const KEY_0: u32 = 48;
const KEY_N: u32 = 78;
const KEY_COMMA: u32 = 188;

/// The window-wide shortcuts. Key-down reaches every key handler
/// (`renderer/events.rs` `broadcast_key_event`), so this one sees all keys
/// and only acts on its own chords.
fn on_key(app: &App, ctx: &blinc_layout::event_handler::EventContext) {
    let chord = ctx.ctrl || ctx.meta;
    let composer_focused = app.composer.lock().map(|s| s.is_focused()).unwrap_or(false);
    match ctx.key_code {
        KEY_N if chord => app.open_draft(),
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
        KEY_ESCAPE if app.bindings.settings_open.get() => app.close_settings(),
        KEY_ESCAPE if app.bindings.draft.get().is_some() => {
            // An open folder list closes first; the page only then.
            if !new_session::close_chooser() {
                app.bindings.draft.signal().set(None);
            }
        }
        KEY_ESCAPE => {
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
        KEY_ENTER if !chord && !composer_focused => {
            if app.bindings.draft.get().is_some() {
                // Typed text in the folder list is committed first; Enter
                // again creates. Nothing typed: Enter takes the defaults.
                if !new_session::accept_typed_folder(app) {
                    app.create_from_draft();
                }
            } else if app.bindings.selected.get().is_none() && !app.bindings.settings_open.get() {
                app.open_draft();
            }
        }
        _ => {}
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
        .child(shell::body(ctx, app))
        .on_key_down(move |ctx| on_key(&keys, ctx))
}
