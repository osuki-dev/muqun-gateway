//! A QA harness for the timeline, driven by environment variables so a
//! headed run needs no synthetic input (no focus to steal, nothing typed
//! into another window). Off unless one of the variables is set.
//!
//! * `MUQUN_QA_SYNTH=N` -- show a synthetic session of N mixed items, fed
//!   through the same inbox, drain and list as a real one.
//! * `MUQUN_QA_OPEN=<asid>` -- open that session.
//! * `MUQUN_QA_PROMPT=<text>` with `MUQUN_QA_CWD=<dir>` -- start an OpenCode
//!   session in the directory and send the prompt once it is open.
//! * `MUQUN_QA_SCROLL=1` -- once rows are shown, scroll top to bottom and
//!   back at three speeds, counting frames where the viewport shows rows
//!   the window has not built (blank gaps).
//! * `MUQUN_QA_TOUR=1` -- once rows are shown: hold at the bottom, jump
//!   to the top, the middle, then fling down at 1600 px a frame, holding
//!   four seconds at each stop and logging `qa: tour at <stop>` (for
//!   screenshots).
//! * `MUQUN_QA_SCHEME=light|dark` -- start in that colour scheme.
//! * `MUQUN_QA_TOGGLE_AFTER=<secs>` -- flip the scheme once, that long
//!   after start, the way the top bar's toggle does (`App::apply_scheme`: live
//!   recolouring, and the choice is remembered in the configuration).
//! * `MUQUN_QA_THEMES=<secs>` -- open the themes panel that long after
//!   start (what Ctrl+Shift+T does).
//! * `MUQUN_QA_THEME=<id>@<secs>[,<id>@<secs>…]` -- apply theme `<id>` that
//!   long after start, in the scheme on screen, the way the panel's Apply
//!   does; several apply in turn (e.g. a theme and back).
//!
//! Frame times come from Blinc's own `blinc_app::frame_timing` trace.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blinc_theme::{ColorScheme, ThemeState};
use muqun_gateway::agents::ports::AgentSessionSnapshot;
use muqun_gateway::agents::{
    AgentPart, AgentSessionId, TimelineItem, TimelineRole, TodoItem, ToolCall, ToolCallStatus,
    ToolTime,
};

use super::controller::Controller;
use crate::model::commands::Command;
use crate::model::inbox::Update;
use crate::ui::App;

const SYNTH_ASID: &str = "qa-synthetic";

#[derive(Default)]
struct Script {
    started: Option<Instant>,
    prompt: Option<(String, Option<String>)>,
    scroll: Option<Scroll>,
    want_scroll: bool,
    toggle_after: Option<Duration>,
    themes_after: Option<Duration>,
    /// Themes still to apply, soonest first.
    theme_after: Vec<(String, Duration)>,
    tour: Option<Tour>,
    done: bool,
}

#[derive(Default)]
struct Tour {
    stop: usize,
    since: Option<Instant>,
    fling: u32,
    logged: bool,
}

const TOUR_HOLD: Duration = Duration::from_secs(4);

#[derive(Default)]
struct Scroll {
    phase: usize,
    frames: u64,
    gaps: u64,
    phase_gaps: u64,
    phase_frames: u64,
    shown_at: Option<Instant>,
}

/// A phase ends at the end of the list or after this many frames.
const PHASE_FRAMES: u64 = 360;

/// (pixels per frame, direction: true = down).
const PHASES: [(f32, bool); 6] = [
    (60.0, false),
    (60.0, true),
    (400.0, false),
    (400.0, true),
    (1600.0, false),
    (1600.0, true),
];

pub fn start(app: &App) {
    let synth = std::env::var("MUQUN_QA_SYNTH")
        .ok()
        .and_then(|n| n.parse::<usize>().ok());
    let open = std::env::var("MUQUN_QA_OPEN").ok();
    let prompt = std::env::var("MUQUN_QA_PROMPT").ok();
    let want_scroll = std::env::var("MUQUN_QA_SCROLL").is_ok();
    match std::env::var("MUQUN_QA_SCHEME").as_deref() {
        Ok("light") => ThemeState::get().set_scheme(ColorScheme::Light),
        Ok("dark") => ThemeState::get().set_scheme(ColorScheme::Dark),
        _ => {}
    }
    let toggle_after = std::env::var("MUQUN_QA_TOGGLE_AFTER")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    let themes_after = std::env::var("MUQUN_QA_THEMES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    let mut theme_after: Vec<(String, Duration)> = std::env::var("MUQUN_QA_THEME")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| {
            let (id, secs) = v.trim().rsplit_once('@')?;
            Some((id.to_string(), Duration::from_secs(secs.parse().ok()?)))
        })
        .collect();
    theme_after.sort_by_key(|(_, after)| *after);
    let tour = std::env::var_os("MUQUN_QA_TOUR").map(|_| Tour::default());
    if synth.is_none()
        && open.is_none()
        && prompt.is_none()
        && !want_scroll
        && toggle_after.is_none()
        && themes_after.is_none()
        && theme_after.is_empty()
        && tour.is_none()
    {
        return;
    }
    tracing::info!(
        ?synth,
        ?open,
        prompt = prompt.is_some(),
        want_scroll,
        "qa harness on"
    );
    if let Some(n) = synth {
        app.drain.select(SYNTH_ASID);
        app.drain.inject(Update::Snapshot(Box::new(synthetic(n))));
    }
    if let Some(asid) = open {
        app.open_session(&AgentSessionId(asid));
    }
    let script = Arc::new(Mutex::new(Script {
        prompt: prompt.map(|p| (p, None)),
        want_scroll,
        toggle_after,
        themes_after,
        theme_after,
        tour,
        ..Script::default()
    }));
    if let Some((_, slot)) = script.lock().unwrap().prompt.as_mut() {
        let cwd =
            std::env::var("MUQUN_QA_CWD").unwrap_or_else(|_| "/tmp/opencode/desktop-qa".into());
        app.send(Command::CreateSession {
            agent: crate::hub::OPENCODE.into(),
            cwd: cwd.into(),
        });
        *slot = Some(String::new());
    }
    let Some(scheduler) = blinc_animation::try_get_scheduler() else {
        return;
    };
    let app = app.clone();
    let id_slot: Arc<Mutex<Option<blinc_animation::TickCallbackId>>> = Arc::new(Mutex::new(None));
    let slot = id_slot.clone();
    let id = scheduler.register_tick_callback(move |_dt| {
        let mut s = script.lock().unwrap();
        s.started.get_or_insert_with(Instant::now);
        tick(&app, &mut s);
        if s.done {
            if let (Some(id), Some(sch)) = (
                slot.lock().unwrap().take(),
                blinc_animation::try_get_scheduler(),
            ) {
                sch.remove_tick_callback(id);
                tracing::info!("qa harness finished; frames now stop when idle");
            }
        }
    });
    *id_slot.lock().unwrap() = id;
}

fn tick(app: &App, s: &mut Script) {
    // Send the prompt once the new session is the one shown.
    if let Some((text, Some(_))) = s.prompt.as_ref() {
        if let Some(header) = app.bindings.header.get() {
            if header.asid.0 != SYNTH_ASID && header.directory.is_some() {
                tracing::info!(asid = %header.asid.0, "qa: sending the prompt");
                app.send(Command::SendPrompt {
                    asid: header.asid.clone(),
                    text: text.clone(),
                });
                s.prompt = None;
            }
        }
    }
    if s.want_scroll && s.scroll.is_none() {
        let ctl = Controller::get(app);
        if ctl.row_count() > 0 {
            s.scroll = Some(Scroll {
                shown_at: Some(Instant::now()),
                ..Scroll::default()
            });
        }
    }
    if let Some(scroll) = s.scroll.as_mut() {
        if scroll_step(app, scroll) {
            tracing::info!(
                target: "muqun::qa",
                frames = scroll.frames,
                gap_frames = scroll.gaps,
                "qa: scroll run done"
            );
            s.scroll = None;
            s.want_scroll = false;
        }
    }
    if let Some(after) = s.toggle_after {
        if s.started.is_some_and(|t| t.elapsed() >= after) {
            let next = ThemeState::get().scheme().toggle();
            tracing::info!(target: "muqun::qa", ?next, "qa: switching the colour scheme");
            app.apply_scheme(match next {
                ColorScheme::Light => crate::config::Scheme::Light,
                ColorScheme::Dark => crate::config::Scheme::Dark,
            });
            s.toggle_after = None;
        }
    }
    if s.themes_after
        .is_some_and(|after| s.started.is_some_and(|t| t.elapsed() >= after))
    {
        tracing::info!(target: "muqun::qa", "qa: opening the themes panel");
        crate::ui::themes::toggle();
        s.themes_after = None;
    }
    if s.theme_after
        .first()
        .is_some_and(|(_, after)| s.started.is_some_and(|t| t.elapsed() >= *after))
    {
        let (id, _) = s.theme_after.remove(0);
        let scheme = match ThemeState::get().scheme() {
            ColorScheme::Light => crate::config::Scheme::Light,
            ColorScheme::Dark => crate::config::Scheme::Dark,
        };
        tracing::info!(target: "muqun::qa", %id, ?scheme, "qa: applying a theme");
        app.send(Command::ThemeApply(id, scheme));
    }
    if let Some(tour) = s.tour.as_mut() {
        if tour_step(app, tour) {
            s.tour = None;
        }
    }
    s.done = s.prompt.is_none()
        && !s.want_scroll
        && s.toggle_after.is_none()
        && s.themes_after.is_none()
        && s.theme_after.is_empty()
        && s.tour.is_none();
}

/// One frame of the scroll run. True when finished.
fn scroll_step(app: &App, s: &mut Scroll) -> bool {
    // Let the first layout settle.
    if s.shown_at
        .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
    {
        return false;
    }
    let ctl = Controller::get(app);
    let (window, visible) = ctl.coverage();
    s.frames += 1;
    s.phase_frames += 1;
    if !visible.is_empty() && (visible.start < window.start || visible.end > window.end) {
        s.gaps += 1;
        s.phase_gaps += 1;
    }
    let Some(&(speed, down)) = PHASES.get(s.phase) else {
        return true;
    };
    let scroll = &app.bindings.timeline_scroll;
    let (top, max) = {
        let p = ctl.physics();
        let p = p.lock().unwrap();
        (-p.offset_y, (p.content_height - p.viewport_height).max(0.0))
    };
    let at_end = if down { top >= max - 1.0 } else { top <= 0.5 };
    if (at_end && s.phase_frames > 2) || s.phase_frames >= PHASE_FRAMES {
        tracing::info!(
            target: "muqun::qa",
            phase = s.phase,
            speed_px_per_frame = speed,
            down,
            frames = s.phase_frames,
            gap_frames = s.phase_gaps,
            rows = ctl.row_count(),
            "qa: scroll phase done"
        );
        s.phase += 1;
        s.phase_frames = 0;
        s.phase_gaps = 0;
        return s.phase >= PHASES.len();
    }
    // A wheel's delta: negative scrolls down.
    scroll.scroll_by(0.0, if down { -speed } else { speed });
    // A programmatic scroll does not pass the wheel handler: make sure the
    // controller runs next frame (as it would after a scrollbar drag).
    app.drain.wake();
    false
}

/// One frame of the tour. True when finished.
fn tour_step(app: &App, t: &mut Tour) -> bool {
    let ctl = Controller::get(app);
    if ctl.row_count() == 0 {
        return false;
    }
    let since = *t.since.get_or_insert_with(Instant::now);
    let held = since.elapsed();
    let max = {
        let p = ctl.physics();
        let p = p.lock().unwrap();
        (p.content_height - p.viewport_height).max(0.0)
    };
    let scroll = &app.bindings.timeline_scroll;
    let stops = ["bottom", "top", "middle", "after-fling"];
    // Stop 3 flings for 60 frames before it holds.
    if t.stop == 3 && t.fling < 60 {
        scroll.scroll_by(0.0, -1600.0);
        t.fling += 1;
        t.since = Some(Instant::now());
        app.drain.wake();
        return false;
    }
    if held < TOUR_HOLD {
        if !t.logged {
            t.logged = true;
            tracing::info!(target: "muqun::qa", stop = stops[t.stop], "qa: tour at");
        }
        app.drain.wake();
        return false;
    }
    t.stop += 1;
    t.logged = false;
    t.since = Some(Instant::now());
    match t.stop {
        1 => scroll.set_scroll_offset(0.0, 0.0),
        2 => scroll.set_scroll_offset(0.0, max / 2.0),
        3 => scroll.set_scroll_offset(0.0, max / 4.0),
        _ => return true,
    }
    app.drain.wake();
    false
}

fn item(
    n: usize,
    message: &str,
    ordinal: u64,
    role: TimelineRole,
    part: AgentPart,
) -> TimelineItem {
    TimelineItem {
        id: format!("{message}:{ordinal}"),
        message_id: message.into(),
        role,
        part,
        seq: n as u64,
        updated_ms: 1_000 + n as u64 * 4_000,
        ordinal,
        attachments: None,
        image_assets: None,
        generation: None,
    }
}

const PROSE: &str = "The windowed list builds only the rows near the viewport; everything above and below is a spacer as tall as the model says. Measured heights replace estimates as rows are laid out, and the anchor keeps the reader's place while they change.";

/// A session of `n` items cycling through the part kinds.
pub fn synthetic(n: usize) -> AgentSessionSnapshot {
    let mut timeline = Vec::with_capacity(n);
    let mut message = 0usize;
    let mut ordinal = 0u64;
    for i in 0..n {
        if i % 6 == 0 {
            message += 1;
            ordinal = 0;
        }
        let msg = format!("msg_{message:06}");
        let k = i % 6;
        let (role, part) = match k {
            0 => (
                TimelineRole::User,
                AgentPart::Text {
                    text: format!("Question {message}: how does the timeline stay fast at {n} items?"),
                },
            ),
            1 => (
                TimelineRole::Assistant,
                AgentPart::Reasoning {
                    text: "Thinking about windowing.".into(),
                    duration_ms: Some(1200 + (i as u64 % 7) * 300),
                },
            ),
            2 => (
                TimelineRole::Assistant,
                AgentPart::Text {
                    text: format!(
                        "## Answer {message}\n\n{}\n\n- one\n- two\n\n```rust\nfn row(i: usize) -> usize {{\n    i * 2 // item {i}\n}}\n```\n\n{}",
                        PROSE.repeat(1 + i % 3),
                        PROSE
                    ),
                },
            ),
            3 => (
                TimelineRole::Assistant,
                AgentPart::Tool(ToolCall {
                    id: format!("call_{i}"),
                    name: "bash".into(),
                    title: Some(format!("cargo test -p muqun-desktop # {i}")),
                    input: serde_json::json!({"command": "cargo test -p muqun-desktop"}),
                    output: Some(serde_json::Value::String("test result: ok. 42 passed".into())),
                    content: None,
                    metadata: None,
                    state: ToolCallStatus::Completed,
                    status: ToolCallStatus::Completed,
                    error: None,
                    child_session_id: None,
                    background: false,
                    input_partial: None,
                    truncated: false,
                    time: ToolTime {
                        created: Some(0),
                        ran: Some(1000),
                        completed: Some(3400),
                    },
                }),
            ),
            4 => (
                TimelineRole::Assistant,
                AgentPart::Diff {
                    file: format!("src/file_{i}.rs"),
                    diff: format!(
                        "--- a/src/file_{i}.rs\n+++ b/src/file_{i}.rs\n@@ -1,16 +1,16 @@\n fn a() {{}}\n-fn b() {{}}\n+fn b() -> u8 {{ 1 }}\n fn c() {{}}\n{}",
                        (0..18)
                            .map(|k| if k % 2 == 0 {
                                format!("-let x{k} = {k};")
                            } else {
                                format!("+let x{k} = {};", k * 2)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                },
            ),
            _ => (
                TimelineRole::Assistant,
                AgentPart::Todo {
                    items: vec![
                        TodoItem {
                            text: "Window the list".into(),
                            done: true,
                        },
                        TodoItem {
                            text: "Measure frame time".into(),
                            done: false,
                        },
                    ],
                },
            ),
        };
        timeline.push(item(i, &msg, ordinal, role, part));
        ordinal += 1;
    }
    let info = serde_json::from_value(serde_json::json!({
        "asid": SYNTH_ASID,
        "agent_id": "opencode",
        "title": format!("QA: {n} synthetic items"),
        "status": "idle",
        "updated_ms": 1,
        "directory": "/tmp/opencode/desktop-qa"
    }))
    .expect("static session info");
    AgentSessionSnapshot {
        info,
        timeline,
        permissions: vec![],
        forms: vec![],
        inbox: vec![],
        seq: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::view::{Changes, ViewState};

    #[test]
    fn a_synthetic_session_of_ten_thousand_items_loads_in_order() {
        let started = Instant::now();
        let snapshot = synthetic(10_000);
        let mut view = ViewState::default();
        view.select(SYNTH_ASID);
        let mut changes = Changes::default();
        view.apply(Update::Snapshot(Box::new(snapshot)), &mut changes);
        let order = view.order();
        eprintln!("10k synthetic snapshot applied in {:?}", started.elapsed());
        assert_eq!(order.ids.len(), 10_000);
        assert_eq!(order.shapes.len(), 10_000);
        assert!(changes.opened);
    }
}
