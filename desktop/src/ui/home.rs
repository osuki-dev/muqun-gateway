//! The start page: pick an agent and a folder, write the first prompt.

use std::path::PathBuf;

use rune::{col, el, row, text, El, ScrollSpec, Tx};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::composer::{self, branch_menu, catalog_models, model_menu};
use super::copy::{tr, tr_ref};
use super::{muted, Mode, OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Target};
use crate::theme::look::Look;

/// The widest the composer gets, on a very wide window.
const DOCK_MAX: f32 = 1180.0;
/// The share of the main card the composer takes on a wide window, and the
/// least it takes there; a narrower card gives it all but its gutters.
const DOCK_SHARE: f32 = 0.75;
const DOCK_MIN_WIDE: f32 = 960.0;
/// The composer's height before it is laid out.
const DOCK_GUESS: f32 = 190.0;
/// The tallest the theme's art stands over the composer.
const ART_MAX_H: f32 = 760.0;
/// The least height a figure is worth drawing at.
const ART_MIN_H: f32 = 140.0;
/// How far below the card's top edge the art may rise.
const ART_TOP: f32 = 20.0;
/// The start card's padding.
/// The folder picker's width.
const FOLDER_PICKER_W: f32 = 308.0;
/// The most folders the folder picker lists.
const FOLDERS_SHOWN: usize = 8;

pub fn submit_start(s: &mut UiState) {
    // The aggregate list has no execution target. Never use the previous hub.
    if s.current_computer() == "all" || s.pending_start.is_some() {
        return;
    }
    let prompt = s.start_draft.text.trim().to_string();
    if !composer::ready_to_send(s, Slot::Start) {
        return;
    }
    if s.start_agent.is_empty() {
        s.status = "No agent available".into();
        return;
    }
    let typed = s.start_cwd.text.trim();
    let cwd = if typed.is_empty() {
        s.start_default_directory(&s.start_agent)
    } else {
        PathBuf::from(typed)
    };
    let (agent, worktree) = (
        s.start_agent.clone(),
        s.start_worktree && s.current_computer() == "local",
    );
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

    let pending_asid = format!("new-{token}");
    let pending_title = if prompt.trim().is_empty() {
        agent.clone()
    } else {
        prompt.chars().take(40).collect()
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if let Ok(pending_info) = serde_json::from_value(serde_json::json!({
        "asid": pending_asid,
        "agent_id": agent,
        "title": pending_title,
        "directory": cwd,
        "status": "idle",
        "updated_ms": now_ms,
    })) {
        s.sessions.insert(0, pending_info);
    }

    let staged_files = s.staged.clone();
    if !prompt.trim().is_empty() || !staged_files.is_empty() {
        s.echo_sent(&pending_asid, token, prompt.clone(), &staged_files, None);
    }
    let gateway_mao = s.cfg.ui.mao.enabled
        && !worktree
        && matches!(&deliver, crate::model::compose::Deliver::Prompt(_))
        && super::orchestration::dispatch_gateway_start(
            s,
            &pending_asid,
            token,
            &prompt,
            &cwd.display().to_string(),
            &agent,
            model.as_ref(),
            mode.as_deref(),
            None,
            &attachments,
        );
    if !gateway_mao {
        super::orchestration::on_start_submitted(
            s,
            &pending_asid,
            &prompt,
            &cwd.display().to_string(),
            &agent,
            model.clone(),
        );
    }
    s.pending_start = Some((pending_asid.clone(), token));
    s.tiles_restored = true;
    if let Some(i) = s
        .pending_tile
        .take()
        .filter(|&i| s.tiles.get(i).is_some_and(|t| t.is_empty()))
    {
        s.focus_tile(i);
        s.open_here(&pending_asid);
    } else {
        s.open_session_current(&pending_asid);
    }
    s.timelines
        .entry(pending_asid)
        .or_default()
        .earlier_exhausted = true;

    s.start_draft.text.clear();
    s.start_draft.anchor = 0;
    s.start_draft.focus = 0;
    s.staged.clear();

    if !gateway_mao {
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
            auto_approve: s.cfg.ui.mao.enabled,
        }));
    }
}

pub(super) fn pick_agent(s: &mut UiState, id: String) {
    if !s.start_cwd_explicit {
        s.start_cwd = rune::TextEdit::new(s.start_default_directory(&id).display().to_string());
    }
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
pub(super) fn pick_folder_path(s: &mut UiState, path: String) {
    s.start_cwd_explicit = true;
    s.start_cwd = rune::TextEdit::new(path);
    s.completions.clear();
    s.start_model = None;
    s.start_mode = None;
    s.start_catalog = None;
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

/// A compact agent selector; all available agents remain one click away.
fn agents(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
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
                tr(language, "No agent is ready. Turn one on, or install one."),
            ))
            .child(
                Button::new(tr_ref(language, "Open Agents settings"))
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
    let open = s.menu == Some(OpenMenu::StartAgents);
    let selected = super::agents::name(&s.start_agent, &s.agents);
    let agent_id = s.start_agent.clone();
    let dark = s.dark;
    let arrow = super::art::directional_arrow(
        s,
        k,
        12.0,
        if open {
            -std::f32::consts::FRAC_PI_2
        } else {
            std::f32::consts::FRAC_PI_2
        },
        k.c.icon_muted,
    );
    let mut menu = rune_kit::Menu::new("start-agents")
        .placement(rune_kit::Placement::TopStart)
        .trigger(
            Button::new(&selected)
                .label_with(move |font, ink| {
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(super::agents::mark(&agent_id, composer::AGENT_ICON, dark))
                        .child(text(selected).font(font).color(ink))
                        .child(arrow)
                })
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .expanded(open)
                .on_press(move |s: &mut UiState| s.menu = (!open).then_some(OpenMenu::StartAgents))
                .build(k)
                .key("agents-more")
                .radius(k.radius.lg)
                .h(32.0)
                .px(12.0)
                .py(0.0)
                .border(1.0, k.c.info.icon.alpha(0.35)),
        )
        .open(open)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.menu = None);
    for agent in usable {
        let id = agent.id;
        menu = menu.item(
            rune_kit::MenuItem::new(super::agents::name(&id, &s.agents))
                .lead(super::agents::mark(&id, 15.0, s.dark))
                .on_select(move |s: &mut UiState| pick_agent(s, id.clone())),
        );
    }
    menu.build(k, layer)
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
        .unwrap_or_else(|| tr(language, "Choose a folder"));
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
            tr(language, "Project folder")
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
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .padding(6.0)
        .label(tr(language, "Project folder"))
        .on_close(close_folders);
    if open {
        let choices = folder_choices(s, &s.folder_query.text);
        let first = choices.first().map(|c| c.path.clone());
        pop = pop.child(
            TextInput::new(&s.folder_query)
                .placeholder(tr_ref(language, "Filter, or type a path"))
                .label(tr_ref(language, "Filter folders"))
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
        if s.current_computer() == "local" {
            pop = pop.child(
                Button::new(tr_ref(language, "Choose folder…"))
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
    }
    pop.build(k, layer)
}

/// The worktree choice as a compact toggle in the bottom row: a branch
/// mark and a word, lit while on.
fn worktree(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let on = s.start_worktree;
    Button::new(tr(language, "Worktree"))
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
                .child(text(tr(language, "Worktree")).font(font).color(ink))
        })
        .help(tr(language, "Start in a new git worktree"))
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
    if view.is_none() && s.start_catalog_failed {
        line = line.child(
            Button::new(tr_ref(language, "Reload model list"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(UiState::refresh_start_catalog)
                .build(k)
                .key("start-model-reload"),
        );
        any = true;
    }
    if !models.is_empty() {
        let selected = picked.and_then(|model| super::labels::catalog_model_info(view?, model));
        let label = picked
            .map(|model| {
                super::labels::format_model_name(
                    model,
                    tr_ref(language, "Model"),
                    selected.map(|entry| entry.name.as_str()),
                )
            })
            .unwrap_or_else(|| tr(language, "Model"));
        line = line.child(model_menu(
            s,
            k,
            layer,
            OpenMenu::StartModel,
            "home",
            Slot::Start,
            &s.start_agent,
            s.menu == Some(OpenMenu::StartModel),
            label,
            view,
            &models,
            picked,
            |s: &mut UiState, provider_id: String, model_id: String| {
                s.menu = None;
                let current = s
                    .start_model
                    .as_ref()
                    .or_else(|| s.start_catalog.as_ref()?.catalog.defaults.model.as_ref());
                let chosen = composer::choose_model(
                    s.start_catalog.as_deref(),
                    current,
                    provider_id,
                    model_id,
                );
                if !s.start_agent.is_empty() {
                    s.edit(ConfigEdit::RememberRecentModel {
                        agent: s.start_agent.clone(),
                        model: format!("{}/{}", chosen.provider_id, chosen.model_id),
                    });
                }
                s.start_model = Some(chosen);
            },
        ));
        if let Some(variants) = selected
            .and_then(|entry| entry.variants.as_ref())
            .filter(|levels| !levels.is_empty())
        {
            let levels = variants.iter().map(|v| v.id.clone()).collect::<Vec<_>>();
            line = line.child(composer::effort_menu(
                k,
                layer,
                "home",
                Slot::Start,
                s.menu == Some(OpenMenu::StartEffort),
                picked.and_then(|model| model.variant.as_deref()),
                &levels,
            ));
        }
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
            .or_else(|| defaults.and_then(|d| d.mode.clone()))
            .or_else(|| modes.first().map(|(id, _)| id.clone()));
        let at = current.as_ref().and_then(|m| {
            modes
                .iter()
                .position(|(id, _)| id == m || id.eq_ignore_ascii_case(m))
        });
        let open = s.menu == Some(OpenMenu::StartMode);
        let label = at.map_or_else(|| tr(language, "Mode"), |i| modes[i].1.clone());
        let mut menu = rune_kit::Menu::new("start-mode")
            .placement(rune_kit::Placement::TopStart)
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
        any = true;
    }
    any.then_some(line)
}

// ---- the page ------------------------------------------------------------------------

/// Context above the editor; agent and delivery actions below it.
fn quick_start(s: &UiState, k: &Kit, look: &Look, layer: &mut Vec<El>, size: [f32; 2]) -> El {
    let language = s.cfg.general.language;
    let mut card = composer::body().key("home-card");
    let mut context = row()
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(folder(s, k, layer));
    if s.current_computer() == "local" {
        if let Some(branch) = start_branch(s, k, layer) {
            context = context.child(branch);
        }
        context = context.child(worktree(s, k));
    }
    let context = composer::context_capsule(k, look, context);
    composer::popup(s, k, layer, Slot::Start);
    if s.drop_hover {
        card = card.child(composer::drop_hint(k, look, language));
    }
    let placeholder = tr_ref(language, "Describe the task…");
    card = card.child(composer::editor(s, k, Slot::Start, placeholder, 2, 12));
    // Where and how, in one row with the button that acts on them.
    let mut controls = row()
        .key("start-controls")
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(agents(s, k, layer));
    if let Some(pills) = start_pills(s, k, layer) {
        controls = controls.child(pills);
    }

    if let Some(i) = s.pending_tile {
        controls = controls.child(
            rune_kit::Badge::new(format!(
                "{} {}",
                tr_ref(language, "into split view pane"),
                i + 1
            ))
            .variant(rune_kit::BadgeVariant::Accent)
            .build(k),
        );
    }
    let attach_cwd = PathBuf::from(s.start_cwd.text.trim());
    let tools = composer::attachment_button(k, language, Some(attach_cwd), true, "start-attach");
    let left_tools = row()
        .gap(6.0)
        .center_y()
        .flex_wrap()
        .child(tools)
        .child(super::orchestration::composer_bar(s, k, layer, Slot::Start));
    let actions = row()
        .key("start-actions")
        .gap(8.0)
        .center_y()
        .child(left_tools.grow(1.0).min_w(0.0))
        .child(
            look.glow(
                Button::new(tr_ref(language, "Start"))
                    .label_with(|_, ink| {
                        rune::icon(&composer::SEND, composer::SEND_ICON).color(ink)
                    })
                    .help(format!(
                        "{} · {}",
                        tr_ref(language, "Start"),
                        super::keymap::label(super::keymap::Action::Send)
                    ))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Large)
                    .disabled(
                        s.pending_submit.is_some()
                            || !composer::ready_to_send(s, Slot::Start)
                            || s.start_agent.is_empty(),
                    )
                    .on_press(submit_start)
                    .build(k)
                    .key("start")
                    .size(36.0, 36.0)
                    .radius(k.radius.lg),
                k.c.bg_accent,
                if !composer::ready_to_send(s, Slot::Start) {
                    0.0
                } else {
                    0.6
                },
            ),
        );
    card = card.child(actions.w_full());
    let card = composer::composer_card(k, look, card, s.start_draft.focused, false, s.drop_hover)
        .on_drop_files(|s: &mut UiState, files, _| composer::attach_dropped_files(s, files))
        .on_drop_hover(|s: &mut UiState, over, _| s.drop_hover = over);
    // A frame the pack drew to reach past the field sits over its edges.
    col()
        .key("home-dock")
        .w_full()
        .gap(12.0)
        .child(row().child(context))
        .children(composer::chips(s, k, layer))
        .child(card)
        .child(controls)
        .children(composer_frame(s, size))
}

/// Where the stage's parts sit in the main card, `w` × `h` points (the art
/// may rise under the card's empty toolbar row): the composer `dock` wide
/// and tall, its top edge at `dock_top`.
struct Stage {
    w: f32,
    h: f32,
    dock: [f32; 2],
    dock_top: f32,
}

/// The theme's world standing behind the composer, as if on a stage whose
/// front edge the composer is. A cut-out figure or scene stands whole above
/// it, its foot going behind the composer and dissolving there, with light
/// from behind; a painting fills the card's top and dissolves down into the
/// page before the composer. With no art, the brand mark rests above it.
fn stage_art(s: &UiState, k: &Kit, look: &Look, stage: Stage) -> Vec<El> {
    let Stage {
        w, dock, dock_top, ..
    } = stage;
    let mut layers = Vec::new();
    let Some(images) = s.images.as_ref() else {
        return layers;
    };
    let Some(art) = crate::theme::apply::home_artwork(s.scheme()) else {
        layers.extend(mark(s, k, look, w, dock_top));
        return layers;
    };
    if art.look.cover {
        // A painting: edge to edge from the card's top, its foot going
        // under the composer's top and dissolving well before it.
        let ph = (dock_top + dock[1] * 0.3).max(0.0);
        if ph < ART_MIN_H {
            return layers;
        }
        layers.push(
            super::art::place(images, &art, "home-figure", w, ph)
                .clip_fade(0.0, 0.0, 0.0, ph * 0.45)
                .absolute(0.0, 0.0)
                .opacity(art.look.opacity),
        );
        return layers;
    }
    // A cut-out: whole, as tall as the room over the composer allows, a
    // little wider than the composer at most, centred on it.
    let room_h = (dock_top - ART_TOP).max(0.0);
    let overlap = (room_h * 0.1).clamp(24.0, 64.0);
    let (figure, [fw, fh]) = super::art::contained(
        images,
        &art,
        (w - 32.0).min(dock[0] + 280.0),
        (room_h + overlap).min(ART_MAX_H.max(stage.h * 0.6)),
    );
    if fh < ART_MIN_H {
        return layers;
    }
    let overlap = (fh * 0.1).clamp(24.0, 64.0).min(overlap);
    let (x, y) = ((w - fw) * 0.5, dock_top + overlap - fh);
    // Light from behind, reaching past the figure so its falloff never
    // meets an edge.
    layers.push(
        look.light(
            "home-light",
            [0.5, 0.45],
            [fw * 0.62, fh * 0.5],
            k.c.bg_accent,
        )
        .absolute(x - fw, y - fh * 0.5)
        .size(fw * 3.0, fh * 2.0),
    );
    // Its foot fades from above the composer's edge to under it, so neither
    // a waist-cut figure nor a painted ground ends on a line, even through
    // the composer's glass; a square-cut side melts a little too.
    layers.push(
        figure
            .key("home-figure")
            .clip_fade(fw * 0.04, 0.0, fw * 0.04, overlap + fh * 0.14)
            .absolute(x, y)
            .opacity(art.look.opacity),
    );
    layers
}

/// With no art: the Muqun mark (or the pack's logo) resting above the
/// composer, lit from behind.
fn mark(s: &UiState, k: &Kit, look: &Look, w: f32, dock_top: f32) -> Option<El> {
    let id = crate::theme::apply::identity();
    let images = s.images.as_ref()?;
    let (mark, size) = if let Some(logo) = id.logo.clone() {
        (
            super::art::picture(images, logo, rune::SurfaceFit::Fit)
                .key("home-logo")
                .size(72.0, 72.0),
            72.0,
        )
    } else if id.mark {
        (
            look.glow(
                el().key("home-badge")
                    .size(56.0, 56.0)
                    .radius(k.radius.xl)
                    .center()
                    .fill(k.c.layer_02)
                    .children(s.fx_on().then(|| {
                        super::fx::metal("home-metal", 56.0, k.radius.xl, k.c.bg_accent, 0.0)
                            .absolute(0.0, 0.0)
                    }))
                    .child(
                        super::art::picture(
                            images,
                            std::sync::Arc::<[u8]>::from(crate::branding::APP_MARK_PNG),
                            rune::SurfaceFit::Fill,
                        )
                        .size(32.0, 32.0)
                        .radius(8.0)
                        .clip_rounded(8.0),
                    ),
                k.c.bg_accent,
                0.5,
            ),
            56.0,
        )
    } else {
        return None;
    };
    let y = (dock_top * 0.62 - size * 0.5).max(ART_TOP);
    Some(
        el().key("home-mark-stage")
            .absolute(0.0, 0.0)
            .size(w, dock_top)
            .a11y_hidden()
            .child(
                look.light(
                    "home-light",
                    [0.5, (y + size * 0.5) / dock_top.max(1.0)],
                    [size * 4.0, size * 3.0],
                    k.c.bg_accent,
                )
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)),
            )
            .child(mark.absolute((w - size) * 0.5, y)),
    )
}

/// The pack's composer frame (`home.composer.background` drawn to reach
/// past the field, its `outset`) around a composer `size` points: over the
/// field's edges, as wide as the frame, hung from its focal edge, so its
/// rails and posts land on the field's edges as drawn. A picture meant to
/// sit inside the field is left out: on Home the theme's art already stands
/// right above the composer, and inside it a picture only muddies the
/// controls.
fn composer_frame(s: &UiState, size: [f32; 2]) -> Option<El> {
    let images = s.images.as_ref()?;
    let art = crate::theme::apply::art("home.composer.background", s.scheme())?;
    let [l, r, b] = art.look.outset;
    if l + r + b <= 0.0 {
        return None;
    }
    let (w, h) = (size[0] + l + r, size[1] + b);
    let mut art = art;
    if !art.look.cover {
        // Fitted to the frame's width rather than inside its height.
        art.look.cover = art.size[0] / art.size[1] <= w / h;
    }
    Some(
        super::art::place(images, &art, "home-card-art", w, h)
            .absolute(-l, 0.0)
            .opacity(art.look.opacity),
    )
}

/// Home, composed for a main card `card` points wide and tall, the
/// composer `dock` (when laid out): the theme's art on a stage, and the
/// composer at the stage's front edge, at the card's foot.
pub fn view(
    s: &UiState,
    k: &Kit,
    card: [f32; 2],
    dock: Option<[f32; 2]>,
    layer: &mut Vec<El>,
) -> El {
    if s.current_computer() == "all" {
        let language = s.cfg.general.language;
        let mut computers = row().gap(12.0).flex_wrap();
        for computer in s.all_computers() {
            let id = computer.server_id;
            if id == "all" {
                continue;
            }
            computers = computers.child(
                Button::new(computer.name)
                    .variant(ButtonVariant::Neutral)
                    .on_press(move |s: &mut UiState| s.switch_computer(id.clone()))
                    .build(k),
            );
        }
        let panel = col()
            .key("computer-picker-panel")
            .w((card[0] - 48.0).clamp(0.0, 520.0))
            .padding(24.0)
            .radius(k.radius.xl)
            .fill(k.c.bg_base.alpha(0.97))
            .gap(16.0)
            .child(
                text(tr(language, "Choose a computer to start a session"))
                    .font(k.ty.body_medium)
                    .color(k.c.text_base),
            )
            .child(muted(
                k,
                tr(language, "Or open an existing session from the sidebar."),
            ))
            .child(computers);
        return col()
            .key("all-computers-home")
            .w_full()
            .h_full()
            .items_center()
            .justify_center()
            .child(panel);
    }
    let look = s.look();
    let [w, h] = card;
    let side = (w * 0.04).clamp(24.0, 56.0);
    let gutters = (w - side * 2.0).max(0.0);
    let dock_w = (w * DOCK_SHARE).clamp(DOCK_MIN_WIDE, DOCK_MAX).min(gutters);
    let dock_h = dock.map_or(DOCK_GUESS, |d| d[1]);
    let bottom = (h * 0.05).clamp(20.0, 48.0);
    // Home has no toolbar: its stage rises under the card's empty toolbar
    // row, so the art starts at the card's top edge (`card` is the whole
    // card, that row included).
    let lift = rune_kit::shell::TOOLBAR_HEIGHT;
    let full_h = h;
    let dock_top = full_h - bottom - dock_h;
    let stage = Stage {
        w,
        h,
        dock: [dock_w, dock_h],
        dock_top,
    };
    let art = stage_art(s, k, &look, stage);
    let composer = quick_start(s, k, &look, layer, [dock_w, dock_h]);
    // The ground the composer stands on: the page colour rising behind it,
    // so the window's wallpaper and the art's foot settle there. On a solid
    // card it is the card itself.
    let ground = look.glass.then(|| {
        let from = ((dock_top - dock_h * 0.6) / full_h.max(1.0)).clamp(0.0, 0.95);
        el().key("home-ground")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .linear_gradient(
                std::f32::consts::PI,
                [
                    (0.0, k.c.bg_base.alpha(0.0)),
                    (from, k.c.bg_base.alpha(0.0)),
                    (1.0, k.c.bg_base.alpha(0.6)),
                ],
            )
            .a11y_hidden()
    });
    let stage = el()
        .key("home-stage")
        .w_full()
        .h(full_h)
        .offset(0.0, -lift)
        .offset_motion(rune::Anim::INSTANT)
        .children(art.into_iter().map(|a| look.arrive(a.a11y_hidden(), 0)))
        .children(ground)
        .child(
            col()
                .key("home-dock-row")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .items_center()
                .justify_end()
                .pb(bottom)
                .child(look.arrive(composer.w(dock_w), 1)),
        );
    el().key("home-page")
        .w_full()
        .h_full()
        .child(stage)
        .on_drop_files(|s: &mut UiState, files, _| composer::attach_dropped_files(s, files))
        .on_drop_hover(|s: &mut UiState, over, _| s.drop_hover = over)
}

#[cfg(test)]
mod aggregate_tests {
    #[test]
    fn aggregate_home_cannot_submit_to_previous_computer() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = super::UiState::new(tx);
        s.cfg.current_computer = "all".into();
        s.start_agent = "codex".into();
        s.start_draft = rune::TextEdit::new("keep this draft");
        while rx.try_recv().is_ok() {}
        super::submit_start(&mut s);
        assert!(rx.try_recv().is_err());
        assert_eq!(s.start_draft.text, "keep this draft");
        assert!(s.pending_start.is_none());
    }
}
