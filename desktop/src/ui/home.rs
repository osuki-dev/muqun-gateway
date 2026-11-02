//! Home (architecture §6.0): what the main panel shows when no session is
//! selected, including at startup. Every module shows real data or is left
//! out -- no placeholders.

use blinc_layout::prelude::*;
use muqun_gateway::{home_relative, relative_since};

use super::agents::{self, Standing};
use super::sessions::{agent_row, title_of};
use super::text::ui_text;
use super::App;
use crate::branding::{mark_data_uri, APP_NAME};

const RECENT: usize = 8;
/// Below this width the side column joins the main one.
const TWO_COLUMNS_FROM: f32 = 1100.0;

pub fn home(app: &App, width: f32, height: f32) -> Div {
    let agents_state = app.bindings.agents.clone();
    let sessions = app.bindings.sessions.clone();
    let attention = app.bindings.attention.clone();
    let app = app.clone();
    let content = stateful_with_key::<NoState>("mq-home")
        .deps([
            agents_state.signal_id(),
            sessions.signal_id(),
            attention.signal_id(),
        ])
        .on_state(move |_| {
            let main = div()
                .class("mq-home-column")
                .flex_col()
                .flex_1()
                .min_w(0.0)
                .child(launch(&app))
                .child(continue_working(&app));
            let side = side_column(&app);
            let mut body = div().class("mq-home-body").w_full();
            body = if width >= TWO_COLUMNS_FROM {
                body.flex_row()
                    .child(main)
                    .child(side.w(320.0).flex_shrink_0())
            } else {
                body.flex_col().child(main).child(side)
            };
            div()
                .class("mq-home")
                .w_full()
                .flex_col()
                .child(hero(&app))
                .child(body)
        });
    div()
        .w_full()
        .h(height)
        .child(scroll().w_full().h(height).child(content))
}

fn usable(app: &App) -> Vec<String> {
    agents::by_standing(&app.bindings.agents.get())
        .into_iter()
        .filter(|(_, s)| *s == Standing::Usable)
        .map(|(a, _)| a.id)
        .collect()
}

fn hero(app: &App) -> Div {
    let count = usable(app).len();
    div()
        .class("mq-hero")
        .w_full()
        .flex_row()
        .items_center()
        .child(image(mark_data_uri()).size(40.0, 40.0).rounded(10.0))
        .child(
            div()
                .class("mq-hero-text")
                .flex_col()
                .child(ui_text(APP_NAME).size(20.0).semibold().no_wrap())
                .child(ui_text(app.strings.agents_available(count)).size(13.0)),
        )
}

fn section(title: &str) -> Div {
    div().class("mq-home-section").w_full().flex_col().child(
        div()
            .class("mq-home-section-title")
            .child(ui_text(title.to_string()).size(12.0).semibold()),
    )
}

/// One card per usable agent: its name, the folder it would start in, and
/// a button that starts it there at once.
fn launch(app: &App) -> Div {
    let agents_list = app.bindings.agents.get();
    let ids = usable(app);
    let mut section = section(app.strings.launch());
    if ids.is_empty() {
        return section.child(
            div()
                .class("mq-empty")
                .child(ui_text(app.strings.no_usable_agent()).size(13.0)),
        );
    }
    let home = dirs::home_dir();
    let mut cards = div().class("mq-launch").w_full().flex_row().flex_wrap();
    for id in ids {
        let cwd = app.config.get().general.initial_cwd(&id);
        let shown = home_relative(&cwd.to_string_lossy(), home.as_deref());
        let start = app.clone();
        let (agent, folder) = (id.clone(), cwd.clone());
        cards = cards.child(
            div()
                .class("mq-launch-card")
                .class(agents::brand_class(&id))
                .w(220.0)
                .flex_col()
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .child(agents::dot(&id))
                        .child(
                            div().class("mq-launch-name").child(
                                ui_text(agents::name(&id, &agents_list))
                                    .size(14.0)
                                    .semibold()
                                    .no_wrap(),
                            ),
                        ),
                )
                .child(
                    div()
                        .class("mq-launch-dir")
                        .overflow_clip()
                        .child(ui_text(shown).size(12.0).no_wrap()),
                )
                .child(super::new_session::launch_button(&start, agent, folder)),
        );
    }
    section = section.child(cards);
    section
}

/// The most recent sessions of every agent.
fn continue_working(app: &App) -> Div {
    let sessions = app.bindings.sessions.get();
    if sessions.is_empty() {
        return div();
    }
    let home = dirs::home_dir();
    let mut list = div().class("mq-recent").w_full().flex_col();
    for session in sessions.iter().take(RECENT) {
        let open = app.clone();
        let asid = session.asid.clone();
        let dir = session
            .directory
            .as_deref()
            .map(|d| home_relative(d, home.as_deref()))
            .unwrap_or_default();
        list = list.child(
            div()
                .class("mq-recent-row")
                .w_full()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .child(agents::badge(&session.agent_id))
                .child(
                    div()
                        .class("mq-recent-main")
                        .flex_col()
                        .flex_1()
                        .min_w(0.0)
                        .overflow_clip()
                        .child(
                            ui_text(title_of(app, session))
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        )
                        .child(
                            div()
                                .class("mq-recent-dir")
                                .child(ui_text(dir).size(11.0).no_wrap().no_cursor()),
                        ),
                )
                .child(
                    ui_text(relative_since(u128::from(session.updated_ms)))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| open.open_session(&asid)),
        );
    }
    section(app.strings.continue_working()).child(list)
}

/// Needs attention (only when something does) and the Agents module.
fn side_column(app: &App) -> Div {
    let mut side = div().class("mq-home-column").flex_col();
    let attention = app.bindings.attention.get();
    if !attention.is_empty() {
        let sessions = app.bindings.sessions.get();
        let mut list = div().class("mq-attention").w_full().flex_col();
        for request in attention.iter() {
            let session = sessions.iter().find(|s| s.asid == request.asid);
            let open = app.clone();
            let asid = request.asid.clone();
            let mut row = div()
                .class("mq-attention-row")
                .w_full()
                .flex_row()
                .items_center()
                .cursor_pointer();
            if let Some(session) = session {
                row = row.child(agents::badge(&session.agent_id));
            }
            list = list.child(
                row.child(
                    div()
                        .flex_col()
                        .flex_1()
                        .min_w(0.0)
                        .overflow_clip()
                        .child(
                            ui_text(request.prompt.clone())
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        )
                        .child(
                            ui_text(session.map(|s| title_of(app, s)).unwrap_or_default())
                                .size(11.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                )
                .on_click(move |_| open.open_session(&asid)),
            );
        }
        side = side.child(section(app.strings.needs_attention()).child(list));
    }
    let agents_list = app.bindings.agents.get();
    if !agents_list.is_empty() {
        let mut rows = div().class("mq-home-agents").w_full().flex_col();
        for (info, standing) in agents::by_standing(&agents_list) {
            rows = rows.child(agent_row(&app.strings, &info, standing));
        }
        side = side.child(section(app.strings.agents()).child(rows));
    }
    side
}
