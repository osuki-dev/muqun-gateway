//! The start page: pick an agent and a folder, write the first prompt.

use std::path::PathBuf;

use muqun_gateway::agents::{AgentSessionInfo, ModelRef};
use rune::{col, el, row, text, El, ScrollSpec, Tx};
use rune_kit::{
    Button, ButtonVariant, ControlSize, Kit, ScrollBar, SegmentedControl, TextArea, TextInput,
};

use super::composer::{self, branch_menu, catalog_models, model_menu};
use super::copy::{tr, tr_ref};
use super::{muted, Mode, OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Target};
use crate::theme::look::{Look, PANEL_RADIUS};

/// The widest the content column gets, its side padding included.
const COLUMN: f32 = 1160.0;
/// The column's side padding.
const SIDE: f32 = 40.0;
/// The least height of the Home band: the theme's art lives here, the greeting sits
/// on its lower edge and the page starts under it.
const HERO_H: f32 = 196.0;
/// The band's height with no art in it: room for the greeting and a breath above.
const HERO_BARE_H: f32 = 150.0;
/// The least room a cut-out figure is drawn in.
const FIGURE_MIN_W: f32 = 120.0;
/// The start card's padding.
const START_PAD: f32 = 24.0;
/// The folder picker's width.
const FOLDER_PICKER_W: f32 = 308.0;
/// The most folders the folder picker lists.
const FOLDERS_SHOWN: usize = 8;

pub fn submit_start(s: &mut UiState) {
    let prompt = s.start_draft.text.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    if s.start_agent.is_empty() {
        s.status = "No agent available".into();
        return;
    }
    let typed = s.start_cwd.text.trim();
    let cwd = if typed.is_empty() {
        s.cfg.general.initial_cwd(&s.start_agent)
    } else {
        PathBuf::from(typed)
    };
    let (agent, worktree) = (s.start_agent.clone(), s.start_worktree);
    let (model, mode) = (s.start_model.clone(), s.start_mode.clone());
    if model.is_some() || mode.is_some() {
        s.edit(ConfigEdit::Remember {
            agent: agent.clone(),
            cwd: cwd.display().to_string(),
            model: model
                .as_ref()
                .map(|m| format!("{}/{}", m.provider_id, m.model_id)),
            mode: mode.clone(),
        });
    }
    let attachments: Vec<String> = s.staged.iter().map(|f| f.path.clone()).collect();
    let deliver =
        composer::deliver_of(s.start_catalog.as_deref(), &prompt, !attachments.is_empty());
    composer::note_skill_args(s, &deliver);
    let token = s.token(Slot::Start);
    s.status = "Starting…".into();
    s.send(Command::Compose(ComposeCommand::Submit {
        token,
        target: Target::New {
            agent,
            cwd,
            model,
            mode,
            worktree,
        },
        deliver,
        attachments,
        delivery: None,
    }));
}

fn pick_agent(s: &mut UiState, id: String) {
    s.start_cwd = rune::TextEdit::new(s.cfg.general.initial_cwd(&id).display().to_string());
    s.completions.clear();
    s.send(Command::LoadProjects { agent: id.clone() });
    s.start_agent = id;
    s.start_model = None;
    s.start_mode = None;
    s.start_catalog = None;
    s.menu = None;
    s.refresh_start_catalog();
}

/// Take `path` as the start folder and look up what it offers.
fn pick_folder_path(s: &mut UiState, path: String) {
    s.start_cwd = rune::TextEdit::new(path);
    s.completions.clear();
    s.start_model = None;
    s.start_mode = None;
    s.refresh_start_catalog();
}

/// The usable agents, the ones the owner works with most first: by how
/// many listed sessions each has, then by the usual preference.
fn usable_agents(s: &UiState) -> Vec<muqun_gateway::platform::discovery::AgentDiscoveryInfo> {
    let mut usable: Vec<_> = super::agents::by_standing(&s.agents)
        .into_iter()
        .filter(|(_, st)| *st == super::agents::Standing::Usable)
        .map(|(a, _)| a)
        .collect();
    let used = |id: &str| s.sessions.iter().filter(|i| i.agent_id == id).count();
    usable.sort_by_key(|a| std::cmp::Reverse(used(&a.id)));
    usable
}

/// Which of `widths` (in order) fit on one line `room` wide with `gap`
/// between them, keeping `keep` in and leaving `more` for the overflow
/// button when any are left out. Returns the shown indices, in order.
fn fit_on_line(widths: &[f32], room: f32, gap: f32, more: f32, keep: Option<usize>) -> Vec<usize> {
    let total: f32 = widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
    if total <= room {
        return (0..widths.len()).collect();
    }
    let room = room - more - gap;
    let mut shown = Vec::new();
    let mut used = 0.0;
    for (i, w) in widths.iter().enumerate() {
        let next = used + if shown.is_empty() { 0.0 } else { gap } + w;
        if next > room {
            break;
        }
        used = next;
        shown.push(i);
    }
    if let Some(k) = keep {
        if !shown.contains(&k) {
            // The chosen one stays on the line, in place of the last that fit.
            while !shown.is_empty() {
                let width: f32 = shown.iter().map(|&i| widths[i]).sum::<f32>()
                    + widths[k]
                    + gap * shown.len() as f32;
                if width <= room {
                    break;
                }
                shown.pop();
            }
            shown.push(k);
        }
    }
    shown
}

/// The agent picker: one line of chips, the agents in use first; the rest
/// in a "More" menu, so the line never wraps into a ragged second row.
fn agents(s: &UiState, k: &Kit, layer: &mut Vec<El>, room: f32) -> El {
    let usable = usable_agents(s);
    let language = s.cfg.general.language;
    if usable.is_empty() {
        return row()
            .key("agents-none")
            .w_full()
            .gap(10.0)
            .center_y()
            .flex_wrap()
            .child(muted(
                k,
                tr(
                    language,
                    "No agent is ready. Turn one on, or install one.",
                    "还没有可用的 Agent。启用一个，或先安装。",
                ),
            ))
            .child(
                Button::new(tr_ref(language, "Open Agents settings", "打开 Agents 设置"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        s.settings_tab = super::SettingsTab::Agents;
                        s.go(Mode::Settings);
                    })
                    .build(k)
                    .key("agents-settings"),
            );
    }
    let dark = s.dark;
    let names: Vec<String> = usable
        .iter()
        .map(|a| super::agents::name(&a.id, &s.agents))
        .collect();
    // A chip: its padding, the mark and the gap beside it, and the label.
    let widths: Vec<f32> = names
        .iter()
        .map(|n| rune::text::measure(n, &[], k.ty.ui, None).0 + 12.0 * 2.0 + 15.0 + 7.0 + 2.0)
        .collect();
    let more_label = |n: usize| {
        super::copy::localize(
            language,
            tr(language, &format!("+{n} more"), &format!("另外 {n} 个")),
        )
    };
    let more_w = rune::text::measure(&more_label(widths.len()), &[], k.ty.small, None).0 + 40.0;
    let picked = usable.iter().position(|a| a.id == s.start_agent);
    let shown = fit_on_line(&widths, room, 8.0, more_w, picked);
    let mut line = row().key("agents").w_full().gap(8.0).center_y();
    for &i in &shown {
        let a = &usable[i];
        let selected = a.id == s.start_agent;
        let id = a.id.clone();
        let label = names[i].clone();
        let brand = super::agents::color(&a.id, dark);
        let chip = Button::new(label.clone())
            .variant(if selected {
                ButtonVariant::Outline
            } else {
                ButtonVariant::GhostMuted
            })
            .control_size(ControlSize::Regular)
            .label_with({
                let id = id.clone();
                move |font, ink| {
                    row()
                        .gap(7.0)
                        .center_y()
                        .child(if selected {
                            super::agents::mark(&id, 15.0, dark)
                        } else {
                            super::agents::mark_idle(&id, 15.0, ink)
                        })
                        .child(text(label).font(font).color(ink))
                }
            })
            .on_press(move |s: &mut UiState| pick_agent(s, id.clone()))
            .build(k)
            .key(format!("agent-{}", a.id));
        // The chosen agent is lit in its own colour.
        let chip = if selected {
            s.look()
                .glow(chip.border(1.0, brand.alpha(0.55)), brand, 0.5)
        } else {
            chip
        };
        line = line.child(chip);
    }
    let rest: Vec<usize> = (0..usable.len()).filter(|i| !shown.contains(i)).collect();
    if !rest.is_empty() {
        let open = s.menu == Some(OpenMenu::StartAgents);
        let mut menu = rune_kit::Menu::new("start-agents")
            .trigger(
                Button::new(more_label(rest.len()))
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Regular)
                    .expanded(open)
                    .on_press(move |s: &mut UiState| {
                        s.menu = (!open).then_some(OpenMenu::StartAgents)
                    })
                    .build(k)
                    .key("agents-more"),
            )
            .open(open)
            .min_width(200.0)
            .on_close(|s: &mut UiState| s.menu = None);
        for i in rest {
            let id = usable[i].id.clone();
            menu = menu.item(
                rune_kit::MenuItem::new(names[i].clone())
                    .lead(super::agents::mark(&id, 15.0, dark))
                    .on_select(move |s: &mut UiState| pick_agent(s, id.clone())),
            );
        }
        line = line.child(menu.build(k, layer));
    }
    line
}

/// One folder the picker offers: what to call it, where it is, its branch.
struct FolderChoice {
    name: String,
    path: String,
    branch: Option<String>,
}

/// The folders to offer, best first: the agent's own projects, then where
/// recent sessions ran, then folders typed so far; narrowed by `query`.
fn folder_choices(s: &UiState, query: &str) -> Vec<FolderChoice> {
    let query = query.trim().to_lowercase();
    let mut out: Vec<FolderChoice> = Vec::new();
    let mut add = |name: String, path: String| {
        if path.is_empty() || out.iter().any(|c| c.path == path) {
            return;
        }
        let hit = query.is_empty()
            || name.to_lowercase().contains(&query)
            || path.to_lowercase().contains(&query);
        if hit {
            let branch = s.directories.get(&path).and_then(|d| d.branch.clone());
            out.push(FolderChoice { name, path, branch });
        }
    };
    for p in s
        .agent_projects
        .get(&s.start_agent)
        .into_iter()
        .flatten()
        .filter(|p| !p.missing)
    {
        add(p.name.clone(), p.canonical.clone());
    }
    for info in &s.sessions {
        if let (Some(dir), Some(name)) = (info.directory.clone(), UiState::folder_name(info)) {
            add(name, dir);
        }
    }
    for entry in &s.completions {
        add(entry.name.clone(), entry.path.clone());
    }
    out.truncate(FOLDERS_SHOWN);
    out
}

fn close_folders(s: &mut UiState) {
    s.menu = None;
    s.folder_query = rune::TextEdit::new("");
}

fn take_folder(s: &mut UiState, path: String) {
    close_folders(s);
    pick_folder_path(s, path);
}

/// The folder control in the start card's bottom row: a folder icon and
/// the chosen folder's name; pressing it opens a short picker of recent
/// folders with a filter (a typed path completes too) and "Choose folder…".
fn folder(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::StartFolder);
    let cwd = s.start_cwd.text.trim().to_string();
    let name = std::path::Path::new(&cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| tr(language, "Choose a folder", "选择文件夹"));
    let trigger = Button::new(name.clone())
        .variant(ButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label_with(move |font, ink| {
            row()
                .gap(6.0)
                .center_y()
                .child(rune::icon(&rune_kit::glyphs::FOLDER, 14.0).color(ink))
                .child(text(super::timeline::clip(&name, 28)).font(font).color(ink))
        })
        .help(if cwd.is_empty() {
            tr(language, "Project folder", "项目文件夹")
        } else {
            cwd.clone()
        })
        .on_press(move |s: &mut UiState| {
            if open {
                close_folders(s);
            } else {
                s.menu = Some(OpenMenu::StartFolder);
                s.folder_query = rune::TextEdit::new("");
                s.folder_query.focused = true;
            }
        })
        .build(k)
        .key("start-folder");
    let mut pop = rune_kit::Popover::new("start-folders")
        .trigger(trigger)
        .open(open)
        .padding(6.0)
        .label(tr(language, "Project folder", "项目文件夹"))
        .on_close(close_folders);
    if open {
        let choices = folder_choices(s, &s.folder_query.text);
        let first = choices.first().map(|c| c.path.clone());
        pop = pop.child(
            TextInput::new(&s.folder_query)
                .placeholder(tr_ref(
                    language,
                    "Filter, or type a path",
                    "筛选，或输入路径",
                ))
                .label(tr_ref(language, "Filter folders", "筛选文件夹"))
                .leading_icon(&rune_kit::glyphs::SEARCH)
                .leading_icon_size(14.0)
                .width(Some(FOLDER_PICKER_W))
                .on_edit(|s: &mut UiState| &mut s.folder_query)
                .on_change(|s: &mut UiState, _: &mut Tx| {
                    let typed = s.folder_query.text.trim().to_string();
                    if typed.starts_with('/') || typed.starts_with('~') {
                        s.send(Command::CompleteDirectory { prefix: typed });
                    }
                })
                .on_submit(move |s: &mut UiState, _: &mut Tx| {
                    let typed = s.folder_query.text.trim().to_string();
                    let path = if typed.starts_with('/') || typed.starts_with('~') {
                        Some(typed)
                    } else {
                        first.clone()
                    };
                    if let Some(path) = path {
                        take_folder(s, path);
                    }
                })
                .build(k)
                .key("folder-filter")
                .mb(4.0),
        );
        let mut list = col().key("folder-list").w_full().role("list");
        for c in choices {
            let on = c.path == cwd;
            let path = c.path.clone();
            let mut line = row()
                .key(format!("folder-{}", c.path))
                .w_full()
                .h(40.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .role("listitem")
                .label(c.name.clone())
                .description(c.path.clone())
                .focusable(true)
                .help(c.path.clone())
                .child(
                    rune::icon(&rune_kit::glyphs::FOLDER, 14.0)
                        .color(k.c.icon_muted)
                        .a11y_hidden(),
                )
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .child(
                            text(c.name.clone())
                                .font(k.ty.ui)
                                .color(k.c.text_base)
                                .truncate(),
                        )
                        .child(
                            text(c.path.clone())
                                .font(k.ty.caption())
                                .color(k.c.text_faint)
                                .single_line()
                                .truncate_head()
                                .w_full(),
                        )
                        .a11y_hidden(),
                );
            if let Some(branch) = c.branch {
                line = line.child(
                    row()
                        .gap(4.0)
                        .center_y()
                        .child(rune::icon(&super::icons::BRANCH, 12.0).color(k.c.icon_faint))
                        .child(
                            text(super::timeline::clip(&branch, 18))
                                .font(k.ty.small)
                                .color(k.c.text_faint),
                        )
                        .a11y_hidden(),
                );
            }
            if on {
                line = line.fill(k.c.pressed);
            }
            list = list.child(
                k.interactive(line)
                    .on_press(move |s: &mut UiState| take_folder(s, path.clone())),
            );
        }
        let start = (!cwd.is_empty()).then(|| PathBuf::from(&cwd));
        pop = pop.child(
            col()
                .key("folder-scroll")
                .w_full()
                .max_h(340.0)
                .shrink()
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        );
        pop = pop.child(
            Button::new(tr_ref(language, "Choose folder…", "选择文件夹…"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    close_folders(s);
                    crate::platform::dialogs::choose_project(start.clone());
                })
                .build(k)
                .key("choose-folder")
                .self_start()
                .mt(4.0),
        );
    }
    pop.build(k, layer)
}

/// The worktree choice as a compact toggle in the bottom row: a branch
/// mark and a word, lit while on.
fn worktree(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let on = s.start_worktree;
    Button::new(tr(language, "Worktree", "Worktree"))
        .variant(if on {
            ButtonVariant::Outline
        } else {
            ButtonVariant::GhostMuted
        })
        .control_size(ControlSize::Small)
        .label_with(move |font, ink| {
            row()
                .gap(6.0)
                .center_y()
                .child(rune::icon(&super::icons::BRANCH, 14.0).color(ink))
                .child(
                    text(tr(language, "Worktree", "Worktree"))
                        .font(font)
                        .color(ink),
                )
        })
        .help(tr(
            language,
            "Start in a new git worktree",
            "在新的 git worktree 中开始",
        ))
        .on_press(|s: &mut UiState| s.start_worktree = !s.start_worktree)
        .build(k)
        .role("switch")
        .checked(on)
        .key("worktree")
}

/// The chosen folder's branch, when it is a checkout (a menu with others).
fn start_branch(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let cwd = s.start_cwd.text.trim();
    s.branches.get(cwd).and_then(|b| {
        branch_menu(
            k,
            layer,
            OpenMenu::StartBranch,
            s.menu == Some(OpenMenu::StartBranch),
            cwd,
            b,
        )
    })
}

/// Model and mode for the session about to start.
fn start_pills(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let language = s.cfg.general.language;
    let view = s.start_catalog.as_deref();
    let models = view.map(catalog_models).unwrap_or_default();
    let defaults = view.map(|v| &v.catalog.defaults);
    let picked = s
        .start_model
        .as_ref()
        .or_else(|| defaults.and_then(|d| d.model.as_ref()));
    let mut line = row().key("start-pills").gap(8.0).center_y();
    let mut any = false;
    if !models.is_empty() {
        let label = picked
            .map(|m| {
                let catalog = models
                    .iter()
                    .find(|(p, id, _)| {
                        (*p == m.provider_id && *id == m.model_id)
                            || (super::labels::bare_catalog_id(id)
                                == super::labels::bare_catalog_id(&m.model_id))
                    })
                    .map(|(_, _, n)| n.as_str());
                super::labels::format_model_name(m, tr_ref(language, "Model", "模型"), catalog)
            })
            .unwrap_or_else(|| tr(language, "Model", "模型"));
        line = line.child(model_menu(
            k,
            layer,
            OpenMenu::StartModel,
            s.menu == Some(OpenMenu::StartModel),
            label,
            &models,
            picked.map(|m| (m.provider_id.as_str(), m.model_id.as_str())),
            |s: &mut UiState, provider_id: String, model_id: String| {
                s.menu = None;
                s.start_model = Some(ModelRef {
                    provider_id,
                    model_id,
                    variant: None,
                });
            },
        ));
        any = true;
    }
    let modes: Vec<(String, String)> = view
        .map(|v| {
            v.catalog
                .modes
                .iter()
                .filter(|m| !m.hidden)
                .map(|m| {
                    let name = if m.name.trim().is_empty() || m.name == m.id {
                        super::labels::readable_catalog_id(&m.id)
                    } else {
                        m.name.clone()
                    };
                    (m.id.clone(), name)
                })
                .collect()
        })
        .unwrap_or_default();
    if modes.len() >= 2 {
        let current = s
            .start_mode
            .clone()
            .or_else(|| defaults.and_then(|d| d.mode.clone()));
        let at = current
            .as_ref()
            .and_then(|m| modes.iter().position(|(id, _)| id == m));
        let ids: Vec<String> = modes.iter().map(|(id, _)| id.clone()).collect();
        if modes.len() <= 4 {
            let labels: Vec<&str> = modes.iter().map(|(_, n)| n.as_str()).collect();
            line = line.child(
                SegmentedControl::new(&labels, at)
                    .fit()
                    .label(tr_ref(language, "Mode", "模式"))
                    .on_pick(move |s: &mut UiState, i, _: &mut Tx| {
                        s.start_mode = Some(ids[i].clone());
                    })
                    .build(k)
                    .key("start-modes"),
            );
        } else {
            let open = s.menu == Some(OpenMenu::StartMode);
            let label = at.map_or_else(|| tr(language, "Mode", "模式"), |i| modes[i].1.clone());
            let mut menu = rune_kit::Menu::new("start-mode")
                .trigger(super::composer::pill(
                    k,
                    "start-mode-pill",
                    label,
                    open,
                    OpenMenu::StartMode,
                ))
                .open(open)
                .on_close(|s: &mut UiState| s.menu = None);
            for (i, (id, name)) in modes.iter().enumerate() {
                let id = id.clone();
                menu = menu.item(
                    rune_kit::MenuItem::new(name.clone())
                        .checked(at == Some(i))
                        .on_select(move |s: &mut UiState| {
                            s.menu = None;
                            s.start_mode = Some(id.clone());
                        }),
                );
            }
            line = line.child(menu.build(k, layer));
        }
        any = true;
    }
    any.then_some(line)
}

// ---- the page ------------------------------------------------------------------------

fn first_run(s: &UiState) -> bool {
    !s.agents.is_empty()
        && s.sessions.is_empty()
        && s.cfg.general.last_agent.is_none()
        && s.cfg.general.last_cwd.is_empty()
}

/// Every session with something waiting on the owner, newest first.
fn needing(s: &UiState) -> Vec<&AgentSessionInfo> {
    s.sessions
        .iter()
        .filter(|i| s.session_waiting(&i.asid.0))
        .collect()
}

/// The greeting and its one-line summary of what is going on.
fn greeting(s: &UiState) -> (String, String) {
    let language = s.cfg.general.language;
    let zh = language.is_zh();
    let ready = s
        .agents
        .iter()
        .filter(|a| super::agents::Standing::of(a) == super::agents::Standing::Usable)
        .count();
    let running = s.running().len();
    let waiting = needing(s).len();
    let tasks = s.tasks.iter().filter(|t| t.state == "working").count();
    if first_run(s) {
        let title = tr(language, "Welcome to Muqun", "欢迎使用 Muqun");
        if ready == 0 {
            return (
                title,
                tr(
                    language,
                    "No agent is ready yet. Turn one on in Settings → Agents.",
                    "还没有可用的 Agent。在「设置 → Agents」中启用一个。",
                ),
            );
        }
        let sub = if zh {
            format!("已检测到 {ready} 个 agent，选一个开始。")
        } else {
            format!(
                "{ready} agent{} ready. Pick one and describe the task.",
                if ready == 1 { "" } else { "s" }
            )
        };
        return (title, super::copy::localize(language, sub));
    }
    let title = if waiting > 0 {
        if zh {
            format!("{waiting} 项等你处理")
        } else if waiting == 1 {
            "One thing needs you".to_string()
        } else {
            format!("{waiting} things need you")
        }
    } else if running > 0 {
        tr(language, "Agents are working", "Agent 正在工作")
    } else {
        tr(language, "What should we work on?", "今天想做什么？")
    };
    let mut parts = Vec::new();
    if zh {
        parts.push(format!("{ready} 个 agent 就绪"));
        if running > 0 {
            parts.push(format!("{running} 个会话工作中"));
        }
        if tasks > 0 {
            parts.push(format!("{tasks} 个任务运行中"));
        }
    } else {
        parts.push(format!(
            "{ready} agent{} ready",
            if ready == 1 { "" } else { "s" }
        ));
        if running > 0 {
            parts.push(format!(
                "{running} session{} working",
                if running == 1 { "" } else { "s" }
            ));
        }
        if tasks > 0 {
            parts.push(format!(
                "{tasks} task{} running",
                if tasks == 1 { "" } else { "s" }
            ));
        }
    }
    (
        super::copy::localize(language, title),
        super::copy::localize(language, parts.join(" · ")),
    )
}

/// The start form: which agent, the first prompt, and under it one quiet
/// row of choices (folder, branch, model, mode, worktree) beside Start.
fn quick_start(s: &UiState, k: &Kit, look: &Look, layer: &mut Vec<El>, inner: f32) -> El {
    let language = s.cfg.general.language;
    let mut card = col()
        .key("home-card")
        .w_full()
        .gap(16.0)
        .px(START_PAD)
        .pt(START_PAD - 4.0)
        .pb(START_PAD - 6.0)
        .children(composer_art(s))
        .child(agents(s, k, layer, inner - START_PAD * 2.0));
    composer::popup(s, k, layer, Slot::Start);
    let placeholder = tr_ref(
        language,
        "Describe the task…  (/ commands, $ skills, @ files)",
        "描述任务…（/ 命令，$ 技能，@ 文件）",
    );
    let accent = k.c.bg_accent;
    let info = k.c.info.icon;
    card = card.child(
        TextArea::new(&s.start_draft)
            .placeholder(placeholder)
            .label(tr_ref(language, "First prompt", "第一条消息"))
            .width(None)
            .auto_grow(3, 12)
            .pill_style(move |id| {
                if id.starts_with("computer:") {
                    rune::PillStyle::default().bg(accent.alpha(0.22)).fg(accent)
                } else if id.starts_with("file:") {
                    rune::PillStyle::default().bg(info.alpha(0.18)).fg(info)
                } else {
                    rune::PillStyle::default()
                }
            })
            .on_edit(|s: &mut UiState| &mut s.start_draft)
            .on_key(|s: &mut UiState, ev: &rune::KeyEvent, _: &mut Tx| {
                composer::popup_key(s, Slot::Start, ev)
            })
            .on_change(|s: &mut UiState, _: &mut Tx| composer::on_draft_change(s, Slot::Start))
            .on_submit(|s: &mut UiState, _: &mut Tx| submit_start(s))
            .on_input_paste(|s: &mut UiState, content, _: &mut Tx| composer::take_paste(s, content))
            .build(k)
            .key("start-draft")
            .w_full(),
    );
    // Where and how, in one row with the button that acts on them.
    let mut actions = row()
        .key("start-actions")
        .w_full()
        .gap(4.0)
        .center_y()
        .child(folder(s, k, layer));
    if let Some(branch) = start_branch(s, k, layer) {
        actions = actions.child(branch);
    }
    if let Some(pills) = start_pills(s, k, layer) {
        actions = actions.child(pills.ml(4.0));
    }
    actions = actions.child(worktree(s, k));
    if let Some(i) = s.pending_tile {
        actions = actions.child(
            rune_kit::Badge::new(format!(
                "{} {}",
                tr_ref(language, "into tile", "进入分屏"),
                i + 1
            ))
            .variant(rune_kit::BadgeVariant::Accent)
            .build(k),
        );
    }
    actions = actions
        .child(el().grow(1.0).a11y_hidden())
        .child(
            muted(
                k,
                format!(
                    "{} {}",
                    crate::platform::keys::label("Primary+Enter"),
                    tr_ref(language, "to start", "开始")
                ),
            )
            .mr(8.0),
        )
        .child(
            look.glow(
                Button::new(tr_ref(language, "Start", "开始"))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Large)
                    .disabled(s.pending_submit.is_some())
                    .on_press(submit_start)
                    .build(k)
                    .key("start"),
                k.c.bg_accent,
                if s.start_draft.text.trim().is_empty() {
                    0.0
                } else {
                    0.6
                },
            ),
        );
    card = card.child(actions);
    look.accent_rim(k, look.panel(k, card, PANEL_RADIUS), k.c.bg_accent)
}

/// The Home band's height for a card `h` points tall: enough for the
/// theme's art beside the greeting, never a screenful of empty sky.
fn hero_height(card_h: f32, cover_header: bool) -> f32 {
    if cover_header {
        (card_h * 0.22).clamp(HERO_H, 280.0)
    } else {
        (card_h * 0.18).clamp(HERO_H - 24.0, 230.0)
    }
}

/// The brand block over the greeting: the Muqun mark (or the pack's logo)
/// and the pack's own name, as its `homeIdentity` asks.
fn identity(s: &UiState, k: &Kit, look: &Look) -> Option<El> {
    let id = crate::theme::apply::identity();
    let images = s.images.as_ref()?;
    let mark = if let Some(logo) = id.logo.clone() {
        Some(
            super::art::picture(images, logo, rune::SurfaceFit::Fit)
                .key("home-logo")
                .size(56.0, 56.0)
                .a11y_hidden(),
        )
    } else if id.mark && s.fx_on() {
        Some(
            look.glow(
                el().key("home-badge")
                    .size(44.0, 44.0)
                    .radius(13.0)
                    .center()
                    .fill(k.c.layer_02)
                    .child(
                        super::fx::metal("home-metal", 44.0, 13.0, k.c.bg_accent, 0.0)
                            .absolute(0.0, 0.0),
                    )
                    .child(
                        super::art::picture(
                            images,
                            std::sync::Arc::<[u8]>::from(crate::branding::APP_MARK_PNG),
                            rune::SurfaceFit::Fill,
                        )
                        .size(26.0, 26.0)
                        .radius(7.0)
                        .clip_rounded(7.0),
                    )
                    .a11y_hidden(),
                k.c.bg_accent,
                0.5,
            ),
        )
    } else {
        None
    };
    let name = id.name.map(|name| {
        text(name.to_uppercase())
            .key("home-theme-name")
            .font(rune::TextStyle {
                tracking: 1.6,
                ..k.ty.small_medium
            })
            .color(k.c.text_accent)
    });
    if mark.is_none() && name.is_none() {
        return None;
    }
    Some(
        col()
            .key("home-identity")
            .gap(10.0)
            .mb(4.0)
            .children(mark)
            .children(name),
    )
}

/// Where the theme's art may sit in the Home band, `w` × `hero` points:
/// right of `clear` (the greeting's far edge and a margin), and no further
/// right than `reach` (a little past the content column, so on a very wide
/// window the art stays with the greeting instead of drifting to the edge).
struct Stage {
    w: f32,
    hero: f32,
    clear: f32,
    reach: f32,
}

/// The theme's world on Home: a cut-out figure stands whole beside the
/// greeting, its inner edges melting into the page; a painting fills the
/// band right of the greeting and dissolves toward it. Neither goes under
/// the text. Behind it all, a scrim on the greeting's side keeps the
/// window's wallpaper from fighting the title.
fn hero_art(s: &UiState, k: &Kit, look: &Look, stage: Stage) -> Vec<El> {
    let Stage {
        w,
        hero,
        clear,
        reach,
    } = stage;
    let mut layers = Vec::new();
    let scheme = s.scheme();
    if look.glass {
        if let Some(wall) = crate::theme::apply::backdrop(scheme, true) {
            let wash = crate::theme::look::wash_for(
                k.c.bg_base,
                &[(k.c.text_base, 4.5), (k.c.text_muted, 4.5)],
                wall.look.opacity,
            );
            if wash > 0.0 {
                layers.push(
                    el().key("home-title-scrim")
                        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                        .linear_gradient(
                            std::f32::consts::FRAC_PI_2,
                            [
                                (0.0, k.c.bg_base.alpha(wash)),
                                (0.42, k.c.bg_base.alpha(wash * 0.85)),
                                (0.75, k.c.bg_base.alpha(0.0)),
                            ],
                        )
                        .a11y_hidden(),
                );
            }
        }
    }
    let (Some(images), Some(art)) = (s.images.as_ref(), crate::theme::apply::home_artwork(scheme))
    else {
        return layers;
    };
    if art.look.cover {
        // A painting: from the greeting's edge to the band's far side and
        // a little below, its near edge and foot dissolving.
        let bw = (w - clear).max(w * 0.38).min(w);
        let bh = hero + 48.0;
        layers.push(
            super::art::cover(images, &art, "home-figure", bw, bh)
                .clip_fade(bw * 0.3, 0.0, 0.0, bh * 0.4)
                .pin(None, Some(0.0), Some(0.0), None)
                .opacity(art.look.opacity),
        );
        return layers;
    }
    // A cut-out: whole, as large as the room beside the greeting allows.
    // Its own transparency shapes it; only the edges that face the page
    // fade, so a square-cut figure never ends on a line.
    let edge = (w * 0.03).clamp(16.0, 56.0);
    let right_x = reach.min(w - edge);
    let room_w = (right_x - clear).min(hero * 1.9).max(0.0);
    if room_w < FIGURE_MIN_W {
        return layers;
    }
    let (figure, [fw, fh]) = super::art::contained(images, &art, room_w, hero - 12.0);
    let right = w - right_x;
    // The light reaches past the band, so its falloff never meets an edge.
    let light_h = hero * 1.6;
    let cx = (right_x - fw * 0.5) / w;
    let cy = (hero - fh * 0.55) / light_h;
    layers.push(
        look.light(
            "home-light",
            [cx, cy],
            [fw * 0.75, fh * 0.75],
            k.c.bg_accent,
        )
        .pin(Some(0.0), Some(0.0), Some(0.0), None)
        .h(light_h),
    );
    layers.push(
        figure
            .key("home-figure")
            .clip_fade(fw * 0.08, 0.0, 0.0, fh * 0.16)
            .pin(None, None, Some(right), Some(0.0))
            .opacity(art.look.opacity),
    );
    layers
}

/// The pack's composer artwork inside the start card, at its opacity and
/// anchored where its focal point says (most hang from the top edge).
fn composer_art(s: &UiState) -> Option<El> {
    let images = s.images.as_ref()?;
    let art = crate::theme::apply::art("home.composer.background", s.scheme())?;
    let fit = if art.look.cover {
        rune::SurfaceFit::Fill
    } else {
        rune::SurfaceFit::Fit
    };
    let image = super::art::image(images, &art.bytes);
    let pic = el()
        .key("home-card-art")
        .pixels_fit(image.pixels(), fit)
        .opacity(art.look.opacity)
        .a11y_hidden();
    // A contain picture keeps its aspect: hang it from the focal edge.
    let pic = if art.look.cover {
        pic.pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
    } else if art.look.focal[1] > 0.66 {
        pic.pin(Some(0.0), None, Some(0.0), Some(0.0)).w_full()
    } else {
        pic.pin(Some(0.0), Some(0.0), Some(0.0), None).w_full()
    };
    Some(pic)
}

/// Home, composed for a main card `card` points wide and tall.
pub fn view(s: &UiState, k: &Kit, card: [f32; 2], layer: &mut Vec<El>) -> El {
    let look = s.look();
    let (title, subtitle) = greeting(s);
    let [card_w, card_h] = card;
    // The greeting grows a little with a large window, to hold its band.
    let size = (card_w / 40.0).clamp(32.0, 40.0);
    let title_font = rune::TextStyle {
        size,
        line_height: size * 1.2,
        ..k.ty.ui_medium
    };
    let id = crate::theme::apply::identity();
    let column_w = card_w.min(COLUMN);
    let inner = column_w - SIDE * 2.0;
    let left = (card_w - column_w) * 0.5 + SIDE;
    let words = rune::text::measure(&title, &[], title_font, None)
        .0
        .max(rune::text::measure(&subtitle, &[], k.ty.body, None).0);
    let clear = left + words.min(inner) + 40.0;
    let reach = left + inner + 160.0;
    // With no art to show beside the greeting (none, or no room for a
    // figure in a narrow window) the band is only as tall as its words.
    let art_shown = crate::theme::apply::home_artwork(s.scheme())
        .is_some_and(|art| art.look.cover || reach.min(card_w - 16.0) - clear >= FIGURE_MIN_W);
    let hero_h = if art_shown {
        hero_height(card_h, id.cover_header)
    } else {
        HERO_BARE_H
    };
    let stage = Stage {
        w: card_w,
        hero: hero_h,
        clear,
        reach,
    };

    let mut hero = col()
        .key("home-hero")
        .w_full()
        .max_w(COLUMN)
        .px(SIDE)
        .h(hero_h)
        .justify_end()
        .gap(8.0);
    if let Some(identity) = identity(s, k, &look) {
        hero = hero.child(identity);
    }
    let hero = hero
        .child(
            text(title)
                .font(title_font)
                .color(k.c.text_base)
                .key("home-title"),
        )
        .child(
            text(subtitle)
                .font(k.ty.body)
                .color(k.c.text_muted)
                .key("home-subtitle"),
        );
    // The band: the theme's world behind the greeting, scrolling with it.
    let band = col()
        .key("home-band")
        .w_full()
        .h(hero_h)
        .items_center()
        .children(hero_art(s, k, &look, stage))
        .child(look.arrive(hero, 0));

    let mut column = col()
        .key("home")
        .w_full()
        .max_w(COLUMN)
        .gap(32.0)
        .px(SIDE)
        .pt(28.0)
        .pb(48.0);
    column = column.child(look.arrive(quick_start(s, k, &look, layer, inner), 1));

    // The floor: clear under the band, so the window's wallpaper and the
    // figure read as one scene, then the page colour under the sections so
    // their headings stay readable. On a solid card it is the card itself.
    let floor = if look.glass { 0.84 } else { 1.0 };
    let stage = el().key("home-stage").w_full().h_full().min_h(0.0).child(
        el().key("home-floor")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .linear_gradient(
                std::f32::consts::PI,
                [
                    (0.0, k.c.bg_base.alpha(0.0)),
                    (
                        ((hero_h * 0.55) / card_h.max(1.0)).min(0.9),
                        k.c.bg_base.alpha(floor * 0.25),
                    ),
                    (
                        ((hero_h + 40.0) / card_h.max(1.0)).min(0.95),
                        k.c.bg_base.alpha(floor),
                    ),
                    (1.0, k.c.bg_base.alpha(floor)),
                ],
            )
            .a11y_hidden(),
    );
    stage.child(
        col()
            .key("home-scroll")
            .w_full()
            .h_full()
            .items_center()
            .child(band)
            .child(column)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .scroll_bar(k),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_chips_stay_on_one_line_with_the_chosen_one() {
        let widths = [100.0, 100.0, 100.0, 100.0, 100.0];
        // All fit: no overflow.
        assert_eq!(
            fit_on_line(&widths, 600.0, 10.0, 60.0, Some(4)),
            [0, 1, 2, 3, 4]
        );
        // Some fit, the rest go to "More"; the chosen one is among them.
        assert_eq!(fit_on_line(&widths, 400.0, 10.0, 60.0, Some(1)), [0, 1, 2]);
        // A chosen one past the fold takes the last shown place.
        assert_eq!(fit_on_line(&widths, 400.0, 10.0, 60.0, Some(4)), [0, 1, 4]);
    }
}
