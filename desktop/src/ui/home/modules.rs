//! Home's modules: Launch, Continue working, Needs attention, Agents and the
//! footer strip. Content sits on solid surfaces; hierarchy comes from size
//! and whitespace; agent colour appears only as 3 px bars, badges and dots.

use blinc_animation::MultiKeyframeAnimation;
use blinc_layout::prelude::*;
use muqun_gateway::agents::{
    AgentSessionInfo, AgentSessionStatus, PermissionDecision, PermissionRequest,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::{home_relative, relative_since};

use super::facts::{compact_count, middle_ellipsis, Layout, Usage};
use super::scale;
use super::words::Words;
use crate::config::Language;
use crate::model::commands::Command;
use crate::ui::agents::{self, Standing};
use crate::ui::motion::{self, MotionPrefs, Stagger, CARD_STAGGER, HOME_VISIT, ROW_STAGGER};
use crate::ui::sessions::{now_ms, title_of};
use crate::ui::text::{mono_text, ui_text};
use crate::ui::{shortcut, App};

/// Launch cards (§6.0): 160 × 112.
pub const CARD_W: f32 = 160.0;
pub const CARD_H: f32 = 112.0;
/// A card line holds this many monospace characters (12 px JetBrains Mono
/// is 7.2 px wide; the card's text box is 160 − 2 × 16 − 3 px).
pub const CARD_CHARS: usize = 17;
/// The first-run card spans two cards and their gap.
pub const FIRST_CARD_W: f32 = 2.0 * CARD_W + 12.0;
/// The gap between launch cards (`--mq-space-3`).
pub const CARD_GAP: f32 = 12.0;

/// How many launch cards one row of `content_width` holds.
pub fn cards_that_fit(content_width: f32) -> usize {
    (((content_width + CARD_GAP) / (CARD_W + CARD_GAP)).floor() as usize).max(2)
}
/// Continue working lists this many sessions.
pub const RECENT: usize = 6;

/// One cascade's position among this build's newly shown items.
struct Cascade {
    stagger: Stagger,
    next: usize,
}

impl Cascade {
    fn new(stagger: Stagger) -> Self {
        Self { stagger, next: 0 }
    }
}

/// What every module needs to draw itself.
pub struct Ctx<'a> {
    pub app: &'a App,
    pub words: Words,
    pub prefs: MotionPrefs,
    pub now: u64,
}

impl Ctx<'_> {
    /// `child` entering with `animation` after `delay_ms` -- the first time
    /// `key` appears in this visit only; afterwards just `child`.
    pub fn entering(
        &self,
        key: &str,
        animation: MultiKeyframeAnimation,
        delay_ms: u32,
        child: Div,
        fit: bool,
    ) -> Div {
        if !HOME_VISIT.with(|v| v.borrow_mut().first_time(key)) {
            return child;
        }
        let m = motion::enter(key, animation, delay_ms, child);
        div().child(if fit { m.fit_content() } else { m })
    }

    /// A cascade member: the next delay of `cascade` the first time `key`
    /// appears this visit; past the cascade's cap, or seen before, it just
    /// appears.
    fn cascaded(&self, cascade: &mut Cascade, key: &str, child: Div, fit: bool) -> Div {
        if HOME_VISIT.with(|v| v.borrow().has_shown(key)) {
            return child;
        }
        let index = cascade.next;
        cascade.next += 1;
        match self.prefs.stagger(cascade.stagger, index) {
            Some(delay) => self.entering(
                key,
                self.prefs.rise(motion::MEDIUM_MS, motion::ITEM_RISE_PX),
                delay,
                child,
                fit,
            ),
            None => {
                HOME_VISIT.with(|v| v.borrow_mut().first_time(key));
                child
            }
        }
    }
}

/// A section title: 13 px, uppercase, tracked.
fn title(label: &str, language: Language) -> Div {
    let label = match language {
        Language::En => label.to_uppercase(),
        Language::ZhCn => label.to_string(),
    };
    div().class("mq-home-title").child(
        ui_text(label)
            .size(scale::FONT_13)
            .semibold()
            .letter_spacing(scale::TITLE_TRACKING)
            .no_wrap(),
    )
}

/// A section: its title, then its body.
fn section(label: &str, language: Language, body: impl ElementBuilder + 'static) -> Div {
    div()
        .class("mq-home-section")
        .w_full()
        .flex_col()
        .child(title(label, language))
        .child(body)
}

/// A solid card, the surface every module's content sits on.
fn card() -> Div {
    div().class("mq-home-card").w_full().flex_col()
}

/// The quiet line an empty module shows, saying what will appear.
fn teach(line: &str) -> Div {
    div()
        .class("mq-home-teach")
        .child(ui_text(line.to_string()).size(scale::FONT_13))
}

fn hairline() -> Div {
    div()
        .class("mq-home-hairline")
        .w_full()
        .h(1.0)
        .flex_shrink_0()
}

fn bar(agent: &str) -> Div {
    div()
        .class("mq-home-bar")
        .class(agents::brand_class(agent))
        .absolute()
        .left(0.0)
        .top(0.0)
        .bottom(0.0)
        .w(scale::BAR)
}

fn dot(class: &str) -> Div {
    div()
        .class("mq-home-dot")
        .class(class)
        .w(scale::DOT)
        .h(scale::DOT)
        .rounded(scale::DOT_RADIUS)
        .flex_shrink_0()
}

fn usable(list: &[AgentDiscoveryInfo]) -> Vec<String> {
    agents::by_standing(list)
        .into_iter()
        .filter(|(_, s)| *s == Standing::Usable)
        .map(|(a, _)| a.id)
        .collect()
}

/// A folder as a card shows it: home-relative, and the home folder itself
/// as `~/` so it reads as a path rather than a stray glyph.
fn card_path(cwd: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home_relative(&cwd.to_string_lossy(), home).as_str() {
        "~" => "~/".to_string(),
        other => other.to_string(),
    }
}

/// "model · mode" of the newest session of `agent`, if it reported either.
fn model_line(sessions: &[AgentSessionInfo], agent: &str) -> Option<String> {
    let s = sessions.iter().find(|s| s.agent_id == agent)?;
    let model = s.model.as_ref().map(|m| m.model_id.clone());
    match (model, s.mode.clone()) {
        (Some(m), Some(mode)) => Some(format!("{m} · {mode}")),
        (m, mode) => m.or(mode),
    }
}

/// The Launch row, below the hero: "New session", then one card per usable
/// agent. A card with a remembered folder starts there at once; without one
/// it opens the New-session page for that agent. First run: one big card.
pub fn launch(
    ctx: &Ctx,
    content_width: f32,
    first: bool,
    agent_list: &[AgentDiscoveryInfo],
    sessions: &[AgentSessionInfo],
) -> Div {
    let app = ctx.app;
    let strings = app.strings;
    if first {
        let open = app.clone();
        let card = div()
            .class("mq-home-launch-card")
            .class("mq-home-launch-card--primary")
            .w(FIRST_CARD_W)
            .h(CARD_H)
            .flex_col()
            .justify_between()
            .cursor_pointer()
            .child(
                ui_text(ctx.words.first_session())
                    .size(scale::FONT_18)
                    .semibold()
                    .no_cursor(),
            )
            .child(
                div().class("mq-home-launch-hint").child(
                    ui_text(ctx.words.first_session_hint())
                        .size(scale::FONT_12)
                        .no_cursor(),
                ),
            )
            .on_click(move |_| open.open_draft());
        let row = div().class("mq-home-launch").flex_row().child(ctx.entering(
            "mq-home:launch:first",
            ctx.prefs.rise(motion::MEDIUM_MS, motion::ITEM_RISE_PX),
            0,
            card,
            true,
        ));
        return section(strings.launch(), ctx.words.language, row);
    }

    let home = dirs::home_dir();
    let general = app.config.get().general;
    let ids = usable(agent_list);

    let open = app.clone();
    let mut cards: Vec<(String, Div)> = vec![(
        "new".into(),
        div()
            .class("mq-home-launch-card")
            .class("mq-home-launch-card--primary")
            .w(CARD_W)
            .h(CARD_H)
            .flex_shrink_0()
            .flex_col()
            .justify_between()
            .cursor_pointer()
            .child(ui_text("+").size(scale::FONT_24).no_cursor())
            .child(
                div()
                    .flex_col()
                    .child(
                        ui_text(ctx.words.new_card())
                            .size(scale::FONT_15)
                            .semibold()
                            .no_wrap()
                            .no_cursor(),
                    )
                    .child(
                        div()
                            .class("mq-home-launch-key")
                            .child(mono_text(shortcut("N")).size(scale::FONT_12).no_cursor()),
                    ),
            )
            .on_click(move |_| open.open_draft()),
    )];

    for id in &ids {
        let remembered = general
            .last_cwd
            .get(id)
            .or(general.default_cwd.as_ref())
            .cloned();
        let mut lines = div().class("mq-home-launch-lines").flex_col();
        lines = lines.child(match &remembered {
            Some(cwd) => div().class("mq-home-launch-meta").child(
                mono_text(middle_ellipsis(
                    &card_path(cwd, home.as_deref()),
                    CARD_CHARS,
                ))
                .size(scale::FONT_12)
                .no_wrap()
                .no_cursor(),
            ),
            None => div().class("mq-home-launch-choose").child(
                ui_text(ctx.words.choose_folder())
                    .size(scale::FONT_12)
                    .no_wrap()
                    .no_cursor(),
            ),
        });
        if let Some(model) = model_line(sessions, id) {
            lines = lines.child(
                div().class("mq-home-launch-meta").child(
                    mono_text(middle_ellipsis(&model, CARD_CHARS))
                        .size(scale::FONT_12)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        let start = app.clone();
        let agent = id.clone();
        let card = div()
            .class("mq-home-launch-card")
            .relative()
            .w(CARD_W)
            .h(CARD_H)
            .flex_shrink_0()
            .flex_col()
            .justify_between()
            .cursor_pointer()
            .overflow_clip()
            .child(bar(id))
            .child(
                div()
                    .class("mq-home-launch-name")
                    .flex_row()
                    .items_center()
                    .child(agents::mark(id, 20.0))
                    .child(
                        ui_text(agents::name(id, agent_list))
                            .size(scale::FONT_15)
                            .semibold()
                            .no_wrap()
                            .no_cursor(),
                    ),
            )
            .child(lines)
            .on_click(move |_| match &remembered {
                Some(cwd) => start.create(agent.clone(), cwd.clone()),
                None => start.draft_for(agent.clone()),
            });
        cards.push((id.clone(), card));
    }

    // One row, never wrapped: when the agents outnumber the row, the last
    // place is "N more", which opens the New-session page listing them all.
    // (A sideways-scrolling row was tried: Blinc's scroll physics then kept
    // waking the idle page at ~7 fps, against red line 3.)
    let fit = cards_that_fit(content_width);
    if cards.len() > fit {
        let hidden = cards.len() - (fit - 1);
        cards.truncate(fit - 1);
        let open = app.clone();
        cards.push((
            "more".into(),
            div()
                .class("mq-home-launch-card")
                .class("mq-home-launch-card--more")
                .w(CARD_W)
                .h(CARD_H)
                .flex_shrink_0()
                .flex_col()
                .justify_between()
                .cursor_pointer()
                .child(
                    ui_text(format!("+{hidden}"))
                        .size(scale::FONT_24)
                        .no_cursor(),
                )
                .child(
                    ui_text(ctx.words.more_agents())
                        .size(scale::FONT_15)
                        .semibold()
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| open.open_draft()),
        ));
    }
    let mut row = div().class("mq-home-launch").flex_row();
    let mut cascade = Cascade::new(CARD_STAGGER);
    for (key, card) in cards {
        row = row.child(ctx.cascaded(&mut cascade, &format!("mq-home:launch:{key}"), card, true));
    }
    let mut body = div()
        .class("mq-home-launch-body")
        .w_full()
        .flex_col()
        .child(row);
    if ids.is_empty() && !agent_list.is_empty() {
        body = body.child(teach(ctx.words.launch_empty()));
    }
    section(strings.launch(), ctx.words.language, body)
}

/// Continue working (8 columns) beside Needs attention and Agents (4).
/// Below 1100 px the side column moves under the main one.
pub fn grid(
    ctx: &Ctx,
    layout: Layout,
    first: bool,
    agent_list: &[AgentDiscoveryInfo],
    sessions: &[AgentSessionInfo],
    attention: &[PermissionRequest],
) -> Div {
    let side = div()
        .class("mq-home-side")
        .flex_col()
        .min_w(0.0)
        .child(needs_attention(ctx, sessions, attention))
        .child(agents_module(ctx, agent_list));
    let main = (!first && !sessions.is_empty()).then(|| continue_working(ctx, sessions, attention));
    let mut grid = div().class("mq-home-grid").w_full();
    match (layout.two_columns, main) {
        (true, Some(main)) => {
            grid = grid
                .flex_row()
                .items_start()
                .child(main.flex_1().flex_grow_value(2.0).min_w(0.0))
                .child(side.flex_1().flex_grow_value(1.0));
        }
        (true, None) => {
            // First run: the side modules take the main column's place.
            grid = grid
                .flex_row()
                .items_start()
                .child(side.flex_1().flex_grow_value(2.0).min_w(0.0))
                .child(div().flex_1().flex_grow_value(1.0));
        }
        (false, main) => {
            grid = grid.flex_col();
            if let Some(main) = main {
                grid = grid.child(main);
            }
            grid = grid.child(side.w_full());
        }
    }
    grid
}

fn status_dot(waiting: bool, status: AgentSessionStatus) -> Div {
    dot(if waiting {
        "mq-home-dot--waiting"
    } else {
        match status {
            AgentSessionStatus::Busy | AgentSessionStatus::Retry => "mq-home-dot--busy",
            AgentSessionStatus::Failed => "mq-home-dot--failed",
            _ => "mq-home-dot--idle",
        }
    })
}

/// The most recent sessions of every agent: 72 px rows in one card,
/// separated by hairlines.
fn continue_working(
    ctx: &Ctx,
    sessions: &[AgentSessionInfo],
    attention: &[PermissionRequest],
) -> Div {
    let app = ctx.app;
    let home = dirs::home_dir();
    let mut list = card().class("mq-home-list");
    let mut cascade = Cascade::new(ROW_STAGGER);
    for (index, session) in sessions.iter().take(RECENT).enumerate() {
        if index > 0 {
            list = list.child(hairline());
        }
        let open = app.clone();
        let asid = session.asid.clone();
        let waiting = attention.iter().any(|p| p.asid == session.asid);
        let mut meta = div()
            .class("mq-home-row-meta")
            .flex_row()
            .items_center()
            .min_w(0.0)
            .overflow_clip();
        if let Some(dir) = session.directory.as_deref() {
            meta = meta.child(
                mono_text(home_relative(dir, home.as_deref()))
                    .size(scale::FONT_12)
                    .no_wrap()
                    .no_cursor(),
            );
        }
        if let Some(branch) = session.git_info.as_ref().and_then(|g| g.branch.clone()) {
            meta = meta.child(
                mono_text(format!("⎇ {branch}"))
                    .size(scale::FONT_12)
                    .no_wrap()
                    .no_cursor(),
            );
        }
        if let Some(model) = &session.model {
            meta = meta.child(
                mono_text(model.model_id.clone())
                    .size(scale::FONT_12)
                    .no_wrap()
                    .no_cursor(),
            );
        }
        let row = div()
            .class("mq-home-row")
            .w_full()
            .flex_row()
            .items_center()
            .cursor_pointer()
            .overflow_clip()
            .child(status_dot(waiting, session.status))
            .child(
                div()
                    .class("mq-home-row-main")
                    .flex_col()
                    .flex_1()
                    .min_w(0.0)
                    .overflow_clip()
                    .child(
                        div().class("mq-home-row-title").child(
                            ui_text(title_of(app, session))
                                .size(scale::FONT_15)
                                .medium()
                                .no_wrap()
                                .no_cursor(),
                        ),
                    )
                    .child(meta),
            )
            .child(agents::mark(&session.agent_id, 18.0))
            .child(
                div().class("mq-home-row-when").flex_shrink_0().child(
                    mono_text(relative_since(u128::from(session.updated_ms)))
                        .size(scale::FONT_12)
                        .no_wrap()
                        .no_cursor(),
                ),
            )
            .on_click(move |_| open.open_session(&asid));
        list = list.child(ctx.cascaded(
            &mut cascade,
            &format!("mq-home:recent:{}", session.asid.0),
            row,
            false,
        ));
    }
    section(app.strings.continue_working(), ctx.words.language, list).class("mq-home-main")
}

/// Permissions waiting in any session, answered here. Empty: what will
/// appear (§6.1d item 2).
fn needs_attention(
    ctx: &Ctx,
    sessions: &[AgentSessionInfo],
    attention: &[PermissionRequest],
) -> Div {
    let app = ctx.app;
    let label = app.strings.needs_attention();
    if attention.is_empty() {
        return section(
            label,
            ctx.words.language,
            card()
                .class("mq-home-card--padded")
                .child(teach(ctx.words.attention_empty())),
        );
    }
    let mut list = card().class("mq-home-attention");
    let mut cascade = Cascade::new(ROW_STAGGER);
    for (index, request) in attention.iter().enumerate() {
        if index > 0 {
            list = list.child(hairline());
        }
        let session = sessions.iter().find(|s| s.asid == request.asid);
        let open = app.clone();
        let asid = request.asid.clone();
        let reply = |label: &str, class: &str, decision: PermissionDecision| {
            let app = app.clone();
            let (asid, id) = (request.asid.clone(), request.id.clone());
            div()
                .class("mq-home-reply")
                .class(class)
                .cursor_pointer()
                .child(
                    ui_text(label.to_string())
                        .size(scale::FONT_13)
                        .medium()
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| {
                    app.send(Command::ReplyPermission {
                        asid: asid.clone(),
                        id: id.clone(),
                        decision,
                    })
                })
        };
        let mut body = div()
            .class("mq-home-attention-body")
            .flex_col()
            .w_full()
            .min_w(0.0)
            .overflow_clip()
            .child(
                div()
                    .class("mq-home-attention-prompt")
                    .cursor_pointer()
                    .on_click(move |_| open.open_session(&asid))
                    .child(
                        ui_text(request.prompt.clone())
                            .size(scale::FONT_15)
                            .medium()
                            .no_cursor(),
                    ),
            );
        if let Some(resource) = request.resources.first() {
            body = body.child(
                div().class("mq-home-row-meta").child(
                    mono_text(resource.clone())
                        .size(scale::FONT_12)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        if let Some(session) = session {
            body = body.child(
                div().class("mq-home-row-meta").child(
                    ui_text(format!(
                        "{} · {}",
                        title_of(app, session),
                        relative_since(u128::from(session.updated_ms))
                    ))
                    .size(scale::FONT_12)
                    .no_wrap()
                    .no_cursor(),
                ),
            );
        }
        body = body.child(
            div()
                .class("mq-home-replies")
                .flex_row()
                .justify_end()
                .child(reply(
                    ctx.words.deny(),
                    "mq-home-reply--deny",
                    PermissionDecision::Deny,
                ))
                .child(reply(
                    ctx.words.allow(),
                    "mq-home-reply--allow",
                    PermissionDecision::Allow,
                )),
        );
        let mut item = div()
            .class("mq-home-attention-item")
            .relative()
            .w_full()
            .overflow_clip()
            .child(body);
        if let Some(session) = session {
            item = item.child(bar(&session.agent_id));
        }
        list = list.child(ctx.cascaded(
            &mut cascade,
            &format!("mq-home:attention:{}", request.id),
            item,
            false,
        ));
    }
    section(label, ctx.words.language, list)
}

/// Every agent, two columns of rows: dot, name, status word on the right.
fn agents_module(ctx: &Ctx, agent_list: &[AgentDiscoveryInfo]) -> Div {
    let label = ctx.words.agents();
    if agent_list.is_empty() {
        return section(
            label,
            ctx.words.language,
            card()
                .class("mq-home-card--padded")
                .child(teach(ctx.words.agents_checking())),
        );
    }
    let ranked = agents::by_standing(agent_list);
    let mut grid = card().class("mq-home-card--padded").class("mq-home-agents");
    for pair in ranked.chunks(2) {
        let mut row = div().class("mq-home-agents-row").w_full().flex_row();
        for (info, standing) in pair {
            let marker = if *standing == Standing::Usable {
                agents::mark(&info.id, 18.0)
            } else {
                agents::mark_idle(&info.id, 18.0)
            };
            row = row.child(
                div()
                    .class("mq-home-agent")
                    .flex_1()
                    .min_w(0.0)
                    .flex_row()
                    .items_center()
                    .child(marker)
                    .child(
                        div()
                            .class("mq-home-agent-name")
                            .flex_1()
                            .min_w(0.0)
                            .overflow_clip()
                            .child(
                                ui_text(agents::name(&info.id, agent_list))
                                    .size(scale::FONT_13)
                                    .no_wrap(),
                            ),
                    )
                    .child(
                        div().class(standing.class()).flex_shrink_0().child(
                            ui_text(standing.word(&ctx.app.strings))
                                .size(scale::FONT_12)
                                .no_wrap(),
                        ),
                    ),
            );
        }
        if pair.len() == 1 {
            row = row.child(div().flex_1());
        }
        grid = grid.child(row);
    }
    section(label, ctx.words.language, grid)
}

/// The strip pinned under the page: today's usage when the agents report
/// any, the version, and the command-palette shortcut.
pub fn footer(app: &App, offset: i64) -> Div {
    let words = Words {
        language: app.strings.language(),
    };
    let sessions = app.bindings.sessions.get();
    let usage = Usage::today(&sessions, now_ms(), offset);
    let mut parts = Vec::new();
    if let Some(tokens) = usage.tokens {
        parts.push(format!("{} tokens", compact_count(tokens)));
    }
    if let Some(cost) = usage.cost {
        parts.push(format!("${cost:.2}"));
    }
    let mut left = div().class("mq-home-footer-part").flex_row().items_center();
    if !parts.is_empty() {
        left = left
            .child(ui_text(words.today_usage()).size(scale::FONT_12).no_wrap())
            .child(mono_text(parts.join(" · ")).size(scale::FONT_12).no_wrap());
    }
    div()
        .class("mq-home-footer")
        .w_full()
        .h_full()
        .flex_row()
        .items_center()
        .justify_between()
        .child(left)
        .child(
            div()
                .class("mq-home-footer-part")
                .flex_row()
                .items_center()
                .child(
                    mono_text(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(scale::FONT_12)
                        .no_wrap(),
                )
                .child(mono_text(shortcut("K")).size(scale::FONT_12).no_wrap()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_row_holds_seven_at_full_width() {
        assert_eq!(cards_that_fit(1280.0), 7);
        assert_eq!(cards_that_fit(620.0), 3);
        assert_eq!(cards_that_fit(100.0), 2, "always New and one more");
    }
}
