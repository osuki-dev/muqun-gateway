//! The window's frame (architecture §6.1b): a slim top bar carrying the
//! session header, then three columns -- sidebar, main panel, and a right
//! column that starts collapsed (its panels arrive in later tasks).
//!
//! The main panel shows exactly one of: Home (nothing selected), the
//! New-session page, or the selected session.

use blinc_app::windowed::WindowedContext;
use blinc_cn::prelude::{button, resizable_group, resizable_panel, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use blinc_theme::{ColorScheme, ThemeState};
use muqun_gateway::home_relative;

use super::agents;
use super::sessions::title_of;
use super::text::ui_text;
use super::{composer, errors, home, new_session, sessions, settings, timeline, App};
use crate::branding::{mark_data_uri, APP_NAME};
use crate::config::{ConfigEdit, Scheme};
use crate::model::commands::Command;

pub const TOP_BAR_HEIGHT: f32 = 44.0;
use crate::config::UiPrefs;
const SIDEBAR_MIN: f32 = UiPrefs::SIDEBAR_MIN;
const SIDEBAR_MAX: f32 = UiPrefs::SIDEBAR_MAX;
pub const RIGHT_MAX: f32 = 420.0;

pub fn top_bar(app: &App) -> Div {
    let header = app.bindings.header.clone();
    let agents_state = app.bindings.agents.clone();
    let header_app = app.clone();
    let session_header = stateful_with_key::<NoState>("mq-session-header")
        .deps([header.signal_id(), agents_state.signal_id()])
        .on_state(move |_| {
            let Some(info) = header.get() else {
                return div();
            };
            let agents_list = agents_state.get();
            let mut line = div()
                .class("mq-session-header")
                .flex_row()
                .items_center()
                .min_w(0.0)
                .overflow_clip()
                .child(
                    div().class("mq-header-title").child(
                        ui_text(title_of(&header_app, &info))
                            .size(13.0)
                            .semibold()
                            .no_wrap(),
                    ),
                )
                .child(ui_text("·").size(13.0))
                .child(agents::mark(&info.agent_id, 16.0))
                .child(
                    ui_text(agents::name(&info.agent_id, &agents_list))
                        .size(13.0)
                        .no_wrap(),
                );
            if let Some(model) = &info.model {
                line = line
                    .child(ui_text("·").size(13.0))
                    .child(ui_text(model.model_id.clone()).size(12.0).no_wrap());
            }
            if let Some(dir) = &info.directory {
                let home = dirs::home_dir();
                line = line.child(ui_text("·").size(13.0)).child(
                    ui_text(home_relative(dir, home.as_deref()))
                        .size(12.0)
                        .no_wrap(),
                );
            }
            line
        });

    let home_app = app.clone();
    div()
        .class("mq-topbar")
        .w_full()
        .h(TOP_BAR_HEIGHT)
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .child(
            div()
                .class("mq-brand")
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .cursor_pointer()
                .child(image(mark_data_uri()).size(20.0, 20.0).rounded(5.0))
                .child(
                    ui_text(APP_NAME)
                        .size(13.0)
                        .semibold()
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| home_app.drain.go_home()),
        )
        .child(div().class("mq-topbar-sep").flex_shrink_0())
        .child(div().flex_1().min_w(0.0).child(session_header))
        .child(scheme_toggle(app))
}

fn scheme_toggle(app: &App) -> impl ElementBuilder {
    let app = app.clone();
    let label = match ThemeState::get().scheme() {
        ColorScheme::Dark => app.strings.light(),
        ColorScheme::Light => app.strings.dark(),
    };
    button(label)
        .variant(ButtonVariant::Ghost)
        .size(ButtonSize::Small)
        .on_click(move |_| {
            // An explicit choice stops following the system.
            let next = match ThemeState::get().scheme().toggle() {
                ColorScheme::Light => Scheme::Light,
                ColorScheme::Dark => Scheme::Dark,
            };
            app.apply_scheme(next);
        })
}

pub fn body(ctx: &WindowedContext, app: &App) -> Div {
    let height = (ctx.height - TOP_BAR_HEIGHT).max(0.0);
    let sidebar_width = app.config.get().ui.sidebar();
    let main_width = (ctx.width - sidebar_width).max(0.0);
    let remember = app.clone();
    div().class("mq-body").w_full().h(height).child(
        resizable_group()
            .key("mq-shell")
            .horizontal()
            .handle_thickness(1.0)
            .on_resize(move |sizes| {
                if let Some(width) = sizes.first().copied().filter(|w| *w > 0.0) {
                    remember.send(Command::Edit(ConfigEdit::SidebarWidth(width)));
                }
            })
            .panel(
                resizable_panel()
                    .default_size(sidebar_width)
                    .min_size(SIDEBAR_MIN)
                    .max_size(SIDEBAR_MAX)
                    .child(sessions::sidebar(app, height)),
            )
            .panel(
                // Absolutely placed, so the panel's width never depends on its
                // content: a flex panel is sized from its content's natural
                // width, and a long unwrapped line in a message would shrink
                // the sidebar below the width asked for.
                resizable_panel().flex_grow().child(
                    div()
                        .absolute()
                        .left(0.0)
                        .top(0.0)
                        .right(0.0)
                        .bottom(0.0)
                        .child(main_panel(app, main_width, height)),
                ),
            )
            .panel(
                resizable_panel()
                    .default_size(0.0)
                    .min_size(0.0)
                    .max_size(RIGHT_MAX)
                    .child(div().class("mq-right").w_full().h(height)),
            ),
    )
}

fn main_panel(app: &App, width: f32, height: f32) -> Div {
    let selected = app.bindings.selected.clone();
    let draft = app.bindings.draft.clone();
    let settings_open = app.bindings.settings_open.clone();
    let strip = errors::strip(app);
    let app = app.clone();
    let view = stateful_with_key::<NoState>("mq-main-view")
        .deps([
            selected.signal_id(),
            draft.signal_id(),
            settings_open.signal_id(),
        ])
        .on_state(move |_| {
            if settings_open.get() {
                return settings::page(&app, height);
            }
            if draft.get().is_some() {
                return new_session::page(&app, height);
            }
            if selected.get().is_none() {
                return home::home(&app, width, height);
            }
            let timeline_height = (height - composer::COMPOSER_HEIGHT).max(0.0);
            div()
                .w_full()
                .h(height)
                .flex_col()
                .child(timeline::timeline(&app, timeline_height))
                .child(composer::composer(&app))
        });
    div()
        .class("mq-main")
        .relative()
        .w_full()
        .h(height)
        .child(view)
        // Last, and absolutely placed over the top, so the strip never
        // moves the layout under the reader.
        .child(strip)
}
