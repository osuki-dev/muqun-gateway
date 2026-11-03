//! The start page (architecture §6.0‴ B): "What are we doing?", recent
//! sessions above, the composer card pinned at the foot. Nothing else -- no
//! statistics, no launch cards, no agents panel. Home *is* the composer: the
//! caret is in it on entry, and Enter starts a session with every default.
//!
//! No colour or family literal lives here: colours come from theme variables
//! in `styles.css` (`/* start */`), text families from `ui::text`.

use std::path::PathBuf;
use std::sync::OnceLock;

use blinc_layout::prelude::*;
use muqun_gateway::agents::AgentSessionInfo;
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;

use super::agents::Standing;
use super::composer;
use super::sessions::title_of;
use super::text::{display_text, mono_text, ui_text};
use super::App;
use crate::config::General;

/// The title's size.
const TITLE_SIZE: f32 = 28.0;
/// Recent rows at most, and a row's height.
const RECENT_ROWS: usize = 8;
const ROW_HEIGHT: f32 = 44.0;
/// Opacity of the theme's cover behind the page (dark / light).
const COVER_DARK: f32 = 0.22;
const COVER_LIGHT: f32 = 0.16;
/// The cover fills this much of the page's width, anchored at the upper right.
const COVER_WIDTH_SHARE: f32 = 0.72;
const COVER_HEIGHT_SHARE: f32 = 0.88;
const TITLE_BLOCK: f32 = 58.0;
const SUBTITLE_BLOCK: f32 = 28.0;
const CARD_ESTIMATE: f32 = 150.0;
const LABEL_BLOCK: f32 = 28.0;
/// Foot padding under the composer card (matches the session slot).
const BOTTOM_PAD: f32 = 16.0;
const TOP_PAD: f32 = 28.0;

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

/// Where the start page puts its composer card: centred, pinned to the foot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartLayout {
    pub card_width: f32,
    /// Estimated card height used for popup placement.
    pub card_height: f32,
}

pub fn start_layout(main_width: f32) -> StartLayout {
    StartLayout {
        card_width: START_CARD_MAX.min((main_width - 80.0).max(320.0)),
        card_height: CARD_ESTIMATE,
    }
}

/// The card's top-left corner in the window, and its width, for the list
/// that hangs from it (the registry's element bounds are parent-local).
pub fn start_card_origin(app: &App) -> (f32, f32, f32) {
    let (vw, vh) = blinc_core::BlincContextState::get().viewport_size();
    let left = super::shell::side_width(app, vw);
    let right = super::shell::right_width(app);
    let main = (vw - left - right).max(0.0);
    let layout = start_layout(main);
    (
        left + (main - layout.card_width) / 2.0,
        vh - BOTTOM_PAD - layout.card_height,
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
    let pins = super::sessions::pins_signal_id(app);
    let page = stateful_with_key::<NoState>("mq-start")
        .deps([
            agents_state.signal_id(),
            sessions.signal_id(),
            theme_epoch.signal_id(),
            pins,
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

    let layout = start_layout(width);
    let card_width = layout.card_width;
    let head = TOP_PAD
        + TITLE_BLOCK
        + if subtitle.is_some() {
            SUBTITLE_BLOCK
        } else {
            0.0
        }
        + LABEL_BLOCK;
    let foot = layout.card_height + BOTTOM_PAD + 24.0;
    let fit = ((height - head - foot) / ROW_HEIGHT).floor().max(0.0) as usize;
    let rows = RECENT_ROWS.min(fit);

    let mut upper = div()
        .class("mq-start-upper")
        .w(card_width)
        .flex_1()
        .min_h(0.0)
        .flex_col()
        .child(div().h(TOP_PAD).flex_shrink_0())
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
        upper = upper.child(
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
    upper = upper.child(
        div()
            .w_full()
            .flex_1()
            .min_h(0.0)
            .child(scroll().w_full().h_full().child(recents(app, &sessions, rows))),
    );

    let column = div()
        .class("mq-start-column")
        .w(card_width)
        .h_full()
        .flex_col()
        .child(upper)
        .child(
            div()
                .class("mq-start-card")
                .w_full()
                .flex_shrink_0()
                .child(composer::start_card(app, card_width)),
        )
        .child(div().h(BOTTOM_PAD).flex_shrink_0());

    let mut page = div()
        .class("mq-start-page")
        .absolute()
        .inset(0.0)
        .flex_col()
        .items_center()
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
    let pinned = (*super::sessions::side_pins(app)).clone();
    let mut list = div().class("mq-start-rows").w_full().flex_col();
    for session in recent_order(sessions, &pinned, rows) {
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
            .child(super::sessions::badged_mark(
                &session.agent_id,
                16.0,
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
                    super::sessions::toggle_session_pin(&pin_app, &asid);
                })
        };
        let mut row = div()
            .class("mq-start-row")
            .w_full()
            .h(ROW_HEIGHT)
            .relative()
            .flex_row()
            .items_center()
            .child(main)
            .child(
                // Overlay on hover so the when/project columns keep their place.
                div()
                    .class("mq-start-actions")
                    .absolute()
                    .right(0.0)
                    .h(ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .child(open_label)
                    .child(pin),
            );
        if is_pinned {
            row = row.class("mq-start-row--pinned");
        }
        list = list.child(row);
    }
    block.child(list)
}

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
