//! The sidebar: navigation rows and the project tree of sessions.

use std::collections::HashMap;
use std::path::PathBuf;

use muqun_gateway::agents::{AgentProject, AgentSessionStatus};
use rune::El;
use rune::{el, Tx};
use rune_kit::glyphs::{CHECK, FOLDER, FOLDER_OPEN, KEYBOARD, SEARCH, SPARKLE};
use rune_kit::shell::{RowStatus, Sidebar, SidebarRow};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::{title_of, Mode, UiState};
use crate::model::commands::Command;
use crate::model::projects::{derive, flatten, Project, Sources, TreeOptions, TreeRow};
use crate::model::types::ListedSession;

const DEFAULT_OPEN: usize = 3;
const PER_PROJECT: usize = 8;

fn nav(s: &UiState, k: &Kit, label: &str, icon: &'static rune::Icon, mode: Mode) -> El {
    let mut row = SidebarRow::new(label)
        .icon(icon)
        .selected(s.mode == mode)
        .on_select(move |s: &mut UiState, _: &mut Tx| s.go(mode));
    if mode == Mode::Inbox {
        let waiting = s.waiting_count();
        if waiting > 0 {
            row = row.badge(waiting.to_string());
        }
    }
    row.build(k).key(format!("nav-{label}"))
}

fn projects(s: &UiState) -> Vec<Project> {
    let listed: Vec<ListedSession> = s
        .sessions
        .iter()
        .map(|x| ListedSession {
            asid: x.asid.0.clone(),
            directory: x.directory.clone(),
        })
        .collect();
    let known: Vec<AgentProject> = s.known_projects.values().flatten().cloned().collect();
    let recent: Vec<PathBuf> = s.cfg.general.last_cwd.values().cloned().collect();
    let home = dirs::home_dir();
    derive(&Sources {
        sessions: &listed,
        dirs: &s.directories,
        known: &known,
        recent: &recent,
        prefs: &s.cfg.ui.projects,
        session_pins: &s.cfg.general.pinned,
        home: home.as_deref(),
    })
}

fn project_label(p: &Project) -> String {
    if p.is_no_folder() {
        "No folder".into()
    } else if p.is_other() {
        "Other".into()
    } else {
        format!("{}{}", p.display.0, p.display.1)
    }
}

fn tree(s: &UiState, k: &Kit) -> Vec<El> {
    let all = projects(s);
    let by_id: HashMap<&str, &Project> = all.iter().map(|p| (p.id.as_str(), p)).collect();
    let query = s.search.text.trim().to_lowercase();
    let rows = flatten(
        &all,
        &TreeOptions {
            query: &query,
            open: &s.open_projects,
            full: &s.full_projects,
            default_open: DEFAULT_OPEN,
            per_project: PER_PROJECT,
        },
        |asid| s.session_by_id(asid).map(title_of),
    );
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        out.push(match row {
            TreeRow::Gap => el().key(format!("gap-{i}")).h(8.0).w_full(),
            TreeRow::Project { id, open } => {
                let project = by_id.get(id.as_str());
                let label = project.map_or_else(|| id.clone(), |p| project_label(p));
                let mut row = SidebarRow::new(label)
                    .icon(if open { &FOLDER_OPEN } else { &FOLDER })
                    .select_only();
                if let Some(p) = project {
                    if let Some(branch) = &p.branch {
                        row = row.detail(branch.clone());
                    }
                }
                let key = format!("project-{id}");
                row.on_select(move |s: &mut UiState, _: &mut Tx| {
                    s.open_projects.insert(id.clone(), !open);
                })
                .build(k)
                .key(key)
            }
            TreeRow::Session { asid, .. } => session_row(s, k, &asid),
            TreeRow::More { project, hidden } => {
                let key = format!("more-{project}");
                SidebarRow::new(format!("More ({hidden})"))
                    .depth(1)
                    .select_only()
                    .on_select(move |s: &mut UiState, _: &mut Tx| {
                        s.full_projects.insert(project.clone());
                    })
                    .build(k)
                    .key(key)
            }
        });
    }
    out
}

fn session_row(s: &UiState, k: &Kit, asid: &str) -> El {
    let Some(info) = s.session_by_id(asid) else {
        return el().key(format!("session-{asid}")).h(0.0);
    };
    let waiting = s.waiting_perms.get(asid).is_some_and(|p| !p.is_empty())
        || s.waiting_forms.get(asid).is_some_and(|f| !f.is_empty());
    let mut row = SidebarRow::new(title_of(info))
        .depth(1)
        .selected(s.mode == Mode::Session && s.selected.as_deref() == Some(asid));
    if waiting {
        row = row.status(RowStatus::Attention);
    } else if matches!(
        info.status,
        AgentSessionStatus::Busy | AgentSessionStatus::Retry
    ) {
        row = row.status(RowStatus::Busy);
    }
    let id = asid.to_string();
    row.on_select(move |s: &mut UiState, _: &mut Tx| s.open_session(&id))
        .build(k)
        .key(format!("session-{asid}"))
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let mut sidebar = Sidebar::new()
        .label("Sessions")
        .header(
            TextInput::new(&s.search)
                .placeholder("Search sessions")
                .leading_icon(&SEARCH)
                .clear_button()
                .on_edit(|s: &mut UiState| &mut s.search)
                .build(k)
                .key("search"),
        )
        .child(nav(s, k, "Home", &SPARKLE, Mode::Home))
        .child(nav(s, k, "Inbox", &CHECK, Mode::Inbox))
        .child(nav(s, k, "Tasks", &KEYBOARD, Mode::Tasks))
        .child(nav(s, k, "Settings", &FOLDER, Mode::Settings));
    if s.sessions.is_empty() {
        sidebar = sidebar.child(
            rune::text("No sessions yet")
                .key("empty-sessions")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(12.0)
                .py(8.0),
        );
    }
    for row in tree(s, k) {
        sidebar = sidebar.child(row);
    }
    sidebar
        .footer(
            Button::new("Refresh")
                .control_size(ControlSize::Large)
                .variant(ButtonVariant::Outline)
                .on_press(|s: &mut UiState| {
                    s.status = "Refreshing…".into();
                    s.send(Command::Refresh);
                })
                .build(k)
                .key("refresh")
                .w_full(),
        )
        .build(k)
}
