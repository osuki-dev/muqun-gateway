//! The sidebar (architecture §6.1b): New session, Home, agent filter chips,
//! every agent's sessions grouped by day, and the Agents panel.

use blinc_cn::prelude::{button, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use muqun_gateway::agents::AgentSessionInfo;
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::untitled_placeholder;

use super::agents::{self, Standing};
use super::text::ui_text;
use super::App;
use crate::config::{ConfigEdit, Grouping};
use crate::model::commands::Command;
use crate::model::view::{local_day, row_state, ListedSession, SessionRow};

/// Rows a sidebar group shows before "Show more". Every row is a few layout
/// nodes, and Blinc's subtree restyle walks the whole tree once per container
/// it patches, so the list is kept to what a reader scans; "Show more" opens a
/// group further, [`GROUP_STEP`] rows at a time.
pub const GROUP_ROWS: usize = 15;
pub const GROUP_STEP: usize = 50;

pub fn sidebar(app: &App, height: f32) -> Div {
    div()
        .class("mq-sidebar")
        .w_full()
        .h(height)
        .flex_col()
        .child(
            div()
                .class("mq-sidebar-top")
                .w_full()
                .flex_col()
                .flex_shrink_0()
                .child(new_session_button(app))
                .child(home_entry(app))
                .child(filter_chips(app))
                .child(grouping_button(app)),
        )
        .child(
            div()
                .class("mq-sidebar-list")
                .w_full()
                .flex_1()
                .min_h(0.0)
                .child(scroll().w_full().h_full().child(session_list(app))),
        )
        .child(agents_panel(app))
        .child(settings_entry(app))
}

fn settings_entry(app: &App) -> impl ElementBuilder {
    let open = app.bindings.settings_open.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-settings-entry")
        .deps([open.signal_id()])
        .on_state(move |_| {
            let mut row = div().class("mq-nav-item").class("mq-settings-entry");
            if open.get() {
                row = row.class("mq-nav-item--active");
            }
            let click = app.clone();
            let row = row
                .w_full()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .child(super::icons::settings())
                .child(
                    div().class("mq-nav-label").flex_1().child(
                        ui_text(click.strings.settings())
                            .size(13.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                )
                .child(
                    ui_text(super::shortcut(","))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| {
                    if click.bindings.settings_open.get() {
                        click.close_settings();
                    } else {
                        click.open_settings();
                    }
                });
            div()
                .class("mq-settings-wrap")
                .w_full()
                .flex_shrink_0()
                .child(row)
        })
}

/// One button that steps through Time → Agent → Folder, remembered.
fn grouping_button(app: &App) -> impl ElementBuilder {
    let grouping = app.bindings.grouping.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-grouping")
        .deps([grouping.signal_id()])
        .on_state(move |_| {
            let current = grouping.get();
            let step = app.clone();
            div().w_full().child(
                button(app.strings.group_by(current))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| {
                        let next = step.bindings.grouping.get().next();
                        step.bindings.grouping.signal().set(next);
                        step.send(Command::Edit(ConfigEdit::Grouping(next)));
                    }),
            )
        })
}

fn new_session_button(app: &App) -> impl ElementBuilder {
    let app = app.clone();
    div().class("mq-new-session").w_full().child(
        button(format!("+  {}", app.strings.new_session()))
            .variant(ButtonVariant::Primary)
            .size(ButtonSize::Small)
            .on_click(move |_| app.open_draft()),
    )
}

fn home_entry(app: &App) -> impl ElementBuilder {
    let selected = app.bindings.selected.clone();
    let draft = app.bindings.draft.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-home-entry")
        .deps([selected.signal_id(), draft.signal_id()])
        .on_state(move |_| {
            let at_home = selected.get().is_none() && draft.get().is_none();
            let mut row = div().class("mq-nav-item");
            if at_home {
                row = row.class("mq-nav-item--active");
            }
            let click = app.clone();
            row.w_full()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .child(ui_text(click.strings.home()).size(13.0).no_cursor())
                .child(div().flex_1())
                .child(ui_text(super::shortcut("0")).size(11.0).no_cursor())
                .on_click(move |_| click.drain.go_home())
        })
}

/// The agents a filter chip is shown for: usable ones, and any with listed
/// sessions.
fn chip_agents(agents: &[AgentDiscoveryInfo], sessions: &[ListedSession]) -> Vec<String> {
    agents::by_standing(agents)
        .into_iter()
        .filter(|(a, s)| *s == Standing::Usable || sessions.iter().any(|x| x.agent_id == a.id))
        .map(|(a, _)| a.id)
        .collect()
}

fn filter_chips(app: &App) -> impl ElementBuilder {
    let agents_state = app.bindings.agents.clone();
    let sessions = app.bindings.listing.clone();
    let filter = app.bindings.filter.clone();
    let strings = app.strings;
    let remember = app.clone();
    stateful_with_key::<NoState>("mq-filter-chips")
        .deps([
            agents_state.signal_id(),
            sessions.signal_id(),
            filter.signal_id(),
        ])
        .on_state(move |_| {
            let agents = agents_state.get();
            let current = filter.get();
            let chip = |label: String, value: Option<String>, brand: Option<&str>| {
                let mut c = div().class("mq-chip");
                if current == value {
                    c = c.class("mq-chip--active");
                }
                if let Some(id) = brand {
                    c = c.child(agents::mark(id, 14.0));
                }
                let filter = filter.clone();
                let remember = remember.clone();
                c.flex_row()
                    .items_center()
                    .cursor_pointer()
                    .child(ui_text(label).size(11.0).no_wrap().no_cursor())
                    .on_click(move |_| {
                        filter.signal().set(value.clone());
                        remember.send(Command::Edit(ConfigEdit::AgentFilter(value.clone())));
                    })
            };
            let mut row = div().class("mq-chips").w_full().flex_row().flex_wrap();
            row = row.child(chip(strings.all().to_string(), None, None));
            for id in chip_agents(&agents, &sessions.get().sessions) {
                row = row.child(chip(
                    agents::name(&id, &agents),
                    Some(id.clone()),
                    Some(&id),
                ));
            }
            row
        })
}

/// Today / Yesterday / This week / Earlier, by local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayGroup {
    Today,
    Yesterday,
    ThisWeek,
    Earlier,
}

impl DayGroup {
    /// The group of an update at `updated_ms`, seen at `now_ms`. (Its only
    /// caller was the old Home dashboard; the sidebar's grouping rewrite owns it.)
    #[allow(dead_code)]
    pub fn of(updated_ms: u64, now_ms: u64, utc_offset_s: i64) -> Self {
        Self::of_day(
            local_day(updated_ms, utc_offset_s),
            local_day(now_ms, utc_offset_s),
        )
    }

    /// The group of local day `day` (see [`local_day`]) seen on day `today`.
    pub fn of_day(day: i64, today: i64) -> Self {
        match today - day {
            i64::MIN..=0 => Self::Today,
            1 => Self::Yesterday,
            2..=6 => Self::ThisWeek,
            _ => Self::Earlier,
        }
    }

    pub fn label(self, strings: &super::strings::Strings) -> &'static str {
        match self {
            Self::Today => strings.today(),
            Self::Yesterday => strings.yesterday(),
            Self::ThisWeek => strings.this_week(),
            Self::Earlier => strings.earlier(),
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The sidebar list. It is rebuilt only when its layout changes (sessions
/// listed, their order or grouping, the filter, a group opened further); what
/// a row shows lives in that row's own keyed state, so a status change, a new
/// title or the selection moving rebuilds one or two rows, not the list.
fn session_list(app: &App) -> impl ElementBuilder {
    let listing = app.bindings.listing.clone();
    let filter = app.bindings.filter.clone();
    let grouping = app.bindings.grouping.clone();
    let agents_state = app.bindings.agents.clone();
    let expanded = app.bindings.expanded.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-session-list")
        .deps([
            listing.signal_id(),
            filter.signal_id(),
            grouping.signal_id(),
            agents_state.signal_id(),
            expanded.signal_id(),
        ])
        .on_state(move |_| {
            let started = std::time::Instant::now();
            let listing = listing.get();
            let filter = filter.get();
            let expanded = expanded.get();
            let mut list = div().class("mq-session-list").w_full().flex_col();
            let shown: Vec<&ListedSession> = listing
                .sessions
                .iter()
                .filter(|s| filter.as_deref().is_none_or(|f| s.agent_id == f))
                .collect();
            if shown.is_empty() {
                return list.child(
                    div()
                        .class("mq-empty")
                        .child(ui_text(app.strings.no_sessions()).size(13.0)),
                );
            }
            let agents_list = agents_state.get();
            let today = local_day(now_ms(), crate::platform::shell::local_utc_offset());
            let mut built = 0;
            for (heading, members) in group_sessions(&shown, grouping.get(), today) {
                let key = heading.key();
                let label = match heading {
                    Heading::Day(day) => day.label(&app.strings).to_string(),
                    Heading::Agent(id) => agents::name(&id, &agents_list),
                    Heading::Folder(path) => {
                        let home = dirs::home_dir();
                        muqun_gateway::home_relative(&path, home.as_deref())
                    }
                    Heading::NoFolder => app.strings.no_folder().to_string(),
                };
                list = list.child(
                    div()
                        .class("mq-group")
                        .child(ui_text(label).size(11.0).semibold().no_wrap()),
                );
                let limit = expanded.get(&key).copied().unwrap_or(GROUP_ROWS);
                for session in members.iter().take(limit) {
                    list = list.child(session_row(&app, &session.asid));
                }
                built += members.len().min(limit);
                if members.len() > limit {
                    list = list.child(show_more(&app, key, limit, members.len() - limit));
                }
            }
            tracing::debug!(
                listed = shown.len(),
                rows = built,
                build_us = started.elapsed().as_micros() as u64,
                "session list built"
            );
            list
        })
}

/// "Show 40 more" under a group cut at `limit` rows.
fn show_more(app: &App, key: String, limit: usize, hidden: usize) -> impl ElementBuilder {
    let label = match app.strings.language() {
        crate::config::Language::ZhCn => format!("再显示 {} 个", hidden.min(GROUP_STEP)),
        crate::config::Language::En => format!("Show {} more", hidden.min(GROUP_STEP)),
    };
    let expanded = app.bindings.expanded.clone();
    div().class("mq-group").w_full().child(
        button(label)
            .variant(ButtonVariant::Ghost)
            .size(ButtonSize::Small)
            .on_click(move |_| {
                let mut open = (*expanded.get()).clone();
                open.insert(key.clone(), limit + GROUP_STEP);
                expanded.signal().set(std::sync::Arc::new(open));
            }),
    )
}

/// What a group of sessions is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heading {
    Day(DayGroup),
    Agent(String),
    Folder(String),
    NoFolder,
}

impl Heading {
    /// A stable key for remembering how far the group is open.
    fn key(&self) -> String {
        match self {
            Self::Day(day) => format!("day:{day:?}"),
            Self::Agent(id) => format!("agent:{id}"),
            Self::Folder(path) => format!("folder:{path}"),
            Self::NoFolder => "folder:".to_string(),
        }
    }
}

/// `sessions` (newest first) split into headed groups. Time keeps the
/// calendar-day order; Agent follows the gateway's preference order; Folder
/// puts the folder with the most recently active session first. Within a
/// group the order stays newest first. `today` is [`local_day`] of now.
pub fn group_sessions<'a>(
    sessions: &[&'a ListedSession],
    grouping: Grouping,
    today: i64,
) -> Vec<(Heading, Vec<&'a ListedSession>)> {
    let mut groups: Vec<(Heading, Vec<&ListedSession>)> = Vec::new();
    for session in sessions {
        let heading = match grouping {
            Grouping::Time => Heading::Day(DayGroup::of_day(session.day, today)),
            Grouping::Agent => Heading::Agent(session.agent_id.clone()),
            Grouping::Folder => match session.directory.as_deref() {
                Some(dir) => Heading::Folder(dir.to_string()),
                None => Heading::NoFolder,
            },
        };
        match groups.iter_mut().find(|(h, _)| *h == heading) {
            Some((_, members)) => members.push(session),
            None => groups.push((heading, vec![session])),
        }
    }
    if grouping == Grouping::Agent {
        let rank = |h: &Heading| match h {
            Heading::Agent(id) => crate::hub::AGENT_PREFERENCE
                .iter()
                .position(|p| p == id)
                .unwrap_or(usize::MAX),
            _ => usize::MAX,
        };
        groups.sort_by_key(|(h, _)| rank(h));
    }
    groups
}

pub fn title_of(app: &App, session: &AgentSessionInfo) -> String {
    display_title(app, &session.asid.0, &session.title)
}

/// A session's title as shown: "Untitled" for none or a placeholder.
pub fn display_title(app: &App, asid: &str, title: &str) -> String {
    if title.trim().is_empty() || untitled_placeholder(asid, title) {
        app.strings.untitled().to_string()
    } else {
        title.to_string()
    }
}

/// One sidebar row, rebuilt when -- and only when -- its own state is
/// written.
fn session_row(app: &App, asid: &str) -> impl ElementBuilder {
    let state = row_state(asid);
    let app = app.clone();
    stateful_with_key::<NoState>(format!("mq-srow.{asid}"))
        .deps([state.signal_id()])
        .on_state(move |_| match state.get() {
            Some(row) => render_row(&app, &row),
            None => div(),
        })
}

fn render_row(app: &App, session: &SessionRow) -> Div {
    let asid = muqun_gateway::agents::AgentSessionId(session.asid.clone());
    let click = app.clone();
    let mut row = div().class("mq-session");
    if session.active {
        row = row.class("mq-session--active");
    }
    row.w_full()
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(super::home::mark_with_badge(
            &session.agent_id,
            session.status,
            session.waiting,
        ))
        .child(
            div().flex_1().min_w(0.0).ml(2.0).overflow_clip().child(
                ui_text(display_title(app, &session.asid, &session.title))
                    .size(13.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        )
        .child(
            ui_text(session.since.clone())
                .size(10.0)
                .no_wrap()
                .no_cursor(),
        )
        .on_click(move |_| click.open_session(&asid))
}

/// Each known agent's status word and the gateway's reason, collapsible.
fn agents_panel(app: &App) -> impl ElementBuilder {
    let agents_state = app.bindings.agents.clone();
    let open = app.bindings.agents_open.clone();
    let strings = app.strings;
    stateful_with_key::<NoState>("mq-agents-panel")
        .deps([agents_state.signal_id(), open.signal_id()])
        .on_state(move |_| {
            let is_open = open.get();
            let agents = agents_state.get();
            let toggle = open.clone();
            let header = div()
                .class("mq-agents-header")
                .w_full()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .child(super::icons::chevron(is_open))
                .child(
                    div()
                        .class("mq-agents-title")
                        .child(ui_text(strings.agents()).size(12.0).semibold().no_cursor()),
                )
                .on_click(move |_| toggle.signal().set(!is_open));
            let mut panel = div()
                .class("mq-agents-panel")
                .w_full()
                .flex_col()
                .flex_shrink_0()
                .child(header);
            if is_open {
                if agents.is_empty() {
                    panel = panel.child(
                        div()
                            .class("mq-agent-row")
                            .child(ui_text(Standing::Checking.word(&strings)).size(12.0)),
                    );
                }
                for (info, standing) in agents::by_standing(&agents) {
                    panel = panel.child(agent_row(&strings, &info, standing));
                }
            }
            panel
        })
}

pub fn agent_row(
    strings: &super::strings::Strings,
    info: &AgentDiscoveryInfo,
    standing: Standing,
) -> Div {
    let mut row = div().class("mq-agent-row").w_full().flex_col().child(
        div()
            .flex_row()
            .items_center()
            .w_full()
            .child(agents::dot(&info.id))
            .child(
                div().class("mq-agent-name").flex_1().child(
                    ui_text(if info.name.is_empty() {
                        info.id.clone()
                    } else {
                        info.name.clone()
                    })
                    .size(12.0)
                    .no_wrap(),
                ),
            )
            .child(
                div()
                    .class("mq-standing")
                    .class(standing.class())
                    .child(ui_text(standing.word(strings)).size(11.0).no_wrap()),
            ),
    );
    if standing != Standing::Usable {
        if let Some(reason) = info.reason.as_ref().filter(|r| !r.is_empty()) {
            row = row.child(
                div()
                    .class("mq-agent-reason")
                    .child(ui_text(reason.clone()).size(10.0)),
            );
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    #[test]
    fn sessions_group_by_local_calendar_day() {
        // 2026-11-02 10:00 UTC; a UTC+8 reader is at 18:00 that day.
        let now = 1_793_613_600_000;
        let offset = 8 * 3600;
        assert_eq!(DayGroup::of(now - 3_600_000, now, offset), DayGroup::Today);
        // 20 hours ago is 22:00 the previous local day.
        assert_eq!(
            DayGroup::of(now - 20 * 3_600_000, now, offset),
            DayGroup::Yesterday
        );
        assert_eq!(DayGroup::of(now - 3 * DAY, now, offset), DayGroup::ThisWeek);
        assert_eq!(DayGroup::of(now - 7 * DAY, now, offset), DayGroup::Earlier);
        assert_eq!(
            DayGroup::of(now + DAY, now, offset),
            DayGroup::Today,
            "a clock ahead"
        );
    }

    fn session(asid: &str, agent: &str, dir: Option<&str>, updated_ms: u64) -> ListedSession {
        ListedSession {
            asid: asid.into(),
            agent_id: agent.into(),
            directory: dir.map(Into::into),
            day: local_day(updated_ms, 0),
        }
    }

    #[test]
    fn sessions_group_by_agent_in_gateway_order_and_by_folder_by_recency() {
        let all = [
            session("a", "claude", Some("/w/x"), 30),
            session("b", "codex", Some("/w/y"), 20),
            session("c", "claude", Some("/w/y"), 10),
            session("d", "codex", None, 5),
        ];
        let refs: Vec<&ListedSession> = all.iter().collect();
        let by_agent = group_sessions(&refs, Grouping::Agent, 0);
        let heads: Vec<_> = by_agent.iter().map(|(h, _)| h.clone()).collect();
        assert_eq!(
            heads,
            [
                Heading::Agent("codex".into()),
                Heading::Agent("claude".into())
            ],
            "codex precedes claude in the gateway's order"
        );
        assert_eq!(by_agent[1].1.len(), 2);

        let by_folder = group_sessions(&refs, Grouping::Folder, 0);
        let heads: Vec<_> = by_folder.iter().map(|(h, _)| h.clone()).collect();
        assert_eq!(
            heads,
            [
                Heading::Folder("/w/x".into()),
                Heading::Folder("/w/y".into()),
                Heading::NoFolder
            ],
            "first appearance in newest-first order is the most recent folder"
        );
        assert_eq!(by_folder[1].1.len(), 2);
    }
}
