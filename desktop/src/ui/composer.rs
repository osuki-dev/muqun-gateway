//! The session composer: draft, attachments, queue, model and mode pills.

use std::path::PathBuf;

use muqun_gateway::agents::AgentSessionId;
use rune::{col, row, El, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, SegmentedControl, Switch,
    TextArea,
};

use super::{muted, Slot, UiState};
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};

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

pub fn submit_session(s: &mut UiState) {
    let text = s.draft.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(asid) = s.selected.clone() else {
        s.status = "Pick a session first".into();
        return;
    };
    let delivery = s.busy().then(|| "queue".to_string());
    let attachments = s.staged.iter().map(|f| f.path.clone()).collect();
    let token = s.token(Slot::Session);
    s.status = "Sending…".into();
    s.send(Command::Compose(ComposeCommand::Submit {
        token,
        target: Target::Session(AgentSessionId(asid)),
        deliver: Deliver::Prompt(text),
        attachments,
        delivery,
    }));
}

/// The `@word` the caret ends on, if any.
fn mention(text: &str) -> Option<&str> {
    let word = text.rsplit(char::is_whitespace).next()?;
    word.strip_prefix('@')
}

fn on_draft_change(s: &mut UiState, _: &mut Tx) {
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

fn insert_hit(s: &mut UiState, path: &str) {
    let text = &s.draft.text;
    let head = text.rfind('@').map_or(text.as_str(), |at| &text[..at]);
    s.draft = fresh(format!("{head}{path} "));
    s.file_hits.clear();
}

fn chips(s: &UiState, k: &Kit) -> Option<El> {
    if s.staged.is_empty() && s.file_hits.is_empty() && s.queue.is_empty() {
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
    for hit in s.file_hits.iter().take(6) {
        let path = hit.path.clone();
        line = line.child(
            Button::new(hit.name.clone())
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .help(hit.path.clone())
                .on_press(move |s: &mut UiState| insert_hit(s, &path))
                .build(k)
                .key(format!("hit-{}", hit.path)),
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

fn pills(s: &UiState, k: &Kit) -> El {
    let mut line = row().key("pills").gap(8.0).center_y().wrap();
    let Some(info) = s.current() else { return line };
    if let Some(model) = &info.model {
        line = line.child(
            Badge::new(model.model_id.clone())
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("model"),
        );
    }
    if let Some(cwd) = &info.directory {
        if let Some(branch) = s.branches.get(cwd).and_then(|b| b.current.clone()) {
            line = line.child(
                Badge::new(format!("⎇ {branch}"))
                    .variant(BadgeVariant::Neutral)
                    .build(k)
                    .key("branch"),
            );
        }
    }
    let modes: Vec<(String, String)> = s
        .catalog
        .as_ref()
        .map(|c| {
            c.catalog
                .modes
                .iter()
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
        let asid = info.asid.clone();
        let ids: Vec<String> = modes.iter().map(|(id, _)| id.clone()).collect();
        line = line.child(
            SegmentedControl::new(&labels, at)
                .fit()
                .label("Mode")
                .on_pick(move |s: &mut UiState, i, _: &mut Tx| {
                    s.send(Command::Compose(ComposeCommand::SwitchMode {
                        asid: asid.clone(),
                        mode: ids[i].clone(),
                    }));
                })
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

pub fn view(s: &UiState, k: &Kit) -> El {
    let busy = s.busy();
    let mut actions = row()
        .key("composer-actions")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(pills(s, k).grow(1.0))
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
    let mut composer = col().key("composer").w_full().px(24.0).py(12.0).gap(8.0);
    if let Some(chips) = chips(s, k) {
        composer = composer.child(chips);
    }
    composer
        .child(
            TextArea::new(&s.draft)
                .placeholder(if s.selected.is_some() {
                    "Message…"
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
