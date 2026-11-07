//! The terminal plane: the Herdr (and tmux) workspaces, tabs and panes the
//! gateway serves, a live view of one pane's screen, and typing into it --
//! what the App's terminal workspace does, through the gateway this desktop
//! is attached to (`hub::TerminalCall`).
//!
//! Nothing here focuses, closes, creates or splits a pane: the desktop only
//! reads the owner's terminals and types where the owner asked it to.
//!
//! The core side ([`run`]) answers [`TerminalCommand`]s; one watcher at a
//! time re-reads the open pane's screen every [`SCREEN_EVERY`] and the list
//! every [`LIST_EVERY`] while the page is open, and only pushes what changed.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::hub::{SessionHub, TerminalCall};
use crate::model::inbox::{Inbox, Update};

/// How often the open pane's screen is read.
pub const SCREEN_EVERY: Duration = Duration::from_millis(500);
/// How often the workspace list is read while the page is open.
pub const LIST_EVERY: Duration = Duration::from_secs(3);
/// Rows asked for: more than any real screen has.
const SCREEN_LINES: u32 = 200;

/// A pane, by the terminal session that has it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneRef {
    pub session: String,
    pub pane: String,
}

#[derive(Debug, Clone)]
pub enum TerminalCommand {
    /// The page opened or closed, or another pane was chosen: keep the list
    /// (and `pane`'s screen) fresh while `open`.
    Watch { open: bool, pane: Option<PaneRef> },
    /// Type `text` into the pane, then Enter when `submit` (the App's
    /// composer: paste, then the `enter` key).
    SendText {
        pane: PaneRef,
        text: String,
        submit: bool,
    },
    /// Press keys in the pane (`enter`, `esc`, `ctrl+c`, `up`, …).
    SendKeys { pane: PaneRef, keys: Vec<String> },
}

#[derive(Debug, Clone)]
pub enum TerminalUpdate {
    List(Arc<TerminalsView>),
    Screen {
        pane: PaneRef,
        screen: Arc<Screen>,
    },
    /// A read or a send failed; `pane` when it was about one pane.
    Failed {
        pane: Option<PaneRef>,
        message: String,
    },
}

/// Everything the page lists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalsView {
    pub sessions: Vec<SessionView>,
    /// The gateway could not be asked at all (no gateway service).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionView {
    pub id: String,
    pub label: String,
    pub connected: bool,
    pub workspaces: Vec<Workspace>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub label: String,
    pub number: Option<u64>,
    pub focused: bool,
    pub status: String,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tab {
    pub id: String,
    pub label: String,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pane {
    pub id: String,
    /// The agent running in it (`claude`, `codex`, …), when one is.
    pub agent: Option<String>,
    /// `working`, `blocked`, `idle`, `done` or `unknown`.
    pub status: String,
    /// What runs in the foreground (`bash`, `nvim`, …).
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// The terminal's own title, escape sequences stripped.
    pub title: Option<String>,
    pub focused: bool,
}

impl Pane {
    /// What the pane is, in a word: the agent, else the foreground program.
    pub fn name(&self) -> String {
        self.agent
            .clone()
            .or_else(|| self.command.clone())
            .unwrap_or_else(|| self.id.clone())
    }
}

impl TerminalsView {
    pub fn pane(&self, at: &PaneRef) -> Option<&Pane> {
        self.sessions
            .iter()
            .filter(|s| s.id == at.session)
            .flat_map(|s| &s.workspaces)
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .find(|p| p.id == at.pane)
    }

    /// Where `at` lives: its workspace and tab labels.
    pub fn place(&self, at: &PaneRef) -> Option<(&Workspace, &Tab)> {
        let session = self.sessions.iter().find(|s| s.id == at.session)?;
        session.workspaces.iter().find_map(|w| {
            w.tabs
                .iter()
                .find(|t| t.panes.iter().any(|p| p.id == at.pane))
                .map(|t| (w, t))
        })
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The `result` of an envelope, or the value itself.
fn result(v: Value) -> Value {
    match v {
        Value::Object(mut map) if map.contains_key("result") => {
            map.remove("result").unwrap_or(Value::Null)
        }
        other => other,
    }
}

/// One session's snapshot (`GET /api/sessions/{id}/snapshot`) as the page's
/// tree, workspaces in Herdr's order and panes with their agents.
pub fn parse_snapshot(session: &mut SessionView, snapshot: Value) {
    let snapshot = result(snapshot);
    let list = |key: &str| {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let agents = list("agents");
    let agent_of = |pane: &str| {
        agents
            .iter()
            .find(|a| a.get("pane_id").and_then(Value::as_str) == Some(pane))
    };
    let panes: Vec<(String, Pane)> = list("panes")
        .iter()
        .filter_map(|p| {
            let id = text(p, "pane_id")?;
            let agent = agent_of(&id);
            Some((
                text(p, "tab_id").unwrap_or_default(),
                Pane {
                    agent: agent
                        .and_then(|a| text(a, "display_agent").or_else(|| text(a, "agent")))
                        .or_else(|| text(p, "agent")),
                    status: agent
                        .and_then(|a| text(a, "agent_status"))
                        .or_else(|| text(p, "agent_status"))
                        .unwrap_or_else(|| "unknown".into()),
                    command: text(p, "foreground_command"),
                    cwd: text(p, "foreground_cwd").or_else(|| text(p, "cwd")),
                    title: text(p, "terminal_title_stripped").or_else(|| text(p, "label")),
                    focused: p.get("focused").and_then(Value::as_bool).unwrap_or(false),
                    id,
                },
            ))
        })
        .collect();
    let tabs: Vec<(String, Tab)> = list("tabs")
        .iter()
        .filter_map(|t| {
            let id = text(t, "tab_id")?;
            Some((
                text(t, "workspace_id").unwrap_or_default(),
                Tab {
                    label: text(t, "label").unwrap_or_else(|| id.clone()),
                    panes: panes
                        .iter()
                        .filter(|(tab, _)| *tab == id)
                        .map(|(_, p)| p.clone())
                        .collect(),
                    id,
                },
            ))
        })
        .collect();
    let mut workspaces: Vec<Workspace> = list("workspaces")
        .iter()
        .filter_map(|w| {
            let id = text(w, "workspace_id")?;
            Some(Workspace {
                label: text(w, "label").unwrap_or_else(|| id.clone()),
                number: w.get("number").and_then(Value::as_u64),
                focused: w.get("focused").and_then(Value::as_bool).unwrap_or(false),
                status: text(w, "agent_status").unwrap_or_else(|| "unknown".into()),
                tabs: tabs
                    .iter()
                    .filter(|(ws, _)| *ws == id)
                    .map(|(_, t)| t.clone())
                    .collect(),
                id,
            })
        })
        .collect();
    workspaces.sort_by_key(|w| w.number.unwrap_or(u64::MAX));
    session.workspaces = workspaces;
}

/// The pane's screen as drawn: text and its styled runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Screen {
    pub text: String,
    pub runs: Vec<(Range<usize>, Sgr)>,
}

/// The ANSI colours, by role: a terminal's 16 colours fold onto the theme's
/// semantic colours so a pane reads in every theme, light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Faint,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Bright,
}

/// One run's graphic rendition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sgr {
    pub ink: Option<Ink>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Sgr {
    fn plain(&self) -> bool {
        *self == Self::default()
    }
}

fn ink_of_ansi(n: u16) -> Option<Ink> {
    Some(match n % 8 {
        0 => Ink::Faint,
        1 => Ink::Red,
        2 => Ink::Green,
        3 => Ink::Yellow,
        4 => Ink::Blue,
        5 => Ink::Magenta,
        6 => Ink::Cyan,
        _ => Ink::Bright,
    })
}

/// The nearest role for an RGB colour (256-colour cube or true colour).
fn ink_of_rgb(r: u8, g: u8, b: u8) -> Option<Ink> {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max - min < 40 {
        // Grey: brighter than the middle reads as text, darker as faint.
        return (max < 128).then_some(Ink::Faint);
    }
    let high = |c: u8| c as u16 * 10 >= max as u16 * 6;
    Some(match (high(r), high(g), high(b)) {
        (true, false, false) => Ink::Red,
        (false, true, false) => Ink::Green,
        (true, true, false) => Ink::Yellow,
        (false, false, true) => Ink::Blue,
        (true, false, true) => Ink::Magenta,
        (false, true, true) => Ink::Cyan,
        _ => return None,
    })
}

fn ink_of_256(n: u16) -> Option<Ink> {
    match n {
        0..=15 => ink_of_ansi(n),
        16..=231 => {
            let n = n - 16;
            let level = |c: u16| if c == 0 { 0 } else { (55 + c * 40) as u8 };
            ink_of_rgb(level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        _ => (n < 244).then_some(Ink::Faint),
    }
}

fn apply_sgr(sgr: &mut Sgr, params: &str) {
    let codes: Vec<u16> = params
        .split([';', ':'])
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *sgr = Sgr::default(),
            1 => sgr.bold = true,
            2 => sgr.dim = true,
            3 => sgr.italic = true,
            4 => sgr.underline = true,
            22 => (sgr.bold, sgr.dim) = (false, false),
            23 => sgr.italic = false,
            24 => sgr.underline = false,
            c @ 30..=37 => sgr.ink = ink_of_ansi(c - 30),
            c @ 90..=97 => sgr.ink = ink_of_ansi(c - 90),
            39 => sgr.ink = None,
            38 => match codes.get(i + 1) {
                Some(5) => {
                    sgr.ink = codes.get(i + 2).and_then(|n| ink_of_256(*n));
                    i += 2;
                }
                Some(2) => {
                    let c = |k: usize| codes.get(i + k).copied().unwrap_or(0).min(255) as u8;
                    sgr.ink = ink_of_rgb(c(2), c(3), c(4));
                    i += 4;
                }
                _ => {}
            },
            // Backgrounds are not drawn; skip their arguments.
            48 | 58 => match codes.get(i + 1) {
                Some(5) => i += 2,
                Some(2) => i += 4,
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
}

/// Parse a pane's ANSI screen: SGR styling kept, every other control
/// sequence dropped, `\r\n` as `\n`, trailing blank rows trimmed.
pub fn parse_screen(raw: &str) -> Screen {
    let mut text = String::with_capacity(raw.len());
    let mut runs: Vec<(Range<usize>, Sgr)> = Vec::new();
    let mut sgr = Sgr::default();
    let mut run_start = 0;
    let mut chars = raw.chars().peekable();
    let flush = |text: &String, runs: &mut Vec<(Range<usize>, Sgr)>, start: usize, sgr: Sgr| {
        if !sgr.plain() && text.len() > start {
            runs.push((start..text.len(), sgr));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut end = None;
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            end = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if end == Some('m') {
                        flush(&text, &mut runs, run_start, sgr);
                        apply_sgr(&mut sgr, &params);
                        run_start = text.len();
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            '\n' | '\t' => text.push(c),
            c if c.is_control() => {}
            c => text.push(c),
        }
    }
    flush(&text, &mut runs, run_start, sgr);
    // Trailing blank rows: the screen below the prompt.
    let keep = text.trim_end().len();
    let keep = text[keep..].find('\n').map_or(text.len(), |at| keep + at);
    text.truncate(keep);
    for (range, _) in runs.iter_mut() {
        range.end = range.end.min(keep);
        range.start = range.start.min(range.end);
    }
    runs.retain(|(r, _)| !r.is_empty());
    // A screen read starts with the line feed that ended the row above it.
    let lead = text.len() - text.trim_start_matches('\n').len();
    if lead > 0 {
        text.drain(..lead);
        for (range, _) in runs.iter_mut() {
            range.start = range.start.saturating_sub(lead);
            range.end = range.end.saturating_sub(lead);
        }
        runs.retain(|(r, _)| !r.is_empty());
    }
    Screen { text, runs }
}

// ---- core side ---------------------------------------------------------------------

/// Which watcher is current; an older one ends at its next tick.
static WATCH: AtomicU64 = AtomicU64::new(0);
/// The page is on screen; the watcher stops reading as soon as it is not.
static VISIBLE: AtomicBool = AtomicBool::new(false);

/// Called by the UI as the page shows and hides.
pub fn set_visible(on: bool) {
    VISIBLE.store(on, Ordering::Release);
}

/// Answer one terminal command.
pub async fn run(hub: Arc<dyn SessionHub>, inbox: Arc<Inbox>, command: TerminalCommand) {
    let push = |update| inbox.push(Update::Terminals(Box::new(update)));
    match command {
        TerminalCommand::Watch { open, pane } => {
            let me = WATCH.fetch_add(1, Ordering::AcqRel) + 1;
            if open {
                watch(hub, inbox, pane, me).await;
            }
        }
        TerminalCommand::SendText { pane, text, submit } => {
            let sent = async {
                if !text.is_empty() {
                    hub.terminal(TerminalCall::SendText {
                        session: &pane.session,
                        pane: &pane.pane,
                        text: &text,
                    })
                    .await?;
                }
                if submit {
                    // The paste lands before Enter, as the App's composer waits for it.
                    hub.terminal(TerminalCall::SendKeys {
                        session: &pane.session,
                        pane: &pane.pane,
                        keys: &["enter".to_string()],
                    })
                    .await?;
                }
                Ok::<_, crate::hub::HubError>(())
            }
            .await;
            if let Err(err) = sent {
                push(TerminalUpdate::Failed {
                    pane: Some(pane),
                    message: err.to_string(),
                });
            }
        }
        TerminalCommand::SendKeys { pane, keys } => {
            let sent = hub
                .terminal(TerminalCall::SendKeys {
                    session: &pane.session,
                    pane: &pane.pane,
                    keys: &keys,
                })
                .await;
            if let Err(err) = sent {
                push(TerminalUpdate::Failed {
                    pane: Some(pane),
                    message: err.to_string(),
                });
            }
        }
    }
}

async fn watch(hub: Arc<dyn SessionHub>, inbox: Arc<Inbox>, pane: Option<PaneRef>, me: u64) {
    let push = |update| inbox.push(Update::Terminals(Box::new(update)));
    let mut last_list: Option<TerminalsView> = None;
    let mut last_raw: Option<String> = None;
    let mut since_list = LIST_EVERY;
    while WATCH.load(Ordering::Acquire) == me && VISIBLE.load(Ordering::Acquire) {
        if since_list >= LIST_EVERY {
            since_list = Duration::ZERO;
            let view = list(&*hub).await;
            if last_list.as_ref() != Some(&view) {
                last_list = Some(view.clone());
                push(TerminalUpdate::List(Arc::new(view)));
            }
        }
        if let Some(at) = &pane {
            let read = hub
                .terminal(TerminalCall::Screen {
                    session: &at.session,
                    pane: &at.pane,
                    lines: SCREEN_LINES,
                })
                .await;
            if WATCH.load(Ordering::Acquire) != me {
                return;
            }
            match read {
                Ok(value) => {
                    let raw = screen_text(value);
                    if last_raw.as_deref() != Some(raw.as_str()) {
                        let screen = parse_screen(&raw);
                        last_raw = Some(raw);
                        push(TerminalUpdate::Screen {
                            pane: at.clone(),
                            screen: Arc::new(screen),
                        });
                    }
                }
                Err(err) => {
                    last_raw = None;
                    push(TerminalUpdate::Failed {
                        pane: Some(at.clone()),
                        message: err.to_string(),
                    });
                }
            }
        }
        tokio::time::sleep(SCREEN_EVERY).await;
        since_list += SCREEN_EVERY;
    }
}

/// `result.read.output` of a pane read.
fn screen_text(value: Value) -> String {
    let value = result(value);
    let read = value.get("read").unwrap_or(&value);
    read.get("output")
        .or_else(|| read.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

async fn list(hub: &dyn SessionHub) -> TerminalsView {
    let sessions = match hub.terminal(TerminalCall::Sessions).await {
        Ok(value) => value,
        Err(err) => {
            return TerminalsView {
                sessions: Vec::new(),
                error: Some(err.to_string()),
            }
        }
    };
    let mut out = Vec::new();
    for s in sessions
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = text(s, "id") else { continue };
        let mut view = SessionView {
            label: text(s, "label").unwrap_or_else(|| id.clone()),
            connected: s.get("connected").and_then(Value::as_bool).unwrap_or(true),
            id,
            ..SessionView::default()
        };
        if view.connected {
            match hub
                .terminal(TerminalCall::Snapshot { session: &view.id })
                .await
            {
                Ok(snapshot) => parse_snapshot(&mut view, snapshot),
                Err(err) => view.error = Some(err.to_string()),
            }
        }
        out.push(view);
    }
    TerminalsView {
        sessions: out,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_screen_keeps_its_colours_and_drops_other_controls() {
        let raw = "\r\n\u{1b}[0m\u{1b}[1m\u{1b}[38;5;6m~/w\u{1b}[0m \u{1b}]0;title\u{7}❯ ls\r\n\
                   \u{1b}[38;5;1mred\u{1b}[0m plain \u{1b}[1;32mok\u{1b}[0m\u{1b}[K\r\n\r\n\r\n";
        let screen = parse_screen(raw);
        assert_eq!(screen.text, "~/w ❯ ls\nred plain ok");
        let styled: Vec<(&str, Sgr)> = screen
            .runs
            .iter()
            .map(|(r, s)| (&screen.text[r.clone()], *s))
            .collect();
        let bold = |ink| Sgr {
            ink: Some(ink),
            bold: true,
            ..Sgr::default()
        };
        assert_eq!(
            styled,
            vec![
                ("~/w", bold(Ink::Cyan)),
                (
                    "red",
                    Sgr {
                        ink: Some(Ink::Red),
                        ..Sgr::default()
                    }
                ),
                ("ok", bold(Ink::Green)),
            ]
        );
    }

    #[test]
    fn a_snapshot_becomes_workspaces_tabs_and_panes_with_their_agents() {
        let snapshot = json!({"result": {
            "workspaces": [
                {"workspace_id": "w2", "label": "two", "number": 2, "agent_status": "idle"},
                {"workspace_id": "w1", "label": "one", "number": 1, "agent_status": "working"},
            ],
            "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "1"}],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "foreground_command": "bash", "cwd": "/w"},
                {"pane_id": "w1:p2", "tab_id": "w1:t1", "agent": null},
            ],
            "agents": [{"pane_id": "w1:p2", "display_agent": "claude", "agent_status": "working"}],
        }});
        let mut session = SessionView::default();
        parse_snapshot(&mut session, snapshot);
        let ids: Vec<&str> = session.workspaces.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w1", "w2"]);
        let panes = &session.workspaces[0].tabs[0].panes;
        assert_eq!(panes[0].name(), "bash");
        assert_eq!(panes[0].status, "unknown");
        assert_eq!(panes[1].agent.as_deref(), Some("claude"));
        assert_eq!(panes[1].status, "working");
    }
}
