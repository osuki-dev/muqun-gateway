//! The session composer: draft, attachments, queue, model / mode / branch
//! pills, and the `@` and `/` popups.

use std::path::PathBuf;

use muqun_gateway::agents::{AgentSessionId, ModelRef};
use rune::{col, row, El, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem, PopupItem,
    PopupList, SegmentedControl, Switch, TextArea,
};

use super::{muted, OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};

/// The most file hits the `@` popup lists.
const MAX_FILE_ROWS: usize = 8;
/// The most commands and skills the `/` popup lists.
const MAX_SLASH_ROWS: usize = 12;

fn fresh(text: String) -> TextEdit {
    let mut edit = TextEdit::new(text);
    edit.focused = true;
    edit
}

pub fn stage_png(s: &mut UiState, png: Vec<u8>) {
    let id = s.next_token;
    s.next_token += 1;
    s.send(Command::Compose(ComposeCommand::Stage {
        id,
        name: "pasted.png".into(),
        source: Source::Png(png),
    }));
}

pub fn stage_path(s: &mut UiState, path: PathBuf) {
    let id = s.next_token;
    s.next_token += 1;
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    s.send(Command::Compose(ComposeCommand::Stage {
        id,
        name,
        source: Source::Path(path),
    }));
}

/// What `text` delivers: a host command or skill when it starts with a
/// `/name` the agent's catalogue knows, else a prompt.
fn deliver_of(s: &UiState, text: &str) -> Deliver {
    let Some(rest) = text.strip_prefix('/') else {
        return Deliver::Prompt(text.to_string());
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim().to_string()),
        None => (rest, String::new()),
    };
    let Some(view) = s.catalog.as_ref() else {
        return Deliver::Prompt(text.to_string());
    };
    if view.catalog.commands.iter().any(|c| c.name == name) {
        return Deliver::Command {
            name: name.to_string(),
            args,
        };
    }
    if let Some(skill) = view
        .catalog
        .skills
        .iter()
        .find(|k| k.slash && (k.id == name || k.name == name))
    {
        return Deliver::Skill {
            name: skill.id.clone(),
            args,
        };
    }
    Deliver::Prompt(text.to_string())
}

pub fn submit_session(s: &mut UiState) {
    let text = s.draft.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(asid) = s.selected.clone() else {
        s.status = "Pick a session first".into();
        return;
    };
    let deliver = deliver_of(s, &text);
    let delivery = s.busy().then(|| "queue".to_string());
    let attachments = s.staged.iter().map(|f| f.path.clone()).collect();
    let token = s.token(Slot::Session);
    s.status = "Sending…".into();
    s.send(Command::Compose(ComposeCommand::Submit {
        token,
        target: Target::Session(AgentSessionId(asid)),
        deliver,
        attachments,
        delivery,
    }));
}

/// The `@word` the caret ends on, if any.
fn mention(text: &str) -> Option<&str> {
    let word = text.rsplit(char::is_whitespace).next()?;
    word.strip_prefix('@')
}

/// The `/word` the draft is, when it is only that.
fn slash(text: &str) -> Option<&str> {
    text.strip_prefix('/')
        .filter(|q| !q.contains(char::is_whitespace))
}

fn on_draft_change(s: &mut UiState, _: &mut Tx) {
    s.persist_draft(Slot::Session);
    let Some(query) = mention(&s.draft.text).map(str::to_string) else {
        s.file_hits.clear();
        return;
    };
    let Some(info) = s.current() else { return };
    let (agent, directory) = (
        info.agent_id.clone(),
        info.directory.clone().unwrap_or_default(),
    );
    let generation = s.next_token;
    s.next_token += 1;
    s.send(Command::Compose(ComposeCommand::FindFiles {
        generation,
        agent,
        directory,
        query,
    }));
}

/// Replace the `@word` at the end of the draft with `insert` and a space.
fn insert_mention(s: &mut UiState, insert: &str) {
    let text = &s.draft.text;
    let head = text.rfind('@').map_or(text.as_str(), |at| &text[..at]);
    s.draft = fresh(format!("{head}{insert} "));
    s.file_hits.clear();
    s.persist_draft(Slot::Session);
}

fn chips(s: &UiState, k: &Kit) -> Option<El> {
    if s.staged.is_empty() && s.queue.is_empty() {
        return None;
    }
    let mut line = row().key("chips").w_full().gap(6.0).wrap().center_y();
    for (i, file) in s.staged.iter().enumerate() {
        line = line.child(
            Button::new(format!("{} ✕", file.name))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    if i < s.staged.len() {
                        s.staged.remove(i);
                    }
                })
                .build(k)
                .key(format!("staged-{i}")),
        );
    }
    for item in &s.queue {
        let (id, asid) = (item.id.clone(), s.selected.clone());
        let label = format!(
            "{} {}",
            if item.steer { "Steer:" } else { "Queued:" },
            super::timeline::clip(&item.text, 32)
        );
        line = line.child(
            Button::new(format!("{label} ✕"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    if let Some(asid) = asid.clone() {
                        s.queue.retain(|q| q.id != id);
                        s.send(Command::Compose(ComposeCommand::InboxCancel {
                            asid: AgentSessionId(asid),
                            item: id.clone(),
                        }));
                    }
                })
                .build(k)
                .key(format!("queued-{}", item.id)),
        );
    }
    Some(line)
}

// ---- popups -------------------------------------------------------------------------

/// Computers first (this one, reachable, offline), then files; ids say which.
fn mention_items(s: &UiState, query: &str) -> Vec<PopupItem> {
    let wanted = query.trim().to_lowercase();
    let mut peers: Vec<_> = s
        .peers
        .iter()
        .filter(|p| wanted.is_empty() || p.name.to_lowercase().starts_with(&wanted))
        .collect();
    peers.sort_by_key(|p| match (p.current, p.reachable) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    });
    let mut items: Vec<PopupItem> = peers
        .into_iter()
        .map(|p| {
            let detail = if p.current {
                "This computer"
            } else if p.reachable {
                "Computer"
            } else {
                "Offline"
            };
            PopupItem::new(format!("p:{}", p.name), format!("@{}", p.name)).description(detail)
        })
        .collect();
    items.extend(s.file_hits.iter().take(MAX_FILE_ROWS).map(|h| {
        PopupItem::new(format!("f:{}", h.path), h.name.clone()).description(h.path.clone())
    }));
    items
}

/// Commands, then skills the agent offers as slash entries, narrowed by `query`.
fn slash_items(s: &UiState, query: &str) -> Vec<PopupItem> {
    let Some(view) = s.catalog.as_ref() else {
        return Vec::new();
    };
    let wanted = query.to_lowercase();
    let fits = |name: &str| {
        wanted.is_empty() || {
            let name = name.to_lowercase();
            name.starts_with(&wanted) || name.contains(&wanted)
        }
    };
    let commands = view
        .catalog
        .commands
        .iter()
        .filter(|c| fits(&c.name))
        .map(|c| {
            PopupItem::new(format!("c:{}", c.name), format!("/{}", c.name))
                .description(c.description.clone().unwrap_or_default())
        });
    let skills = view
        .catalog
        .skills
        .iter()
        .filter(|k| k.slash && fits(&k.id))
        .map(|k| {
            PopupItem::new(format!("s:{}", k.id), format!("/{}", k.id))
                .description(k.description.clone())
        });
    commands.chain(skills).take(MAX_SLASH_ROWS).collect()
}

/// Mount whichever popup the draft calls for, above the composer.
fn popup(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    if let Some(query) = slash(&s.draft.text) {
        let items = slash_items(s, query);
        if !items.is_empty() {
            PopupList::<UiState>::new("slash-popup", "composer")
                .width(360.0)
                .label("Commands")
                .items(items)
                .on_choose(|s: &mut UiState, id: &str, _: &mut Tx| {
                    let name = id.split_once(':').map_or(id, |(_, n)| n);
                    s.draft = fresh(format!("/{name} "));
                    s.persist_draft(Slot::Session);
                })
                .build(k, layer);
        }
        return;
    }
    if let Some(query) = mention(&s.draft.text) {
        let items = mention_items(s, query);
        if !items.is_empty() {
            PopupList::<UiState>::new("mention-popup", "composer")
                .width(360.0)
                .label("Mentions")
                .items(items)
                .on_choose(
                    |s: &mut UiState, id: &str, _: &mut Tx| match id.split_once(':') {
                        Some(("p", name)) => insert_mention(s, &format!("@{name}")),
                        Some(("f", path)) => insert_mention(s, path),
                        _ => {}
                    },
                )
                .build(k, layer);
        }
    }
}

// ---- pills --------------------------------------------------------------------------

/// Remember what the pills show for the open session's agent and folder, so a
/// new session there starts with it.
fn remember(s: &mut UiState) {
    let Some(info) = s.current().cloned() else {
        return;
    };
    let Some(cwd) = info.directory else { return };
    s.edit(ConfigEdit::Remember {
        agent: info.agent_id,
        cwd,
        model: info
            .model
            .map(|m| format!("{}/{}", m.provider_id, m.model_id)),
        mode: info.mode,
    });
}

fn switch_model(s: &mut UiState, provider_id: String, model_id: String) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    let model = ModelRef {
        provider_id,
        model_id,
        variant: None,
    };
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == asid) {
        info.model = Some(model.clone());
    }
    s.send(Command::Compose(ComposeCommand::SwitchModel {
        asid: AgentSessionId(asid),
        model,
    }));
    remember(s);
}

fn switch_mode(s: &mut UiState, mode: String) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == asid) {
        info.mode = Some(mode.clone());
    }
    s.send(Command::Compose(ComposeCommand::SwitchMode {
        asid: AgentSessionId(asid),
        mode,
    }));
    remember(s);
}

/// A pill-sized button that opens `menu` (toggling it).
pub fn pill(k: &Kit, key: &str, label: String, open: bool, menu: OpenMenu) -> El {
    Button::new(label)
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Small)
        .expanded(open)
        .on_press(move |s: &mut UiState| s.menu = (!open).then(|| menu.clone()))
        .build(k)
        .key(key.to_string())
}

/// The model menu: every enabled model of the agent's catalogue, by provider.
/// `on_pick(state, provider, model)` runs on a choice.
#[allow(clippy::too_many_arguments)]
pub fn model_menu(
    k: &Kit,
    layer: &mut Vec<El>,
    kind: OpenMenu,
    open: bool,
    label: String,
    models: &[(String, String, String)],
    current: Option<(&str, &str)>,
    on_pick: impl Fn(&mut UiState, String, String) + Clone + 'static,
) -> El {
    let key = match kind {
        OpenMenu::StartModel => "start-model",
        _ => "model",
    };
    let mut menu = Menu::new(key)
        .trigger(pill(k, &format!("{key}-pill"), label, open, kind))
        .open(open)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.menu = None);
    let mut last: Option<&str> = None;
    for (provider, id, name) in models {
        if last != Some(provider.as_str()) {
            menu = menu.group_label(provider.clone());
            last = Some(provider.as_str());
        }
        let on = current == Some((provider.as_str(), id.as_str()));
        let (p, m, pick) = (provider.clone(), id.clone(), on_pick.clone());
        menu = menu.item(
            MenuItem::new(name.clone())
                .checked(on)
                .on_select(move |s: &mut UiState| pick(s, p.clone(), m.clone())),
        );
    }
    menu.build(k, layer)
}

/// The branch menu: the folder's local branches, the checked-out one ticked.
pub fn branch_menu(
    k: &Kit,
    layer: &mut Vec<El>,
    kind: OpenMenu,
    open: bool,
    cwd: &str,
    info: &crate::model::compose::BranchInfo,
) -> Option<El> {
    let current = info.current.clone()?;
    let key = match kind {
        OpenMenu::StartBranch => "start-branch",
        _ => "branch",
    };
    let label = format!("⎇ {current}");
    if info.local.len() < 2 {
        return Some(
            Badge::new(label)
                .variant(BadgeVariant::Neutral)
                .build(k)
                .key(key),
        );
    }
    let mut menu = Menu::new(key)
        .trigger(pill(k, &format!("{key}-pill"), label, open, kind))
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None);
    for branch in &info.local {
        let (cwd, name) = (cwd.to_string(), branch.clone());
        menu = menu.item(
            MenuItem::new(branch.clone())
                .checked(*branch == current)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::Compose(ComposeCommand::Checkout {
                        cwd: cwd.clone(),
                        branch: name.clone(),
                    }));
                }),
        );
    }
    Some(menu.build(k, layer))
}

/// Catalogue models as `(provider, id, name)`, enabled ones only.
pub fn catalog_models(view: &crate::hub::CatalogView) -> Vec<(String, String, String)> {
    view.catalog
        .models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| (m.provider_id.clone(), m.id.clone(), m.name.clone()))
        .collect()
}

fn pills(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mut line = row().key("pills").gap(8.0).center_y().wrap();
    let Some(info) = s.current() else { return line };
    let models = s.catalog.as_deref().map(catalog_models).unwrap_or_default();
    if let Some(model) = &info.model {
        let name = models
            .iter()
            .find(|(p, id, _)| *p == model.provider_id && *id == model.model_id)
            .map_or_else(|| model.model_id.clone(), |(_, _, n)| n.clone());
        line = line.child(if models.is_empty() {
            Badge::new(name)
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("model")
        } else {
            model_menu(
                k,
                layer,
                OpenMenu::Model,
                s.menu == Some(OpenMenu::Model),
                name,
                &models,
                Some((model.provider_id.as_str(), model.model_id.as_str())),
                switch_model,
            )
        });
    } else if !models.is_empty() {
        line = line.child(model_menu(
            k,
            layer,
            OpenMenu::Model,
            s.menu == Some(OpenMenu::Model),
            "Model".into(),
            &models,
            None,
            switch_model,
        ));
    }
    if let Some(cwd) = &info.directory {
        if let Some(branch) = s.branches.get(cwd).and_then(|b| {
            branch_menu(
                k,
                layer,
                OpenMenu::Branch,
                s.menu == Some(OpenMenu::Branch),
                cwd,
                b,
            )
        }) {
            line = line.child(branch);
        }
    }
    let modes: Vec<(String, String)> = s
        .catalog
        .as_ref()
        .map(|c| {
            c.catalog
                .modes
                .iter()
                .filter(|m| !m.hidden)
                .map(|m| (m.id.clone(), m.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    if (2..=4).contains(&modes.len()) {
        let labels: Vec<&str> = modes.iter().map(|(_, n)| n.as_str()).collect();
        let at = info
            .mode
            .as_ref()
            .and_then(|m| modes.iter().position(|(id, _)| id == m));
        let ids: Vec<String> = modes.iter().map(|(id, _)| id.clone()).collect();
        line = line.child(
            SegmentedControl::new(&labels, at)
                .fit()
                .label("Mode")
                .on_pick(move |s: &mut UiState, i, _: &mut Tx| switch_mode(s, ids[i].clone()))
                .build(k)
                .key("modes"),
        );
    } else if let Some(mode) = &info.mode {
        line = line.child(
            Badge::new(mode.clone())
                .variant(BadgeVariant::Neutral)
                .build(k)
                .key("mode"),
        );
    }
    let on = s.auto_approve.get(&info.asid.0).copied().unwrap_or(false);
    let asid = info.asid.clone();
    line.child(
        Switch::new(on)
            .label("Auto-approve")
            .on_toggle(move |s: &mut UiState| {
                s.send(Command::Compose(ComposeCommand::SetAutoApprove {
                    asid: asid.clone(),
                    enabled: !on,
                }));
            })
            .build(k)
            .key("auto-approve"),
    )
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let busy = s.busy();
    let start = s
        .current()
        .and_then(|i| i.directory.clone().map(PathBuf::from));
    let mut actions = row()
        .key("composer-actions")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(pills(s, k, layer).grow(1.0))
        .child(
            Button::new("Attach")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Large)
                .disabled(s.selected.is_none())
                .on_press(move |_: &mut UiState| {
                    crate::platform::dialogs::attach_files(start.clone());
                })
                .build(k)
                .key("attach"),
        )
        .child(
            Button::new("Paste image")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Large)
                .on_press(
                    |s: &mut UiState| match crate::platform::clipboard::read_image_png() {
                        Some(png) => stage_png(s, png),
                        None => s.toast("Clipboard has no image", None),
                    },
                )
                .build(k)
                .key("paste-image"),
        );
    if busy {
        if let Some(asid) = s.selected.clone() {
            actions = actions.child(
                Button::new("Stop")
                    .variant(ButtonVariant::Danger)
                    .control_size(ControlSize::Large)
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::Interrupt(AgentSessionId(asid.clone())))
                    })
                    .build(k)
                    .key("stop"),
            );
        }
    }
    actions = actions.child(
        Button::new(if busy { "Queue" } else { "Send" })
            .variant(ButtonVariant::Submit)
            .control_size(ControlSize::Large)
            .disabled(s.selected.is_none() || s.pending_submit.is_some())
            .on_press(submit_session)
            .build(k)
            .key("send"),
    );
    popup(s, k, layer);
    let mut composer = col().key("composer").w_full().px(24.0).py(12.0).gap(8.0);
    if let Some(chips) = chips(s, k) {
        composer = composer.child(chips);
    }
    composer
        .child(
            TextArea::new(&s.draft)
                .placeholder(if s.selected.is_some() {
                    "Message…  (@ to mention, / for commands)"
                } else {
                    "Pick a session first"
                })
                .label("Draft")
                .width(None)
                .auto_grow(2, 8)
                .on_edit(|s: &mut UiState| &mut s.draft)
                .on_change(on_draft_change)
                .on_submit(|s: &mut UiState, _: &mut Tx| submit_session(s))
                .build(k)
                .key("draft")
                .w_full(),
        )
        .child(actions)
        .child(muted(
            k,
            format!("{} to send", crate::platform::keys::label("Primary+Enter")),
        ))
}
