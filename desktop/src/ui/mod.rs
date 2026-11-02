//! Blinc views. Everything here runs on the main thread; it reads the
//! [`Bindings`](crate::model::view::Bindings) and sends
//! [`Command`](crate::model::commands::Command)s, nothing else.

pub mod agents;
pub mod composer;
pub mod errors;
pub mod home;
pub mod markdown;
pub mod new_session;
pub mod parts;
pub mod sessions;
pub mod shell;
pub mod strings;
pub mod text;
pub mod timeline;

use std::sync::{Arc, Mutex};

use blinc_app::windowed::WindowedContext;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_area::SharedTextAreaState;
use blinc_theme::SystemSchemeWatcher;

use crate::model::commands::{self, Command, CommandSender};
use crate::model::drain::Drain;
use crate::model::projection::ConfigStore;
use crate::model::view::{Bindings, NewSessionDraft};
use strings::Strings;

/// The app's own stylesheet. Colours only through theme variables.
pub const STYLES: &str = include_str!("styles.css");

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
        self.draft_for(agent);
    }

    pub fn draft_for(&self, agent: String) {
        let cwd = self.config.get().general.initial_cwd(&agent);
        self.bindings
            .draft
            .signal()
            .set(Some(NewSessionDraft { agent, cwd }));
    }

    /// Start a session for `agent` in `cwd` straight away.
    pub fn create(&self, agent: String, cwd: std::path::PathBuf) {
        self.bindings.draft.signal().set(None);
        self.send(Command::CreateSession { agent, cwd });
    }
}

/// "Ctrl+X" or "⌘X".
pub fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

const KEY_ENTER: u32 = 13;
const KEY_ESCAPE: u32 = 27;
const KEY_0: u32 = 48;
const KEY_N: u32 = 78;

/// The window-wide shortcuts. Key-down reaches every key handler
/// (`renderer/events.rs` `broadcast_key_event`), so this one sees all keys
/// and only acts on its own chords.
fn on_key(app: &App, ctx: &blinc_layout::event_handler::EventContext) {
    let chord = ctx.ctrl || ctx.meta;
    let composer_focused = app.composer.lock().map(|s| s.is_focused()).unwrap_or(false);
    match ctx.key_code {
        KEY_N if chord => app.open_draft(),
        KEY_0 if chord => app.drain.go_home(),
        KEY_ESCAPE if composer_focused => {
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
        KEY_ESCAPE if app.bindings.draft.get().is_some() => app.bindings.draft.signal().set(None),
        KEY_ENTER if !chord && !composer_focused => {
            if let Some(draft) = app.bindings.draft.get() {
                app.create(draft.agent, draft.cwd);
            } else if app.bindings.selected.get().is_none() {
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
