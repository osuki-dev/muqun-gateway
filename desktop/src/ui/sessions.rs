//! The sidebar: navigation rows and the project tree of sessions.

use std::collections::HashMap;
use std::path::PathBuf;

use muqun_gateway::agents::{AgentProject, AgentSessionId, AgentSessionStatus};
use rune::El;
use rune::{el, TextEdit, Tx};
use rune_kit::glyphs::{CHECK, FOLDER, FOLDER_OPEN, KEYBOARD, SEARCH, SPARKLE, TRIANGLE_DOWN};
use rune_kit::shell::{RowStatus, Sidebar, SidebarRow};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
    TextInput,
};

use super::{title_of, Mode, OpenMenu, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::projects::{
    derive, flatten, Project, ProjectPrefs, Sources, TreeOptions, TreeRow,
};
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

fn tree(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
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
                let editable = project.is_some_and(|p| !p.is_no_folder() && !p.is_other());
                if editable && s.rename_project.as_deref() == Some(id.as_str()) {
                    row = row.renaming(
                        &s.project_edit,
                        |s: &mut UiState| &mut s.project_edit,
                        commit_project_rename,
                        |s: &mut UiState, _: &mut Tx| s.rename_project = None,
                    );
                } else if let (true, Some(p)) = (editable, project) {
                    row = row.trailing(project_menu(s, k, layer, p));
                }
                let toggle_id = id.clone();
                let mut el = row
                    .on_select(move |s: &mut UiState, _: &mut Tx| {
                        s.open_projects.insert(toggle_id.clone(), !open);
                    })
                    .build(k)
                    .key(key);
                if editable {
                    let id = id.clone();
                    el = el.on_secondary_press(move |s: &mut UiState, _, _: &mut Tx| {
                        s.menu = Some(OpenMenu::Project(id.clone()));
                    });
                }
                el
            }
            TreeRow::Session { asid, .. } => session_row(s, k, layer, &asid),
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

/// The prefs the owner set for a project root (defaults when none).
fn prefs_of(s: &UiState, root: &str) -> ProjectPrefs {
    s.cfg.ui.projects.get(root).cloned().unwrap_or_default()
}

fn set_prefs(s: &mut UiState, root: &str, change: impl FnOnce(&mut ProjectPrefs)) {
    let mut prefs = prefs_of(s, root);
    change(&mut prefs);
    s.menu = None;
    s.edit(ConfigEdit::Project {
        root: root.to_string(),
        prefs,
    });
}

fn commit_project_rename(s: &mut UiState, _: &mut Tx) {
    if let Some(root) = s.rename_project.take() {
        let name = s.project_edit.text.trim().to_string();
        set_prefs(s, &root, |p| p.name = (!name.is_empty()).then_some(name));
    }
}

/// Pin, hide, rename: one project's menu, anchored to its row's button.
fn project_menu(s: &UiState, k: &Kit, layer: &mut Vec<El>, p: &Project) -> El {
    let open = s.menu == Some(OpenMenu::Project(p.id.clone()));
    let prefs = prefs_of(s, &p.id);
    let id = p.id.clone();
    let name = prefs.name.clone().unwrap_or_else(|| p.display.1.clone());
    let (pin_id, hide_id, reset_id) = (id.clone(), id.clone(), id.clone());
    let mut menu = Menu::new(format!("pmenu-{id}"))
        .trigger(menu_button(
            k,
            &format!("pbtn-{id}"),
            "Project actions",
            open,
            {
                let id = id.clone();
                move |s: &mut UiState| s.menu = (!open).then(|| OpenMenu::Project(id.clone()))
            },
        ))
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None)
        .item(
            MenuItem::new(if prefs.pinned {
                "Unpin project"
            } else {
                "Pin project"
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &pin_id, |p| p.pinned = !p.pinned)),
        )
        .item(
            MenuItem::new(if prefs.hidden {
                "Show project"
            } else {
                "Hide project"
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &hide_id, |p| p.hidden = !p.hidden)),
        )
        .item(MenuItem::new("Rename…").on_select(move |s: &mut UiState| {
            s.menu = None;
            s.rename_project = Some(id.clone());
            s.project_edit = TextEdit::new(name.clone());
            s.project_edit.focused = true;
        }));
    if prefs.name.is_some() {
        menu = menu.item(
            MenuItem::new("Reset name")
                .on_select(move |s: &mut UiState| set_prefs(s, &reset_id, |p| p.name = None)),
        );
    }
    let trigger = menu.build(k, layer);
    if open {
        trigger
    } else {
        trigger.group_opacity(0.0, 1.0)
    }
}

/// The small arrow that opens a row's menu.
fn menu_button(
    k: &Kit,
    key: &str,
    label: &str,
    open: bool,
    press: impl Fn(&mut UiState) + 'static,
) -> El {
    IconButton::new(&TRIANGLE_DOWN)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(label.to_string())
        .expanded(open)
        .on_press(press)
        .build(k)
        .key(key.to_string())
}

fn commit_rename(s: &mut UiState, _: &mut Tx) {
    let Some(asid) = s.rename_asid.take() else {
        return;
    };
    let title = s.rename_edit.text.trim().to_string();
    if title.is_empty() {
        return;
    }
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == asid) {
        info.title = title.clone();
    }
    s.send(Command::RenameSession {
        asid: AgentSessionId(asid),
        title,
    });
}

fn start_rename(s: &mut UiState, asid: &str) {
    let title = s.session_by_id(asid).map(title_of).unwrap_or_default();
    s.menu = None;
    s.rename_asid = Some(asid.to_string());
    s.rename_edit = TextEdit::new(title);
    s.rename_edit.focused = true;
}

/// Rename, pin, fork, delete: one session's menu.
fn session_menu(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str) -> El {
    let open = s.menu == Some(OpenMenu::Session(asid.to_string()));
    let pinned = s.cfg.general.pinned.iter().any(|p| p == asid);
    let id = asid.to_string();
    let (rename_id, pin_id, fork_id, delete_id) = (id.clone(), id.clone(), id.clone(), id.clone());
    let trigger = Menu::new(format!("smenu-{id}"))
        .trigger(menu_button(
            k,
            &format!("sbtn-{id}"),
            "Session actions",
            open,
            {
                let id = id.clone();
                move |s: &mut UiState| s.menu = (!open).then(|| OpenMenu::Session(id.clone()))
            },
        ))
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None)
        .item(
            MenuItem::new("Rename…").on_select(move |s: &mut UiState| start_rename(s, &rename_id)),
        )
        .item(
            MenuItem::new(if pinned { "Unpin" } else { "Pin" }).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.edit(ConfigEdit::TogglePin(pin_id.clone()));
                },
            ),
        )
        .item(MenuItem::new("Fork").on_select(move |s: &mut UiState| {
            s.menu = None;
            s.send(Command::ForkSession {
                asid: AgentSessionId(fork_id.clone()),
                message_id: None,
            });
        }))
        .separator()
        .item(
            MenuItem::new("Delete")
                .danger(true)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::DeleteSession(AgentSessionId(delete_id.clone())));
                }),
        )
        .build(k, layer);
    if open {
        trigger
    } else {
        trigger.group_opacity(0.0, 1.0)
    }
}

fn session_row(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str) -> El {
    let Some(info) = s.session_by_id(asid) else {
        return el().key(format!("session-{asid}")).h(0.0);
    };
    let waiting = s.waiting_perms.get(asid).is_some_and(|p| !p.is_empty())
        || s.waiting_forms.get(asid).is_some_and(|f| !f.is_empty());
    let pinned = s.cfg.general.pinned.iter().any(|p| p == asid);
    let mut row = SidebarRow::new(format!(
        "{}{}",
        if pinned { "★ " } else { "" },
        title_of(info)
    ))
    .depth(1)
    .height(45.0)
    .avatar(super::agents::mark(&info.agent_id, 14.0, s.dark))
    .subtitle(s.agent_name(&info.agent_id))
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
    if s.rename_asid.as_deref() == Some(asid) {
        row = row.renaming(
            &s.rename_edit,
            |s: &mut UiState| &mut s.rename_edit,
            commit_rename,
            |s: &mut UiState, _: &mut Tx| s.rename_asid = None,
        );
    } else {
        row = row.trailing(session_menu(s, k, layer, asid));
    }
    let (menu_id, rename_id) = (id.clone(), id.clone());
    row.on_select(move |s: &mut UiState, _: &mut Tx| s.open_session(&id))
        .on_double_click(move |s: &mut UiState, _: &mut Tx| start_rename(s, &rename_id))
        .build(k)
        .key(format!("session-{asid}"))
        .on_secondary_press(move |s: &mut UiState, _, _: &mut Tx| {
            s.menu = Some(OpenMenu::Session(menu_id.clone()));
        })
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let mut sidebar = Sidebar::new()
        .label(super::copy::tr_ref(language, "Sessions", "会话"))
        .header(
            TextInput::new(&s.search)
                .placeholder(super::copy::tr_ref(language, "Search sessions", "搜索会话"))
                .leading_icon(&SEARCH)
                .clear_button()
                .on_edit(|s: &mut UiState| &mut s.search)
                .build(k)
                .key("search"),
        )
        .child(nav(
            s,
            k,
            super::copy::tr_ref(language, "Home", "首页"),
            &SPARKLE,
            Mode::Home,
        ))
        .child(nav(
            s,
            k,
            super::copy::tr_ref(language, "Inbox", "收件箱"),
            &CHECK,
            Mode::Inbox,
        ))
        .child(nav(
            s,
            k,
            super::copy::tr_ref(language, "Tasks", "任务"),
            &KEYBOARD,
            Mode::Tasks,
        ))
        .child(nav(
            s,
            k,
            super::copy::tr_ref(language, "Settings", "设置"),
            &FOLDER,
            Mode::Settings,
        ));
    if s.sessions.is_empty() {
        sidebar = sidebar.child(
            rune::text(super::copy::tr_ref(
                language,
                "No sessions yet",
                "还没有会话",
            ))
            .key("empty-sessions")
            .font(k.ty.small)
            .color(k.c.text_faint)
            .px(12.0)
            .py(8.0),
        );
    }
    for row in tree(s, k, layer) {
        sidebar = sidebar.child(row);
    }
    sidebar
        .footer(
            Button::new(super::copy::tr_ref(language, "Refresh", "刷新"))
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
