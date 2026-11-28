//! The sidebar: navigation rows and the project tree of sessions.

use std::collections::HashMap;
use std::path::PathBuf;

use muqun_gateway::agents::{AgentProject, AgentSessionId};
use rune::El;
use rune::{el, TextEdit, Tx};
use rune_kit::glyphs::{FOLDER, FOLDER_OPEN, SEARCH};
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::{
    ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem, TextInput,
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
const RECENT_LIMIT: usize = 16;
const RECENT_AGE_MS: u64 = 3 * 24 * 60 * 60 * 1000;

pub(super) fn is_hidden_sidebar_session(
    s: &UiState,
    info: &muqun_gateway::agents::AgentSessionInfo,
) -> bool {
    s.is_subagent_session(info) || s.orchestration.is_internal_child_session(&info.asid.0)
}

fn recent_sessions(s: &UiState, now: u64) -> Vec<&muqun_gateway::agents::AgentSessionInfo> {
    let mut sessions: Vec<_> = s
        .sessions
        .iter()
        .filter(|info| !is_hidden_sidebar_session(s, info))
        .collect();
    let last_worked = |info: &muqun_gateway::agents::AgentSessionInfo| {
        info.updated_ms.max(info.time_viewed.unwrap_or(0)).max(
            s.sidebar_opened
                .get(&format!("{}:{}", s.current_computer(), info.asid.0))
                .copied()
                .unwrap_or(0),
        )
    };
    sessions.sort_by_cached_key(|info| std::cmp::Reverse(last_worked(info)));
    let selected_root = s.selected.as_deref().and_then(|id| s.sidebar_root(id));
    let open_tile_roots: std::collections::HashSet<&str> = s
        .tiles
        .iter()
        .filter_map(|tile| tile.asid.as_deref())
        .filter_map(|open| s.sidebar_root(open))
        .collect();
    let active_roots: std::collections::HashSet<&str> = s
        .sessions
        .iter()
        .filter(|info| matches!(s.activity(info), Activity::Working | Activity::NeedsYou))
        .filter_map(|info| s.sidebar_root(&info.asid.0))
        .collect();
    let mut ordinary = 0;
    sessions.retain(|info| {
        let id = &info.asid.0;
        if selected_root == Some(id.as_str())
            || open_tile_roots.contains(id.as_str())
            || s.cfg.general.pinned.contains(id)
            || active_roots.contains(id.as_str())
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
    } else if mode == Mode::Mao {
        let active = s.orchestration.active_count();
        if active > 0 {
            row = row.badge(active.to_string());
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
        s.sessions
            .iter()
            .filter(|info| !is_hidden_sidebar_session(s, info))
            .collect()
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
    let count = if s.sidebar_all_sessions {
        s.sessions
            .iter()
            .filter(|info| !is_hidden_sidebar_session(s, info))
            .count()
    } else {
        recent_sessions(s, now_ms()).len()
    };
    let trigger = k
        .interactive(
            rune::row()
                .key("sidebar-scope")
                .h(28.0)
                .px(7.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.md)
                .fill(if open {
                    k.c.layer_01.alpha(0.55)
                } else {
                    k.c.bg_base.alpha(0.0)
                })
                .role("button")
                .label(title.clone())
                .child(
                    rune::text(title)
                        .font(k.ty.small)
                        .color(if open { k.c.text_base } else { k.c.text_muted })
                        .truncate(),
                )
                .child(
                    rune::row()
                        .h(16.0)
                        .px(5.5)
                        .center()
                        .radius(8.0)
                        .fill(k.c.text_base.alpha(0.08))
                        .child(
                            rune::text(count.to_string())
                                .font(k.ty.caption())
                                .color(k.c.text_faint),
                        ),
                )
                .child(super::art::directional_arrow(
                    s,
                    k,
                    10.0,
                    if open {
                        -std::f32::consts::FRAC_PI_2
                    } else {
                        std::f32::consts::FRAC_PI_2
                    },
                    k.c.text_faint,
                )),
        )
        .on_press(move |s: &mut UiState| {
            s.menu = (!open).then_some(OpenMenu::SidebarScope);
        });
    let mut menu = Menu::new("sidebar-scope-menu")
        .trigger(trigger)
        .open(open)
        .min_width(210.0)
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
            s,
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
    s: &UiState,
    k: &Kit,
    key: &str,
    label: &str,
    open: bool,
    press: impl Fn(&mut UiState) + 'static,
) -> El {
    super::art::direction_button(
        s,
        k,
        label,
        if open {
            -std::f32::consts::FRAC_PI_2
        } else {
            std::f32::consts::FRAC_PI_2
        },
    )
    .variant(ButtonVariant::GhostMuted)
    .control_size(ControlSize::Small)
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
    super::orchestration::on_user_renamed_session(s, &asid, &title);
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
            s,
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
    let mut activity = s.activity(info);
    if activity == super::Activity::Idle && s.orchestration.row_working(asid) {
        activity = super::Activity::Working;
    }
    let pinned = s.cfg.general.pinned.iter().any(|p| p == asid);
    let in_tile = s.in_split() && s.tile_of(asid).is_some();
    let when = ago(info.updated_ms, now_ms(), s.cfg.general.language);
    let flat = !s.sidebar_all_sessions && s.search.text.trim().is_empty();
    let directory = info
        .directory
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned());
    let subtitle = match (
        s.orchestration.row_status(asid, s.cfg.general.language),
        directory,
    ) {
        (Some(mao), Some(dir)) => format!("{mao} · {dir}"),
        (Some(mao), None) => mao,
        (None, Some(dir)) => dir,
        (None, None) => String::new(),
    };
    let mut row = SidebarRow::new(format!(
        "{}{}",
        if pinned { "★ " } else { "" },
        title_of(info)
    ))
    .depth(if flat { 0 } else { 1 })
    .height(if flat { 52.0 } else { 48.0 })
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
        tx.focus("draft", false);
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

    let (glyph, status, status_color) = match curr {
        "all" => (&super::parts::glyphs::NETWORK, "Fleet", k.c.text_accent),
        "local" | "self" => (
            &super::parts::glyphs::COMPUTER,
            "This computer",
            k.c.success.fg,
        ),
        _ => {
            let (label, color) = match s.peer_by_id(curr).map(|p| p.reachable) {
                Some(true) => ("Online", k.c.success.fg),
                Some(false) => ("Offline", k.c.warning.fg),
                None => ("Paired", k.c.icon_muted),
            };
            (&super::parts::glyphs::COMPUTER, label, color)
        }
    };
    let [c0, c1, _] = crate::theme::look::highlight_colors_for(k, k.dark);
    let mut trigger = rune::row()
        .key("computer-switcher-trigger")
        .w_full()
        .h(32.0)
        .px(10.0)
        .gap(7.0)
        .center_y()
        .radius(k.radius.lg)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(super::copy::tr_ref(language, "Switch computer"))
        .child(rune::icon(glyph, 15.0).color(status_color))
        .child(
            rune::text(s.computer_name(curr))
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(status_color)
                .help(super::copy::tr_ref(language, status)),
        )
        .child(super::art::directional_arrow(
            s,
            k,
            10.0,
            if open {
                -std::f32::consts::FRAC_PI_2
            } else {
                std::f32::consts::FRAC_PI_2
            },
            k.c.icon_muted,
        ))
        .on_press(|s: &mut UiState| {
            let open = s.menu == Some(OpenMenu::ComputerPicker);
            s.menu = (!open).then_some(OpenMenu::ComputerPicker);
        });
    if open {
        trigger = trigger.border(1.0, c0.alpha(0.45)).linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [(0.0, c0.alpha(0.18)), (1.0, c1.alpha(0.12))],
        );
    } else {
        trigger = trigger
            .border(1.0, k.c.border_weak)
            .fill(k.c.layer_01.alpha(0.38));
    }

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

        let local = matches!(cid.as_str(), "local" | "self");
        let observed = local || s.peers.iter().any(|p| p.server_id == cid);
        if !observed && !is_paired {
            continue;
        }
        let desc = if !observed {
            super::copy::tr(language, "Paired")
        } else if !reachable {
            super::copy::tr(language, "Offline")
        } else if !is_paired {
            super::copy::tr(language, "Unpaired · Click to link")
        } else if cid == "local" || cid == "self" {
            super::copy::tr(language, "This computer")
        } else {
            super::copy::tr(language, "Online")
        };

        let target_id = cid.clone();
        let status_color = if !observed {
            k.c.icon_muted
        } else if reachable {
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

    menu.separator()
        .item(
            MenuItem::new(super::copy::tr_ref(language, "Manage computers"))
                .lead(rune::icon(&super::icons::CONTROLS, 16.0).color(k.c.text_muted))
                .on_select(|s: &mut UiState| {
                    s.go(Mode::Settings);
                    super::settings::open(s, super::SettingsTab::Computers);
                }),
        )
        .build(k, layer)
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let header_box = rune::col()
        .key("sidebar-header-box")
        .w_full()
        .gap(8.0)
        .pb(6.0)
        .child(
            rune::row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(computer_switcher(s, k, layer).grow(1.0).min_w(0.0))
                .child(
                    IconButton::new(&super::icons::PLUS)
                        .control_size(ControlSize::Regular)
                        .variant(IconButtonVariant::Ghost)
                        .label(super::copy::tr_ref(language, "New session"))
                        .help(super::copy::tr_ref(language, "New session"))
                        .on_press(|s: &mut UiState| s.go(Mode::Home))
                        .build(k)
                        .key("sidebar-new-session")
                        .size(32.0, 32.0)
                        .radius(k.radius.lg)
                        .border(1.0, k.c.bg_accent.alpha(0.28))
                        .fill(k.c.bg_accent.alpha(0.14)),
                ),
        )
        .child(search_toolbar(s, k, layer))
        .child(
            el().w_full()
                .h(1.0)
                .fill(k.c.border_muted.alpha(0.5))
                .a11y_hidden(),
        );

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
    // Measure localized labels at the active text scale; each pair wraps
    // to one column when its two complete labels no longer fit.
    let cell_width = ["Plugins", "Inbox", "Tasks", "MAO", "Settings"]
        .iter()
        .map(|label| {
            rune::text::measure(super::copy::tr_ref(language, label), &[], k.ty.ui, None).0 + 64.0
        })
        .fold(0.0_f32, f32::max)
        .ceil();
    let cell = |item: El| item.w(0.0).grow(1.0).min_w(cell_width);
    let mut entries = vec![plugins];
    for (id, label, glyph, mode) in [
        ("inbox", "Inbox", &INBOX, Mode::Inbox),
        ("tasks", "Tasks", &TASK, Mode::Tasks),
        ("mao", "MAO", &super::parts::glyphs::WORKFLOW, Mode::Mao),
        ("settings", "Settings", &SETTINGS, Mode::Settings),
    ] {
        if crate::plugin::is_enabled(id, &s.cfg.plugins_disabled) {
            entries.push(nav(s, k, super::copy::tr_ref(language, label), glyph, mode));
        }
    }
    let mut footer = rune::col()
        .key("sidebar-navigation")
        .w_full()
        .gap(6.0)
        .pt(10.0);
    let mut entries = entries.into_iter();
    while let Some(left) = entries.next() {
        let mut pair = rune::row().w_full().gap(6.0).flex_wrap().child(cell(left));
        if let Some(right) = entries.next() {
            pair = pair.child(cell(right));
        }
        footer = footer.child(pair);
    }
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

/// Keep the scope and search on one row; retain the input while a query exists.
fn search_toolbar(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let expanded = s.search.focused || !s.search.text.is_empty();
    let motion = if s.cfg.appearance.reduced_motion {
        rune::Spring::NONE
    } else {
        rune::Spring::new(0.3, 0.04)
    };
    rune::row()
        .key("sidebar-search-toolbar")
        .w_full()
        .center_y()
        .child(
            rune::row()
                .key("scope-slot")
                .w(0.0)
                .min_w(0.0)
                .grow(if expanded { 0.0 } else { 1.0 })
                .clip_soft(0.0)
                .opacity(if expanded { 0.0 } else { 1.0 })
                .motion(motion)
                .child(scope(s, k, layer)),
        )
        .child(
            search(s, k)
                .w(28.0)
                .min_w(28.0)
                .grow(if expanded { 1.0 } else { 0.0 })
                .motion(motion),
        )
}

/// The search field: a quiet well the width of the rows, its icon on the
/// rows' icon column, and a clear button only once there is something to clear.
fn search(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    if !s.search.focused && s.search.text.is_empty() {
        return k
            .interactive(
                rune::row()
                    .key("search")
                    .h(28.0)
                    .center()
                    .radius(k.radius.md)
                    .border(1.0, k.c.border_weak)
                    .fill(k.c.layer_01.alpha(0.28))
                    .role("button")
                    .label(super::copy::tr_ref(language, "Search sessions"))
                    .child(rune::icon(&SEARCH, 13.0).color(k.c.text_muted)),
            )
            .on_press(|s: &mut UiState| {
                s.menu = None;
                s.focus_search(s.content.only);
            });
    }
    let only = s.content.only;
    let mut field = TextInput::new(&s.search)
        .placeholder(if !s.search.focused && s.search.text.is_empty() {
            ""
        } else if only {
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
        .on_edit(|s: &mut UiState| {
            if s.menu == Some(OpenMenu::SidebarScope) {
                s.menu = None;
            }
            &mut s.search
        })
        .on_change(|s: &mut UiState, _: &mut Tx| s.search_changed())
        .on_key(|s: &mut UiState, ev: &rune::KeyEvent, tx: &mut Tx| s.search_key(ev, tx));
    if !s.search.text.is_empty() {
        field = field
            .clear_button()
            .on_action(|s: &mut UiState, _: &mut Tx| s.close_search());
    }
    let mut well = rune::row()
        .key("search")
        .on_press(|s: &mut UiState| {
            if !s.search.focused {
                if s.menu == Some(OpenMenu::SidebarScope) {
                    s.menu = None;
                }
                s.focus_search(s.content.only);
            }
        })
        .label(super::copy::tr_ref(language, "Search sessions"))
        .help(super::copy::tr_ref(language, "Search sessions"))
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

    #[test]
    fn collapsed_search_click_focuses_input_and_preserves_selected_session() {
        let (tx, _) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        state.splash = None;
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .unwrap();
        scene(&mut state);
        let selected = state.selected.clone();
        let mut rt = rune::Runtime::new(
            state,
            crate::ui::view,
            (1280.0, 800.0),
            rune::Env::new(true, 1.0),
        );
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let button = rt
            .elements()
            .into_iter()
            .find(|node| node.key.as_deref() == Some("search"))
            .unwrap();
        let x = button.rect[0] + button.rect[2] / 2.0;
        let y = button.rect[1] + button.rect[3] / 2.0;
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        assert!(rt.state.search.focused);
        assert_eq!(rt.state.selected, selected);
    }

    #[test]
    fn selecting_a_sidebar_session_routes_the_next_paste_to_its_draft() {
        let (tx, _) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        state.splash = None;
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .unwrap();
        scene(&mut state);
        state.sessions.push(session("paste-target", now_ms()));
        let mut rt = rune::Runtime::new(
            state,
            crate::ui::view,
            (1280.0, 800.0),
            rune::Env::new(true, 1.0),
        );
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let rect = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some("session-paste-target"))
            .unwrap()
            .rect;
        let (x, y) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.selected.as_deref(), Some("paste-target"));
        rt.paste("pasted after switching");
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.draft.text, "pasted after switching");
    }

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
    fn closing_a_tile_keeps_its_session_in_recent_sidebar_and_refresh_preserves_working_sessions() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let old = now_ms() - RECENT_AGE_MS - 1;
        let mut busy = session("codex-busy", old);
        busy.status = muqun_gateway::agents::AgentSessionStatus::Busy;
        s.sessions = vec![
            session("keep-open", now_ms()),
            session("closed-tile", old),
            busy,
        ];
        s.tiles = vec![
            crate::ui::Tile::session("keep-open"),
            crate::ui::Tile::session("closed-tile"),
        ];
        s.focus_tile(0);
        s.close_tile(1);
        let ids: Vec<_> = recent_sessions(&s, now_ms())
            .iter()
            .map(|i| i.asid.0.as_str())
            .collect();
        assert!(
            ids.contains(&"closed-tile"),
            "closing a tile must not hide its session from Recent sessions: {ids:?}"
        );
        // A partial/stale AgentSessions list must never evict a currently working session.
        s.apply_updates(vec![crate::model::inbox::Update::AgentSessions {
            agent: "codex".into(),
            sessions: vec![session("keep-open", now_ms())],
        }]);
        assert!(
            s.session_by_id("codex-busy").is_some(),
            "working Codex session must survive a partial AgentSessions refresh"
        );
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

        // Even if a remote hub reports `self` with `current: true` while viewing a remote peer,
        // `all_computers()` normalizes `current` so at most one computer is marked active.
        s.peers[0].current = true;
        let normalized = s.all_computers();
        assert_eq!(
            normalized.iter().filter(|p| p.current).count(),
            1,
            "at most one computer may be current: {normalized:?}"
        );
        assert!(normalized.iter().any(|p| p.server_id == "m2" && p.current));
        assert!(normalized
            .iter()
            .any(|p| p.server_id == "self" && !p.current));
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

    #[test]
    fn manually_opened_or_pinned_subagents_and_mao_children_never_appear_in_sidebar() {
        use crate::hub::{SearchHit, TaskRow};
        use crate::ui::orchestration::{MaoRun, MaoSubtaskView};

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".into();
        let now = now_ms();

        let root = session("root", now);
        let mut codex_sub = session("codex-sub", now);
        codex_sub.agent_role = Some("explorer".into());

        let mut claude_sub = session("claude-sub", now);
        claude_sub.agent_id = "claude".into();
        claude_sub.mode = Some("subagent".into());

        let mut cross_child = session("cross-child", now);
        cross_child.agent_id = "opencode".into();

        let mao_rev = session("mao-rev", now);
        let mao_adv = session("mao-adv", now);
        let mao_sub = session("mao-sub", now);
        let search_sub = session("search-sub", now);
        let task_sub = session("task-sub", now);

        s.sessions = vec![
            root,
            codex_sub,
            claude_sub,
            cross_child,
            mao_rev,
            mao_adv,
            mao_sub,
            search_sub,
            task_sub,
        ];

        // Cross-agent child persisted under parent's agent_id ("codex")
        s.cfg
            .ui
            .subagent_contexts
            .push(crate::config::SubagentContext {
                computer: "local".into(),
                agent: "codex".into(),
                asid: "cross-child".into(),
                ancestor: Some("root".into()),
            });

        // MAO run owning reviewer, advisor, and delegated subtask
        s.orchestration.runs.push(MaoRun {
            id: "mao-1".into(),
            coder_asid: "root".into(),
            reviewer_asid: Some("mao-rev".into()),
            advisor_asid: Some("mao-adv".into()),
            subtasks: vec![MaoSubtaskView {
                task_id: "t-sub".into(),
                asid: Some("mao-sub".into()),
                ..MaoSubtaskView::default()
            }],
            ..MaoRun::default()
        });

        // Search hit with parent_asid records lineage on open
        s.content.hits.push(SearchHit {
            asid: "search-sub".into(),
            agent_id: "claude".into(),
            parent_asid: Some("root".into()),
            ..SearchHit::default()
        });
        s.open_hit(0);

        // Child TaskRow records lineage via Update::Tasks
        s.apply_updates(vec![crate::model::inbox::Update::Tasks {
            rows: vec![
                TaskRow {
                    id: "t-root".into(),
                    asid: Some("root".into()),
                    ..TaskRow::default()
                },
                TaskRow {
                    id: "t-child".into(),
                    parent_task_id: Some("t-root".into()),
                    asid: Some("task-sub".into()),
                    agent_id: Some("codex".into()),
                    ..TaskRow::default()
                },
            ],
            default_cwd: None,
        }]);

        let hidden_ids = [
            "codex-sub",
            "claude-sub",
            "cross-child",
            "mao-rev",
            "mao-adv",
            "mao-sub",
            "search-sub",
            "task-sub",
        ];

        // Even when manually selected, pinned, and bumped in sidebar_opened,
        // NONE of these child sessions may ever appear in Recent, All, Palette, or Running.
        for child_id in hidden_ids {
            s.cfg.general.pinned.push(child_id.into());
            s.sidebar_opened
                .insert(format!("local:{child_id}"), now + 999);
            s.open_session(child_id);
            assert_eq!(s.selected.as_deref(), Some(child_id));

            s.sidebar_all_sessions = false;
            assert_eq!(
                listed(&s),
                vec!["root"],
                "recent sidebar leaked child {child_id}"
            );

            s.sidebar_all_sessions = true;
            assert_eq!(
                listed(&s),
                vec!["root"],
                "all-sessions sidebar leaked child {child_id}"
            );

            let palette_sessions: Vec<String> = super::super::palette::entries(&s)
                .into_iter()
                .filter_map(|e| match e.act {
                    super::super::palette::Act::Session(id) => Some(id),
                    _ => None,
                })
                .collect();
            assert_eq!(
                palette_sessions,
                vec!["root"],
                "palette leaked child {child_id}"
            );
        }
    }
}
