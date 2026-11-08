//! The start page: pick an agent and a folder, write the first prompt.

use std::path::PathBuf;

use muqun_gateway::agents::{AgentSessionInfo, ModelRef};
use rune::{col, el, row, text, El, ScrollSpec, Tx};
use rune_kit::glyphs::ARROW_RIGHT;
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, ScrollBar,
    SegmentedControl, Switch, TextArea, TextInput,
};

use super::composer::{self, branch_menu, catalog_models, model_menu};
use super::copy::{tr, tr_ref};
use super::{
    activity_pill, ago, muted, now_ms, timeline, title_of, Activity, Mode, OpenMenu, Slot, UiState,
};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Target};
use crate::theme::look::{Look, CARD_RADIUS, PANEL_RADIUS};

/// The widest the content column gets.
const COLUMN: f32 = 980.0;
/// The least height of the Home band: the theme's art lives here, the greeting sits
/// on its lower edge and the page starts under it.
const HERO_H: f32 = 250.0;

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

fn agents(s: &UiState, k: &Kit) -> El {
    let ranked = super::agents::by_standing(&s.agents);
    let usable: Vec<&muqun_gateway::platform::discovery::AgentDiscoveryInfo> = ranked
        .iter()
        .filter(|(_, st)| *st == super::agents::Standing::Usable)
        .map(|(a, _)| a)
        .collect();
    if usable.is_empty() {
        let language = s.cfg.general.language;
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
    let mut line = row().key("agents").w_full().gap(8.0).flex_wrap().center_y();
    for a in usable {
        let selected = a.id == s.start_agent;
        let id = a.id.clone();
        let label = super::agents::name(&a.id, &s.agents);
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
                let label = label.clone();
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
    line
}

fn folder(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let start = {
        let typed = s.start_cwd.text.trim();
        (!typed.is_empty()).then(|| PathBuf::from(typed))
    };
    let mut block = col().key("folder").w_full().gap(8.0).child(
        row()
            .key("folder-input")
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&s.start_cwd)
                    .placeholder(tr_ref(language, "Project folder", "项目文件夹"))
                    .label(tr_ref(language, "Project folder", "项目文件夹"))
                    .on_edit(|s: &mut UiState| &mut s.start_cwd)
                    .on_change(|s: &mut UiState, _: &mut Tx| {
                        let prefix = s.start_cwd.text.clone();
                        s.send(Command::CompleteDirectory { prefix });
                    })
                    .build(k)
                    .key("cwd")
                    .grow(1.0),
            )
            .child(
                Button::new(tr_ref(language, "Choose folder…", "选择文件夹…"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .on_press(move |_: &mut UiState| {
                        crate::platform::dialogs::choose_project(start.clone());
                    })
                    .build(k)
                    .key("choose-folder"),
            ),
    );
    let mut chips = row().key("folder-chips").w_full().gap(6.0).flex_wrap();
    for entry in s.completions.iter().take(5) {
        let path = entry.path.clone();
        chips = chips.child(
            Button::new(entry.name.clone())
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .help(entry.path.clone())
                .on_press(move |s: &mut UiState| pick_folder_path(s, path.clone()))
                .build(k)
                .key(format!("comp-{}", entry.path)),
        );
    }
    for project in s
        .agent_projects
        .get(&s.start_agent)
        .into_iter()
        .flatten()
        .filter(|p| !p.missing)
        .take(6)
    {
        let path = project.canonical.clone();
        chips = chips.child(
            Button::new(project.name.clone())
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .help(project.canonical.clone())
                .on_press(move |s: &mut UiState| pick_folder_path(s, path.clone()))
                .build(k)
                .key(format!("proj-{}", project.id)),
        );
    }
    block = block.child(chips);
    block.child(
        Switch::new(s.start_worktree)
            .label(tr_ref(
                language,
                "Start in a new git worktree",
                "在新的 git worktree 中开始",
            ))
            .on_toggle(|s: &mut UiState| s.start_worktree = !s.start_worktree)
            .build(k)
            .key("worktree"),
    )
}

/// Model, mode and branch for the session about to start.
fn start_pills(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let language = s.cfg.general.language;
    let view = s.start_catalog.as_deref();
    let models = view.map(catalog_models).unwrap_or_default();
    let defaults = view.map(|v| &v.catalog.defaults);
    let picked = s
        .start_model
        .as_ref()
        .or_else(|| defaults.and_then(|d| d.model.as_ref()));
    let mut line = row()
        .key("start-pills")
        .w_full()
        .gap(8.0)
        .center_y()
        .flex_wrap();
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
    let cwd = s.start_cwd.text.trim();
    if let Some(branch) = s.branches.get(cwd).and_then(|b| {
        branch_menu(
            k,
            layer,
            OpenMenu::StartBranch,
            s.menu == Some(OpenMenu::StartBranch),
            cwd,
            b,
        )
    }) {
        line = line.child(branch);
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

/// A section's heading: a word, a count, and room for an action.
fn heading(k: &Kit, key: &str, title: String, count: Option<usize>, action: Option<El>) -> El {
    let mut line = row()
        .key(format!("{key}-head"))
        .w_full()
        .gap(8.0)
        .center_y()
        .child(text(title).font(k.ty.body_medium).color(k.c.text_base));
    if let Some(n) = count {
        line = line.child(
            row()
                .h(18.0)
                .px(6.0)
                .center()
                .radius(9.0)
                .fill(k.c.layer_03)
                .child(
                    text(n.to_string())
                        .font(k.ty.small_medium)
                        .color(k.c.text_muted),
                )
                .a11y_hidden(),
        );
    }
    line = line.child(el().grow(1.0).a11y_hidden());
    if let Some(a) = action {
        line = line.child(a);
    }
    line
}

/// A small "open beside" button for a session card: the theme's arrow when
/// its pack draws one, turned to point right.
fn split_button(s: &UiState, k: &Kit, asid: &str) -> El {
    let id = asid.to_string();
    let language = s.cfg.general.language;
    let label = tr_ref(language, "Open in split", "在分屏中打开");
    let help = tr_ref(
        language,
        "Open beside the current session",
        "在当前会话旁打开",
    );
    let themed = s.images.as_ref().and_then(|images| {
        super::art::glyph_toward(images, "home.arrow", 14.0, k.c.text_muted, Some(0.0))
    });
    let button = match themed {
        Some(glyph) => k
            .interactive(
                el().size(24.0, 24.0)
                    .center()
                    .radius(k.radius.sm)
                    .role("button")
                    .label(label)
                    .help(help)
                    .child(glyph),
            )
            .on_press(move |s: &mut UiState| s.open_in_tile(&id)),
        None => IconButton::new(&ARROW_RIGHT)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(label)
            .help(help)
            .on_press(move |s: &mut UiState| s.open_in_tile(&id))
            .build(k),
    };
    button.key(format!("split-{asid}")).group_opacity(0.0, 1.0)
}

/// The cards for every permission and form waiting on the owner, with the
/// same Allow / Deny / Submit controls as the session shows.
fn needs_you(s: &UiState, k: &Kit, look: &Look) -> Option<El> {
    let language = s.cfg.general.language;
    let sessions = needing(s);
    if sessions.is_empty() {
        return None;
    }
    let now = now_ms();
    let mut section = col().key("needs").w_full().gap(10.0).child(heading(
        k,
        "needs",
        tr(language, "Needs you", "等你处理"),
        Some(s.waiting_count()),
        Some(
            Button::new(tr_ref(language, "Inbox", "收件箱"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.go(Mode::Inbox))
                .build(k)
                .key("needs-inbox"),
        ),
    ));
    for info in sessions.into_iter().take(3) {
        let asid = info.asid.0.clone();
        let warn = k.c.warning.icon;
        let mut card = col()
            .key(format!("need-{asid}"))
            .w_full()
            .gap(8.0)
            .px(14.0)
            .py(12.0)
            .hover_group();
        // Who is asking.
        let open_id = asid.clone();
        card = card.child(
            row()
                .key("who")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(look.pulse(look.spark(el().size(8.0, 8.0).radius(4.0).fill(warn), warn)))
                .child(super::agents::mark(&info.agent_id, 14.0, s.dark).a11y_hidden())
                .child(
                    text(title_of(info))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                )
                .child(
                    text(format!(
                        "{} · {}",
                        s.agent_name(&info.agent_id),
                        ago(info.updated_ms, now, language)
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
                )
                .child(el().grow(1.0))
                .child(
                    Button::new(tr_ref(language, "Open", "打开"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| s.open_session(&open_id))
                        .build(k)
                        .key(format!("need-open-{asid}")),
                ),
        );
        for request in s.waiting_perms.get(&asid).into_iter().flatten().take(2) {
            card = card.child(timeline::permission_card(s, k, request));
        }
        for form in s.waiting_forms.get(&asid).into_iter().flatten().take(1) {
            card = card.child(timeline::form_card(s, k, form));
        }
        let card = look.card(k, card, CARD_RADIUS);
        section = section.child(look.glow(card, warn, 0.25));
    }
    Some(section)
}

/// One session as a card: mark, title, where it runs, when, and what it does.
fn session_card(s: &UiState, k: &Kit, look: &Look, info: &AgentSessionInfo, now: u64) -> El {
    let language = s.cfg.general.language;
    let asid = info.asid.0.clone();
    let activity = s.activity(info);
    let brand = super::agents::color(&info.agent_id, s.dark);
    let pinned = s.cfg.general.pinned.contains(&asid);
    let mut place = Vec::new();
    if let Some(folder) = UiState::folder_name(info) {
        place.push(folder);
    }
    if let Some(branch) = s.branch_of(info) {
        place.push(branch);
    }
    let when = ago(info.updated_ms, now, language);
    let mut meta = vec![s.agent_name(&info.agent_id)];
    if !place.is_empty() {
        meta.push(place.join(" @ "));
    }
    if !when.is_empty() {
        meta.push(when);
    }
    let open_id = asid.clone();
    let mut card = row()
        .key(format!("card-{asid}"))
        .w_full()
        .h(64.0)
        .px(14.0)
        .gap(12.0)
        .center_y()
        .hover_group()
        .role("button")
        .label(title_of(info))
        .on_press_tx(move |s: &mut UiState, tx: &mut Tx| {
            let mods = tx.mods();
            if mods.alt || mods.shift {
                s.open_in_tile(&open_id);
            } else {
                s.open_session(&open_id);
            }
        })
        .child(
            el().key("card-mark")
                .size(32.0, 32.0)
                .radius(9.0)
                .fill(brand.alpha(0.14))
                .center()
                .child(super::agents::mark(&info.agent_id, 18.0, s.dark))
                .a11y_hidden(),
        )
        .child(
            col()
                .key("card-words")
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(format!(
                        "{}{}",
                        if pinned { "★ " } else { "" },
                        title_of(info)
                    ))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .truncate(),
                )
                .child(
                    text(meta.join(" · "))
                        .font(k.ty.caption())
                        .color(k.c.text_faint)
                        .truncate(),
                ),
        );
    if activity != Activity::Idle {
        card = card.child(activity_pill(k, look, activity, language));
    }
    card = card.child(split_button(s, k, &asid));
    let card = look.pressable(k, look.row_card(k, card, CARD_RADIUS));
    match activity {
        Activity::Working => look.glow(card.border(1.0, brand.alpha(0.45)), brand, 0.35),
        Activity::NeedsYou => card.border(1.0, k.c.warning.icon.alpha(0.5)),
        _ => card,
    }
}

/// Sessions working right now.
fn running(s: &UiState, k: &Kit, look: &Look) -> Option<El> {
    let language = s.cfg.general.language;
    let list = s.running();
    if list.is_empty() {
        return None;
    }
    let now = now_ms();
    let mut grid = row().key("running-grid").w_full().gap(10.0).flex_wrap();
    for info in list.iter().take(4) {
        grid = grid.child(session_card(s, k, look, info, now).w(440.0).grow(1.0));
    }
    Some(
        col()
            .key("running")
            .w_full()
            .gap(10.0)
            .child(heading(
                k,
                "running",
                tr(language, "Running now", "正在进行"),
                Some(list.len()),
                None,
            ))
            .child(grid),
    )
}

/// Recent sessions, pinned first, as a two-column grid.
fn recent(s: &UiState, k: &Kit, look: &Look) -> Option<El> {
    let language = s.cfg.general.language;
    if s.sessions.is_empty() {
        return None;
    }
    let now = now_ms();
    let pinned = &s.cfg.general.pinned;
    let running: Vec<&str> = s.running().iter().map(|i| i.asid.0.as_str()).collect();
    let mut sessions: Vec<_> = s
        .sessions
        .iter()
        .filter(|x| !running.contains(&x.asid.0.as_str()))
        .collect();
    sessions.sort_by_key(|x| !pinned.contains(&x.asid.0));
    if sessions.is_empty() {
        return None;
    }
    let mut grid = row().key("recent-grid").w_full().gap(10.0).flex_wrap();
    for info in sessions.into_iter().take(6) {
        grid = grid.child(session_card(s, k, look, info, now).w(440.0).grow(1.0));
    }
    Some(
        col()
            .key("recent")
            .w_full()
            .gap(10.0)
            .child(heading(
                k,
                "recent",
                tr(language, "Recent", "最近"),
                None,
                Some(muted(
                    k,
                    tr(
                        language,
                        "Shift-click opens in a split",
                        "Shift 点击在分屏中打开",
                    ),
                )),
            ))
            .child(grid),
    )
}

/// The start form: agent, folder, choices, first prompt.
fn quick_start(s: &UiState, k: &Kit, look: &Look, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let mut card = col()
        .key("home-card")
        .w_full()
        .gap(14.0)
        .px(22.0)
        .py(20.0)
        .children(composer_art(s))
        .child(heading(
            k,
            "start",
            tr(language, "Start something", "开始新任务"),
            None,
            s.pending_tile.map(|i| {
                rune_kit::Badge::new(format!(
                    "{} {}",
                    tr_ref(language, "into tile", "进入分屏"),
                    i + 1
                ))
                .variant(rune_kit::BadgeVariant::Accent)
                .build(k)
            }),
        ))
        .child(agents(s, k))
        .child(folder(s, k));
    if let Some(pills) = start_pills(s, k, layer) {
        card = card.child(pills);
    }
    composer::popup(s, k, layer, Slot::Start);
    let placeholder = tr_ref(
        language,
        "Describe the task…  (/ commands, $ skills, @ files)",
        "描述任务…（/ 命令，$ 技能，@ 文件）",
    );
    let accent = k.c.bg_accent;
    let info = k.c.info.icon;
    card = card
        .child(
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
                .on_input_paste(|s: &mut UiState, content, _: &mut Tx| {
                    composer::take_paste(s, content)
                })
                .build(k)
                .key("start-draft")
                .w_full(),
        )
        .child(
            row()
                .key("start-actions")
                .w_full()
                .gap(10.0)
                .center_y()
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
                )
                .child(muted(
                    k,
                    format!(
                        "{} {}",
                        crate::platform::keys::label("Primary+Enter"),
                        tr_ref(language, "to start", "开始")
                    ),
                )),
        );
    look.accent_rim(k, look.panel(k, card, PANEL_RADIUS), k.c.bg_accent)
}

/// The Home hero's height for a card `h` points tall: a tall band when the
/// pack presents Home as a cover, a compact header otherwise.
fn hero_height(card_h: f32, cover_header: bool) -> f32 {
    if cover_header {
        (card_h * 0.42).clamp(HERO_H, 600.0)
    } else {
        (card_h * 0.3).clamp(HERO_H - 30.0, 420.0)
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

/// The theme's world on Home, composed for the card `w`×`hero` band: a
/// cut-out figure stands whole on the right, its inner edges melting into
/// the page; a painting fills the band's far side and dissolves toward the
/// greeting, so its subject is never under text. Behind it all, a scrim on
/// the greeting's side keeps the window's wallpaper from fighting the title.
fn hero_art(s: &UiState, k: &Kit, look: &Look, w: f32, hero: f32) -> Vec<El> {
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
        // A painting: the far 62% of the band and a little below, its near
        // edge and foot dissolving, cropped around its focal point.
        let bw = (w * 0.62).max(w - 560.0).min(w);
        let bh = hero + 60.0;
        layers.push(
            super::art::cover(images, &art, "home-figure", bw, bh)
                .clip_fade(bw * 0.4, 0.0, 0.0, bh * 0.4)
                .pin(None, Some(0.0), Some(0.0), None)
                .opacity(art.look.opacity),
        );
        return layers;
    }
    // A cut-out: whole, as large as the band allows, on the side away from
    // the greeting. Its own transparency shapes it; only the edges that face
    // the page fade, so a square-cut figure never ends on a line.
    let room_w = (w * 0.52).min(hero * 1.9);
    let (figure, [fw, fh]) = super::art::contained(images, &art, room_w, hero - 16.0);
    let right = (w * 0.04).clamp(16.0, 72.0);
    // The light reaches past the band, so its falloff never meets an edge.
    let reach = hero * 1.6;
    let cx = (w - right - fw * 0.5) / w;
    let cy = (hero - fh * 0.55) / reach;
    layers.push(
        look.light(
            "home-light",
            [cx, cy],
            [fw * 0.75, fh * 0.75],
            k.c.bg_accent,
        )
        .pin(Some(0.0), Some(0.0), Some(0.0), None)
        .h(reach),
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
    let size = (card_w / 34.0).clamp(36.0, 46.0);
    let title_font = rune::TextStyle {
        size,
        line_height: size * 1.22,
        ..k.ty.ui_medium
    };
    let id = crate::theme::apply::identity();
    let hero_h = hero_height(card_h, id.cover_header);

    let mut hero = col()
        .key("home-hero")
        .w_full()
        .max_w(COLUMN)
        .px(40.0)
        .h(hero_h)
        .justify_end()
        .gap(6.0)
        .pb(6.0);
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
        .children(hero_art(s, k, &look, card_w, hero_h))
        .child(look.arrive(hero, 0));

    let mut column = col()
        .key("home")
        .w_full()
        .max_w(COLUMN)
        .gap(26.0)
        .px(40.0)
        .pt(26.0)
        .pb(40.0);
    let mut step = 1;
    if let Some(needs) = needs_you(s, k, &look) {
        column = column.child(look.arrive(needs, step));
        step += 1;
    }
    if let Some(running) = running(s, k, &look) {
        column = column.child(look.arrive(running, step));
        step += 1;
    }
    column = column.child(look.arrive(quick_start(s, k, &look, layer), step));
    step += 1;
    if let Some(recent) = recent(s, k, &look) {
        column = column.child(look.arrive(recent, step));
    }

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
