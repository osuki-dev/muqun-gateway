//! The start page (architecture §6.0‴ B): "What are we doing?", the
//! composer card, and the recent sessions. Nothing else -- no statistics, no
//! launch cards, no agents panel. Home *is* the composer: the caret is in it
//! on entry, and Enter starts a session with every default.
//!
//! No colour or family literal lives here: colours come from theme variables
//! in `styles.css` (`/* start */`), text families from `ui::text`.

use std::path::PathBuf;
use std::sync::OnceLock;

use blinc_layout::prelude::*;
use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;

use super::agents::{self, Standing};
use super::composer;
use super::sessions::title_of;
use super::text::{display_text, mono_text, ui_text};
use super::App;
use crate::config::{ConfigEdit, General};
use crate::model::commands::Command;

/// The title's size.
const TITLE_SIZE: f32 = 28.0;
/// Recent rows at most, and a row's height.
const RECENT_ROWS: usize = 8;
const ROW_HEIGHT: f32 = 44.0;
/// Opacity of the theme's cover behind the page (dark / light).
const COVER_DARK: f32 = 0.08;
const COVER_LIGHT: f32 = 0.06;
/// The cover fills this much of the page's width, anchored at the upper right.
const COVER_WIDTH_SHARE: f32 = 0.62;
const COVER_HEIGHT_SHARE: f32 = 0.78;
/// Vertical space the page spends outside the recent rows: top margin share,
/// title, gaps, the card, the label, the bottom margin.
/// The title's box (28 px type, 24 px below it) and the subtitle's.
const TITLE_BLOCK: f32 = 58.0;
const SUBTITLE_BLOCK: f32 = 28.0;
const CARD_ESTIMATE: f32 = 150.0;
const LABEL_BLOCK: f32 = 28.0;
const BOTTOM_MARGIN: f32 = 24.0;

/// Nothing to continue and no session ever started from this app. Before the
/// agents are known nothing is decided.
pub fn first_run(agents_known: bool, sessions: &[AgentSessionInfo], general: &General) -> bool {
    agents_known
        && sessions.is_empty()
        && general.last_agent.is_none()
        && general.last_cwd.is_empty()
}

/// Agents found on this machine, usable or not (the first-run subtitle).
fn detected(agents: &[AgentDiscoveryInfo]) -> usize {
    agents
        .iter()
        .filter(|a| !matches!(Standing::of(a), Standing::NotInstalled | Standing::Disabled))
        .count()
}

/// Where the start page puts its composer card: margin above the title,
/// then the title (and the first-run subtitle), then the card, centred.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartLayout {
    pub top: f32,
    pub card_width: f32,
    /// The card's top edge inside the main panel.
    pub card_top: f32,
}

pub fn start_layout(main_width: f32, main_height: f32, subtitle: bool) -> StartLayout {
    let top = (main_height * 0.17).clamp(24.0, 150.0);
    StartLayout {
        top,
        card_width: START_CARD_MAX.min((main_width - 80.0).max(320.0)),
        card_top: top + TITLE_BLOCK + if subtitle { SUBTITLE_BLOCK } else { 0.0 },
    }
}

/// The card's top-left corner in the window, and its width, for the list
/// that hangs from it (the registry's element bounds are parent-local).
pub fn start_card_origin(app: &App) -> (f32, f32, f32) {
    let (vw, vh) = blinc_core::BlincContextState::get().viewport_size();
    let sidebar = app.config.get().ui.sidebar();
    let main = (vw - sidebar).max(0.0);
    let top_bar = super::shell::TOP_BAR_HEIGHT;
    let config = app.config.get();
    let agents = app.bindings.agents.get();
    let sessions = app.bindings.sessions.get();
    let first = first_run(!agents.is_empty(), &sessions, &config.general);
    let layout = start_layout(main, (vh - top_bar).max(0.0), first);
    (
        sidebar + (main - layout.card_width) / 2.0,
        top_bar + layout.card_top,
        layout.card_width,
    )
}

pub fn home(app: &App, width: f32, height: f32) -> Div {
    // Entering the page puts the caret in the composer.
    composer::focus_input(app);
    let agents_state = app.bindings.agents.clone();
    let sessions = app.bindings.sessions.clone();
    let theme_epoch = app.bindings.theme_epoch.clone();
    let page_app = app.clone();
    let page = stateful_with_key::<NoState>("mq-start")
        .deps([
            agents_state.signal_id(),
            sessions.signal_id(),
            theme_epoch.signal_id(),
        ])
        .on_state(move |_| page(&page_app, width, height));
    div()
        .class("mq-start")
        .w_full()
        .h(height)
        .relative()
        .child(page)
}

fn page(app: &App, width: f32, height: f32) -> Div {
    let config = app.config.get();
    let agents_list = app.bindings.agents.get();
    let sessions = app.bindings.sessions.get();
    let first = first_run(!agents_list.is_empty(), &sessions, &config.general);

    let (title, subtitle) = if first {
        (
            app.strings.welcome().to_string(),
            Some(app.strings.detected_agents(detected(&agents_list))),
        )
    } else {
        (app.strings.start_title().to_string(), None)
    };

    let layout = start_layout(width, height, subtitle.is_some());
    let (top, card_width) = (layout.top, layout.card_width);
    let fixed = layout.card_top + CARD_ESTIMATE + LABEL_BLOCK + BOTTOM_MARGIN + 24.0;
    let fit = ((height - fixed) / ROW_HEIGHT).floor().max(0.0) as usize;
    let rows = RECENT_ROWS.min(fit);

    let mut column = div()
        .class("mq-start-column")
        .w(card_width)
        .flex_col()
        .child(
            div()
                .class("mq-start-title")
                .w_full()
                .flex_row()
                .justify_center()
                .child(
                    display_text(title)
                        .size(TITLE_SIZE)
                        .line_height(1.2)
                        .no_wrap(),
                ),
        );
    if let Some(subtitle) = subtitle {
        column = column.child(
            div()
                .class("mq-start-subtitle")
                .w_full()
                .h(SUBTITLE_BLOCK)
                .flex_shrink_0()
                .flex_row()
                .justify_center()
                .child(ui_text(subtitle).size(13.0).no_wrap()),
        );
    }
    column = column
        .child(
            div()
                .class("mq-start-card")
                .child(composer::start_card(app, card_width)),
        )
        .child(recents(app, &sessions, rows));

    let mut page = div()
        .class("mq-start-page")
        .absolute()
        .inset(0.0)
        .flex_col()
        .items_center()
        .child(div().h(top).flex_shrink_0())
        .child(column);
    if let Some(cover) = cover(app, width, height) {
        // Under everything: the page's own content is added after it.
        page = div().absolute().inset(0.0).child(cover).child(page);
    }
    page
}

/// The widest the card gets.
const START_CARD_MAX: f32 = composer::START_WIDTH;

// ---------------------------------------------------------------------------
// The cover backdrop
// ---------------------------------------------------------------------------

/// The theme's cover as a quiet backdrop at the upper right, fading out
/// towards the page. Hidden with reduced transparency or when the theme has
/// no cover.
fn cover(app: &App, width: f32, height: f32) -> Option<Div> {
    if app.config.get().appearance.reduced_transparency {
        return None;
    }
    let path = cover_file()?;
    let dark = blinc_theme::ThemeState::try_get()
        .is_some_and(|t| t.scheme() == blinc_theme::ColorScheme::Dark);
    let w = (width * COVER_WIDTH_SHARE).max(320.0);
    let h = height * COVER_HEIGHT_SHARE;
    Some(
        div()
            .absolute()
            .top(0.0)
            .right(0.0)
            .w(w)
            .h(h)
            .class("mq-start-cover")
            .pointer_events_none()
            .child(
                image(path)
                    .w(w)
                    .h(h)
                    .cover()
                    .top_right()
                    .opacity(if dark { COVER_DARK } else { COVER_LIGHT })
                    .no_fade(),
            ),
    )
}

/// The current theme's cover image, written once under the cache directory
/// and handed to Blinc as a path: Blinc keys its image cache by the source
/// string, and a large `data:` URI made every rebuild of a page cost tens of
/// milliseconds. `None` when the theme has no cover or the file cannot be
/// written.
fn cover_file() -> Option<String> {
    let bytes = crate::theme::apply::current_cover()?;
    static LAST: OnceLock<std::sync::Mutex<Option<(u64, PathBuf)>>> = OnceLock::new();
    let key = fingerprint(&bytes);
    let cell = LAST.get_or_init(|| std::sync::Mutex::new(None));
    let mut last = cell.lock().ok()?;
    if let Some((k, path)) = last.as_ref() {
        if *k == key && path.exists() {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    let dir = dirs::cache_dir()?
        .join(crate::platform::DIR_NAME)
        .join("covers");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("cover-{key:016x}.img"));
    if !path.exists() {
        let tmp = dir.join(format!(".cover-{key:016x}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &*bytes).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    *last = Some((key, path.clone()));
    Some(path.to_string_lossy().into_owned())
}

/// FNV-1a over the length and a sample of the bytes: enough to tell one
/// theme's cover from another's.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |b: u8| {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in (bytes.len() as u64).to_le_bytes() {
        feed(b);
    }
    let step = (bytes.len() / 4096).max(1);
    for b in bytes.iter().step_by(step) {
        feed(*b);
    }
    hash
}

// ---------------------------------------------------------------------------
// Recent sessions
// ---------------------------------------------------------------------------

/// The sessions the page lists: pinned ones first (newest pin first), then
/// the rest as they come (newest first), at most `rows`.
fn recent_order<'a>(
    sessions: &'a [AgentSessionInfo],
    pinned: &[String],
    rows: usize,
) -> Vec<&'a AgentSessionInfo> {
    let mut out: Vec<&AgentSessionInfo> = pinned
        .iter()
        .filter_map(|id| sessions.iter().find(|s| s.asid.0 == *id))
        .collect();
    out.extend(sessions.iter().filter(|s| !pinned.contains(&s.asid.0)));
    out.truncate(rows);
    out
}

fn recents(app: &App, sessions: &[AgentSessionInfo], rows: usize) -> Div {
    let mut block = div()
        .class("mq-start-recent")
        .w_full()
        .flex_col()
        .flex_shrink_0();
    block = block.child(
        div().class("mq-start-label").child(
            ui_text(app.strings.recent_label().to_uppercase())
                .size(11.0)
                .no_wrap(),
        ),
    );
    if sessions.is_empty() {
        return block.child(
            div()
                .class("mq-start-empty")
                .child(ui_text(app.strings.recents_empty()).size(13.0).no_wrap()),
        );
    }
    let waiting = app.bindings.attention.get();
    let pinned = app.config.get().general.pinned;
    let mut list = div().class("mq-start-rows").w_full().flex_col();
    for (index, session) in recent_order(sessions, &pinned, rows)
        .into_iter()
        .enumerate()
    {
        if index == 0 {
            list = list.child(div().class("mq-start-hairline").w_full().h(1.0));
        }
        let project = session
            .directory
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(super::composer::short_project)
            .unwrap_or_default();
        let is_pinned = pinned.contains(&session.asid.0);
        let when = muqun_gateway::relative_since(u128::from(session.updated_ms));
        let open = app.clone();
        let asid = session.asid.clone();
        // Everything but the hover actions opens the session.
        let main = div()
            .class("mq-start-main")
            .flex_1()
            .min_w(0.0)
            .h(ROW_HEIGHT)
            .flex_row()
            .items_center()
            .cursor_pointer()
            .child(mark_with_badge(
                &session.agent_id,
                session.status,
                waiting.iter().any(|p| p.asid == session.asid),
            ))
            .child(
                div()
                    .class("mq-start-row-title")
                    .flex_1()
                    .min_w(0.0)
                    .overflow_clip()
                    .child(
                        ui_text(title_of(app, session))
                            .size(13.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
            )
            .child(
                div()
                    .class("mq-start-row-project")
                    .w(150.0)
                    .flex_shrink_0()
                    .overflow_clip()
                    .child(mono_text(project).size(12.0).no_wrap().no_cursor()),
            )
            .child(
                div()
                    .class("mq-start-row-when")
                    .w(64.0)
                    .flex_shrink_0()
                    .flex_row()
                    .justify_end()
                    .child(mono_text(when).size(12.0).no_wrap().no_cursor()),
            )
            .on_click({
                let asid = asid.clone();
                move |_| open.open_session(&asid)
            });
        let open_label = {
            let open = app.clone();
            let asid = asid.clone();
            div()
                .class("mq-start-action")
                .cursor_pointer()
                .child(
                    ui_text(app.strings.open_label())
                        .size(12.0)
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| open.open_session(&asid))
        };
        let pin = {
            let pin_app = app.clone();
            let asid = asid.0.clone();
            div()
                .class("mq-start-action")
                .cursor_pointer()
                .child(
                    ui_text(if is_pinned {
                        app.strings.unpin_label()
                    } else {
                        app.strings.pin_label()
                    })
                    .size(12.0)
                    .no_wrap()
                    .no_cursor(),
                )
                .on_click(move |_| {
                    pin_app.send(Command::Edit(ConfigEdit::TogglePin(asid.clone())));
                })
        };
        let mut row = div()
            .class("mq-start-row")
            .w_full()
            .h(ROW_HEIGHT)
            .flex_row()
            .items_center()
            .child(main)
            .child(
                // Shown on hover only (the row's class reveals it); the
                // space is kept so the row does not move.
                div()
                    .class("mq-start-actions")
                    .w(ACTIONS_WIDTH)
                    .flex_shrink_0()
                    .flex_row()
                    .justify_end()
                    .child(open_label)
                    .child(pin),
            );
        if is_pinned {
            row = row.class("mq-start-row--pinned");
        }
        list = list.child(row);
        list = list.child(div().class("mq-start-hairline").w_full().h(1.0));
    }
    block.child(list)
}

/// The room the hover actions keep at a row's end.
const ACTIONS_WIDTH: f32 = 96.0;

/// The agent's 16 px brand mark with a 6 px status badge at its bottom-right
/// corner, ringed in the page's colour: mint while it works, amber when it
/// waits for you, red when it failed, none when idle (the sidebar's rule).
fn mark_with_badge(agent: &str, status: AgentSessionStatus, waiting: bool) -> Div {
    let class = if waiting {
        Some("mq-start-badge--waiting")
    } else {
        match status {
            AgentSessionStatus::Busy | AgentSessionStatus::Retry => Some("mq-start-badge--busy"),
            AgentSessionStatus::Failed => Some("mq-start-badge--failed"),
            _ => None,
        }
    };
    let mut cell = div()
        .class("mq-start-mark")
        .relative()
        .w(16.0)
        .h(16.0)
        .flex_shrink_0()
        .child(agents::mark(agent, 16.0));
    if let Some(class) = class {
        cell = cell.child(
            div()
                .class("mq-start-badge")
                .class(class)
                .absolute()
                .right(-3.0)
                .bottom(-3.0)
                .w(BADGE)
                .h(BADGE)
                .rounded(BADGE / 2.0),
        );
    }
    cell
}

/// The badge: 6 px of colour inside a 1.5 px ring.
const BADGE: f32 = 9.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn session(asid: &str) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid, "title": asid, "status": "idle", "updated_ms": 1
        }))
        .unwrap()
    }

    #[test]
    fn first_run_needs_known_agents_and_no_history() {
        let fresh = General::default();
        assert!(!first_run(false, &[], &fresh), "undecided before discovery");
        assert!(first_run(true, &[], &fresh));
        assert!(!first_run(true, &[session("a")], &fresh));
        let used = General {
            last_agent: Some("codex".into()),
            ..General::default()
        };
        assert!(!first_run(true, &[], &used));
    }

    #[test]
    fn pinned_sessions_lead_the_recent_list() {
        let all = [session("a"), session("b"), session("c"), session("d")];
        let order = |pinned: &[&str], rows| {
            let pinned: Vec<String> = pinned.iter().map(|p| p.to_string()).collect();
            recent_order(&all, &pinned, rows)
                .iter()
                .map(|s| s.asid.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&[], 3), ["a", "b", "c"]);
        assert_eq!(order(&["c", "gone", "a"], 4), ["c", "a", "b", "d"]);
        assert_eq!(order(&["d"], 2), ["d", "a"]);
    }

    #[test]
    fn the_cover_fingerprint_tells_covers_apart() {
        assert_ne!(fingerprint(b"one cover"), fingerprint(b"another"));
        assert_eq!(fingerprint(b"same"), fingerprint(b"same"));
    }
}
