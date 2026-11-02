//! Home (architecture §6.0, visual direction §6.1d): what the main panel
//! shows when no session is selected, including at startup.
//!
//! A hero band -- the theme's cover as a quiet backdrop fading out behind the
//! greeting, one line of real counts and the date -- then the Launch row
//! below it, a 12-column grid (Continue working in 8, Needs attention and
//! Agents in 4) and a footer strip pinned to the bottom. Every module shows
//! real data, teaches what will appear when it is empty, or is left out.
//!
//! No colour, family or size literal lives here: colours come from theme
//! variables in `styles.css`, sizes from [`scale`].

mod cover;
mod facts;
mod modules;
mod scale;
mod words;

use blinc_layout::prelude::*;
use blinc_theme::ThemeState;

use super::motion::{self, MotionPrefs};
use super::sessions::{local_utc_offset, now_ms};
use super::shell::SIDEBAR_DEFAULT;
use super::text::{display_text, ui_text};
use super::App;
use crate::config::Density;
use facts::{first_run, DayPart, Layout, LocalTime, Stats};
use words::Words;

/// The page's widest content, centred, inside 40 px margins (§6.0).
pub const CONTENT_MAX: f32 = 1280.0;
/// The cover figure keeps clear of the date at the hero's right edge.
pub const FIGURE_RIGHT: f32 = 280.0;
/// The footer strip pinned under the page.
pub const FOOTER_HEIGHT: f32 = 36.0;

pub fn home(app: &App, width: f32, height: f32) -> Div {
    let agents_state = app.bindings.agents.clone();
    let sessions = app.bindings.sessions.clone();
    let attention = app.bindings.attention.clone();
    let density = match app.config.get().appearance.density {
        Density::Comfortable => "mq-density--comfortable",
        Density::Compact => "mq-density--compact",
    };
    // A visit ends when a session or the New-session page replaces Home:
    // exactly when one of these two signals is written.
    let token = (
        app.bindings.selected.signal().version().unwrap_or(0),
        app.bindings.draft.signal().version().unwrap_or(0),
    );
    motion::HOME_VISIT.with(|v| v.borrow_mut().begin(token));
    let page_app = app.clone();
    let content = stateful_with_key::<NoState>("mq-home")
        .deps([
            agents_state.signal_id(),
            sessions.signal_id(),
            attention.signal_id(),
        ])
        .on_state(move |_| {
            let started = std::time::Instant::now();
            let page = page(&page_app, width);
            tracing::debug!(
                build_us = started.elapsed().as_micros() as u64,
                "home built"
            );
            page
        });
    let footer_app = app.clone();
    let footer = stateful_with_key::<NoState>("mq-home-footer")
        .deps([sessions.signal_id()])
        .on_state(move |_| modules::footer(&footer_app, local_utc_offset()));
    let body = (height - FOOTER_HEIGHT).max(0.0);
    div()
        .class("mq-home")
        .class(density)
        .w_full()
        .h(height)
        .flex_col()
        .child(scroll().w_full().h(body).child(content))
        .child(
            div()
                .w_full()
                .h(FOOTER_HEIGHT)
                .flex_shrink_0()
                .child(footer),
        )
}

/// What the page is laid out for: the window's width drives the
/// breakpoints, as §6.0 states them.
fn window_width(main_width: f32) -> f32 {
    let (w, _) = blinc_core::BlincContextState::get().viewport_size();
    if w > 0.0 {
        w
    } else {
        main_width + SIDEBAR_DEFAULT
    }
}

fn page(app: &App, main_width: f32) -> Div {
    let config = app.config.get();
    let layout = Layout::for_window(window_width(main_width));
    let words = Words {
        language: app.strings.language,
    };
    let agents = app.bindings.agents.get();
    let sessions = app.bindings.sessions.get();
    let attention = app.bindings.attention.get();
    let offset = local_utc_offset();
    let ctx = modules::Ctx {
        app,
        words,
        prefs: MotionPrefs {
            reduced: config.appearance.reduced_motion,
        },
        now: now_ms(),
    };
    let first = first_run(!agents.is_empty(), &sessions, &config.general);
    let art = cover::art(&config.appearance.theme_id, ThemeState::get().scheme());

    let column = div()
        .class("mq-home-content")
        .w_full()
        .max_w(CONTENT_MAX + 2.0 * scale::SPACE_40)
        .flex_col()
        .child(hero(
            &ctx, layout, first, &agents, &sessions, &attention, offset,
        ))
        .child(modules::launch(
            &ctx,
            (main_width - 2.0 * scale::SPACE_40).min(CONTENT_MAX),
            first,
            &agents,
            &sessions,
        ))
        .child(modules::grid(
            &ctx, layout, first, &agents, &sessions, &attention,
        ));
    div()
        .class("mq-home-page")
        .relative()
        .w_full()
        .flex_col()
        .items_center()
        .child(art_layer(&ctx, art, layout, layout.two_columns && !first))
        .child(column)
}

/// The cover, full-bleed behind the hero band only, fading away towards the
/// left so the text stands on the page background, and into the page over
/// its last 40 %. Fades in once per visit (tier 3).
fn art_layer(ctx: &modules::Ctx, art: Option<cover::Art>, layout: Layout, figure: bool) -> Div {
    let height = layout.hero_height;
    let mut layer = div()
        .class("mq-home-art")
        .relative()
        .w_full()
        .h(height)
        .overflow_clip();
    // The scrim and the bottom fade are in the art's own alpha (see
    // `cover`): the page's background shows through, no colour is added.
    let mut art_box = div().absolute().inset(0.0);
    match art {
        Some(art) => {
            let figure_w = height * cover::FIGURE_ASPECT;
            art_box = art_box.child(
                div().absolute().inset(0.0).child(
                    image(art.paper)
                        .w_full()
                        .h(height)
                        .cover()
                        .position_xy(0.5, 1.0)
                        .opacity(cover::PAPER_OPACITY)
                        .no_fade(),
                ),
            );
            // The figure stands between the greeting and the date; a single
            // column, or the longer first-run welcome, leaves it no room.
            if figure {
                art_box = art_box.child(
                    div()
                        .absolute()
                        .top(0.0)
                        .right(FIGURE_RIGHT)
                        .w(figure_w)
                        .h(height)
                        .child(
                            image(art.figure)
                                .w(figure_w)
                                .h(height)
                                .contain()
                                .top_right()
                                .no_fade(),
                        ),
                );
            }
        }
        None => {
            art_box = art_box
                .child(div().class("mq-home-art--generated").absolute().inset(0.0))
                .child(div().class("mq-home-art--glow").absolute().inset(0.0));
        }
    }
    layer = layer.child(art_box);
    div()
        .absolute()
        .top(0.0)
        .left(0.0)
        .w_full()
        .h(height)
        .pointer_events_none()
        .child(ctx.entering(
            "mq-home:cover",
            ctx.prefs.fade(motion::COVER_FADE_MS),
            0,
            layer,
            false,
        ))
}

/// The greeting, then the line of counts with the date on its baseline.
#[allow(clippy::too_many_arguments)]
fn hero(
    ctx: &modules::Ctx,
    layout: Layout,
    first: bool,
    agents: &[muqun_gateway::platform::discovery::AgentDiscoveryInfo],
    sessions: &[muqun_gateway::agents::AgentSessionInfo],
    attention: &[muqun_gateway::agents::PermissionRequest],
    offset: i64,
) -> Div {
    let local = LocalTime::at(ctx.now, offset);
    let (headline, subline) = if first {
        let need_setup = super::agents::by_standing(agents)
            .iter()
            .filter(|(_, s)| *s == super::agents::Standing::NeedsSetup)
            .count();
        (
            ctx.words.welcome().to_string(),
            Some(ctx.words.detected(agents.len(), need_setup)),
        )
    } else {
        (
            ctx.words.greeting(DayPart::of(local.hour)).to_string(),
            ctx.words
                .stats(Stats::of(sessions, attention, ctx.now, offset)),
        )
    };
    let size = if layout.hero_height < 180.0 {
        scale::FONT_32
    } else {
        scale::FONT_56
    };
    let line = div()
        .class("mq-home-hero-line")
        .w_full()
        .flex_row()
        .items_end()
        .child(
            div().class("mq-home-stats").flex_1().min_w(0.0).child(
                ui_text(subline.unwrap_or_default())
                    .size(scale::FONT_15)
                    .no_wrap(),
            ),
        )
        .child(
            div().class("mq-home-date").flex_shrink_0().child(
                ui_text(ctx.words.date(local))
                    .size(scale::FONT_15)
                    .no_wrap(),
            ),
        );
    let hero = div()
        .class("mq-home-hero")
        .w_full()
        .h(layout.hero_height)
        .flex_col()
        .justify_end()
        .child(
            div().class("mq-home-greeting").child(
                display_text(headline)
                    .size(size)
                    .line_height(1.15)
                    .no_wrap(),
            ),
        )
        .child(line);
    ctx.entering(
        "mq-home:greeting",
        ctx.prefs
            .rise(motion::GREETING_MS, motion::GREETING_RISE_PX),
        0,
        hero,
        false,
    )
}
