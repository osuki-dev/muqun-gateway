//! The sidebar: navigation rows and the project tree of sessions.

use std::collections::HashMap;
use std::path::PathBuf;

use muqun_gateway::agents::{AgentProject, AgentSessionId};
use rune::El;
use rune::{el, TextEdit, Tx};
use rune_kit::glyphs::{FOLDER, FOLDER_OPEN, SEARCH, TRIANGLE_DOWN};
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::{ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem, TextInput};

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

fn project_label(language: crate::config::Language, p: &Project) -> String {
    if p.is_no_folder() {
        super::copy::tr(language, "No folder", "无文件夹")
    } else if p.is_other() {
        super::copy::tr(language, "Other", "其他")
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
            default_open: DEFAULT_OPEN,
            per_project: PER_PROJECT,
        },
        |asid| s.session_by_id(asid).map(title_of),
    )
}

/// The sessions the sidebar shows, top to bottom (Alt+↑ / Alt+↓ walk them).
pub fn listed(s: &UiState) -> Vec<String> {
    tree_rows(s, &projects(s))
        .into_iter()
        .filter_map(|row| match row {
            TreeRow::Session { asid, .. } => Some(asid),
            _ => None,
        })
        .collect()
}

fn tree(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
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
                    super::copy::tr(
                        s.cfg.general.language,
                        &format!("More ({hidden})"),
                        &format!("更多（{hidden}）"),
                    ),
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
    let t = |en: &'static str, zh: &'static str| super::copy::tr_ref(language, en, zh);
    let mut menu = Menu::new(format!("pmenu-{id}"))
        .trigger(menu_button(
            k,
            &format!("pbtn-{id}"),
            t("Project actions", "项目操作"),
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
                t("Unpin project", "取消置顶项目")
            } else {
                t("Pin project", "置顶项目")
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &pin_id, |p| p.pinned = !p.pinned)),
        )
        .item(
            MenuItem::new(if prefs.hidden {
                t("Show project", "显示项目")
            } else {
                t("Hide project", "隐藏项目")
            })
            .on_select(move |s: &mut UiState| set_prefs(s, &hide_id, |p| p.hidden = !p.hidden)),
        )
        .item(
            MenuItem::new(t("Rename…", "重命名…")).on_select(move |s: &mut UiState| {
                s.menu = None;
                s.rename_project = Some(id.clone());
                s.project_edit = TextEdit::new(name.clone());
                s.project_edit.focused = true;
            }),
        );
    if prefs.name.is_some() {
        menu = menu.item(
            MenuItem::new(t("Reset name", "恢复名称"))
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
    let t = |en: &'static str, zh: &'static str| super::copy::tr_ref(language, en, zh);
    let trigger =
        Menu::new(format!("smenu-{id}"))
            .trigger(menu_button(
                k,
                &format!("sbtn-{id}"),
                t("Session actions", "会话操作"),
                open,
                {
                    let id = id.clone();
                    move |s: &mut UiState| s.menu = (!open).then(|| OpenMenu::Session(id.clone()))
                },
            ))
            .open(open)
            .on_close(|s: &mut UiState| s.menu = None)
            .item(
                MenuItem::new(t("Rename…", "重命名…"))
                    .on_select(move |s: &mut UiState| start_rename(s, &rename_id)),
            )
            .item(
                MenuItem::new(if pinned {
                    t("Unpin", "取消置顶")
                } else {
                    t("Pin", "置顶")
                })
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.edit(ConfigEdit::TogglePin(pin_id.clone()));
                }),
            )
            .item(MenuItem::new(t("Add to split view", "加入分屏")).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.open_in_tile(&split_id);
                },
            ))
            .item(
                MenuItem::new(t("Fork", "分叉")).on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::ForkSession {
                        asid: AgentSessionId(fork_id.clone()),
                        message_id: None,
                    });
                }),
            )
            .separator()
            .item(MenuItem::new(t("Delete", "删除")).danger(true).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::DeleteSession(AgentSessionId(delete_id.clone())));
                },
            ))
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
    let mut subtitle = if when.is_empty() {
        s.agent_name(&info.agent_id)
    } else {
        format!("{} · {when}", s.agent_name(&info.agent_id))
    };
    // The state in words too, so the row says it without the mark.
    match activity {
        Activity::Working => {
            subtitle = format!(
                "{} · {subtitle}",
                super::copy::tr(s.cfg.general.language, "Working", "工作中")
            );
        }
        Activity::NeedsYou => {
            subtitle = format!(
                "{} · {subtitle}",
                super::copy::tr(s.cfg.general.language, "Needs you", "等你处理")
            );
        }
        _ => {}
    }
    let mut row = SidebarRow::new(format!(
        "{}{}",
        if pinned { "★ " } else { "" },
        title_of(info)
    ))
    .depth(1)
    .height(45.0)
    .avatar(avatar_with_state(s, k, &look, info, activity))
    .subtitle(subtitle)
    .selected(super::search::row_selected(s, asid));
    if in_tile {
        row = row.detail(super::copy::tr(s.cfg.general.language, "split", "分屏"));
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
    // Alt- or Shift-click opens the session beside the current one.
    row.on_select(move |s: &mut UiState, tx: &mut Tx| {
        let mods = tx.mods();
        if mods.alt || mods.shift {
            s.open_in_tile(&id);
        } else {
            s.open_session(&id);
        }
    })
    .on_double_click(move |s: &mut UiState, _: &mut Tx| start_rename(s, &rename_id))
    .build(k)
    .key(format!("session-{asid}"))
    .on_secondary_press(move |s: &mut UiState, _, _: &mut Tx| {
        s.menu = Some(OpenMenu::Session(menu_id.clone()));
    })
}

fn segment_item(
    k: &Kit,
    key: &str,
    label: &str,
    active: bool,
    press: impl Fn(&mut UiState) + 'static,
) -> El {
    let mut btn = rune::row()
        .key(key)
        .h(26.0)
        .px(8.0)
        .center()
        .grow(1.0)
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .role("tab")
        .child(
            rune::text(label.to_string())
                .font(if active { k.ty.ui_medium } else { k.ty.small })
                .color(if active {
                    k.c.text_base
                } else {
                    k.c.text_muted
                })
                .truncate(),
        )
        .on_press(press);

    if active {
        btn = btn
            .fill(k.c.surface_raised_base)
            .border(1.0, k.c.border_base);
    }

    btn
}

fn computer_segmented_bar(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let curr = s.current_computer();

    let mut segments = Vec::new();

    // 1. All computers
    let is_all = curr == "all";
    segments.push(segment_item(
        k,
        "comp-tab-all",
        &super::copy::tr(language, "All computers", "全部电脑"),
        is_all,
        |s| s.switch_computer("all".to_string()),
    ));

    // 2. This computer
    let is_this = curr == "local" || curr == "self";
    segments.push(segment_item(
        k,
        "comp-tab-local",
        &super::copy::tr(language, "This computer", "本机"),
        is_this,
        |s| s.switch_computer("local".to_string()),
    ));

    // 3. Linked peers (e.g. mini)
    for comp in s.all_computers() {
        let cid = comp.server_id.clone();
        if cid == "self" || cid == "local" || cid == "all" {
            continue;
        }
        let is_curr = curr == cid || curr == comp.name;
        let select_id = cid.clone();
        let name = comp.name.clone();
        segments.push(segment_item(
            k,
            &format!("comp-tab-{}", cid),
            &name,
            is_curr,
            move |s| s.switch_computer(select_id.clone()),
        ));
    }

    rune::row()
        .key("computer-segmented-bar")
        .w_full()
        .h(30.0)
        .px(2.0)
        .py(2.0)
        .gap(2.0)
        .center_y()
        .radius(7.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_muted)
        .children(segments)
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let header_box = rune::col()
        .key("sidebar-header-box")
        .w_full()
        .gap(8.0)
        .child(computer_segmented_bar(s, k))
        .child(search(s, k));

    let mut sidebar = Sidebar::new()
        .label(super::copy::tr_ref(language, "Sessions", "会话"))
        .header(header_box);
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
    let searching = !s.search.text.trim().is_empty();
    if super::search::titles_shown(s) {
        let rows = tree(s, k, layer);
        if searching && !s.sessions.is_empty() && listed(s).is_empty() {
            sidebar = sidebar.child(super::search::note(
                k,
                "no-titles",
                super::copy::tr(language, "No session titles match", "没有标题匹配的会话"),
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
    let sidebar = sidebar
        .footer(nav(
            s,
            k,
            super::copy::tr_ref(language, "Inbox", "收件箱"),
            &INBOX,
            Mode::Inbox,
        ))
        .footer(nav(
            s,
            k,
            super::copy::tr_ref(language, "Tasks", "任务"),
            &TASK,
            Mode::Tasks,
        ))
        .footer(nav(
            s,
            k,
            super::copy::tr_ref(language, "Settings", "设置"),
            &SETTINGS,
            Mode::Settings,
        ));
    match link_warning(s, k) {
        Some(warning) => sidebar.footer(warning),
        None => sidebar,
    }
    .build(k)
}

/// The search field: a quiet well the width of the rows, its icon on the
/// rows' icon column, and a clear button only once there is something to clear.
fn search(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let only = s.content.only;
    let mut field = TextInput::new(&s.search)
        .placeholder(if only {
            super::copy::tr_ref(language, "Search in messages", "搜索消息内容")
        } else if s.content.supported {
            super::copy::tr_ref(language, "Search sessions and messages", "搜索会话和消息")
        } else {
            super::copy::tr_ref(language, "Search sessions", "搜索会话")
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
                    .radius(10.0)
                    .fill(k.c.bg_accent.alpha(if s.dark { 0.22 } else { 0.14 }))
                    .role("button")
                    .label(super::copy::tr(language, "Messages only", "仅消息"))
                    .help(super::keymap::tip(
                        &super::keymap::entry(super::keymap::Action::FocusSearch).what(language),
                        super::keymap::Action::FocusSearch,
                    ))
                    .child(
                        rune::text(super::copy::tr(language, "Messages", "消息"))
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
    let word = super::copy::tr(language, "Gateway disconnected", "Gateway 已断开");
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
                    &format!("正在自动重连，点击立即重试。\n{why}"),
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
                    rune::text(super::copy::tr(language, "Retry", "重试"))
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
    fn computer_switcher_lists_and_switches_computers() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.peers = vec![
            crate::hub::PeerMention {
                server_id: "self".into(),
                name: "studio".into(),
                reachable: true,
                current: true,
            },
            crate::hub::PeerMention {
                server_id: "m2".into(),
                name: "mini".into(),
                reachable: true,
                current: false,
            },
        ];

        let comps = s.all_computers();
        assert_eq!(comps.len(), 2);
        assert_eq!(
            s.computer_name("self"),
            crate::ui::copy::tr(s.cfg.general.language, "This computer", "本机")
        );
        assert_eq!(s.computer_name("m2"), "mini");

        let k = Kit::neutral(s.dark);
        let mut layer = Vec::new();
        let _el = view(&s, &k, &mut layer);

        // Switch to mini
        s.switch_computer("m2".into());
        assert_eq!(s.current_computer(), "m2");
        assert!(s.peers[1].current);
        assert!(!s.peers[0].current);
    }
}
