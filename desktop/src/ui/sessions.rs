//! The sidebar: navigation rows and the project tree of sessions.

use std::collections::HashMap;
use std::path::PathBuf;

use muqun_gateway::agents::{AgentProject, AgentSessionId};
use rune::El;
use rune::{el, TextEdit, Tx};
use rune_kit::glyphs::{FOLDER, FOLDER_OPEN, SEARCH, TRIANGLE_DOWN};
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
    TextInput,
};

use super::icons::{INBOX, SETTINGS, TASK};
use super::{ago, now_ms, title_of, Activity, Mode, OpenMenu, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::projects::{
    derive, flatten, Project, ProjectPrefs, Sources, TreeOptions, TreeRow,
};
use crate::model::types::ListedSession;

const DEFAULT_OPEN: usize = 3;
const PER_PROJECT: usize = 8;
const RECENT_LIMIT: usize = 8;
const RECENT_AGE_MS: u64 = 3 * 24 * 60 * 60 * 1000;

fn recent_sessions(s: &UiState, now: u64) -> Vec<&muqun_gateway::agents::AgentSessionInfo> {
    let mut sessions: Vec<_> = s.sessions.iter().collect();
    let last_worked = |info: &muqun_gateway::agents::AgentSessionInfo| {
        info.updated_ms.max(info.time_viewed.unwrap_or(0)).max(
            s.sidebar_opened
                .get(&format!("{}:{}", s.current_computer(), info.asid.0))
                .copied()
                .unwrap_or(0),
        )
    };
    sessions.sort_by_cached_key(|info| std::cmp::Reverse(last_worked(info)));
    let mut ordinary = 0;
    sessions.retain(|info| {
        let id = &info.asid.0;
        if s.selected.as_ref() == Some(id)
            || s.tile_of(id).is_some()
            || s.cfg.general.pinned.contains(id)
            || matches!(s.activity(info), Activity::Working | Activity::NeedsYou)
        {
            return true;
        }
        if now.saturating_sub(last_worked(info)) > RECENT_AGE_MS || ordinary >= RECENT_LIMIT {
            return false;
        }
        ordinary += 1;
        true
    });
    sessions
}

fn nav(s: &UiState, k: &Kit, label: &str, icon: &'static rune::Icon, mode: Mode) -> El {
    // The theme's own glyph for the row, when its pack draws one.
    let themed = match mode {
        Mode::Settings => Some("chrome.settings"),
        _ => None,
    }
    .and_then(|name| {
        let color = if s.mode == mode {
            k.c.text_base
        } else {
            k.c.text_muted
        };
        super::art::glyph(s.images.as_ref()?, name, 16.0, color)
    });
    let row = SidebarRow::new(label);
    let mut row = match themed {
        Some(glyph) => row.avatar(glyph),
        None => row.icon(icon),
    }
    .selected(
        s.mode == mode && (mode != Mode::Settings || s.settings_tab != super::SettingsTab::Plugins),
    )
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
    // Search always receives the complete inventory, independent of the
    // sidebar scope. Nothing is deleted, archived or hidden in the backend.
    let recent_only = !s.sidebar_all_sessions && s.search.text.trim().is_empty();
    let sessions = if recent_only {
        recent_sessions(s, now_ms())
    } else {
        s.sessions.iter().collect()
    };
    let listed: Vec<ListedSession> = sessions
        .into_iter()
        .map(|x| ListedSession {
            asid: x.asid.0.clone(),
            directory: x.directory.clone(),
        })
        .collect();
    let known: Vec<AgentProject> = s.known_projects.values().flatten().cloned().collect();
    let recent: Vec<PathBuf> = s.cfg.general.last_cwd.values().cloned().collect();
    let home = dirs::home_dir();
    let mut projects = derive(&Sources {
        sessions: &listed,
        dirs: &s.directories,
        known: &known,
        recent: &recent,
        prefs: &s.cfg.ui.projects,
        session_pins: &s.cfg.general.pinned,
        home: home.as_deref(),
    });
    if recent_only {
        projects.retain(|p| p.pinned || !p.sessions.is_empty());
    }
    projects
}

fn scope(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let open = s.menu == Some(OpenMenu::SidebarScope);
    let t = |word| super::copy::tr(s.cfg.general.language, word);
    let title = t(if s.sidebar_all_sessions {
        "All sessions"
    } else {
        "Recent sessions"
    });
    let trigger = Button::new(title)
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Large)
        .on_press(move |s: &mut UiState| {
            s.menu = (!open).then_some(OpenMenu::SidebarScope);
        })
        .build(k)
        .key("sidebar-scope")
        .child(rune::icon(&TRIANGLE_DOWN, 12.0).color(k.c.text_faint));
    let mut menu = Menu::new("sidebar-scope-menu")
        .trigger(trigger)
        .open(open)
        .min_width(220.0)
        .placement(rune_kit::Placement::BottomStart)
        .on_close(|s: &mut UiState| s.menu = None);
    for (all, word) in [(false, "Recent sessions"), (true, "All sessions")] {
        menu = menu.item(
            MenuItem::new(t(word))
                .checked(s.sidebar_all_sessions == all)
                .on_select(move |s: &mut UiState| {
                    s.sidebar_all_sessions = all;
                    s.menu = None;
                }),
        );
    }
    rune::row().w_full().center_y().child(menu.build(k, layer))
}

fn project_label(language: crate::config::Language, p: &Project) -> String {
    if p.is_no_folder() {
        super::copy::tr(language, "No folder")
    } else if p.is_other() {
        super::copy::tr(language, "Other")
    } else {
        format!("{}{}", p.display.0, p.display.1)
    }
}

/// The sidebar's rows, as drawn (folded projects and the search applied).
fn tree_rows(s: &UiState, all: &[Project]) -> Vec<TreeRow> {
    let query = s.search.text.trim().to_lowercase();
    flatten(
        all,
        &TreeOptions {
            query: &query,
            open: &s.open_projects,
            full: &s.full_projects,
            default_open: if !s.sidebar_all_sessions && query.is_empty() {
                usize::MAX
            } else {
                DEFAULT_OPEN
            },
            per_project: if !s.sidebar_all_sessions && query.is_empty() {
                usize::MAX
            } else {
                PER_PROJECT
            },
        },
        |asid| s.session_by_id(asid).map(title_of),
    )
}

/// The sessions the sidebar shows, top to bottom (Alt+↑ / Alt+↓ walk them).
pub fn listed(s: &UiState) -> Vec<String> {
    if !s.sidebar_all_sessions && s.search.text.trim().is_empty() {
        return recent_sessions(s, now_ms())
            .into_iter()
            .map(|i| i.asid.0.clone())
            .collect();
    }
    tree_rows(s, &projects(s))
        .into_iter()
        .filter_map(|row| match row {
            TreeRow::Session { asid, .. } => Some(asid),
            _ => None,
        })
        .collect()
}

fn tree(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    if !s.sidebar_all_sessions && s.search.text.trim().is_empty() {
        return listed(s)
            .into_iter()
            .map(|id| session_row(s, k, layer, &id))
            .collect();
    }
    let all = projects(s);
    let by_id: HashMap<&str, &Project> = all.iter().map(|p| (p.id.as_str(), p)).collect();
    let rows = tree_rows(s, &all);
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        out.push(match row {
            TreeRow::Gap => el().key(format!("gap-{i}")).h(8.0).w_full(),
            TreeRow::Project { id, open } => {
                let project = by_id.get(id.as_str());
                let label = project
                    .map_or_else(|| id.clone(), |p| project_label(s.cfg.general.language, p));
                let mut row = SidebarRow::new(label)
                    .icon(if open { &FOLDER_OPEN } else { &FOLDER })
                    .select_only();
                if let Some(p) = project {
                    if let Some(branch) = &p.branch {
                        row = row.detail(super::timeline::clip(branch, 18));
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
                SidebarRow::new(super::copy::localize(
                    s.cfg.general.language,
                    super::copy::tr(s.cfg.general.language, &format!("More ({hidden})")),
                ))
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
    let language = s.cfg.general.language;
    let t = |en: &'static str| super::copy::tr_ref(language, en);
    let mut menu = Menu::new(format!("pmenu-{id}"))
        .trigger(menu_button(
            k,
            &format!("pbtn-{id}"),
            t("Project actions"),
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
                t("Unpin project")
            } else {
                t("Pin project")
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &pin_id, |p| p.pinned = !p.pinned)),
        )
        .item(
            MenuItem::new(if prefs.hidden {
                t("Show project")
            } else {
                t("Hide project")
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &hide_id, |p| p.hidden = !p.hidden)),
        )
        .item(
            MenuItem::new(t("Rename…")).on_select(move |s: &mut UiState| {
                s.menu = None;
                s.rename_project = Some(id.clone());
                s.project_edit = TextEdit::new(name.clone());
                s.project_edit.focused = true;
            }),
        );
    if prefs.name.is_some() {
        menu = menu.item(
            MenuItem::new(t("Reset name"))
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
    let split_id = id.clone();
    let language = s.cfg.general.language;
    let t = |en: &'static str| super::copy::tr_ref(language, en);
    let trigger = Menu::new(format!("smenu-{id}"))
        .trigger(menu_button(
            k,
            &format!("sbtn-{id}"),
            t("Session actions"),
            open,
            {
                let id = id.clone();
                move |s: &mut UiState| s.menu = (!open).then(|| OpenMenu::Session(id.clone()))
            },
        ))
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None)
        .item(
            MenuItem::new(t("Rename…"))
                .on_select(move |s: &mut UiState| start_rename(s, &rename_id)),
        )
        .item(
            MenuItem::new(if pinned { t("Unpin") } else { t("Pin") }).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.edit(ConfigEdit::TogglePin(pin_id.clone()));
                },
            ),
        )
        .item(
            MenuItem::new(t("Add to split view")).on_select(move |s: &mut UiState| {
                s.menu = None;
                s.open_in_tile(&split_id);
            }),
        )
        .item(MenuItem::new(t("Fork")).on_select(move |s: &mut UiState| {
            s.menu = None;
            s.send(Command::ForkSession {
                asid: AgentSessionId(fork_id.clone()),
                message_id: None,
            });
        }))
        .separator()
        .item(
            MenuItem::new(t("Delete"))
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

/// The agent's mark with a small lit dot at its corner: breathing in the
/// agent's colour while it works, the warning colour while it waits on the
/// owner, the danger colour when it failed.
fn avatar_with_state(
    s: &UiState,
    k: &Kit,
    look: &crate::theme::look::Look,
    info: &muqun_gateway::agents::AgentSessionInfo,
    activity: Activity,
) -> El {
    let mark = super::agents::mark(&info.agent_id, 14.0, s.dark);
    let dot = |c: rune::Color| {
        el().size(7.0, 7.0)
            .radius(3.5)
            .fill(c)
            .border(1.5, k.c.bg_deep)
            .absolute(10.0, 10.0)
    };
    let badge = match activity {
        Activity::Working => {
            let c = super::agents::color(&info.agent_id, s.dark);
            Some(look.pulse(look.spark(dot(c), c)))
        }
        Activity::NeedsYou => {
            let c = k.c.warning.icon;
            Some(look.pulse(look.spark(dot(c), c)))
        }
        Activity::Failed => Some(dot(k.c.danger.icon)),
        Activity::Idle => None,
    };
    let mut avatar = el().size(16.0, 16.0).center().child(mark);
    if let Some(b) = badge {
        avatar = avatar.child(b.a11y_hidden());
    }
    avatar
}

fn session_row(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str) -> El {
    let Some(info) = s.session_by_id(asid) else {
        return el().key(format!("session-{asid}")).h(0.0);
    };
    let look = s.look();
    let activity = s.activity(info);
    let pinned = s.cfg.general.pinned.iter().any(|p| p == asid);
    let in_tile = s.in_split() && s.tile_of(asid).is_some();
    let when = ago(info.updated_ms, now_ms(), s.cfg.general.language);
    let flat = !s.sidebar_all_sessions && s.search.text.trim().is_empty();
    let directory = info
        .directory
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned());
    let subtitle = directory.unwrap_or_default();
    let mut row = SidebarRow::new(format!(
        "{}{}",
        if pinned { "★ " } else { "" },
        title_of(info)
    ))
    .depth(if flat { 0 } else { 1 })
    .height(if flat { 64.0 } else { 52.0 })
    .avatar(avatar_with_state(s, k, &look, info, activity))
    .subtitle(subtitle)
    .subtitle_color(k.c.text_muted.alpha(0.72))
    .detail(when)
    .selected(super::search::row_selected(s, asid));
    if in_tile {
        row = row.detail(super::copy::tr(s.cfg.general.language, "split"));
    } else if s.current_computer() == "all" {
        if let Some(comp) = s.computer_of_session(asid) {
            row = row.detail(s.computer_name(&comp));
        }
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
    // Ctrl-, Cmd-, Alt-, or Shift-click opens the session beside the current one.
    row.on_select(move |s: &mut UiState, tx: &mut Tx| {
        let mods = tx.mods();
        if mods.ctrl || mods.cmd || mods.alt || mods.shift {
            s.open_in_tile(&id);
        } else {
            s.open_session(&id);
        }
    })
    .on_double_click(move |s: &mut UiState, _: &mut Tx| start_rename(s, &rename_id))
    .build(k)
    .key(format!("session-{asid}"))
    .help(info.directory.clone().unwrap_or_default())
    .radius(k.radius.lg)
    .mb(4.0)
    .on_secondary_press(move |s: &mut UiState, _, _: &mut Tx| {
        s.menu = Some(OpenMenu::Session(menu_id.clone()));
    })
}

fn computer_switcher(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let curr = s.current_computer();
    let open = s.menu == Some(OpenMenu::ComputerPicker);

    let (icon_el, display_name, status_desc) = if curr == "all" {
        (
            rune::icon(&super::parts::glyphs::NETWORK, 15.0).color(k.c.text_accent),
            super::copy::tr(language, "All computers"),
            super::copy::tr(language, "Fleet"),
        )
    } else {
        let name = s.computer_name(curr);
        let peer = s.peer_by_id(curr);
        let reachable = peer.map(|p| p.reachable).unwrap_or(true);
        let status_color = if reachable {
            k.c.success.fg
        } else {
            k.c.warning.fg
        };
        (
            rune::icon(&super::parts::glyphs::COMPUTER, 16.0).color(status_color),
            name,
            if reachable {
                super::copy::tr(language, "Online")
            } else {
                super::copy::tr(language, "Offline")
            },
        )
    };

    let trigger = rune::row()
        .key("computer-switcher-trigger")
        .w_full()
        .h(36.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.md)
        .fill(if open {
            k.c.layer_01
        } else {
            k.c.layer_01.alpha(0.35)
        })
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(super::copy::tr_ref(language, "Switch computer"))
        .child(icon_el)
        .child(
            rune::text(display_name)
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            rune::text(status_desc)
                .font(k.ty.caption())
                .color(k.c.text_muted),
        )
        .child(
            rune::text(if open { "▴" } else { "▾" })
                .font(k.ty.small)
                .color(k.c.text_muted),
        )
        .on_press(|s: &mut UiState| {
            let open = s.menu == Some(OpenMenu::ComputerPicker);
            s.menu = (!open).then_some(OpenMenu::ComputerPicker);
        });

    let mut menu = Menu::new("computer-switcher-menu")
        .trigger(trigger)
        .open(open)
        .min_width(240.0)
        .placement(rune_kit::Placement::BottomStart)
        .on_close(|s: &mut UiState| s.menu = None);

    // 1. All computers option
    menu = menu.item(
        MenuItem::new(super::copy::tr_ref(language, "All computers"))
            .description(super::copy::tr_ref(language, "Show all sessions"))
            .lead(rune::icon(&super::parts::glyphs::NETWORK, 15.0).color(k.c.text_accent))
            .checked(curr == "all")
            .on_select(|s: &mut UiState| {
                s.menu = None;
                s.switch_computer("all".to_string());
            }),
    );

    menu = menu.separator();

    // 2. Individual computers
    for comp in s.all_computers() {
        let cid = comp.server_id.clone();
        if cid == "all" {
            continue;
        }
        let is_curr = curr == cid || (curr == "local" && cid == "self");
        let reachable = comp.reachable;
        let is_paired = s.is_computer_paired(&cid);

        let desc = if !reachable {
            super::copy::tr(language, "Offline")
        } else if !is_paired {
            super::copy::tr(language, "Unpaired · Click to link")
        } else if cid == "local" || cid == "self" {
            super::copy::tr(language, "This computer")
        } else {
            super::copy::tr(language, "Online")
        };

        let target_id = cid.clone();
        let status_color = if reachable {
            k.c.success.fg
        } else {
            k.c.warning.fg
        };

        menu = menu.item(
            MenuItem::new(comp.name.clone())
                .description(desc)
                .lead(rune::icon(&super::parts::glyphs::COMPUTER, 16.0).color(status_color))
                .checked(is_curr)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    if s.is_computer_paired(&target_id) {
                        s.switch_computer(target_id.clone());
                    } else {
                        s.open_pairing(target_id.clone());
                    }
                }),
        );
    }

    menu.build(k, layer)
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let header_box = rune::col()
        .key("sidebar-header-box")
        .w_full()
        .gap(12.0)
        .pb(10.0)
        .child(
            rune::row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(computer_switcher(s, k, layer).grow(1.0).min_w(0.0))
                .child(
                    IconButton::new(&super::icons::PLUS)
                        .variant(IconButtonVariant::Ghost)
                        .label(super::copy::tr_ref(language, "New session"))
                        .help(super::copy::tr_ref(language, "New session"))
                        .on_press(|s: &mut UiState| s.go(Mode::Home))
                        .build(k)
                        .key("sidebar-new-session")
                        .size(36.0, 36.0)
                        .radius(k.radius.lg)
                        .fill(k.c.bg_accent.alpha(0.12)),
                ),
        )
        .child(search(s, k))
        .child(scope(s, k, layer));

    let mut sidebar = Sidebar::new()
        .label(super::copy::tr_ref(language, "Sessions"))
        .header(header_box);

    if s.sessions.is_empty() {
        sidebar = sidebar.child(
            rune::text(super::copy::tr_ref(language, "No sessions yet"))
                .key("empty-sessions")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(12.0)
                .py(8.0),
        );
    }
    let searching = !s.search.text.trim().is_empty();
    if !searching
        && !s.sidebar_all_sessions
        && !s.sessions.is_empty()
        && recent_sessions(s, now_ms()).is_empty()
    {
        sidebar = sidebar.child(super::search::note(
            k,
            "no-recent",
            super::copy::tr(
                language,
                "No recent sessions. Open All sessions to continue earlier work.",
            ),
        ));
    }
    if super::search::titles_shown(s) {
        let rows = tree(s, k, layer);
        if searching && !s.sessions.is_empty() && listed(s).is_empty() {
            sidebar = sidebar.child(super::search::note(
                k,
                "no-titles",
                super::copy::tr(language, "No session titles match"),
            ));
        }
        for row in rows {
            sidebar = sidebar.child(row);
        }
    }
    if let Some(group) = super::search::group(s, k) {
        sidebar = sidebar.child(group);
    }
    if let Some(running) = super::running::view(s, k) {
        sidebar = sidebar.footer(running);
    }
    let plugins = SidebarRow::new(super::copy::tr_ref(language, "Plugins"))
        .icon(&super::icons::GRID)
        .selected(s.mode == Mode::Settings && s.settings_tab == super::SettingsTab::Plugins)
        .on_select(|s: &mut UiState, _: &mut Tx| {
            s.go(Mode::Settings);
            super::settings::open(s, super::SettingsTab::Plugins);
        })
        .build(k)
        .key("nav-plugins");
    let footer = rune::col()
        .w_full()
        .gap(6.0)
        .pt(10.0)
        .child(
            rune::row()
                .w_full()
                .gap(6.0)
                .child(plugins.grow(1.0).min_w(0.0))
                .child(
                    nav(
                        s,
                        k,
                        super::copy::tr_ref(language, "Inbox"),
                        &INBOX,
                        Mode::Inbox,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                ),
        )
        .child(
            rune::row()
                .w_full()
                .gap(6.0)
                .child(
                    nav(
                        s,
                        k,
                        super::copy::tr_ref(language, "Tasks"),
                        &TASK,
                        Mode::Tasks,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                )
                .child(
                    nav(
                        s,
                        k,
                        super::copy::tr_ref(language, "Settings"),
                        &SETTINGS,
                        Mode::Settings,
                    )
                    .grow(1.0)
                    .min_w(0.0),
                ),
        );
    let sidebar = sidebar.footer(footer);
    match link_warning(s, k) {
        Some(warning) => sidebar.footer(warning),
        None => sidebar,
    }
    .build(k)
    .key(if s.sidebar_all_sessions {
        "sidebar-all-view"
    } else {
        "sidebar-recent-view"
    })
}

/// The search field: a quiet well the width of the rows, its icon on the
/// rows' icon column, and a clear button only once there is something to clear.
fn search(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let only = s.content.only;
    let mut field = TextInput::new(&s.search)
        .placeholder(if only {
            super::copy::tr_ref(language, "Search in messages")
        } else if s.content.supported {
            super::copy::tr_ref(language, "Search sessions and messages")
        } else {
            super::copy::tr_ref(language, "Search sessions")
        })
        .leading_icon(&SEARCH)
        .leading_icon_size(14.0)
        .width(None)
        .plain()
        .on_edit(|s: &mut UiState| &mut s.search)
        .on_change(|s: &mut UiState, _: &mut Tx| s.search_changed())
        .on_key(|s: &mut UiState, ev: &rune::KeyEvent, tx: &mut Tx| s.search_key(ev, tx));
    if !s.search.text.is_empty() {
        field = field
            .clear_button()
            .on_action(|s: &mut UiState, _: &mut Tx| s.close_search());
    }
    let mut well = rune::row()
        .key("search")
        .w_full()
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.text_base.alpha(0.06))
        .child(field.build(k).grow(1.0).shrink().min_w(0.0));
    if only {
        // The scope, said in the well; a press widens it to titles again.
        well = well.child(
            k.interactive(
                rune::row()
                    .key("search-scope")
                    .h(20.0)
                    .px(7.0)
                    .mr(4.0)
                    .center_y()
                    .radius(k.radius.lg)
                    .fill(k.c.bg_accent.alpha(if s.dark { 0.22 } else { 0.14 }))
                    .role("button")
                    .label(super::copy::tr(language, "Messages only"))
                    .help(super::keymap::tip(
                        &super::keymap::entry(super::keymap::Action::FocusSearch).what(language),
                        super::keymap::Action::FocusSearch,
                    ))
                    .child(
                        rune::text(super::copy::tr(language, "Messages"))
                            .font(k.ty.caption())
                            .color(k.c.text_accent)
                            .a11y_hidden(),
                    ),
            )
            .on_press(|s: &mut UiState| s.content.only = false),
        );
    }
    if s.search.focused {
        well.border(1.0, k.c.border_base)
    } else {
        well
    }
}

/// Nothing while the link to the gateway is fine; while it is down, one
/// compact warning row (the reason in its tooltip) that retries on a click.
fn link_warning(s: &UiState, k: &Kit) -> Option<El> {
    let why = s.hub_down.as_ref()?;
    let language = s.cfg.general.language;
    let word = super::copy::tr(language, "Gateway disconnected");
    let dot = el().size(6.0, 6.0).radius(3.0).fill(k.c.warning.icon);
    let look = s.look();
    Some(
        k.interactive(
            rune::row()
                .key("link-warning")
                .w_full()
                .h(26.0)
                .px(12.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.c.warning.icon.alpha(0.10))
                .role("button")
                .label(word.clone())
                .help(super::copy::tr(
                    language,
                    &format!("Reconnecting on its own. Click to retry now.\n{why}"),
                ))
                .child(look.pulse(look.spark(dot, k.c.warning.icon)).a11y_hidden())
                .child(
                    rune::text(word)
                        .font(k.ty.caption())
                        .color(k.c.warning.fg)
                        .truncate()
                        .grow(1.0)
                        .shrink()
                        .a11y_hidden(),
                )
                .child(
                    rune::text(super::copy::tr(language, "Retry"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .a11y_hidden(),
                ),
        )
        .on_press(|s: &mut UiState| s.send(Command::Refresh)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, updated: u64) -> muqun_gateway::agents::AgentSessionInfo {
        serde_json::from_value(serde_json::json!({"asid":id,"agent_id":"codex","title":id,"status":"idle","directory":"/project","updated_ms":updated})).unwrap()
    }

    #[test]
    fn recent_scope_bounds_idle_history_but_preserves_pinned_open_and_working_sessions() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = (0..20)
            .map(|n| session(&format!("recent-{n}"), now_ms() - n))
            .collect();
        let old = now_ms() - RECENT_AGE_MS - 1;
        s.sessions.extend([
            session("old", old),
            session("pinned", old),
            session("open", old),
            session("working", old),
            session("waiting", old),
        ]);
        s.cfg.general.pinned = vec!["pinned".into()];
        s.selected = Some("open".into());
        s.sessions
            .iter_mut()
            .find(|i| i.asid.0 == "working")
            .unwrap()
            .status = muqun_gateway::agents::AgentSessionStatus::Busy;
        s.waiting_forms.insert("waiting".into(),vec![serde_json::from_value(serde_json::json!({"id":"question","asid":"waiting","title":"Question","fields":[],"nonblocking":false})).unwrap()]);
        let ids: Vec<_> = recent_sessions(&s, now_ms())
            .iter()
            .map(|i| i.asid.0.as_str())
            .collect();
        assert_eq!(ids.len(), RECENT_LIMIT + 4);
        for id in ["pinned", "open", "working", "waiting"] {
            assert!(ids.contains(&id));
        }
        assert!(!ids.contains(&"old"));
        assert!(!ids.contains(&"recent-19"));
    }

    #[test]
    fn recent_navigation_orders_activity_across_directories_and_ignores_folder_folds() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let now = now_ms();
        let mut oldest = session("oldest", now - 30);
        oldest.directory = Some("/a".into());
        let mut newest = session("newest", now);
        newest.directory = Some("/z".into());
        s.sessions = vec![oldest, newest, session("middle", now - 10)];
        s.open_projects.insert("/z".into(), false);
        assert_eq!(listed(&s), vec!["newest", "middle", "oldest"]);
        s.sidebar_opened
            .insert(format!("{}:oldest", s.current_computer()), now + 1);
        assert_eq!(listed(&s), vec!["oldest", "newest", "middle"]);
    }

    #[test]
    fn search_and_all_scope_reveal_old_sessions_without_changing_the_conversation() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![
            session("current", now_ms()),
            session("historic", now_ms() - RECENT_AGE_MS - 1),
        ];
        s.selected = Some("current".into());
        s.draft = TextEdit::new("unfinished draft");
        assert!(!listed(&s).contains(&"historic".into()));
        s.sidebar_all_sessions = true;
        assert!(listed(&s).contains(&"historic".into()));
        assert_eq!(s.selected.as_deref(), Some("current"));
        assert_eq!(s.draft.text, "unfinished draft");
        s.sidebar_all_sessions = false;
        s.search = TextEdit::new("historic");
        assert_eq!(listed(&s), vec!["historic"]);
        s.open_session("historic");
        s.search = TextEdit::new("");
        s.selected = Some("current".into());
        assert!(listed(&s).contains(&"historic".into()));
    }

    #[test]
    fn computer_switcher_lists_and_switches_computers() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.peers = vec![
            crate::hub::PeerMention {
                server_id: "self".into(),
                name: "studio".into(),
                reachable: true,
                current: true,
                url: None,
            },
            crate::hub::PeerMention {
                server_id: "m2".into(),
                name: "mini".into(),
                reachable: true,
                current: false,
                url: None,
            },
        ];

        let comps = s.all_computers();
        assert_eq!(comps.len(), 2);
        assert_eq!(
            s.computer_name("self"),
            crate::ui::copy::tr(s.cfg.general.language, "This computer")
        );
        assert_eq!(s.computer_name("m2"), "mini");

        let k = Kit::neutral(s.dark);
        let mut layer = Vec::new();
        let _el = view(&s, &k, &mut layer);

        // Unpaired computer does not switch directly, it opens pairing dialog
        assert!(!s.is_computer_paired("m2"));
        s.open_pairing("m2".into());
        assert_eq!(s.pairing_target.as_deref(), Some("m2"));
        let mut modal_layer = Vec::new();
        crate::ui::pair_computer::mount(&s, &k, &mut modal_layer);
        assert!(!modal_layer.is_empty());
        s.close_pairing();
        assert_eq!(s.pairing_target, None);

        // Switch to mini
        s.switch_computer("m2".into());
        assert_eq!(s.current_computer(), "m2");
        assert!(s.peers[1].current);
        assert!(!s.peers[0].current);
    }

    #[test]
    fn ctrl_click_session_opens_in_split_view() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("session-1");
        assert_eq!(s.selected.as_deref(), Some("session-1"));
        assert!(!s.in_split());

        // Opening another session with open_in_tile enters split view
        s.open_in_tile("session-2");
        assert!(s.in_split());
        assert_eq!(s.tiles.len(), 2);
        assert_eq!(s.tiles[0].asid.as_deref(), Some("session-1"));
        assert_eq!(s.tiles[1].asid.as_deref(), Some("session-2"));
    }
}
