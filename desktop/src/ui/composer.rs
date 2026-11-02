//! The composer: a multi-line input, Enter sends, Shift+Enter is a newline.
//!
//! The text area inserts a newline on every Enter itself
//! (`blinc_layout/src/widgets/text_area/handlers.rs`, key 13), and key-down
//! is broadcast to every key handler in no guaranteed order
//! (`renderer/events.rs` `broadcast_key_event`). So the wrapper's handler
//! never edits the text itself: it posts [`UiAction::SubmitComposer`] to the
//! inbox, and the drain runs [`submit`] after this input has been dispatched
//! to everyone -- by which time the newline is in, and is taken back out.

use blinc_cn::prelude::{button, spinner, textarea, ButtonSize, ButtonVariant, SpinnerSize};
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_area::refresh_text_area;
use muqun_gateway::agents::{AgentSessionId, AgentSessionStatus};

use super::App;
use crate::model::commands::Command;
use crate::model::inbox::UiAction;

const ENTER: u32 = 13;

/// The composer's fixed height; the timeline takes the rest.
pub const COMPOSER_HEIGHT: f32 = 116.0;

pub fn composer(app: &App) -> impl ElementBuilder {
    let header = app.bindings.header.clone();
    let agents_state = app.bindings.agents.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-composer")
        .deps([header.signal_id(), agents_state.signal_id()])
        .on_state(move |_| {
            let info = header.get();
            let busy = info.as_ref().is_some_and(|i| {
                matches!(
                    i.status,
                    AgentSessionStatus::Busy | AgentSessionStatus::Retry
                )
            });
            let has_session = info.is_some();
            let placeholder = match &info {
                Some(i) => app
                    .strings
                    .composer_placeholder(&super::agents::name(&i.agent_id, &agents_state.get())),
                None => app.strings.composer_no_session().to_string(),
            };
            let asid = info.map(|i| i.asid.clone());

            let input = textarea(&app.composer)
                .rows(3)
                .w_full()
                .placeholder(placeholder)
                .disabled(!has_session);

            let key_app = app.clone();
            let field = div()
                .class("mq-composer-field")
                .flex_1()
                .child(input)
                .on_key_down(move |ctx| {
                    if ctx.key_code != ENTER || ctx.shift {
                        return;
                    }
                    let focused = key_app
                        .composer
                        .lock()
                        .map(|s| s.is_focused())
                        .unwrap_or(false);
                    if focused {
                        key_app.drain.post(UiAction::SubmitComposer);
                    }
                });

            let mut actions = div().class("mq-composer-actions").flex_col();
            if busy {
                let stop_app = app.clone();
                let stop_asid = asid.clone();
                actions = actions.child(spinner().size(SpinnerSize::Small)).child(
                    button(app.strings.stop())
                        .variant(ButtonVariant::Outline)
                        .size(ButtonSize::Small)
                        .on_click(move |_| {
                            if let Some(asid) = stop_asid.clone() {
                                stop_app.send(Command::Interrupt(asid));
                            }
                        }),
                );
            } else {
                let send_app = app.clone();
                actions = actions.child(
                    button(app.strings.send())
                        .size(ButtonSize::Small)
                        .disabled(!has_session)
                        .on_click(move |_| send_from_button(&send_app)),
                );
            }

            div()
                .class("mq-composer")
                .w_full()
                .h(COMPOSER_HEIGHT)
                .flex_row()
                .items_end()
                .child(field)
                .child(actions)
        })
}

fn send_from_button(app: &App) {
    let text = take_text(app, false);
    dispatch(app, text);
}

/// Run on the main thread by the drain for [`UiAction::SubmitComposer`].
pub fn submit(app: &App) {
    let text = take_text(app, true);
    dispatch(app, text);
}

/// The composer's text, cleared. `undo_enter` removes the newline the text
/// area's own Enter handling just inserted at the caret.
fn take_text(app: &App, undo_enter: bool) -> Option<String> {
    let text = {
        let mut state = app.composer.lock().ok()?;
        if undo_enter {
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
        let text = state.value();
        if text.trim().is_empty() {
            None
        } else {
            state.set_value("");
            Some(text)
        }
    };
    refresh_text_area(&app.composer);
    text
}

fn dispatch(app: &App, text: Option<String>) {
    let Some(text) = text else {
        return;
    };
    let asid = app.bindings.header.get().map(|info| info.asid.clone());
    let Some(asid): Option<AgentSessionId> = asid else {
        return;
    };
    app.send(Command::SendPrompt {
        asid,
        text: text.trim_end().to_string(),
    });
}
