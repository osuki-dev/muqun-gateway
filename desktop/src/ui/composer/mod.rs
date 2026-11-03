//! The composer, shared by the start page and the session page: a growing
//! text area, 📎 attach (dialog or a pasted image), staged files above it,
//! a row of selector pills under it (project, agent, model, mode / effort,
//! branch), the context readout and the primary action (architecture
//! §6.0‴ B and C, §6.1c). Waiting prompts live in the right-panel Queue tab.
//!
//! The text area inserts a newline on every Enter itself
//! (`blinc_layout/src/widgets/text_area/handlers.rs`, key 13) and moves the
//! caret on ↑ / ↓, and key-down is broadcast to every key handler in no
//! guaranteed order (`renderer/events.rs` `broadcast_key_event`). So the
//! handlers here never edit the text themselves: they post a [`UiAction`] to
//! the inbox and the drain runs [`on_key`] / [`edited`] after the key has been
//! dispatched to everyone -- by which time the newline is in (and is taken
//! back out) or the caret has moved (and is put back).

pub(crate) mod chips;
pub mod pills;
mod popup;
pub mod qa;
pub(crate) mod state;
pub mod triggers;
mod usage;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::ui::glyphs::Glyph;
use crate::ui::icons;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_area::{focus_text_area_deferred, refresh_text_area};
use muqun_gateway::agents::{AgentSessionId, AgentSessionInfo, AgentSessionStatus, ModelRef};

use super::agents::{self, Standing};
use super::text::{mono_text, ui_text};
use super::App;
use crate::config::{choice_key, ConfigEdit, START_DRAFT};
use crate::hub::{CatalogView, OPENCODE};
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};
use crate::model::inbox::{UiAction, UiError};
use state::{Cmp, Staged, StagedStatus};

// (`restore_after_failure` and `after_submit_ok` are reached from `state.rs`.)
use triggers::{Intent, Kind};

pub use pills::short_project;
pub use state::apply_update;

const KEY_TAB: u32 = 9;
const KEY_ENTER: u32 = 13;
const KEY_UP: u32 = 38;
const KEY_DOWN: u32 = 40;
const KEY_V: u32 = 86;

/// The session page's reserved height for the default two-row card (slot
/// padding + field + pills). Extra text rows and staged files add on top via
/// [`reserved_height`] so the timeline never shows through a translucent
/// overlay. The prompt queue lives in the right panel, not here.
pub const COMPOSER_HEIGHT: f32 = 148.0;
/// Text area rows: 56 px at two, then up to six.
const MIN_ROWS: usize = 2;
const MAX_ROWS: usize = 6;
const EXTRA_ROW_PX: f32 = 20.0;
const STAGED_ROW_PX: f32 = 36.0;

/// Strip height for the open session: base card plus whatever sits above the
/// field right now. Shell subtracts this from the column so the timeline
/// ends above the card instead of painting under it.
pub fn reserved_height() -> f32 {
    let cmp = Cmp::get();
    let extra_rows = cmp.rows.get().saturating_sub(MIN_ROWS) as f32 * EXTRA_ROW_PX;
    let staged_h = if cmp.staged.get().is_empty() {
        0.0
    } else {
        STAGED_ROW_PX
    };
    COMPOSER_HEIGHT + extra_rows + staged_h
}
/// Most files staged on one message.
const MAX_STAGED: usize = 10;
/// The start page's composer card, wide; narrower windows take 100 % − 80.
pub const START_WIDTH: f32 = 720.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Start,
    Session,
}

/// What the composer is aimed at right now.
#[derive(Clone)]
pub struct Aim {
    pub place: Place,
    /// The draft's key: the start page's, or the session's id.
    pub key: String,
    pub agent: String,
    pub agent_name: String,
    pub standing: Standing,
    pub reason: Option<String>,
    /// The folder, `~` expanded.
    pub cwd: Option<String>,
    pub info: Option<Arc<AgentSessionInfo>>,
    pub busy: bool,
    pub catalog: Option<Arc<CatalogView>>,
    pub model: Option<ModelRef>,
    pub mode: Option<String>,
}

impl Aim {
    pub fn asid(&self) -> Option<AgentSessionId> {
        self.info.as_ref().map(|i| i.asid.clone())
    }
}

/// `provider/model#variant` as the config keeps a remembered model.
pub(super) fn model_to_string(model: &ModelRef) -> String {
    let base = format!("{}/{}", model.provider_id, model.model_id);
    match &model.variant {
        Some(v) => format!("{base}#{v}"),
        None => base,
    }
}

fn model_from_string(text: &str) -> Option<ModelRef> {
    let (base, variant) = match text.split_once('#') {
        Some((b, v)) => (b, Some(v.to_string())),
        None => (text, None),
    };
    let (provider, model) = base.split_once('/')?;
    Some(ModelRef {
        provider_id: provider.to_string(),
        model_id: model.to_string(),
        variant,
    })
}

/// Resolve what the composer targets: for a session, the open one; for the
/// start page, the owner's pills over the defaults (the session's folder,
/// the folder and agent last used, what that pair last chose).
pub fn aim(app: &App, place: Place) -> Aim {
    let agents_list = app.bindings.agents.get();
    let cmp = Cmp::get();
    let (agent, info, cwd, key) = match place {
        Place::Session => {
            let info = app.bindings.header.get();
            let agent = info
                .as_ref()
                .map(|i| i.agent_id.clone())
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| OPENCODE.to_string());
            let cwd = info
                .as_ref()
                .and_then(|i| i.directory.clone())
                .filter(|d| !d.is_empty());
            let key = info
                .as_ref()
                .map(|i| i.asid.0.clone())
                .unwrap_or_else(|| START_DRAFT.to_string());
            (agent, info, cwd, key)
        }
        Place::Start => {
            let choice = cmp.start.get();
            let usable = |id: &str| {
                agents_list
                    .iter()
                    .any(|a| a.id == id && Standing::of(a) == Standing::Usable)
            };
            let agent = choice
                .agent
                .clone()
                .filter(|a| usable(a))
                .or_else(|| app.default_agent())
                .unwrap_or_else(|| OPENCODE.to_string());
            let cwd = choice.project.clone().unwrap_or_else(|| {
                app.config
                    .get()
                    .general
                    .initial_cwd(&agent)
                    .to_string_lossy()
                    .into_owned()
            });
            (agent, None, Some(cwd), START_DRAFT.to_string())
        }
    };
    let discovery = agents_list.iter().find(|a| a.id == agent);
    let standing = discovery.map(Standing::of).unwrap_or(Standing::Checking);
    let reason = discovery
        .and_then(|a| a.reason.clone())
        .filter(|r| !r.is_empty());
    let busy = info.as_ref().is_some_and(|i| {
        matches!(
            i.status,
            AgentSessionStatus::Busy | AgentSessionStatus::Retry
        )
    });
    let catalog = cmp
        .catalogs
        .get()
        .loaded
        .get(&state::catalog_key(&agent, cwd.as_deref()))
        .cloned();
    let (model, mode) = match (&info, place) {
        (Some(info), Place::Session) => (info.model.clone(), info.mode.clone()),
        _ => {
            let choice = cmp.start.get();
            let config = app.config.get();
            let remembered = cwd
                .as_deref()
                .map(|c| choice_key(&agent, c))
                .map(|k| {
                    (
                        config.general.last_model.get(&k).cloned(),
                        config.general.last_mode.get(&k).cloned(),
                    )
                })
                .unwrap_or((None, None));
            let defaults = catalog.as_ref().map(|c| &c.catalog.defaults);
            let model = choice
                .model
                .clone()
                .or_else(|| remembered.0.as_deref().and_then(model_from_string))
                .or_else(|| defaults.and_then(|d| d.model.clone()));
            let mode = choice
                .mode
                .clone()
                .or(remembered.1)
                .or_else(|| defaults.and_then(|d| d.mode.clone()));
            (model, mode)
        }
    };
    Aim {
        place,
        key,
        agent_name: agents::name(&agent, &agents_list),
        agent,
        standing,
        reason,
        cwd,
        info,
        busy,
        catalog,
        model,
        mode,
    }
}

/// Ask the core for what the pills and popups need, once per agent and
/// folder (and once per folder for its branch).
fn ensure_loaded(app: &App, aim: &Aim) {
    // Don't fight a cold session open for CPU with a 500-model catalogue.
    if app.bindings.order.get().opening {
        return;
    }
    // Catalogues (OpenCode: hundreds of models) can take well over 6s; a short
    // retry stampedes concurrent RPCs and fights session open for CPU.
    let catalog_retry = std::time::Duration::from_secs(45);
    let retry = std::time::Duration::from_secs(6);
    let key = state::catalog_key(&aim.agent, aim.cwd.as_deref());
    if aim.standing == Standing::Usable && state::ask_due(&key, aim.catalog.is_some(), catalog_retry)
    {
        app.send(Command::Compose(ComposeCommand::Catalog {
            agent: aim.agent.clone(),
            directory: aim.cwd.clone(),
            asid: aim.asid(),
        }));
    }
    if let Some(cwd) = &aim.cwd {
        let known = Cmp::get().branches.get().contains_key(cwd);
        if state::ask_due(&format!("branch\u{1f}{cwd}"), known, retry) {
            app.send(Command::Compose(ComposeCommand::Branch {
                cwd: cwd.clone(),
            }));
        }
    }
    if aim.place == Place::Start && aim.standing == Standing::Usable {
        let known = app.bindings.projects.get().agent == aim.agent;
        if state::ask_due(&format!("projects\u{1f}{}", aim.agent), known, retry) {
            app.send(Command::LoadProjects {
                agent: aim.agent.clone(),
            });
        }
    }
    let peers_known = !Cmp::get().peers.get().is_empty();
    if state::ask_due("peers", peers_known, std::time::Duration::from_secs(30)) {
        app.send(Command::Compose(ComposeCommand::ListPeers));
    }
}

// ---------------------------------------------------------------------------
// The surface the text area belongs to: drafts survive switching
// ---------------------------------------------------------------------------

/// Called whenever the composer is built: when the surface changed, put that
/// surface's own draft into the text area. Nothing is saved here -- drafts
/// are saved as they are typed.
///
/// Must not notify deps that this same view rebuilds on unless the value
/// actually changed: start and session composers share `staged` / `rows`, and
/// a spurious `set` while both still answer those signals flips
/// `current_surface` forever until the stack overflows.
fn sync_surface(app: &App, key: &str) {
    if state::current_surface().as_deref() == Some(key) {
        return;
    }
    let first = state::current_surface().is_none();
    state::set_surface(key);
    let cmp = Cmp::get();
    let draft = app
        .config
        .get()
        .general
        .drafts
        .get(key)
        .cloned()
        .unwrap_or_default();
    if let Ok(mut s) = app.composer.lock() {
        if s.value() != draft || !first {
            s.set_value(&draft);
        }
    }
    refresh_text_area(&app.composer);
    state::draft_changed(key, &draft);
    if !cmp.staged.get().is_empty() {
        cmp.staged.signal().set(Arc::new(Vec::new()));
    }
    set_popup(state::Popup::default());
    pills::close_menu();
    let rows = rows_for(&draft);
    if cmp.rows.get() != rows {
        cmp.rows.signal().set(rows);
    }
    if key == START_DRAFT || state::inbox_of().as_deref() != Some(key) {
        state::clear_inbox();
    }
}

/// How many rows `text` needs: its lines, plus a line for every ~80
/// characters a long one wraps to.
fn rows_for(text: &str) -> usize {
    let rows: usize = text
        .split('\n')
        .map(|l| 1 + l.chars().count().saturating_sub(1) / 80)
        .sum();
    rows.clamp(MIN_ROWS, MAX_ROWS)
}

pub(crate) fn current_text(app: &App) -> (String, usize) {
    match app.composer.lock() {
        Ok(s) => {
            let caret = triggers::caret_index(&s.lines, s.cursor.line, s.cursor.column);
            (s.value(), caret)
        }
        Err(_) => (String::new(), 0),
    }
}

pub(crate) fn set_text(app: &App, text: &str, caret: usize) {
    if let Ok(mut s) = app.composer.lock() {
        s.set_value(text);
        let (line, column) = triggers::caret_position(text, caret);
        s.cursor = blinc_layout::widgets::text_area::TextPosition::new(line, column);
    }
    refresh_text_area(&app.composer);
}

// ---------------------------------------------------------------------------
// After an edit: draft, height, trigger popup
// ---------------------------------------------------------------------------

/// The text or caret may have changed (the drain runs this once per batch of
/// keystrokes, after the text area has seen them).
pub fn edited(app: &App) {
    let Some(key) = state::current_surface() else {
        return;
    };
    let cmp = Cmp::get();
    let (text, caret) = current_text(app);
    if state::draft_changed(&key, &text) {
        app.send(Command::Edit(ConfigEdit::Draft {
            key: key.clone(),
            text: text.clone(),
        }));
    }
    let rows = rows_for(&text);
    if cmp.rows.get() != rows {
        cmp.rows.signal().set(rows);
    }
    update_popup(app, &key, &text, caret);
}

fn place_of(key: &str) -> Place {
    if key == START_DRAFT {
        Place::Start
    } else {
        Place::Session
    }
}

fn update_popup(app: &App, key: &str, text: &str, caret: usize) {
    let cmp = Cmp::get();
    let current = cmp.popup.get();
    let found = triggers::find(text, caret);
    let aim = aim(app, place_of(key));
    // A trigger only opens what the composer can answer. `@` always may open
    // (computers need no folder; files need cwd for the walk). `/` and `$`
    // open as soon as the agent is usable so a cold catalogue still shows
    // Loading… instead of a dead caret.
    let found = found.filter(|t| match t.kind {
        Kind::File => true,
        Kind::Command | Kind::Skill => {
            aim.standing == Standing::Usable || aim.catalog.is_some()
        }
    });
    let found = found.filter(|t| current.dismissed_at != Some(t.start));
    let dismissed_at = match (&found, current.dismissed_at) {
        // Still the dismissed word: stay dismissed; anything else clears it.
        (None, Some(at)) if triggers::find(text, caret).is_some_and(|t| t.start == at) => Some(at),
        _ => None,
    };
    if found == current.trigger && dismissed_at == current.dismissed_at {
        return;
    }
    let selected = match (&found, &current.trigger) {
        (Some(a), Some(b)) if a.kind == b.kind && a.start == b.start => current.selected,
        _ => 0,
    };
    if let Some(trigger) = &found {
        if trigger.kind == Kind::File
            && current
                .trigger
                .as_ref()
                .is_none_or(|t| t.kind != Kind::File || t.query != trigger.query)
        {
            let generation = state::next_generation();
            if let Some(directory) = aim.cwd.clone() {
                app.send(Command::Compose(ComposeCommand::FindFiles {
                    generation,
                    agent: aim.agent.clone(),
                    directory,
                    query: trigger.query.clone(),
                }));
            }
        }
        if matches!(trigger.kind, Kind::Command | Kind::Skill)
            && aim.standing == Standing::Usable
            && aim.catalog.is_none()
        {
            // Popup-path refresh: do not wait on the 45s pill retry.
            let key = format!(
                "slash-cat\u{1f}{}",
                state::catalog_key(&aim.agent, aim.cwd.as_deref())
            );
            if state::ask_due(&key, false, std::time::Duration::from_secs(2)) {
                app.send(Command::Compose(ComposeCommand::Catalog {
                    agent: aim.agent.clone(),
                    directory: aim.cwd.clone(),
                    asid: aim.asid(),
                }));
            }
        }
    }
    set_popup(state::Popup {
        trigger: found,
        selected,
        dismissed_at,
    });
}

/// The rows the popup shows now.
fn popup_rows(app: &App, popup: &state::Popup) -> Vec<triggers::Row> {
    let Some(trigger) = &popup.trigger else {
        return Vec::new();
    };
    let key = state::current_surface().unwrap_or_default();
    let aim = aim(app, place_of(&key));
    match trigger.kind {
        Kind::File => {
            let cmp = Cmp::get();
            let mut rows = triggers::computer_rows(
                &trigger.query,
                &cmp.peers.get(),
                &app.strings.popup_this_computer(),
                &app.strings.popup_computer(),
                &app.strings.popup_computer_offline(),
            );
            rows.extend(cmp.files.get().hits.iter().map(|hit| {
                triggers::Row::item(hit.path.clone(), hit.name.clone(), hit.path.clone())
            }));
            rows
        }
        Kind::Command => match aim.catalog.as_ref() {
            None => vec![triggers::Row::status(app.strings.popup_loading())],
            Some(c) => {
                let rows =
                    triggers::command_rows(&trigger.query, &c.catalog.commands, &c.catalog.skills);
                if rows.is_empty() {
                    let has_any = !c.catalog.commands.is_empty()
                        || c.catalog.skills.iter().any(|s| s.slash);
                    vec![triggers::Row::status(if has_any {
                        app.strings.popup_no_matches()
                    } else {
                        app.strings.popup_no_commands()
                    })]
                } else {
                    rows
                }
            }
        },
        Kind::Skill => match aim.catalog.as_ref() {
            None => vec![triggers::Row::status(app.strings.popup_loading())],
            Some(c) => {
                let rows = triggers::skill_rows(&trigger.query, &c.catalog.skills);
                if rows.is_empty() {
                    vec![triggers::Row::status(if c.catalog.skills.is_empty() {
                        app.strings.popup_no_skills()
                    } else {
                        app.strings.popup_no_matches()
                    })]
                } else {
                    rows
                }
            }
        },
    }
}

/// Take the popup's row `index`: the word becomes the row's text.
fn pick(app: &App, index: usize) {
    let cmp = Cmp::get();
    let popup = cmp.popup.get();
    let rows = popup_rows(app, &popup);
    let (Some(trigger), Some(row)) = (popup.trigger.clone(), rows.get(index)) else {
        return;
    };
    if !row.selectable {
        return;
    }
    let (text, _) = current_text(app);
    let (next, caret) = triggers::insert(&text, &trigger, &row.insert);
    set_text(app, &next, caret);
    set_popup(state::Popup::default());
    edited(app);
}

fn set_popup(popup: state::Popup) {
    Cmp::get().popup.signal().set(Arc::new(popup));
    sync_popup();
}

/// Open or close the list's overlay to match the popup state.
fn sync_popup() {
    if let Some(app) = APP.with(|a| a.borrow().clone()) {
        popup::sync(&app);
    }
}

/// Esc: close what is open, most local first. True when something was.
pub fn dismiss() -> bool {
    let cmp = Cmp::get();
    if cmp.menu.get().is_some() {
        pills::close_menu();
        return true;
    }
    let popup = cmp.popup.get();
    if let Some(trigger) = &popup.trigger {
        set_popup(state::Popup {
            trigger: None,
            selected: 0,
            dismissed_at: Some(trigger.start),
        });
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The key handler on the field: post what must run after the text area
/// has seen the key (see the module's note).
fn key_down(app: &App, ctx: &blinc_layout::event_handler::EventContext) {
    let focused = app.composer.lock().map(|s| s.is_focused()).unwrap_or(false);
    if !focused {
        return;
    }
    let (line, column) = app
        .composer
        .lock()
        .map(|s| (s.cursor.line, s.cursor.column))
        .unwrap_or((0, 0));
    let chord = ctx.ctrl || ctx.meta;
    match ctx.key_code {
        KEY_ENTER if !ctx.shift => app.drain.post(UiAction::ComposerKey {
            key: KEY_ENTER,
            line,
            column,
        }),
        KEY_UP | KEY_DOWN | KEY_TAB => app.drain.post(UiAction::ComposerKey {
            key: ctx.key_code,
            line,
            column,
        }),
        KEY_V if chord => {
            // The text area pastes text itself; an image on the clipboard
            // is staged as a file.
            paste_image(app);
            app.drain.post(UiAction::ComposerEdited);
        }
        _ => app.drain.post(UiAction::ComposerEdited),
    }
}

/// Run by the drain for [`UiAction::ComposerKey`].
pub fn on_key(app: &App, key: u32, line: usize, column: usize) {
    let cmp = Cmp::get();
    let popup = cmp.popup.get();
    let rows = popup_rows(app, &popup);
    let open = popup.trigger.is_some() && !rows.is_empty();
    // Put the caret back where it was before the text area moved it.
    let restore = |app: &App| {
        if let Ok(mut s) = app.composer.lock() {
            s.cursor = blinc_layout::widgets::text_area::TextPosition::new(line, column);
            s.selection_start = None;
        }
        refresh_text_area(&app.composer);
    };
    // An open pill menu takes ↑ / ↓ / Enter before the popup or submit.
    if pills::menu_open() && matches!(key, KEY_UP | KEY_DOWN | KEY_ENTER) {
        if key == KEY_ENTER {
            undo_enter(app);
        } else {
            restore(app);
        }
        let _ = pills::on_menu_key(key);
        return;
    }
    match key {
        KEY_UP | KEY_DOWN if open => {
            restore(app);
            let last = rows.len() - 1;
            let selected = if key == KEY_UP {
                popup.selected.saturating_sub(1)
            } else {
                (popup.selected + 1).min(last)
            };
            set_popup(state::Popup {
                selected,
                ..(*popup).clone()
            });
        }
        KEY_UP => {
            // ↑ in an empty box: the message sent last, to edit.
            let (text, _) = current_text(app);
            if text.is_empty() {
                if let Some(last) = state::last_sent(&state::current_surface().unwrap_or_default())
                {
                    let end = last.chars().count();
                    set_text(app, &last, end);
                    edited(app);
                }
            }
        }
        KEY_ENTER | KEY_TAB if open => {
            if key == KEY_ENTER {
                undo_enter(app);
            }
            let index = popup.selected.min(rows.len() - 1);
            if rows.get(index).is_some_and(|r| r.selectable) {
                pick(app, index);
            }
        }
        KEY_ENTER => submit_after_enter(app),
        _ => edited(app),
    }
}

/// Take back the newline the text area's own Enter just inserted.
fn undo_enter(app: &App) {
    if let Ok(mut state) = app.composer.lock() {
        let before = state.value();
        state.delete_backward();
        let after = state.value();
        // Only a removed newline is undone; anything else is restored.
        if before.chars().count() != after.chars().count() + 1
            || before.matches('\n').count() != after.matches('\n').count() + 1
        {
            state.set_value(&before);
        }
    }
    refresh_text_area(&app.composer);
}

fn submit_after_enter(app: &App) {
    undo_enter(app);
    submit(app);
}

/// Run by the drain for [`UiAction::SubmitComposer`] (the button).
pub fn submit_from_button(app: &App) {
    submit(app);
}

// ---------------------------------------------------------------------------
// Submitting
// ---------------------------------------------------------------------------

/// What a submit in flight carries, to restore it if delivery fails after
/// the session was made.
struct InFlight {
    key: String,
    text: String,
    staged: Vec<Staged>,
}

thread_local! {
    static IN_FLIGHT: RefCell<Option<InFlight>> = const { RefCell::new(None) };
}

fn notice(app: &App, message: String) {
    app.bindings.error.signal().set(Some(UiError {
        message,
        detail: None,
        unreachable: false,
    }));
}

/// Send the draft: a prompt, a host command or a skill, with the staged
/// files; on the start page, in a new session first. The draft and the
/// staged files stay until the core says it was accepted.
pub fn submit(app: &App) {
    let cmp = Cmp::get();
    if cmp.submitting.get().is_some() {
        return;
    }
    let Some(key) = state::current_surface() else {
        return;
    };
    let place = place_of(&key);
    let aim = aim(app, place);
    let (raw, _) = current_text(app);
    let text = raw.trim().to_string();
    let staged = (*cmp.staged.get()).clone();
    if text.is_empty() && staged.is_empty() {
        return;
    }
    if aim.standing != Standing::Usable && place == Place::Start {
        notice(app, blocked_message(app, &aim));
        return;
    }
    if staged
        .iter()
        .any(|s| matches!(s.status, StagedStatus::Uploading))
    {
        notice(app, app.strings.attachments_uploading().to_string());
        return;
    }
    if let Some(bad) = staged.iter().find_map(|s| match &s.status {
        StagedStatus::Failed(why) => Some((s.name.clone(), why.clone())),
        _ => None,
    }) {
        notice(app, app.strings.attachment_blocked(&bad.0, &bad.1));
        return;
    }
    let paths: Vec<String> = staged
        .iter()
        .filter_map(|s| match &s.status {
            StagedStatus::Ready(file) => Some(file.path.clone()),
            _ => None,
        })
        .collect();
    // A line is a command only without files, as the App decides it.
    let intent = if paths.is_empty() {
        aim.catalog.as_ref().map_or(Intent::Prompt, |c| {
            triggers::read_intent(&text, &c.catalog.commands, &c.catalog.skills)
        })
    } else {
        Intent::Prompt
    };
    let deliver = match intent {
        Intent::Prompt => Deliver::Prompt(text.clone()),
        Intent::Command { name, args } => Deliver::Command { name, args },
        Intent::Skill { name, args } => {
            if !args.is_empty() {
                // A skill takes no arguments on the wire: say so, as the App.
                notice(app, app.strings.skill_args_not_sent(&args));
            }
            Deliver::Skill { name, args }
        }
    };
    let target = match (place, aim.asid()) {
        (Place::Session, Some(asid)) => Target::Session(asid),
        (Place::Session, None) => return,
        (Place::Start, _) => {
            let Some(raw_cwd) = aim.cwd.clone() else {
                return;
            };
            let Some(cwd) = muqun_gateway::agents::directories::expand_home(raw_cwd.trim()) else {
                notice(app, app.strings.folder_unusable(&raw_cwd));
                return;
            };
            // The pills' choices become what this pair starts with next time.
            app.send(Command::Edit(ConfigEdit::Remember {
                agent: aim.agent.clone(),
                cwd: raw_cwd,
                model: aim.model.as_ref().map(model_to_string),
                mode: aim.mode.clone(),
            }));
            Target::New {
                agent: aim.agent.clone(),
                cwd,
                model: aim.model.clone(),
                mode: aim.mode.clone(),
                worktree: Cmp::get().start.get().worktree,
            }
        }
    };
    let token = state::next_id();
    cmp.submitting.signal().set(Some(token));
    state::remember_sent(&key, &text);
    IN_FLIGHT.with(|f| *f.borrow_mut() = Some(InFlight { key, text, staged }));
    let queued = place == Place::Session && aim.busy;
    if queued {
        crate::ui::shell::open_right(app, crate::model::view::RightTab::Queue);
    }
    app.send(Command::Compose(ComposeCommand::Submit {
        token,
        target,
        deliver,
        attachments: paths,
        delivery: queued.then(|| "queue".to_string()),
    }));
}

fn blocked_message(app: &App, aim: &Aim) -> String {
    match &aim.reason {
        Some(reason) => format!("{}: {}", aim.agent_name, reason),
        None => format!("{}: {}", aim.agent_name, aim.standing.word(&app.strings)),
    }
}

/// The core accepted the submit (called from the state's update): empty the
/// draft (unless it was edited meanwhile) and the staged files.
fn after_submit_ok() {
    let Some(sent) = IN_FLIGHT.with(|f| f.borrow_mut().take()) else {
        return;
    };
    let Some(app) = APP.with(|a| a.borrow().clone()) else {
        return;
    };
    Cmp::get().staged.signal().set(Arc::new(Vec::new()));
    // Always forget what was sent under its key; clear the box only when
    // it is still that surface and the text was not edited since.
    app.send(Command::Edit(ConfigEdit::Draft {
        key: sent.key.clone(),
        text: String::new(),
    }));
    state::draft_changed(&sent.key, "");
    if state::current_surface().as_deref() == Some(sent.key.as_str()) {
        let (now, _) = current_text(&app);
        if now.trim() == sent.text {
            set_text(&app, "", 0);
            Cmp::get().rows.signal().set(MIN_ROWS);
        }
    }
    let cmp = Cmp::get();
    cmp.popup.signal().set(Arc::new(state::Popup::default()));
    // A start-page worktree choice is one-shot.
    if cmp.start.get().worktree {
        let mut choice = (*cmp.start.get()).clone();
        choice.worktree = false;
        cmp.start.signal().set(Arc::new(choice));
    }
}

thread_local! {
    /// The app, for the state's updates to reach the text area.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// A submit failed after the session may have been made: put the words and
/// files back in whatever composer is showing.
fn restore_after_failure() {
    let Some(app) = APP.with(|a| a.borrow().clone()) else {
        return;
    };
    let app = &app;
    let Some(sent) = IN_FLIGHT.with(|f| f.borrow_mut().take()) else {
        return;
    };
    let (now, _) = current_text(app);
    if now.trim().is_empty() {
        let end = sent.text.chars().count();
        set_text(app, &sent.text, end);
    }
    let cmp = Cmp::get();
    if cmp.staged.get().is_empty() {
        cmp.staged.signal().set(Arc::new(sent.staged));
    }
    edited(app);
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

fn stage(app: &App, name: String, source: Source) {
    let cmp = Cmp::get();
    let mut list = (*cmp.staged.get()).clone();
    if list.len() >= MAX_STAGED {
        notice(app, app.strings.attachments_limit(MAX_STAGED));
        return;
    }
    let id = state::next_id();
    list.push(Staged {
        id,
        name: name.clone(),
        status: StagedStatus::Uploading,
    });
    cmp.staged.signal().set(Arc::new(list));
    app.send(Command::Compose(ComposeCommand::Stage { id, name, source }));
}

/// Files came back from the dialog.
pub fn files_picked(app: &App, paths: Vec<PathBuf>) {
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        stage(app, name, Source::Path(path));
    }
}

/// Window-level file drop: `.muqun-theme` imports a theme; every other file
/// is staged on the composer. Installed once (themes panel used to own this).
///
/// winit 0.30 delivers drops on X11, macOS and Windows; native Wayland has no
/// drag-and-drop, so Attach… remains the way in there.
pub fn install_file_drop(app: &App) {
    static SET: AtomicBool = AtomicBool::new(false);
    if SET.swap(true, Ordering::Relaxed) {
        return;
    }
    let drain = app.drain.clone();
    let commands = app.commands.clone();
    blinc_app::dnd::on_file_drop(move |event| {
        if let blinc_app::dnd::DropEvent::Dropped(paths) = event {
            let mut files = Vec::new();
            for path in paths {
                if path.extension().and_then(|e| e.to_str()) == Some("muqun-theme") {
                    crate::model::commands::send(&commands, Command::ThemeImportFile(path));
                } else if path.is_file() {
                    files.push(path);
                }
            }
            if !files.is_empty() {
                drain.post(UiAction::FilesPicked(files));
            }
        }
    });
}

/// An image came off the clipboard.
pub fn image_pasted(app: &App, png: Vec<u8>) {
    let n = Cmp::get().staged.get().len() + 1;
    stage(app, format!("pasted-image-{n}.png"), Source::Png(png));
}

fn pick_files(app: &App) {
    let drain = app.drain.clone();
    let title = app.strings.attach_title();
    let start = dirs::home_dir().unwrap_or_default();
    let pick = move || {
        let picked = blinc_app::dialog::open_file()
            .title(title)
            .directory(start)
            .pick_many();
        if !picked.is_empty() {
            drain.post(UiAction::FilesPicked(picked));
        }
    };
    // The dialog blocks its caller: on macOS it must run on the main
    // thread, elsewhere beside the UI so frames keep coming.
    if crate::platform::shell::dialogs_need_main_thread() {
        pick();
    } else {
        std::thread::spawn(pick);
    }
}

/// Look for an image on the clipboard, off the UI thread; the answer comes
/// back through the inbox.
fn paste_image(app: &App) {
    let drain = app.drain.clone();
    std::thread::spawn(move || {
        if let Some(png) = crate::platform::clipboard::read_image_png() {
            drain.post(UiAction::ImagePasted(png));
        }
    });
}

/// The folder picker for the start page's project pill.
pub fn browse_project(app: &App) {
    let title = app.strings.choose_folder();
    let drain = app.drain.clone();
    let start = Cmp::get()
        .start
        .get()
        .project
        .as_deref()
        .and_then(muqun_gateway::agents::directories::expand_home)
        .filter(|p| p.is_dir())
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let pick = move || {
        if let Some(cwd) = blinc_app::dialog::pick_folder()
            .title(title)
            .directory(start)
            .pick()
        {
            drain.post(UiAction::ProjectPicked(cwd));
        }
    };
    if crate::platform::shell::dialogs_need_main_thread() {
        pick();
    } else {
        std::thread::spawn(pick);
    }
}

/// A folder came back from the picker: it is the start page's project.
pub fn project_picked(cwd: PathBuf) {
    set_project(cwd.to_string_lossy().into_owned());
}

pub fn set_project(path: String) {
    let cmp = Cmp::get();
    let mut choice = (*cmp.start.get()).clone();
    if choice.project.as_deref() != Some(path.as_str()) {
        choice.project = Some(path);
        // The new folder has its own model and mode.
        choice.model = None;
        choice.mode = None;
        cmp.start.signal().set(Arc::new(choice));
    }
}

/// The start page was opened for a folder (a project row's `+`): that
/// folder, with the agent kept and its model and mode forgotten.
// Called by the sidebar's project `+` (see `App::open_draft_in`).
#[allow(dead_code)]
pub fn start_in(path: String) {
    let cmp = Cmp::get();
    let mut choice = (*cmp.start.get()).clone();
    choice.project = Some(path);
    choice.worktree = false;
    choice.model = None;
    choice.mode = None;
    cmp.start.signal().set(Arc::new(choice));
}

/// Put the caret in the text area when a page with a composer is entered.
pub fn focus_input(app: &App) {
    focus_text_area_deferred(&app.composer);
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

fn glyph(g: Glyph, size: f32) -> impl ElementBuilder {
    icons::tinted(g, size)
}

/// The session page's composer, in its reserved strip at the foot.
/// `panel_width` is the main column width (already minus sidebar / right git).
pub fn composer(app: &App, panel_width: f32) -> impl ElementBuilder {
    let cmp = Cmp::get();
    let header = app.bindings.header.clone();
    let right = app.bindings.diff_panel.clone();
    let app = app.clone();
    let deps = [
        header.signal_id(),
        app.bindings.agents.signal_id(),
        cmp.staged.signal_id(),
        cmp.inbox.signal_id(),
        cmp.rows.signal_id(),
        // Recompute card width when the git column opens/closes.
        right.signal_id(),
    ];
    let strip = stateful_with_key::<NoState>("mq-composer")
        .deps(deps)
        .on_state(move |_| {
            APP.with(|a| *a.borrow_mut() = Some(app.clone()));
            // Home may still notify these shared signals while a session
            // strip is answering them; do not sync until a session is open.
            if app.bindings.selected.get().is_none() {
                return div().class("mq-cmp-slot");
            }
            let aim = aim(&app, Place::Session);
            sync_surface(&app, &aim.key);
            ensure_loaded(&app, &aim);
            let height = reserved_height();
            // Always keep SLOT_SIDE inset; never grow past SESSION_MAX. Do not
            // clamp up to 320 when the panel is narrower — that ate the margins.
            let card_w = session_card_width_for(panel_width);
            div()
                .class("mq-cmp-strip")
                .w_full()
                .h(height)
                .flex_row()
                .items_end()
                .justify_center()
                .child(
                    div()
                        .class("mq-cmp-slot")
                        .w_full()
                        .flex_row()
                        .justify_center()
                        .child(div().w(card_w).child(card(&app, &aim, card_w))),
                )
        });
    // Outer shell reserves max space via [`reserved_height`]; this wrapper
    // rebuilds with the inbox / staged / rows so the strip matches.
    div().w_full().flex_shrink_0().child(strip)
}

/// The widest the session page's card gets.
const SESSION_MAX: f32 = 860.0;
/// Card padding (12 px a side) and the attach column (24 px) with its gap
/// (8 px): what the card's width loses before the text.
const FIELD_INSET: f32 = 24.0 + 32.0;
/// The slot's side padding (matches `.mq-cmp-slot` → `--mq-space-5`).
const SLOT_SIDE: f32 = 20.0;
pub(crate) const SLOT_BOTTOM: f32 = 20.0;

/// The text area's width inside a card `card` wide. The field lays its lines
/// out at its configured width (it does not read the layout's), so the width
/// is given, not left to fill.
fn field_width(card: f32) -> f32 {
    (card - FIELD_INSET).max(160.0)
}

/// Card width for a main-panel column of `panel` px: slot inset preserved on
/// both sides, capped at [`SESSION_MAX`].
pub(crate) fn session_card_width_for(panel: f32) -> f32 {
    (panel - 2.0 * SLOT_SIDE).max(0.0).min(SESSION_MAX)
}

/// Card width from the window (popup / overlays that are not under `main_panel`).
pub(crate) fn session_card_width(app: &App) -> f32 {
    let (window, _) = blinc_core::BlincContextState::get().viewport_size();
    // Match `shell::body`'s main column: frame pad + column gap + side + right.
    let panel = (window
        - 2.0 * crate::ui::shell::FRAME_PAD
        - crate::ui::shell::side_width(app, window)
        - crate::ui::shell::right_width(app)
        - 8.0)
        .max(0.0);
    session_card_width_for(panel)
}

/// The start page's composer card, `width` wide.
pub fn start_card(app: &App, width: f32) -> impl ElementBuilder {
    let cmp = Cmp::get();
    let app = app.clone();
    let deps = [
        app.bindings.agents.signal_id(),
        cmp.staged.signal_id(),
        cmp.rows.signal_id(),
        cmp.start.signal_id(),
    ];
    stateful_with_key::<NoState>("mq-start-composer")
        .deps(deps)
        .on_state(move |_| {
            APP.with(|a| *a.borrow_mut() = Some(app.clone()));
            // Session composer may still notify these shared signals while a
            // start-card stateful is answering them; do not steal the surface.
            if app.bindings.selected.get().is_some() {
                return div().w(width);
            }
            let aim = aim(&app, Place::Start);
            sync_surface(&app, &aim.key);
            ensure_loaded(&app, &aim);
            div().w(width).relative().child(card(&app, &aim, width))
        })
}

fn placeholder(app: &App, aim: &Aim) -> String {
    if aim.standing != Standing::Usable {
        return blocked_message(app, aim);
    }
    // `/` and `$` are only named when the agent has commands or skills.
    let has_slash = aim
        .catalog
        .as_ref()
        .is_none_or(|c| !c.catalog.commands.is_empty() || !c.catalog.skills.is_empty());
    if has_slash {
        app.strings.composer_hint().to_string()
    } else {
        app.strings.composer_hint_files().to_string()
    }
}

/// The card: waiting prompts, staged files, the field, the pills.
fn card(app: &App, aim: &Aim, width: f32) -> Div {
    let cmp = Cmp::get();
    let rows = cmp.rows.get();
    let blocked = aim.standing != Standing::Usable;

    let attach = {
        let app = app.clone();
        div()
            .class("mq-cmp-attach")
            .cursor_pointer()
            .flex_shrink_0()
            .child(glyph(Glyph::Attachment, 16.0))
            .on_click(move |_| pick_files(&app))
    };
    let input = blinc_layout::widgets::text_area::text_area(&app.composer)
        .font_size(14.0)
        .rows(rows)
        .wrap(true)
        .w(field_width(width))
        .rounded(8.0)
        .border_width(0.0)
        .placeholder(placeholder(app, aim))
        .disabled(blocked && aim.place == Place::Session)
        .class("mq-cmp-input");
    let down = app.clone();
    let text_in = app.clone();
    let field = div()
        .class("mq-cmp-field")
        .flex_1()
        .min_w(0.0)
        .child(input)
        .on_key_down(move |ctx| key_down(&down, ctx))
        .on_text_input(move |_| text_in.drain.post(UiAction::ComposerEdited));
    let input_row = div()
        .class("mq-cmp-input-row")
        .w_full()
        .flex_row()
        .items_start()
        .child(attach)
        .child(field);

    let mut body = div()
        .class("mq-cmp-card")
        .class(match aim.place {
            Place::Start => "mq-cmp-card--start",
            Place::Session => "mq-cmp-card--session",
        })
        .w_full()
        .flex_col()
        .id("mq-cmp-card");
    body = body.w(width);
    if let Some(staged) = chips::staged(app) {
        body = body.child(staged);
    }
    body = body.child(input_row).child(pill_row(app, aim.place));
    // The trigger list hangs above the card, over whatever is there.
    div().relative().w_full().child(body)
}

/// The selector row is its own view: opening a menu, loading a catalogue or
/// a branch, or a choice changing must not rebuild the field above it.
fn pill_row(app: &App, place: Place) -> impl ElementBuilder {
    let cmp = Cmp::get();
    let app = app.clone();
    stateful_with_key::<NoState>(match place {
        Place::Start => "mq-cmp-pills-start",
        Place::Session => "mq-cmp-pills-session",
    })
    .deps([
        app.bindings.header.signal_id(),
        app.bindings.agents.signal_id(),
        app.bindings.diff_panel.signal_id(),
        app.bindings.right_tab.signal_id(),
        cmp.menu.signal_id(),
        cmp.catalogs.signal_id(),
        cmp.branches.signal_id(),
        cmp.start.signal_id(),
        cmp.submitting.signal_id(),
        cmp.inbox.signal_id(),
    ])
    .on_state(move |_| {
        let aim = aim(&app, place);
        pills::row(&app, &aim)
    })
}

/// The primary action at the row's right edge: `Start ⏎`, `Send ⏎` or
/// `Queue ⏎`, with `Stop` beside it while the agent works.
pub(crate) fn actions(app: &App, aim: &Aim) -> Div {
    let cmp = Cmp::get();
    let submitting = cmp.submitting.get().is_some();
    let blocked = aim.standing != Standing::Usable;
    let (label, key) = match (aim.place, aim.busy) {
        (Place::Start, _) => (app.strings.start(), "⏎"),
        (Place::Session, false) => (app.strings.send(), "⏎"),
        (Place::Session, true) => (app.strings.queue(), "⏎"),
    };
    let mut row = div().class("mq-cmp-actions").flex_row().items_center();
    if aim.place == Place::Session && aim.busy {
        if let Some(asid) = aim.asid() {
            let stop_app = app.clone();
            row = row.child(
                div()
                    .class("mq-cmp-stop")
                    .flex_row()
                    .items_center()
                    .cursor_pointer()
                    .child(ui_text(app.strings.stop()).size(12.0).no_wrap().no_cursor())
                    .child(
                        mono_text(super::shortcut("."))
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    )
                    .on_click(move |_| stop_app.send(Command::Interrupt(asid.clone()))),
            );
        }
    }
    let go = app.clone();
    let mut primary = div()
        .class("mq-cmp-primary")
        .flex_row()
        .items_center()
        .child(ui_text(label).size(12.0).semibold().no_wrap().no_cursor())
        .child(mono_text(key).size(12.0).no_wrap().no_cursor());
    if blocked && aim.place == Place::Start || submitting {
        primary = primary.class("mq-cmp-primary--off");
    } else {
        primary = primary
            .cursor_pointer()
            .on_click(move |_| go.drain.post(UiAction::SubmitComposer));
    }
    row.child(primary)
}

pub(crate) fn mark_idle(id: &str) -> Svg {
    agents::mark_idle(id, 14.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_grow_with_lines_and_long_lines() {
        assert_eq!(rows_for(""), MIN_ROWS);
        assert_eq!(rows_for("a\nb\nc"), 3);
        assert_eq!(rows_for(&"x".repeat(170)), 3);
        assert_eq!(rows_for("1\n2\n3\n4\n5\n6\n7\n8"), MAX_ROWS);
    }

    #[test]
    fn a_remembered_model_round_trips() {
        let model = ModelRef {
            provider_id: "anthropic".into(),
            model_id: "claude/sonnet".into(),
            variant: Some("high".into()),
        };
        assert_eq!(model_from_string(&model_to_string(&model)), Some(model));
        assert_eq!(model_from_string("nonsense"), None);
    }
}
