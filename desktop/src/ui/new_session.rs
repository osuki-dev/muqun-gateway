//! The New-session page (Ctrl/Cmd+N): agent, then folder, then create.
//! The defaults are filled in -- the last agent used, and that agent's last
//! folder -- so Enter alone starts a session (architecture §6.1b).
//!
//! The folder is chosen with the system picker for now; the chooser with
//! the agent's known projects and typed completion replaces it next.

use std::path::PathBuf;

use blinc_cn::prelude::{button, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use muqun_gateway::home_relative;

use super::agents::{self, Standing};
use super::text::{mono_text, ui_text};
use super::App;

pub fn page(app: &App, height: f32) -> Div {
    let draft = app.bindings.draft.clone();
    let agents_state = app.bindings.agents.clone();
    let app = app.clone();
    let content = stateful_with_key::<NoState>("mq-new-session")
        .deps([draft.signal_id(), agents_state.signal_id()])
        .on_state(move |_| {
            let Some(current) = draft.get() else {
                return div();
            };
            let agents_list = agents_state.get();
            let ranked = agents::by_standing(&agents_list);

            let mut usable = div().class("mq-pick-list").w_full().flex_col();
            let mut more = div().class("mq-pick-list").w_full().flex_col();
            for (info, standing) in &ranked {
                let chosen = info.id == current.agent;
                let mut row = div().class("mq-pick");
                if chosen {
                    row = row.class("mq-pick--active");
                }
                row = row
                    .w_full()
                    .flex_row()
                    .items_center()
                    .child(agents::dot(&info.id))
                    .child(
                        div().class("mq-pick-name").flex_1().child(
                            ui_text(agents::name(&info.id, &agents_list))
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                    );
                if *standing == Standing::Usable {
                    let pick = app.clone();
                    let id = info.id.clone();
                    usable = usable.child(
                        row.cursor_pointer()
                            .on_click(move |_| pick.draft_for(id.clone())),
                    );
                } else {
                    let reason = info
                        .reason
                        .clone()
                        .filter(|r| !r.is_empty())
                        .map(|r| format!(" — {r}"))
                        .unwrap_or_default();
                    more = more.child(
                        row.class("mq-pick--disabled").child(
                            ui_text(format!("{}{reason}", standing.word(&app.strings)))
                                .size(11.0)
                                .no_cursor(),
                        ),
                    );
                }
            }

            let home = dirs::home_dir();
            let folder = home_relative(&current.cwd.to_string_lossy(), home.as_deref());
            let browse = {
                let app = app.clone();
                let cwd = current.cwd.clone();
                let agent = current.agent.clone();
                button(app.strings.browse())
                    .variant(ButtonVariant::Outline)
                    .size(ButtonSize::Small)
                    .on_click(move |_| pick_folder(&app, agent.clone(), cwd.clone()))
            };
            let create = {
                let app = app.clone();
                let current = current.clone();
                button(app.strings.create())
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.create(current.agent.clone(), current.cwd.clone()))
            };
            let cancel = {
                let app = app.clone();
                button(app.strings.cancel())
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.bindings.draft.signal().set(None))
            };

            let mut page = div()
                .class("mq-new-page")
                .w_full()
                .flex_col()
                .child(ui_text(app.strings.new_session()).size(20.0).semibold())
                .child(
                    div()
                        .class("mq-field-label")
                        .child(ui_text(app.strings.agent()).size(12.0).semibold()),
                )
                .child(usable);
            if ranked.iter().any(|(_, s)| *s != Standing::Usable) {
                page = page
                    .child(
                        div()
                            .class("mq-field-label")
                            .child(ui_text(app.strings.more_agents()).size(12.0).semibold()),
                    )
                    .child(more);
            }
            page.child(
                div()
                    .class("mq-field-label")
                    .child(ui_text(app.strings.folder()).size(12.0).semibold()),
            )
            .child(
                div()
                    .class("mq-folder")
                    .w_full()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .class("mq-folder-path")
                            .flex_1()
                            .min_w(0.0)
                            .overflow_clip()
                            .child(mono_text(folder).size(13.0).no_wrap()),
                    )
                    .child(browse),
            )
            .child(
                div()
                    .class("mq-new-actions")
                    .flex_row()
                    .items_center()
                    .child(create)
                    .child(cancel)
                    .child(ui_text(app.strings.new_session_hint()).size(12.0)),
            )
        });
    div()
        .w_full()
        .h(height)
        .child(scroll().w_full().h(height).child(content))
}

/// A launch card's button: start `agent` in `cwd` now.
/// The system folder picker; the choice goes back into the draft. The
/// dialog blocks its caller: on macOS it must run on the main thread,
/// elsewhere it runs beside the UI so frames keep coming. Either way the
/// draft is written from the main thread by the drain's next run, through
/// the inbox -- never from the picker's thread.
fn pick_folder(app: &App, agent: String, start: PathBuf) {
    let title = app.strings.choose_folder();
    let drain = app.drain.clone();
    let pick = move || {
        let picked = blinc_app::dialog::pick_folder()
            .title(title)
            .directory(start)
            .pick();
        if let Some(cwd) = picked {
            drain.post(crate::model::inbox::UiAction::SetDraft { agent, cwd });
        }
    };
    if cfg!(target_os = "macos") {
        pick();
    } else {
        std::thread::spawn(pick);
    }
}
