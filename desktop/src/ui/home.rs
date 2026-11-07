//! The start page: pick an agent and a folder, write the first prompt.

use std::path::PathBuf;

use rune::{col, row, text, El, ScrollSpec, Tx};
use rune_kit::shell::SidebarRow;
use rune_kit::{
    Button, ButtonVariant, ControlSize, Kit, ScrollBar, SegmentedControl, Switch, TextArea,
    TextInput,
};

use super::{muted, title_of, Slot, UiState};
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Target};

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
    let attachments = s.staged.iter().map(|f| f.path.clone()).collect();
    let token = s.token(Slot::Start);
    s.status = "Starting…".into();
    s.send(Command::Compose(ComposeCommand::Submit {
        token,
        target: Target::New {
            agent,
            cwd,
            model: None,
            mode: None,
            worktree,
        },
        deliver: Deliver::Prompt(prompt),
        attachments,
        delivery: None,
    }));
}

fn pick_agent(s: &mut UiState, id: String) {
    s.start_cwd = rune::TextEdit::new(s.cfg.general.initial_cwd(&id).display().to_string());
    s.completions.clear();
    s.send(Command::LoadProjects { agent: id.clone() });
    s.start_agent = id;
}

fn agents(s: &UiState, k: &Kit) -> El {
    let usable: Vec<(&str, &str)> = s
        .agents
        .iter()
        .filter(|a| a.enabled)
        .map(|a| (a.id.as_str(), a.name.as_str()))
        .collect();
    if usable.is_empty() {
        return muted(k, "No agent is enabled. Turn one on in Settings → Agents.");
    }
    let labels: Vec<&str> = usable.iter().map(|(_, n)| *n).collect();
    let ids: Vec<String> = usable.iter().map(|(id, _)| id.to_string()).collect();
    let at = usable.iter().position(|(id, _)| *id == s.start_agent);
    SegmentedControl::new(&labels, at)
        .fit()
        .label("Agent")
        .on_pick(move |s: &mut UiState, i, _: &mut Tx| pick_agent(s, ids[i].clone()))
        .build(k)
        .key("agents")
}

fn folder(s: &UiState, k: &Kit) -> El {
    let mut block = col().key("folder").w_full().gap(8.0).child(
        TextInput::new(&s.start_cwd)
            .placeholder("Project folder")
            .label("Project folder")
            .on_edit(|s: &mut UiState| &mut s.start_cwd)
            .on_change(|s: &mut UiState, _: &mut Tx| {
                let prefix = s.start_cwd.text.clone();
                s.send(Command::CompleteDirectory { prefix });
            })
            .build(k)
            .key("cwd"),
    );
    let mut chips = row().key("folder-chips").w_full().gap(6.0).wrap();
    for entry in s.completions.iter().take(5) {
        let path = entry.path.clone();
        chips = chips.child(
            Button::new(entry.name.clone())
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .help(entry.path.clone())
                .on_press(move |s: &mut UiState| {
                    s.start_cwd = rune::TextEdit::new(path.clone());
                    s.completions.clear();
                })
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
                .on_press(move |s: &mut UiState| s.start_cwd = rune::TextEdit::new(path.clone()))
                .build(k)
                .key(format!("proj-{}", project.id)),
        );
    }
    block = block.child(chips);
    block.child(
        Switch::new(s.start_worktree)
            .label("Start in a new git worktree")
            .on_toggle(|s: &mut UiState| s.start_worktree = !s.start_worktree)
            .build(k)
            .key("worktree"),
    )
}

fn recent(s: &UiState, k: &Kit) -> Option<El> {
    if s.sessions.is_empty() {
        return None;
    }
    let pinned = &s.cfg.general.pinned;
    let mut sessions: Vec<_> = s.sessions.iter().collect();
    sessions.sort_by_key(|x| !pinned.contains(&x.asid.0));
    let mut list = col()
        .key("recent")
        .w_full()
        .gap(4.0)
        .child(muted(k, "Recent sessions"));
    for info in sessions.into_iter().take(5) {
        let asid = info.asid.0.clone();
        let pin = if pinned.contains(&asid) { "★ " } else { "" };
        list = list.child(
            SidebarRow::new(format!("{pin}{}", title_of(info)))
                .subtitle(s.agent_name(&info.agent_id))
                .height(45.0)
                .on_select(move |s: &mut UiState, _: &mut Tx| s.open_session(&asid))
                .build(k)
                .key(format!("recent-{}", info.asid.0)),
        );
    }
    Some(list)
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let mut page = col()
        .key("home")
        .w_full()
        .max_w(720.0)
        .gap(16.0)
        .px(24.0)
        .py(32.0)
        .child(
            text("What should we work on?")
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(agents(s, k))
        .child(folder(s, k))
        .child(
            TextArea::new(&s.start_draft)
                .placeholder("Describe the task…")
                .label("First prompt")
                .width(None)
                .auto_grow(4, 12)
                .on_edit(|s: &mut UiState| &mut s.start_draft)
                .on_submit(|s: &mut UiState, _: &mut Tx| submit_start(s))
                .build(k)
                .key("start-draft")
                .w_full(),
        )
        .child(
            row()
                .key("start-actions")
                .gap(8.0)
                .center_y()
                .child(
                    Button::new("Start")
                        .variant(ButtonVariant::Submit)
                        .control_size(ControlSize::Large)
                        .disabled(s.pending_submit.is_some())
                        .on_press(submit_start)
                        .build(k)
                        .key("start"),
                )
                .child(muted(
                    k,
                    format!("{} to start", crate::platform::keys::label("Primary+Enter")),
                )),
        );
    if let Some(recent) = recent(s, k) {
        page = page.child(recent);
    }
    col()
        .key("home-scroll")
        .w_full()
        .items_center()
        .child(page)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k)
}
