//! The session (and Home) composer: draft, attachments, queue, model / mode /
//! branch pills, and the `@` / `$` / `/` popups.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use muqun_gateway::agents::{AgentSessionId, ModelRef};
use rune::{col, el, row, El, ImageCache, PillStyle, SurfaceFit, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem, PopupItem,
    PopupList, TextArea,
};

use super::art;
use super::copy::tr_ref;
use super::{muted, OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};

/// Chip thumbnail size (wide enough to read the picture, short for the row).
const CHIP_THUMB_W: f32 = 56.0;
const CHIP_THUMB_H: f32 = 40.0;

/// The most file hits the `@` popup lists (App `FILE_MENTION_LIMIT`); the
/// list scrolls, as `/` and `$` (uncapped, as the App) do.
const MAX_FILE_ROWS: usize = 20;

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

/// Ctrl/Cmd+V into the draft: stage copied files or a clipboard image; leave
/// plain text to the editor. Returns true when the paste was consumed.
pub fn take_paste(s: &mut UiState, content: &rune::ClipboardContent) -> bool {
    let files = content.files();
    if !files.is_empty() {
        for path in files {
            stage_path(s, path.clone());
        }
        s.status = if files.len() == 1 {
            "Attached file".into()
        } else {
            format!("Attached {} files", files.len())
        };
        return true;
    }
    if let Some(image) = content.image() {
        stage_png(s, image.png().to_vec());
        s.status = "Attached image".into();
        return true;
    }
    false
}

/// Catalogue for the active composer surface.
fn catalog_of(s: &UiState, slot: Slot) -> Option<&crate::hub::CatalogView> {
    match slot {
        Slot::Session => s.catalog.as_deref(),
        Slot::Start => s.start_catalog.as_deref(),
    }
}

fn draft_of(s: &UiState, slot: Slot) -> &TextEdit {
    match slot {
        Slot::Session => &s.draft,
        Slot::Start => &s.start_draft,
    }
}

fn draft_of_mut(s: &mut UiState, slot: Slot) -> &mut TextEdit {
    match slot {
        Slot::Session => &mut s.draft,
        Slot::Start => &mut s.start_draft,
    }
}

/// What `text` delivers — App `readSlashCommand` / `readSkillInvocation`.
/// Attachments force a plain prompt (App: slash/skill only when `!hasAttachments`).
pub(super) fn deliver_of(
    catalog: Option<&crate::hub::CatalogView>,
    text: &str,
    has_attachments: bool,
) -> Deliver {
    use crate::model::composer_popup::{read_skill_invocation, read_slash_command, SlashCommand};
    if has_attachments {
        return Deliver::Prompt(text.to_string());
    }
    let Some(view) = catalog else {
        return Deliver::Prompt(text.to_string());
    };
    let command_names: Vec<&str> = view
        .catalog
        .commands
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let slash_skills: Vec<&str> = view
        .catalog
        .skills
        .iter()
        .filter(|k| k.slash)
        .map(|k| k.id.as_str())
        .collect();
    if let Some(cmd) = read_slash_command(text, &command_names, &slash_skills) {
        return match cmd {
            SlashCommand::Server { name, args } => Deliver::Command { name, args },
            SlashCommand::Skill { name, args } => Deliver::Skill { name, args },
        };
    }
    let all_skills: Vec<&str> = view.catalog.skills.iter().map(|k| k.id.as_str()).collect();
    if let Some((name, args)) = read_skill_invocation(text, &all_skills) {
        return Deliver::Skill { name, args };
    }
    Deliver::Prompt(text.to_string())
}

/// App toast: agent skills take no wire arguments.
pub(super) fn note_skill_args(s: &mut UiState, deliver: &Deliver) {
    let Deliver::Skill { args, .. } = deliver else {
        return;
    };
    if args.is_empty() {
        return;
    }
    s.toast(
        "Skill started",
        Some(format!(
            "“{args}” was not sent: a skill takes no arguments."
        )),
    );
}

/// Send the draft. While the agent works it steers the running turn, as on
/// the App; [`queue_session`] (Alt+Enter) waits for the turn to end instead.
/// A composer card (a [`Look::panel`](crate::theme::look::Look::panel))
/// while its field has focus: the surface a faint step toward the accent,
/// no light or rim. Working shows in the hint line under it, not on the card.
pub fn focus_tone(k: &Kit, look: &crate::theme::look::Look, card: El, focused: bool) -> El {
    if focused {
        card.fill(look.emphasis(look.panel_fill(k), k.c.bg_accent))
    } else {
        card
    }
}

/// The kit a composer's text field is built with: the field draws no box of
/// its own (fill, shadow, focus ring), so the composer card around it is the
/// one surface and the one focus mark.
pub fn bare_field(k: &Kit) -> Kit {
    let mut bare = k.clone();
    bare.c.bg_base = k.c.bg_base.alpha(0.0);
    bare.c.hover = k.c.hover.alpha(0.0);
    bare.c.border_focus = k.c.border_focus.alpha(0.0);
    bare.elevation.button_neutral = Vec::new();
    bare
}

/// No selection and the caret after the last character.
pub fn caret_at_end(edit: &rune::TextEdit) -> bool {
    edit.range() == (edit.text.len(), edit.text.len())
}

pub fn submit_session(s: &mut UiState) {
    send_session(s, false);
}

/// Send the draft to run after the current turn (Alt+Enter while busy).
pub fn queue_session(s: &mut UiState) {
    send_session(s, true);
}

fn send_session(s: &mut UiState, queue: bool) {
    let text = s.draft.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(asid) = s.selected.clone() else {
        s.status = "Pick a session first".into();
        return;
    };
    let attachments: Vec<String> = s.staged.iter().map(|f| f.path.clone()).collect();
    let deliver = deliver_of(catalog_of(s, Slot::Session), &text, !attachments.is_empty());
    note_skill_args(s, &deliver);
    let delivery = s
        .busy()
        .then(|| if queue { "queue" } else { "steer" }.to_string());
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

fn draft_caret(edit: &TextEdit) -> usize {
    edit.range().1
}

fn mention_search_target(s: &UiState, slot: Slot) -> Option<(String, String)> {
    match slot {
        Slot::Session => {
            let info = s.current()?;
            Some((
                info.agent_id.clone(),
                info.directory.clone().unwrap_or_default(),
            ))
        }
        Slot::Start => {
            if s.start_agent.is_empty() {
                return None;
            }
            let directory = {
                let typed = s.start_cwd.text.trim();
                if typed.is_empty() {
                    s.cfg
                        .general
                        .initial_cwd(&s.start_agent)
                        .display()
                        .to_string()
                } else {
                    typed.to_string()
                }
            };
            Some((s.start_agent.clone(), directory))
        }
    }
}

fn mention_tokens(s: &UiState) -> Vec<crate::model::composer_mentions::MentionToken> {
    use crate::model::composer_mentions::{MentionKind, MentionToken};
    let mut tokens: Vec<_> = s
        .peers
        .iter()
        .map(|p| MentionToken {
            text: format!("@{}", p.name),
            kind: MentionKind::Computer,
        })
        .collect();
    tokens.extend(s.mention_file_tokens.iter().map(|path| MentionToken {
        text: path.clone(),
        kind: MentionKind::File,
    }));
    tokens
}

/// Turn standing `@name` / file-path tokens into pills so they highlight and
/// backspace as one chip (Rune `Mark::Pill`).
fn ensure_mention_pills(s: &mut UiState, slot: Slot) {
    use crate::model::composer_mentions::{mention_ranges, MentionKind};
    let tokens = mention_tokens(s);
    let draft = draft_of(s, slot);
    let text = draft.text.clone();
    let ranges = mention_ranges(&text, &tokens);
    if ranges.is_empty() {
        return;
    }
    let covered: Vec<(usize, usize)> = draft.pills().map(|(r, _)| (r.start, r.end)).collect();
    // Right-to-left so earlier offsets stay valid.
    for range in ranges.into_iter().rev() {
        if covered
            .iter()
            .any(|&(a, b)| a <= range.start && range.end <= b)
        {
            continue;
        }
        let label = text[range.start..range.end].to_string();
        let id = match range.kind {
            MentionKind::Computer => format!("computer:{}", label.trim_start_matches('@')),
            MentionKind::File => format!("file:{label}"),
        };
        draft_of_mut(s, slot).replace_with_pill(range.start..range.end, id, &label);
    }
}

pub(super) fn on_draft_change(s: &mut UiState, slot: Slot) {
    use crate::model::composer_mentions::{delete_mention_atomically, mention_ranges};

    let prev = s.draft_snap.clone();
    let prev_caret = s.draft_snap_caret;
    let next = draft_of(s, slot).text.clone();

    // App atomic backspace for typed mentions that are not yet pills.
    if !prev.is_empty() {
        let ranges = mention_ranges(&prev, &mention_tokens(s));
        if let Some((_text, at)) = delete_mention_atomically(&prev, &next, &ranges, prev_caret) {
            let draft = draft_of_mut(s, slot);
            let end = draft_caret(draft).max(at);
            draft.set_selection(at, end);
            draft.insert("");
            draft.set_caret(at);
        }
    }

    ensure_mention_pills(s, slot);
    s.persist_draft(slot);

    let snap_text = draft_of(s, slot).text.clone();
    let snap_caret = draft_caret(draft_of(s, slot));
    s.draft_snap = snap_text;
    s.draft_snap_caret = snap_caret;

    // Forget an Esc dismissal once the trigger it belonged to is gone.
    if let Some(at) = s.popup_dismissed_at {
        use crate::model::composer_popup::{scan_query, TriggerAnchor};
        let caret = draft_caret(draft_of(s, slot));
        let still = ['/', '$', '@'].iter().any(|&ch| {
            let anchor = if ch == '@' {
                TriggerAnchor::Word
            } else {
                TriggerAnchor::Start
            };
            scan_query(&draft_of(s, slot).text, caret, ch, anchor).is_some_and(|q| q.start == at)
        });
        if !still {
            s.popup_dismissed_at = None;
        }
    }
    let caret = draft_caret(draft_of(s, slot));
    let Some(query) = crate::model::composer_popup::scan_query(
        &draft_of(s, slot).text,
        caret,
        '@',
        crate::model::composer_popup::TriggerAnchor::Word,
    )
    .map(|q| q.term) else {
        s.file_hits.clear();
        return;
    };
    let Some((agent, directory)) = mention_search_target(s, slot) else {
        // No folder to search: no stale hits from another one.
        s.file_hits.clear();
        return;
    };
    let generation = s.next_token;
    s.next_token += 1;
    s.send(Command::Compose(ComposeCommand::FindFiles {
        generation,
        agent,
        directory,
        query,
    }));
}

enum PickKind {
    Plain,
    Computer,
    File,
}

/// Apply a popup pick into the draft (App `insertComposerPick`).
fn apply_pick(
    s: &mut UiState,
    slot: Slot,
    query: &crate::model::composer_popup::PopupQuery,
    insert: &str,
    kind: PickKind,
) {
    let draft = draft_of_mut(s, slot);
    let tail_space = draft
        .text
        .get(query.end..)
        .is_some_and(|t| t.starts_with(' '));
    draft.set_selection(query.start, query.end);
    match kind {
        PickKind::Computer => {
            let name = insert.trim_start_matches('@');
            draft.insert_pill(format!("computer:{name}"), insert);
            if !tail_space {
                draft.insert(" ");
            }
        }
        PickKind::File => {
            draft.insert_pill(format!("file:{insert}"), insert);
            if !tail_space {
                draft.insert(" ");
            }
            if !s.mention_file_tokens.iter().any(|p| p == insert) {
                s.mention_file_tokens.push(insert.to_string());
            }
        }
        PickKind::Plain => {
            let text = if tail_space {
                insert.to_string()
            } else {
                format!("{insert} ")
            };
            draft.insert(&text);
        }
    }
    s.file_hits.clear();
    s.popup_dismissed_at = None;
    s.draft_snap = draft_of(s, slot).text.clone();
    s.draft_snap_caret = draft_caret(draft_of(s, slot));
    s.persist_draft(slot);
}

/// Pull a queued prompt back into the draft (App parity). Attachments cannot
/// follow, so those items stay cancel-only.
pub(super) fn edit_queued(s: &mut UiState, id: &str, text: String) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    s.queue.retain(|q| q.id != id);
    s.send(Command::Compose(ComposeCommand::InboxCancel {
        asid: AgentSessionId(asid),
        item: id.to_string(),
    }));
    s.draft = fresh(text);
    s.persist_draft(Slot::Session);
}

fn cancel_queued(s: &mut UiState, id: &str) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    s.queue.retain(|q| q.id != id);
    s.send(Command::Compose(ComposeCommand::InboxCancel {
        asid: AgentSessionId(asid),
        item: id.to_string(),
    }));
}

fn toggle_queued_steer(s: &mut UiState, id: &str, steer: bool) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    if let Some(item) = s.queue.iter_mut().find(|q| q.id == id) {
        item.steer = steer;
    }
    s.send(Command::Compose(ComposeCommand::InboxDelivery {
        asid: AgentSessionId(asid),
        item: id.to_string(),
        steer,
    }));
}

fn chip_thumb(k: &Kit, images: Option<&ImageCache>, path: &str, key: &str) -> El {
    if let Some(images) = images {
        if let Ok(bytes) = std::fs::read(path) {
            return art::picture(images, Arc::<[u8]>::from(bytes), SurfaceFit::Fill)
                .key(key.to_string())
                .w(CHIP_THUMB_W)
                .h(CHIP_THUMB_H)
                .radius(k.radius.sm)
                .clip_rounded(k.radius.sm);
        }
    }
    el().key(key.to_string())
        .w(CHIP_THUMB_W)
        .h(CHIP_THUMB_H)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
}

/// Thumbnails for queued image attachments (composer chips and the inspector).
pub(super) fn queue_thumbs(s: &UiState, k: &Kit, prefix: &str, paths: &[&str]) -> Option<El> {
    image_thumbs(k, s.images.as_ref(), prefix, paths)
}

fn image_thumbs(k: &Kit, images: Option<&ImageCache>, prefix: &str, paths: &[&str]) -> Option<El> {
    if paths.is_empty() {
        return None;
    }
    let mut row = row().key(format!("{prefix}-thumbs")).gap(4.0).center_y();
    for (i, path) in paths.iter().take(4).enumerate() {
        row = row.child(chip_thumb(k, images, path, &format!("{prefix}-thumb-{i}")));
    }
    if paths.len() > 4 {
        row = row.child(
            Badge::new(format!("+{}", paths.len() - 4))
                .variant(BadgeVariant::Subtle)
                .build(k),
        );
    }
    Some(row)
}

fn staged_chip(s: &UiState, k: &Kit, i: usize, file: &crate::model::compose::StagedFile) -> El {
    let language = s.cfg.general.language;
    let lead = if file.image && Path::new(&file.path).is_file() {
        chip_thumb(
            k,
            s.images.as_ref(),
            &file.path,
            &format!("staged-thumb-{i}"),
        )
    } else {
        rune::icon(&rune_kit::glyphs::FILE, 14.0)
            .color(k.c.icon_muted)
            .ml(4.0)
    };
    let remove = rune_kit::IconButton::new(k.glyph.close)
        .variant(rune_kit::IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label(format!(
            "{} {}",
            tr_ref(language, "Remove", "移除"),
            file.name
        ))
        .on_press(move |s: &mut UiState| {
            if i < s.staged.len() {
                s.staged.remove(i);
            }
        })
        .build(k)
        .key(format!("staged-btn-{i}"));
    row()
        .key(format!("staged-{i}"))
        .h(32.0)
        .pl(4.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.text_base.alpha(0.06))
        .help(file.path.clone())
        .child(lead.a11y_hidden())
        .child(
            rune::text(super::timeline::clip(&file.name, 32))
                .font(k.ty.small)
                .color(k.c.text_base)
                .a11y_hidden(),
        )
        .child(remove)
}

fn queued_chip(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    item: &crate::model::compose::InboxItem,
) -> El {
    let id = item.id.clone();
    let open = s.menu == Some(OpenMenu::Queued(id.clone()));
    let language = s.cfg.general.language;
    let label = format!(
        "{} {}",
        if item.steer {
            tr_ref(language, "Steer:", "插队：")
        } else {
            tr_ref(language, "Queued:", "已排队：")
        },
        super::timeline::clip(&item.text, 32)
    );
    let mut menu = Menu::new(format!("queued-{id}"))
        .trigger(
            Button::new(label)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .expanded(open)
                .on_press({
                    let id = id.clone();
                    move |s: &mut UiState| {
                        s.menu = (!open).then(|| OpenMenu::Queued(id.clone()));
                    }
                })
                .build(k)
                .key(format!("queued-pill-{id}")),
        )
        .open(open)
        .min_width(160.0)
        .on_close(|s: &mut UiState| s.menu = None);
    if item.files.is_empty() {
        let (edit_id, text) = (id.clone(), item.text.clone());
        menu = menu.item(
            MenuItem::new(tr_ref(language, "Edit", "编辑"))
                .on_select(move |s: &mut UiState| edit_queued(s, &edit_id, text.clone())),
        );
    }
    let (steer_id, next_steer) = (id.clone(), !item.steer);
    menu = menu.item(
        MenuItem::new(if item.steer {
            tr_ref(language, "Keep queued", "保持排队")
        } else {
            tr_ref(language, "Send next", "下一个发送")
        })
        .on_select(move |s: &mut UiState| toggle_queued_steer(s, &steer_id, next_steer)),
    );
    let cancel_id = id.clone();
    let pill = menu
        .item(
            MenuItem::new(tr_ref(language, "Cancel", "取消"))
                .danger(true)
                .on_select(move |s: &mut UiState| cancel_queued(s, &cancel_id)),
        )
        .build(k, layer);
    let thumbs: Vec<&str> = item
        .files
        .iter()
        .filter(|f| f.image)
        .map(|f| f.path.as_str())
        .collect();
    match queue_thumbs(s, k, &format!("q-{id}"), &thumbs) {
        Some(thumbs) => row()
            .key(format!("queued-row-{id}"))
            .gap(6.0)
            .center_y()
            .px(4.0)
            .py(2.0)
            .radius(k.radius.md)
            .fill(k.c.bg_base)
            .border(1.0, k.c.border_weak)
            .child(thumbs)
            .child(pill),
        None => pill,
    }
}

fn chips(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    if s.staged.is_empty() && s.queue.is_empty() {
        return None;
    }
    let mut line = row().key("chips").w_full().gap(6.0).flex_wrap().center_y();
    for (i, file) in s.staged.iter().enumerate() {
        line = line.child(staged_chip(s, k, i, file));
    }
    for item in &s.queue {
        line = line.child(queued_chip(s, k, layer, item));
    }
    Some(line)
}

// ---- popups -------------------------------------------------------------------------

use crate::model::composer_popup::{
    read_popup, PopupRow, PopupState, Trigger, TriggerAnchor, TriggerItem,
};

fn slash_trigger(s: &UiState, slot: Slot) -> Trigger {
    // `/` is host commands only. Skills live under `$` (App agent-composer).
    let items = catalog_of(s, slot)
        .map(|view| {
            view.catalog
                .commands
                .iter()
                .map(|c| {
                    let name = if c.name.starts_with('/') {
                        c.name.clone()
                    } else {
                        format!("/{}", c.name)
                    };
                    let mut description = c.description.clone().unwrap_or_default();
                    // App shows argsHint as a placeholder; template present → "…"
                    if c.template.as_ref().is_some_and(|t| !t.is_empty()) {
                        if description.is_empty() {
                            description = "…".into();
                        } else {
                            description = format!("{description}  …");
                        }
                    }
                    TriggerItem {
                        search: vec![name.clone(), description.clone()],
                        row: PopupRow {
                            id: format!("c:{}", c.name),
                            label: name.clone(),
                            description,
                            insert: name,
                            disabled: false,
                        },
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Trigger {
        char: '/',
        anchor: TriggerAnchor::Start,
        items,
        limit: None,
    }
}

fn skill_trigger(s: &UiState, slot: Slot) -> Trigger {
    // Every catalog skill, including ones the slash menu hides.
    let items = catalog_of(s, slot)
        .map(|view| {
            view.catalog
                .skills
                .iter()
                .map(|k| {
                    let label = format!("${}", k.id);
                    let description = if !k.name.is_empty()
                        && !k.description.is_empty()
                        && !k.name.eq_ignore_ascii_case(&k.id)
                    {
                        format!("{} · {}", k.name, k.description)
                    } else if !k.description.is_empty() {
                        k.description.clone()
                    } else {
                        k.name.clone()
                    };
                    TriggerItem {
                        search: vec![k.id.clone(), k.name.clone(), k.description.clone()],
                        row: PopupRow {
                            id: format!("s:{}", k.id),
                            label,
                            description,
                            insert: format!("${}", k.id),
                            disabled: false,
                        },
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Trigger {
        char: '$',
        anchor: TriggerAnchor::Start,
        items,
        limit: None,
    }
}

fn section_header(id: &str, label: &str) -> TriggerItem {
    TriggerItem {
        search: vec![],
        row: PopupRow {
            id: id.into(),
            label: label.into(),
            description: String::new(),
            insert: String::new(),
            disabled: true,
        },
    }
}

/// The `@` popup (App `file-mention-panel`): computers whose name starts
/// with the term (this one, then reachable, then offline, which cannot be
/// picked) under their heading, then the gateway's file matches as it
/// ranked them, headed only before anything is typed.
fn mention_popup(s: &UiState, slot: Slot) -> PopupState {
    use crate::model::composer_popup::{scan_query, ClosedReason};
    let draft = draft_of(s, slot);
    let caret = draft_caret(draft);
    let Some(query) = scan_query(&draft.text, caret, '@', TriggerAnchor::Word) else {
        return PopupState::Closed {
            reason: ClosedReason::NoQuery,
        };
    };
    if s.popup_dismissed_at == Some(query.start) || query.term.chars().count() > 96 {
        return PopupState::Closed {
            reason: ClosedReason::Dismissed,
        };
    }
    let language = s.cfg.general.language;
    let term = query.term.to_lowercase();
    let mut peers: Vec<_> = s
        .peers
        .iter()
        .filter(|p| p.name.to_lowercase().starts_with(&term))
        .collect();
    peers.sort_by_key(|p| match (p.current, p.reachable) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    });
    let mut rows: Vec<PopupRow> = Vec::new();
    if !peers.is_empty() {
        rows.push(section_header("hdr:computers", tr_ref(language, "Computers", "电脑")).row);
        rows.extend(peers.into_iter().map(|p| {
            let detail = if p.current {
                tr_ref(language, "This computer", "本机")
            } else if p.reachable {
                ""
            } else {
                tr_ref(language, "Offline", "离线")
            };
            PopupRow {
                id: format!("p:{}", p.name),
                label: format!("@{}", p.name),
                description: detail.into(),
                insert: format!("@{}", p.name),
                disabled: !p.reachable,
            }
        }));
    }
    let files = s.file_hits.iter().take(MAX_FILE_ROWS);
    if !s.file_hits.is_empty() && term.is_empty() {
        rows.push(
            section_header(
                "hdr:files",
                tr_ref(language, "Files in this project", "此项目中的文件"),
            )
            .row,
        );
    }
    rows.extend(files.map(|h| PopupRow {
        id: format!("f:{}", h.path),
        label: h.name.clone(),
        description: h.path.clone(),
        insert: h.path.clone(),
        disabled: false,
    }));
    if rows.is_empty() {
        return PopupState::Closed {
            reason: ClosedReason::NoMatch,
        };
    }
    PopupState::Open { query, rows }
}

/// The popup the draft calls for now: `/` commands, `$` skills or `@`
/// mentions (one at a time), with its key and label.
fn current_popup(s: &UiState, slot: Slot) -> Option<(&'static str, &'static str, PopupState)> {
    let draft = draft_of(s, slot);
    let caret = Some(draft_caret(draft));
    let dismissed = s.popup_dismissed_at;
    let language = s.cfg.general.language;
    let slash = read_popup(&draft.text, caret, &slash_trigger(s, slot), dismissed, true);
    if slash.open() {
        return Some(("slash-popup", tr_ref(language, "Commands", "命令"), slash));
    }
    let skills = read_popup(&draft.text, caret, &skill_trigger(s, slot), dismissed, true);
    if skills.open() {
        return Some(("skill-popup", tr_ref(language, "Skills", "技能"), skills));
    }
    let mentions = mention_popup(s, slot);
    mentions.open().then(|| {
        (
            "mention-popup",
            tr_ref(language, "Mentions", "提及"),
            mentions,
        )
    })
}

fn pickable(row: &PopupRow) -> bool {
    !row.disabled && !row.id.starts_with("hdr:")
}

/// The keyboard's row in an open popup: the one it moved to while the
/// query stays the same, else the first that can be picked.
fn active_row(s: &UiState, state: &PopupState) -> Option<usize> {
    let PopupState::Open { query, rows } = state else {
        return None;
    };
    s.popup_active
        .as_ref()
        .filter(|(start, term, at)| {
            *start == query.start && *term == query.term && rows.get(*at).is_some_and(pickable)
        })
        .map(|(_, _, at)| *at)
        .or_else(|| rows.iter().position(pickable))
}

/// Pick row `id` of the open popup into the draft.
fn choose(s: &mut UiState, slot: Slot, query: &crate::model::composer_popup::PopupQuery, id: &str) {
    if id.starts_with("hdr:") {
        return;
    }
    let (insert, kind) = match id.split_once(':') {
        Some(("c", name)) => {
            let insert = if name.starts_with('/') {
                name.to_string()
            } else {
                format!("/{name}")
            };
            (insert, PickKind::Plain)
        }
        Some(("s", id)) => (format!("${id}"), PickKind::Plain),
        Some(("p", name)) => (format!("@{name}"), PickKind::Computer),
        Some(("f", path)) => (path.to_string(), PickKind::File),
        _ => return,
    };
    s.popup_active = None;
    apply_pick(s, slot, query, &insert, kind);
}

/// The draft field's keys while a popup is open: ↑ ↓ move (wrapping, over
/// headers and offline computers), Tab or Return pick, Esc dismisses.
pub(super) fn popup_key(s: &mut UiState, slot: Slot, ev: &rune::KeyEvent) -> bool {
    if ev.mods.cmd || ev.mods.ctrl || ev.mods.alt {
        return false;
    }
    let Some((_, _, state)) = current_popup(s, slot) else {
        return false;
    };
    let PopupState::Open { query, rows } = &state else {
        return false;
    };
    let picks: Vec<usize> = (0..rows.len()).filter(|i| pickable(&rows[*i])).collect();
    let at = active_row(s, &state);
    match ev.key {
        rune::Key::Up | rune::Key::Down => {
            if picks.is_empty() {
                return true;
            }
            let here = at
                .and_then(|a| picks.iter().position(|p| *p == a))
                .unwrap_or(0);
            let next = if ev.key == rune::Key::Down {
                (here + 1) % picks.len()
            } else {
                (here + picks.len() - 1) % picks.len()
            };
            s.popup_active = Some((query.start, query.term.clone(), picks[next]));
            true
        }
        rune::Key::Tab | rune::Key::Enter if !ev.mods.shift => {
            let Some(at) = at else {
                return false;
            };
            let (query, id) = (query.clone(), rows[at].id.clone());
            choose(s, slot, &query, &id);
            true
        }
        rune::Key::Escape => {
            s.popup_dismissed_at = Some(query.start);
            s.popup_active = None;
            true
        }
        _ => false,
    }
}

fn popup_rows(state: &PopupState) -> Vec<PopupItem> {
    state
        .rows()
        .iter()
        .map(|r| {
            PopupItem::new(r.id.clone(), r.label.clone())
                .description(r.description.clone())
                .disabled(r.disabled)
        })
        .collect()
}

fn mount_popup(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    key: &str,
    label: &str,
    slot: Slot,
    state: PopupState,
) {
    if !state.open() {
        return;
    }
    let items = popup_rows(&state);
    if items.is_empty() {
        return;
    }
    let active = active_row(s, &state);
    let query = state.query().cloned();
    let hover_query = query.clone();
    // Over the field the draft is typed in (below it when the window has
    // no room above), never the window's corner: Home's field is not the
    // session composer.
    let anchor = match slot {
        Slot::Session => "draft",
        Slot::Start => "start-draft",
    };
    PopupList::<UiState>::new(key, anchor)
        .anchored(anchor, rune::Side::Top, rune::Align::Start, 6.0)
        .width(380.0)
        .max_height(300.0)
        .label(label)
        .items(items)
        .active(active)
        .on_active(move |s: &mut UiState, at: usize| {
            if let Some(q) = hover_query.as_ref() {
                s.popup_active = Some((q.start, q.term.clone(), at));
            }
        })
        .on_choose(move |s: &mut UiState, id: &str, _: &mut Tx| {
            if let Some(query) = query.as_ref() {
                choose(s, slot, query, id);
            }
        })
        .on_close(move |s: &mut UiState| {
            if let Some((_, _, PopupState::Open { query, .. })) = current_popup(s, slot) {
                s.popup_dismissed_at = Some(query.start);
            }
        })
        .build(k, layer);
}

/// Mount whichever popup the draft calls for, at its field.
pub(super) fn popup(s: &UiState, k: &Kit, layer: &mut Vec<El>, slot: Slot) {
    if let Some((key, label, state)) = current_popup(s, slot) {
        mount_popup(s, k, layer, key, label, slot, state);
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

pub(super) fn switch_model(s: &mut UiState, provider_id: String, model_id: String) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    let keep_variant = s
        .sessions
        .iter()
        .find(|i| i.asid.0 == asid)
        .and_then(|i| i.model.as_ref())
        .and_then(|m| m.variant.clone());
    let model = ModelRef {
        provider_id,
        model_id,
        variant: keep_variant,
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

pub(super) fn switch_effort(s: &mut UiState, variant: Option<String>) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    let Some(current) = s
        .sessions
        .iter()
        .find(|i| i.asid.0 == asid)
        .and_then(|i| i.model.clone())
    else {
        return;
    };
    let model = ModelRef {
        provider_id: current.provider_id,
        model_id: current.model_id,
        variant,
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

pub(super) fn switch_mode(s: &mut UiState, mode: String) {
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
        .variant(ButtonVariant::GhostMuted)
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
    // Composers sit at the window's foot: their menus open upward.
    let mut menu = Menu::new(key)
        .placement(rune_kit::Placement::TopStart)
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
    let menu = Menu::new(key)
        .placement(rune_kit::Placement::TopStart)
        .trigger(pill(k, &format!("{key}-pill"), label, open, kind))
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None);
    Some(branch_items(menu, cwd, info).build(k, layer))
}

/// The folder's local branches as menu items, the checked-out one ticked.
pub(super) fn branch_items(
    mut menu: Menu,
    cwd: &str,
    info: &crate::model::compose::BranchInfo,
) -> Menu {
    for branch in &info.local {
        let (cwd, name) = (cwd.to_string(), branch.clone());
        menu = menu.item(
            MenuItem::new(branch.clone())
                .checked(info.current.as_ref() == Some(branch))
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::Compose(ComposeCommand::Checkout {
                        cwd: cwd.clone(),
                        branch: name.clone(),
                    }));
                }),
        );
    }
    menu
}

/// Catalogue models as `(provider, id, name)`, enabled ones only.
pub fn catalog_models(view: &crate::hub::CatalogView) -> Vec<(String, String, String)> {
    view.catalog
        .models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let name = if m.name.trim().is_empty() || m.name == m.id {
                super::labels::format_model_name(
                    &ModelRef {
                        provider_id: m.provider_id.clone(),
                        model_id: m.id.clone(),
                        variant: None,
                    },
                    &m.id,
                    None,
                )
            } else {
                m.name.clone()
            };
            (m.provider_id.clone(), m.id.clone(), name)
        })
        .collect()
}

pub(super) fn mode_label(id: &str, catalog_name: Option<&str>) -> String {
    if let Some(name) = catalog_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != id)
    {
        return name.to_string();
    }
    super::labels::readable_catalog_id(id)
}

pub(super) fn effort_menu(
    k: &Kit,
    layer: &mut Vec<El>,
    open: bool,
    current: Option<&str>,
    variants: &[String],
) -> El {
    let label = current
        .map(super::labels::readable_catalog_id)
        .unwrap_or_else(|| "Default".into());
    let mut menu = Menu::new("effort")
        .trigger(pill(k, "effort-pill", label, open, OpenMenu::Effort))
        .open(open)
        .min_width(160.0)
        .on_close(|s: &mut UiState| s.menu = None);
    menu = menu.item(
        MenuItem::new("Default")
            .checked(current.is_none())
            .on_select(|s: &mut UiState| switch_effort(s, None)),
    );
    for variant in variants {
        let name = variant.clone();
        menu = menu.item(
            MenuItem::new(super::labels::readable_catalog_id(variant))
                .checked(current == Some(variant.as_str()))
                .on_select(move |s: &mut UiState| switch_effort(s, Some(name.clone()))),
        );
    }
    menu.build(k, layer)
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let language = s.cfg.general.language;
    let busy = s.busy();
    let has_session = s.selected.is_some();
    let focused = s.draft.focused && has_session;
    let brand = s
        .current()
        .map(|i| super::agents::color(&i.agent_id, s.dark))
        .unwrap_or(k.c.bg_accent);
    let start = s
        .current()
        .and_then(|i| i.directory.clone().map(PathBuf::from));
    let has_text = !s.draft.text.trim().is_empty() || !s.staged.is_empty();

    // The row under the field: the session's controls on the left (see
    // `composer_row`), the actions on the right.
    let fit = super::Fit::of("composer");
    let attach_help = format!(
        "{} · {}",
        tr_ref(language, "Add files or an image", "添加文件或图片"),
        crate::platform::keys::label("Primary+V")
    );
    let attach_press = move |_: &mut UiState| {
        crate::platform::dialogs::attach_files(start.clone());
    };
    let attach = rune_kit::IconButton::new(&super::parts::glyphs::ATTACH)
        .variant(rune_kit::IconButtonVariant::GhostMuted)
        .label(tr_ref(language, "Attach", "附件"))
        .help(attach_help)
        .disabled(!has_session)
        .on_press(attach_press)
        .build(k)
        .key("attach");
    let stop_label = tr_ref(language, "Stop", "停止");
    let send_label = if busy {
        tr_ref(language, "Steer", "插话")
    } else {
        tr_ref(language, "Send", "发送")
    };

    // The controls fold to fit beside the buttons, measured on the card
    // itself (a split pane is narrower than the window); only when even
    // their narrowest fold does not fit do they take a line of their own.
    use super::composer_row::{self as controls_row, ACTION_GAP, CARD_PAD};
    let inner = rune::memo::layout_with("composer-card", |r| r[2].round())
        .map_or(f32::INFINITY, |w| w - CARD_PAD);
    let button_w = |label: &str| rune::text::measure(label, &[], k.ty.ui_medium, None).0 + 24.0;
    // Mic and Attach (24 pt each), Send, Stop while working, the gaps.
    let mut buttons = 24.0 + 24.0 + button_w(send_label) + 3.0 * ACTION_GAP;
    if busy {
        buttons += button_w(stop_label) + ACTION_GAP;
    }
    // Narrower still, Mic and Attach join the controls' line so Stop and
    // Send keep their words.
    let squeezed = inner < buttons;
    let mut tools = Some(
        row()
            .gap(ACTION_GAP)
            .center_y()
            .child(super::voice::mic_button(s, k))
            .child(attach),
    );
    let tools_w = 48.0 + 2.0 * ACTION_GAP;
    let controls = controls_row::Controls::read(s);
    let beside = controls
        .as_ref()
        .and_then(|c| controls_row::beside(s, c, k, layer, inner - buttons));
    let mut own_line = None;
    if beside.is_none() {
        let mut line = row()
            .key("composer-controls")
            .w_full()
            .gap(ACTION_GAP)
            .center_y();
        let mut room = inner;
        if squeezed {
            line = line.children(tools.take());
            room -= tools_w;
        }
        if let Some(c) = &controls {
            line = line.child(controls_row::own_line(s, c, k, layer, room));
        }
        own_line = Some(line);
    }
    let mut actions = row()
        .key("composer-actions")
        .w_full()
        .gap(ACTION_GAP)
        .center_y();
    if let Some(r) = beside {
        actions = actions.child(r.shrink().min_w(0.0)).child(el().grow(1.0));
    }
    if let Some(tools) = tools {
        actions = actions.child(tools);
    }
    if own_line.is_some() {
        actions = actions.child(el().grow(1.0));
    }
    if busy {
        if let Some(asid) = s.selected.clone() {
            actions = actions.child(
                Button::new(stop_label)
                    .variant(ButtonVariant::Danger)
                    .control_size(ControlSize::Regular)
                    .shortcut(super::keymap::label(super::keymap::Action::Stop))
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::Interrupt(AgentSessionId(asid.clone())))
                    })
                    .build(k)
                    .key("stop"),
            );
        }
    }
    let send = Button::new(send_label)
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Regular)
        .disabled(!has_session || s.pending_submit.is_some())
        .shortcut("Primary+Enter")
        .help(if busy {
            format!(
                "{} · {} {}",
                tr_ref(language, "Steer the running turn", "对运行中的回合插话"),
                crate::platform::keys::label("Alt+Enter"),
                tr_ref(language, "queues it instead", "改为排队")
            )
        } else {
            tr_ref(language, "Send", "发送").to_string()
        })
        .on_press(submit_session)
        .build(k)
        .key("send");
    // The send button lights up once there is something to send.
    actions = actions.child(look.glow(
        send,
        k.c.bg_accent,
        if has_text && has_session { 0.6 } else { 0.0 },
    ));

    popup(s, k, layer, Slot::Session);
    let mut card = col()
        .key("composer-card")
        .w_full()
        .gap(8.0)
        .px(12.0)
        .pt(10.0)
        .pb(10.0);
    if let Some(chips) = chips(s, k, layer) {
        card = card.child(chips);
    }
    let placeholder = if has_session {
        let full = tr_ref(
            language,
            "Message…  (/ commands, $ skills, @ files)",
            "发消息…（/ 命令，$ 技能，@ 文件）",
        );
        // A narrow card keeps the hint to one line.
        if rune::text::measure(full, &[], k.ty.ui, None).0 + 24.0 <= inner {
            full
        } else {
            tr_ref(language, "Message…  (/ $ @)", "发消息…（/ $ @）")
        }
    } else {
        tr_ref(language, "Pick a session first", "先选一个会话")
    };
    let accent = k.c.bg_accent;
    let info = k.c.info.icon;
    if let Some(dictation) = super::voice::strip(s, k) {
        card = card.child(dictation);
    }
    card = card.child(
        TextArea::new(&s.draft)
            .placeholder(placeholder)
            .label(tr_ref(language, "Draft", "草稿"))
            .width(None)
            .auto_grow(2, 8)
            .pill_style(move |id| {
                if id.starts_with("computer:") {
                    PillStyle::default().bg(accent.alpha(0.22)).fg(accent)
                } else if id.starts_with("file:") {
                    PillStyle::default().bg(info.alpha(0.18)).fg(info)
                } else {
                    PillStyle::default()
                }
            })
            .on_edit(|s: &mut UiState| &mut s.draft)
            .on_change(|s: &mut UiState, _: &mut Tx| on_draft_change(s, Slot::Session))
            .on_key(|s: &mut UiState, ev: &rune::KeyEvent, tx: &mut Tx| {
                // Alt+Enter while the agent works: queue after this turn.
                let alt_enter = ev.key == rune::Key::Enter && ev.mods.alt && !ev.mods.cmd;
                if alt_enter && s.busy() {
                    queue_session(s);
                    return true;
                }
                // Sidebar and pane keys, ahead of the field's caret moves.
                let at_end = caret_at_end(&s.draft);
                super::keymap::field_key(s, ev, tx, at_end)
            })
            .on_submit(|s: &mut UiState, _: &mut Tx| submit_session(s))
            .on_input_paste(|s: &mut UiState, content, _: &mut Tx| take_paste(s, content))
            .build(&bare_field(k))
            .key("draft")
            .w_full(),
    );
    if let Some(line) = own_line {
        card = card.child(line);
    }
    card = card.child(actions);

    // The card is the one surface: a quiet rim at rest, a faint step toward
    // the accent while the field has focus.
    let radius = crate::theme::look::PANEL_RADIUS;
    let card = look.panel(k, card, radius).motion(k.motion.control);
    let card = focus_tone(k, &look, card, focused);

    // The status line: what the keys do, and a word while the agent works.
    let mut hint = row()
        .key("composer-hint")
        .w_full()
        .gap(8.0)
        .center_y()
        .px(4.0);
    if busy {
        // The whole sentence while it fits, then its gist; cut short last.
        let room = inner + CARD_PAD - 8.0 - 18.0;
        let queue_key = crate::platform::keys::label("Alt+Enter");
        let long = format!(
            "{} · {queue_key} {}",
            tr_ref(
                language,
                "Agent is working; messages steer it",
                "Agent 正在工作，消息会插话",
            ),
            tr_ref(language, "to queue", "排队"),
        );
        let line = if rune::text::measure(&long, &[], k.ty.small, None).0 <= room {
            long
        } else {
            format!(
                "{} · {queue_key} {}",
                tr_ref(language, "Working", "工作中"),
                tr_ref(language, "queues", "排队"),
            )
        };
        hint = hint
            .child(rune_kit::Spinner::new().size(10.0).color(brand).build(k))
            .child(
                rune::text(line)
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .truncate()
                    .shrink()
                    .min_w(0.0),
            )
            .child(el().grow(1.0));
    } else {
        hint = hint.child(el().grow(1.0));
    }
    // The keys are a wide column's footnote; a narrow one keeps the room.
    if fit == super::Fit::Wide {
        hint = hint.child(muted(
            k,
            format!(
                "{} {} · {} {}",
                crate::platform::keys::label("Primary+Enter"),
                tr_ref(language, "to send", "发送"),
                crate::platform::keys::label("Primary+V"),
                tr_ref(language, "to paste files or an image", "粘贴文件或图片"),
            ),
        ));
    }

    // The messages' reading column: as wide and as centred as they are on
    // a wide window, the whole width on a narrow one.
    let column = super::timeline::FEED_MAX_W;
    col()
        .key("composer")
        .w_full()
        .items_center()
        .px(16.0)
        .pt(8.0)
        .pb(10.0)
        .gap(6.0)
        .child(card.w_full().max_w(column))
        .child(hint.max_w(column))
}
