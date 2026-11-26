//! The session (and Home) composer: draft, attachments, queue, model / mode /
//! branch pills, and the `@` / `$` / `/` popups.

use std::path::{Path, PathBuf};

use muqun_gateway::agents::{AgentSessionId, ModelRef};
use rune::{col, el, row, text, Cursor, El, ImageCache, PillStyle, SurfaceFit, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem, ScrollBar,
    TextArea,
};

use super::copy::tr_ref;
use super::{OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};

/// Chip thumbnail size (wide enough to read the picture, short for the row).
const CHIP_THUMB_W: f32 = 56.0;
const CHIP_THUMB_H: f32 = 40.0;

/// The most file hits the `@` popup lists (App `FILE_MENTION_LIMIT`); the
/// list scrolls, as `/` and `$` (uncapped, as the App) do.
const MAX_FILE_ROWS: usize = 20;

pub(crate) fn fresh(text: impl Into<String>) -> TextEdit {
    let mut edit = TextEdit::new(text).rich();
    edit.focused = true;
    edit
}

pub static SEND: rune::Icon = rune::Icon {
    name: "composer-send",
    view: 24.0,
    body: r#"<path d="M6 18 18 6M6 6h12v12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static PHOTO: rune::Icon = rune::Icon {
    name: "composer-photo",
    view: 24.0,
    body: r#"<rect width="18" height="18" x="3" y="3" rx="2" ry="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="8.5" cy="8.5" r="1.5" fill="none" stroke="currentColor" stroke-width="2"/><polyline points="21 15 16 10 5 21" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static DROP_ICON: rune::Icon = rune::Icon {
    name: "composer-drop-icon",
    view: 24.0,
    body: r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>"#,
};

pub fn insert_image_pill(draft: &mut TextEdit, token: u64) -> usize {
    let count = draft
        .pills()
        .filter(|(_, id)| id.starts_with("image:"))
        .count()
        + 1;
    let label = format!("[Image #{count}]");
    let (_, b) = draft.range();
    let tail_space = draft.text.get(b..).is_some_and(|t| t.starts_with(' '));
    draft.insert_pill(format!("image:{token}"), &label);
    if !tail_space {
        draft.insert(" ");
    }
    count
}

pub fn stage_png(s: &mut UiState, png: Vec<u8>) {
    let id = s.next_token;
    s.pending_image_uploads.insert(id);
    s.next_token += 1;
    s.send(Command::Compose(ComposeCommand::Stage {
        id,
        name: "pasted.png".into(),
        source: Source::Png(png),
    }));
}

pub fn stage_path(s: &mut UiState, path: PathBuf) {
    let id = s.next_token;
    let target = (s.mode == super::Mode::Session)
        .then(|| s.selected.clone())
        .flatten();
    s.pending_file_targets
        .insert(id, (s.current_computer().to_owned(), target));
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

/// Attach files dropped from the OS or file manager into the active draft.
pub fn attach_dropped_files(s: &mut UiState, files: &[PathBuf]) {
    s.drop_hover = false;
    if files.is_empty() {
        return;
    }
    for path in files {
        let is_img = crate::model::compose::image_name(&path.to_string_lossy());
        let token = s.next_token;
        stage_path(s, path.clone());
        if is_img {
            s.pending_image_uploads.insert(token);
            let draft = if s.mode == super::Mode::Session {
                &mut s.draft
            } else {
                &mut s.start_draft
            };
            insert_image_pill(draft, token);
        }
    }
}

/// Ctrl/Cmd+V into the draft: stage copied files or a clipboard image; leave
/// plain text to the editor. Returns true when the paste was consumed.
pub fn take_paste(s: &mut UiState, content: &rune::ClipboardContent) -> bool {
    let files = content.files();
    if !files.is_empty() {
        attach_dropped_files(s, files);
        return true;
    }
    if let Some(image) = content.image() {
        let token = s.next_token;
        stage_png(s, image.png().to_vec());
        let draft = if s.mode == super::Mode::Session {
            &mut s.draft
        } else {
            &mut s.start_draft
        };
        let count = insert_image_pill(draft, token);
        let language = s.cfg.general.language;
        let msg = format!(
            "{} #{}",
            crate::ui::copy::t(language, "Attached image"),
            count
        );
        s.status = msg.clone();
        s.toast(msg, None);
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

pub(super) fn ready_to_send(s: &UiState, slot: Slot) -> bool {
    let draft = draft_of(s, slot);
    let target = match slot {
        Slot::Start => None,
        Slot::Session => s.selected.as_ref(),
    };
    (!draft.text.trim().is_empty() || !s.staged.is_empty())
        && !draft.pills().any(|(_, id)| {
            id.strip_prefix("image:")
                .and_then(|id| id.parse().ok())
                .is_some_and(|id| s.pending_image_uploads.contains(&id))
        })
        && !s
            .pending_file_targets
            .iter()
            .any(|(id, (computer, session))| {
                !s.pending_image_uploads.contains(id)
                    && computer == s.current_computer()
                    && session.as_ref() == target
            })
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

/// A compact floating capsule for the active project or session controls.
pub fn context_capsule(k: &Kit, look: &crate::theme::look::Look, content: El) -> El {
    look.panel(k, content.px(14.0).py(8.0), k.radius.xl)
}

/// A borderless frosted surface with a silky animated theme-accent gradient
/// aura on focus (zero hard border stroke).
pub fn composer_card(
    k: &Kit,
    look: &crate::theme::look::Look,
    card: El,
    focused: bool,
    busy: bool,
    drop_hover: bool,
) -> El {
    let active = focused || drop_hover;
    let [c0, _, _] = look.highlight_colors(k);
    let card = look
        .panel(k, card, k.radius.xl)
        .border(0.0, c0.alpha(0.0))
        .box_shadow(rune::Shadow {
            x: 0.0,
            y: if active { 8.0 } else { 4.0 },
            blur: if active { 26.0 } else { 18.0 },
            spread: -8.0,
            color: k.c.black.alpha(if look.dark { 0.44 } else { 0.09 }),
            inset: false,
        })
        .motion(look.focus_fade());
    let card = look.glow(card, c0, if active { 0.38 } else { 0.0 });
    if busy && look.motion && look.glass && super::fx::fx_enabled() {
        card.child(
            el().key("composer-energy")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .radius(k.radius.xl)
                .effect(super::fx::COMPOSER_RIM)
                .effect_data(super::fx::data(c0, 1.25))
                .a11y_hidden(),
        )
    } else if active && look.glass && super::fx::fx_enabled() {
        card.child(
            el().key("composer-focus-aura")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .radius(k.radius.xl)
                .effect(super::fx::FOCUS_AURA)
                .effect_data(super::fx::data(c0, 1.0))
                .a11y_hidden(),
        )
    } else {
        card
    }
}

/// A composer card (a [`Look::panel`](crate::theme::look::Look::panel))
/// while its field has focus: the surface a faint step toward the accent.
#[allow(dead_code)]
pub fn focus_tone(k: &Kit, look: &crate::theme::look::Look, card: El, focused: bool) -> El {
    composer_card(k, look, card, focused, false, false)
}

/// The kit a composer's text field is built with: the field draws no box of
/// its own (fill, shadow, focus ring), so the composer card around it is the
/// one surface and the one focus mark. Selection contrast is boosted so
/// double-click and drag selections stand out clearly against the card fill.
pub fn bare_field(k: &Kit) -> Kit {
    let mut bare = k.clone();
    bare.blend_over = None;
    bare.c.bg_base = k.c.bg_base.alpha(0.0);
    bare.c.hover = k.c.hover.alpha(0.0);
    bare.c.border_focus = k.c.border_focus.alpha(0.0);
    bare.c.text_accent = if k.dark {
        k.c.bg_accent.mix(k.c.white, 0.25)
    } else {
        k.c.bg_accent.mix(k.c.text_accent, 0.5)
    };
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
    if !ready_to_send(s, Slot::Session) {
        return;
    }
    let Some(asid) = s.selected.clone() else {
        s.status = "Pick a session first".into();
        return;
    };
    if s.pending_start
        .as_ref()
        .is_some_and(|(pending, _)| *pending == asid)
    {
        return;
    }
    s.record_prompt_history(&text);
    let busy = s.busy() || !s.queue.is_empty();
    let is_mao_session = super::orchestration::is_session_mao_enabled(s, &asid);
    let attachments: Vec<String> = s.staged.iter().map(|f| f.path.clone()).collect();
    let deliver = deliver_of(catalog_of(s, Slot::Session), &text, !attachments.is_empty());
    note_skill_args(s, &deliver);
    let delivery = busy.then_some(if queue {
        super::echo::BusyDelivery::Queue
    } else {
        super::echo::BusyDelivery::Steer
    });
    let token = s.token(Slot::Session);
    s.status = "Sending…".into();
    // A prompt shows in the timeline at once and leaves the draft; a queued
    // one is its inbox item in the queue strip, as before.
    if let Deliver::Prompt(words) = &deliver {
        let staged = std::mem::take(&mut s.staged);
        s.echo_sent(&asid, token, words.clone(), &staged, delivery);
        s.draft = fresh("");
        s.persist_draft(Slot::Session);
    }
    let run_mao_for_turn =
        !busy && is_mao_session && !super::orchestration::is_conversational_query(&text);
    let gateway_mao = if run_mao_for_turn && matches!(&deliver, Deliver::Prompt(_)) {
        let info = s.session_by_id(&asid).cloned();
        let coder_agent = info
            .as_ref()
            .map(|i| i.agent_id.clone())
            .unwrap_or_else(|| s.start_agent.clone());
        let coder_model = info.as_ref().and_then(|i| i.model.clone());
        let coder_mode = info.as_ref().and_then(|i| i.mode.clone());
        let cwd = info
            .as_ref()
            .and_then(|i| i.directory.clone())
            .unwrap_or_default();
        super::orchestration::dispatch_gateway_start(
            s,
            &asid,
            token,
            &text,
            &cwd,
            &coder_agent,
            coder_model.as_ref(),
            coder_mode.as_deref(),
            Some(&asid),
            &attachments,
        )
    } else {
        false
    };
    if run_mao_for_turn && !gateway_mao {
        super::orchestration::on_session_submitted(s, &asid, &text);
    }
    if !gateway_mao {
        s.send(Command::Compose(ComposeCommand::Submit {
            token,
            target: Target::Session(AgentSessionId(asid)),
            deliver,
            attachments,
            delivery: delivery.map(super::echo::BusyDelivery::wire),
            auto_approve: run_mao_for_turn,
        }));
    }
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
                    s.start_default_directory(&s.start_agent)
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
        .all_computers()
        .iter()
        .flat_map(|p| {
            let mut list = vec![MentionToken {
                text: format!("@{}", p.name),
                kind: MentionKind::Computer,
            }];
            if p.server_id != p.name && !p.server_id.is_empty() {
                list.push(MentionToken {
                    text: format!("@{}", p.server_id),
                    kind: MentionKind::Computer,
                });
            }
            list
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
    s.reset_prompt_history_cursor();
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

    let live: Vec<String> = draft_of(s, slot)
        .pills()
        .map(|(_, id)| id.to_owned())
        .collect();
    s.staged.retain(|file| {
        file.draft_token
            .is_none_or(|id| live.contains(&format!("image:{id}")))
    });
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

/// Pull a queued prompt and its uploaded attachments back into the draft.
pub(super) fn edit_queued(s: &mut UiState, id: &str, text: String) {
    s.menu = None;
    let Some(asid) = s.selected.clone() else {
        return;
    };
    let Some(item) = s.queue.iter().find(|q| q.id == id).cloned() else {
        return;
    };
    s.composer_queue_delivery = !item.steer;
    for file in item.files {
        if !s.staged.iter().any(|f| f.path == file.path) {
            s.staged.push(crate::model::compose::StagedFile {
                draft_token: None,
                path: file.path,
                name: file.name,
                image: file.image,
                size: 0,
            });
        }
    }
    s.forget_queue_echo(&asid, id);
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
    s.forget_queue_echo(&asid, id);
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
        let image = super::parts::images::picture_file(images, Path::new(path));
        return el()
            .pixels_fit(image.pixels(), SurfaceFit::Fill)
            .key(key.to_string())
            .w(CHIP_THUMB_W)
            .h(CHIP_THUMB_H)
            .radius(k.radius.sm)
            .clip_rounded(k.radius.sm);
    }
    el().key(key.to_string())
        .w(CHIP_THUMB_W)
        .h(CHIP_THUMB_H)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
}

/// Thumbnails for queued image attachments (composer chips and the inspector).
pub(super) fn queue_thumbs(s: &UiState, k: &Kit, prefix: &str, paths: &[&str]) -> Option<El> {
    image_thumbs(s, k, prefix, paths)
}

fn openable_picture(k: &Kit, face: El, key: String, label: &str, path: &Path) -> El {
    let path = path.to_owned();
    k.interactive(face)
        .key(key)
        .label(label.to_string())
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut UiState| s.open_attachment_image(&path))
}

fn image_thumbs(s: &UiState, k: &Kit, prefix: &str, paths: &[&str]) -> Option<El> {
    if paths.is_empty() {
        return None;
    }
    let mut row = row().key(format!("{prefix}-thumbs")).gap(4.0).center_y();
    for (i, path) in paths.iter().take(4).enumerate() {
        let cached = crate::model::media::cached_upload(s.current_computer(), path);
        let path = cached.as_deref().unwrap_or_else(|| Path::new(path));
        let thumb = chip_thumb(
            k,
            s.images.as_ref(),
            &path.to_string_lossy(),
            &format!("{prefix}-thumb-{i}"),
        );
        let label = path.file_name().unwrap_or_default().to_string_lossy();
        row = row.child(openable_picture(
            k,
            thumb,
            format!("{prefix}-preview-{i}"),
            &label,
            path,
        ));
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
    let preview_path = crate::model::media::cached_upload(s.current_computer(), &file.path);
    let preview_path = preview_path
        .as_deref()
        .unwrap_or_else(|| Path::new(&file.path));
    let lead = if file.image && preview_path.is_file() {
        chip_thumb(
            k,
            s.images.as_ref(),
            &preview_path.to_string_lossy(),
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
        .label(format!("{} {}", tr_ref(language, "Remove"), file.name))
        .on_press(move |s: &mut UiState| {
            if i < s.staged.len() {
                let file = s.staged.remove(i);
                if let Some(id) = file.draft_token {
                    let slot = if s.mode == super::Mode::Session {
                        Slot::Session
                    } else {
                        Slot::Start
                    };
                    remove_image_pill(draft_of_mut(s, slot), id);
                    on_draft_change(s, slot);
                }
            }
        })
        .build(k)
        .key(format!("staged-btn-{i}"));
    let label = rune::text(super::timeline::clip(&file.name, 32))
        .font(k.ty.small)
        .color(k.c.text_base);
    let preview = row().center_y().gap(6.0).child(lead).child(label);
    let preview = if file.image && preview_path.is_file() {
        openable_picture(
            k,
            preview,
            format!("staged-preview-{i}"),
            &file.name,
            preview_path,
        )
    } else {
        preview
    };
    row()
        .key(format!("staged-{i}"))
        .h(32.0)
        .pl(4.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.text_base.alpha(0.06))
        .help(file.path.clone())
        .child(preview)
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
            tr_ref(language, "Steer:")
        } else {
            tr_ref(language, "Queued:")
        },
        super::timeline::clip(&item.text, 32)
    );
    let mut menu = Menu::new(format!("queued-{id}"))
        .placement(rune_kit::Placement::TopStart)
        .trigger(
            Button::new(label.clone())
                .label_with(move |font, ink| {
                    row().grow(1.0).w_full().min_w(0.0).center_y().child(
                        text(label.clone())
                            .font(font)
                            .color(ink)
                            .truncate()
                            .min_w(0.0),
                    )
                })
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Large)
                .expanded(open)
                .on_press({
                    let id = id.clone();
                    move |s: &mut UiState| {
                        s.menu = (!open).then(|| OpenMenu::Queued(id.clone()));
                    }
                })
                .build(k)
                .key(format!("queued-pill-{id}"))
                .min_w(0.0)
                .w_full(),
        )
        .open(open)
        .min_width(160.0)
        .on_close(|s: &mut UiState| s.menu = None);
    let (steer_id, next_steer) = (id.clone(), !item.steer);
    menu = menu.item(
        MenuItem::new(if item.steer {
            tr_ref(language, "Keep queued")
        } else {
            tr_ref(language, "Send now")
        })
        .on_select(move |s: &mut UiState| toggle_queued_steer(s, &steer_id, next_steer)),
    );
    let pill = menu.build(k, layer);
    let chip_row = row()
        .key(format!("queued-row-{id}"))
        .matched(format!("queued-row-{id}"))
        .w_full()
        .min_w(0.0)
        .gap(8.0)
        .center_y();
    let mut content = row()
        .grow(1.0)
        .min_w(0.0)
        .center_y()
        .px(4.0)
        .radius(k.radius.xl)
        .fill(k.c.surface_raised_base)
        .border(1.0, k.c.border_weak);
    let mut thumbs: Vec<&str> = item
        .files
        .iter()
        .filter(|f| f.image)
        .map(|f| f.path.as_str())
        .collect();
    if item.files.is_empty() {
        if let Some(asid) = s.selected.as_deref() {
            thumbs.extend(s.queue_echo_images(asid, &id).iter().map(String::as_str));
        }
    }
    if let Some(thumbs) = queue_thumbs(s, k, &format!("q-{id}"), &thumbs) {
        content = content.child(thumbs);
    }
    let mut chip_row = chip_row.child(content.child(pill.grow(1.0).min_w(0.0)));
    if !item.steer {
        let send_id = id.clone();
        chip_row = chip_row.child(
            rune_kit::IconButton::new(&SEND)
                .label(tr_ref(language, "Send now"))
                .help(tr_ref(language, "Send now"))
                .variant(rune_kit::IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Large)
                .on_press(move |s: &mut UiState| toggle_queued_steer(s, &send_id, true))
                .build(k)
                .key(format!("queued-send-{id}"))
                .radius(k.radius.xl)
                .fill(k.c.bg_accent.alpha(0.12))
                .border(1.0, k.c.border_active.alpha(0.45)),
        );
    }
    {
        let (edit_id, text) = (id.clone(), item.text.clone());
        chip_row = chip_row.child(
            rune_kit::IconButton::new(&super::parts::glyphs::EDIT_FILE)
                .label(tr_ref(language, "Edit"))
                .help(tr_ref(language, "Edit"))
                .variant(rune_kit::IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Large)
                .on_press(move |s: &mut UiState| edit_queued(s, &edit_id, text.clone()))
                .build(k)
                .key(format!("queued-edit-{id}"))
                .radius(k.radius.xl)
                .fill(k.c.surface_raised_base)
                .border(1.0, k.c.border_weak),
        );
    }
    let cancel_inline_id = id.clone();
    chip_row.child(
        rune_kit::IconButton::new(k.glyph.close)
            .variant(rune_kit::IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Large)
            .label(tr_ref(language, "Cancel"))
            .on_press(move |s: &mut UiState| cancel_queued(s, &cancel_inline_id))
            .build(k)
            .key(format!("queued-cancel-{id}"))
            .radius(k.radius.xl)
            .fill(k.c.surface_raised_base)
            .border(1.0, k.c.border_weak),
    )
}

pub(super) fn chips(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    if s.mode == super::Mode::Home {
        if s.staged.is_empty() {
            return None;
        }
        let mut files = row().w_full().gap(6.0).flex_wrap();
        for (i, file) in s.staged.iter().enumerate() {
            files = files.child(staged_chip(s, k, i, file));
        }
        return Some(col().key("chips").w_full().gap(8.0).child(files));
    }
    let queue = &s.queue;
    let mut sending = s
        .pending_queue_echoes(s.selected.as_deref().unwrap_or_default())
        .peekable();
    if s.staged.is_empty() && queue.is_empty() && sending.peek().is_none() {
        return None;
    }
    let look = s.look();
    let mut line = col().key("chips").w_full().gap(8.0);
    for echo in sending {
        let key = format!("pending-queue-{}", echo.token);
        let mut receipt = row()
            .key(key.clone())
            .gap(8.0)
            .px(8.0)
            .py(4.0)
            .center_y()
            .radius(k.radius.md)
            .fill(k.c.surface_raised_base)
            .border(1.0, k.c.border_weak)
            .role("status");
        let paths = echo.images.iter().map(String::as_str).collect::<Vec<_>>();
        if let Some(thumbs) = queue_thumbs(s, k, &key, &paths) {
            receipt = receipt.child(thumbs);
        }
        let label = match (echo.state, echo.delivery) {
            (super::echo::EchoState::Sent, Some(super::echo::BusyDelivery::Queue)) => "Queued:",
            (super::echo::EchoState::Sent, _) => "Steer:",
            _ => "Sending…",
        };
        line = line.child(
            receipt.child(
                text(format!(
                    "{} {}",
                    tr_ref(s.cfg.general.language, label),
                    super::timeline::clip(&echo.text, 32)
                ))
                .font(k.ty.small)
                .color(k.c.text_muted),
            ),
        );
    }
    if !s.staged.is_empty() {
        let mut files = row().w_full().gap(6.0).flex_wrap();
        for (i, file) in s.staged.iter().enumerate() {
            files = files.child(staged_chip(s, k, i, file));
        }
        line = line.child(files);
    }
    if !queue.is_empty() {
        let page = s
            .composer_queue_page
            .as_ref()
            .and_then(|id| queue.iter().position(|q| &q.id == id))
            .unwrap_or(queue.len() - 1);
        let mut deck = col().key("queue-deck").w_full().max_w(600.0).gap(6.0);
        if queue.len() > 1 {
            let next = queue[(page + 1) % queue.len()].id.clone();
            deck = deck.child(
                row()
                    .gap(2.0)
                    .center_y()
                    .child(
                        Button::new(format!(
                            "{} {}/{}",
                            tr_ref(s.cfg.general.language, "Queued:"),
                            page + 1,
                            queue.len()
                        ))
                        .variant(ButtonVariant::GhostMuted)
                        .control_size(ControlSize::Small)
                        .on_press(|s: &mut UiState| {
                            s.composer_queue_expanded = !s.composer_queue_expanded
                        })
                        .build(k)
                        .key("queue-deck-expand"),
                    )
                    .child(
                        Button::new("›")
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .help(tr_ref(s.cfg.general.language, "Next"))
                            .on_press(move |s: &mut UiState| {
                                s.composer_queue_page = Some(next.clone())
                            })
                            .build(k)
                            .key("queue-deck-next"),
                    ),
            );
        }
        if s.composer_queue_expanded {
            let mut pages = col().w_full().gap(6.0);
            for item in queue {
                pages = pages.child(look.arrive(queued_chip(s, k, layer, item), 0));
            }
            deck = deck.child(
                col()
                    .w_full()
                    .h((queue.len() as f32 * 44.0).min(220.0))
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .clip_soft(0.0)
                    .child(pages)
                    .scroll_bar(k),
            );
        } else {
            let depth = (queue.len() - 1).min(2);
            let trailing_w = if queue[page].steer { 88.0 } else { 132.0 };
            let mut stack = el().w_full().pt(depth as f32 * 5.0);
            for n in (1..=depth).rev() {
                stack = stack.child(
                    el().key(format!("queue-page-{n}"))
                        .pin(
                            Some(n as f32 * 8.0),
                            Some((depth - n) as f32 * 5.0),
                            Some(trailing_w + n as f32 * 8.0),
                            Some(n as f32 * 5.0),
                        )
                        .radius(k.radius.xl)
                        .fill(k.c.layer_01)
                        .border(1.0, k.c.border_weak)
                        .a11y_hidden(),
                );
            }
            stack = stack.child(look.arrive(queued_chip(s, k, layer, &queue[page]), 0));
            deck = deck.child(stack);
        }
        line = line.child(deck);
    }
    Some(line)
}

/// A visual indicator bar shown when dragged files are hovering over the composer.
pub fn drop_hint(
    k: &Kit,
    look: &crate::theme::look::Look,
    language: crate::config::Language,
) -> El {
    let accent = k.c.bg_accent;
    let dark = look.dark;
    row()
        .key("composer-drop-hint")
        .w_full()
        .center()
        .gap(8.0)
        .py(6.0)
        .radius(k.radius.xl)
        .fill(accent.alpha(if dark { 0.20 } else { 0.12 }))
        .border(1.0, accent.alpha(0.6))
        .child(rune::icon(&DROP_ICON, 14.0).color(accent))
        .child(
            rune::text(tr_ref(language, "Drop files or images here to attach"))
                .font(k.ty.small_medium)
                .color(accent),
        )
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
    let all_comps = s.all_computers();
    let mut peers: Vec<_> = all_comps
        .into_iter()
        .filter(|p| {
            p.name.to_lowercase().starts_with(&term)
                || p.server_id.to_lowercase().starts_with(&term)
                || term.is_empty()
        })
        .collect();
    peers.sort_by_key(|p| match (p.current, p.reachable) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    });
    let mut rows: Vec<PopupRow> = Vec::new();
    if !peers.is_empty() {
        rows.push(section_header("hdr:computers", tr_ref(language, "Computers")).row);
        rows.extend(peers.into_iter().map(|p| {
            let detail = if p.current {
                tr_ref(language, "This computer")
            } else if p.reachable {
                ""
            } else {
                tr_ref(language, "Offline")
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
        rows.push(section_header("hdr:files", tr_ref(language, "Files in this project")).row);
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
        return Some(("slash-popup", tr_ref(language, "Commands"), slash));
    }
    let skills = read_popup(&draft.text, caret, &skill_trigger(s, slot), dismissed, true);
    if skills.open() {
        return Some(("skill-popup", tr_ref(language, "Skills"), skills));
    }
    let mentions = mention_popup(s, slot);
    mentions
        .open()
        .then(|| ("mention-popup", tr_ref(language, "Mentions"), mentions))
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

/// The draft field's keys while a popup is open: ↑ ↓ and Tab switch (wrapping, over
/// headers and offline computers), Enter picks, Esc dismisses.
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
        rune::Key::Tab => {
            if picks.is_empty() {
                return true;
            }
            if ev.mods.shift {
                let here = at
                    .and_then(|a| picks.iter().position(|p| *p == a))
                    .unwrap_or(0);
                let next = (here + picks.len() - 1) % picks.len();
                s.popup_active = Some((query.start, query.term.clone(), picks[next]));
                true
            } else if picks.len() == 1 {
                let at = picks[0];
                let (query, id) = (query.clone(), rows[at].id.clone());
                choose(s, slot, &query, &id);
                true
            } else {
                let here = at
                    .and_then(|a| picks.iter().position(|p| *p == a))
                    .unwrap_or(0);
                let next = (here + 1) % picks.len();
                s.popup_active = Some((query.start, query.term.clone(), picks[next]));
                true
            }
        }
        rune::Key::Enter if !ev.mods.shift => {
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
    let PopupState::Open { query, rows } = &state else {
        return;
    };
    if rows.is_empty() {
        return;
    }
    let active = active_row(s, &state);
    let look = s.look();
    let anchor = match slot {
        Slot::Session => "draft",
        Slot::Start => "start-draft",
    };

    let close_query = query.clone();
    layer.push(
        el().key(format!("{key}-scrim"))
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .on_press(move |s: &mut UiState| {
                s.popup_dismissed_at = Some(close_query.start);
                s.popup_active = None;
            }),
    );

    let header = row()
        .key(format!("{key}-header"))
        .w_full()
        .px(10.0)
        .py(4.0)
        .center_y()
        .child(
            text(label.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint),
        );

    let mut row_els = col()
        .key("popup-rows")
        .w_full()
        .growth(rune::GrowthSpec::default());

    for (i, r) in rows.iter().enumerate() {
        let is_active = active == Some(i);
        let id = r.id.clone();

        if r.id.starts_with("hdr:") {
            let hdr = row()
                .key(format!("{key}-hdr-{i}"))
                .w_full()
                .h(24.0)
                .px(10.0)
                .center_y()
                .child(text(&r.label).font(k.ty.small_medium).color(k.c.text_faint));
            row_els = row_els.child(hdr);
            continue;
        }

        let glyph = if id.starts_with("c:") {
            &super::parts::glyphs::TERMINAL
        } else if id.starts_with("s:") {
            &super::parts::glyphs::WORKFLOW
        } else if id.starts_with("p:") {
            &super::parts::glyphs::COMPUTER
        } else {
            &super::parts::glyphs::DOCUMENT
        };

        let icon_color = if is_active {
            k.c.text_accent
        } else {
            k.c.icon_muted
        };

        let mut item = row()
            .key(format!("{key}-opt-{id}"))
            .role("option")
            .label(&r.label)
            .description(&r.description)
            .position_in_set(i + 1, rows.len())
            .focus_selected(is_active)
            .w_full()
            .h(30.0)
            .px(8.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.md);

        if is_active {
            item = item.child(
                el().key("accent-bar")
                    .w(3.0)
                    .h(14.0)
                    .radius(1.5)
                    .fill(k.c.bg_accent),
            );
        } else {
            item = item.child(el().key("indent").w(3.0).h(14.0));
        }

        item = item.child(rune::icon(glyph, 14.0).color(icon_color).a11y_hidden());

        if !r.description.is_empty() {
            item = item
                .child(
                    text(&r.label)
                        .font(k.ty.ui_compact)
                        .color(if is_active {
                            k.c.text_accent
                        } else {
                            k.c.text_base
                        })
                        .shrink()
                        .truncate()
                        .a11y_hidden(),
                )
                .child(
                    text(&r.description)
                        .key("desc")
                        .font(k.ty.ui_compact)
                        .color(if is_active {
                            k.c.text_muted
                        } else {
                            k.c.text_weak
                        })
                        .grow(1.0)
                        .shrink()
                        .truncate()
                        .a11y_hidden(),
                );
        } else {
            item = item.child(
                text(&r.label)
                    .font(k.ty.ui_compact)
                    .color(if is_active {
                        k.c.text_accent
                    } else {
                        k.c.text_base
                    })
                    .grow(1.0)
                    .shrink()
                    .truncate()
                    .a11y_hidden(),
            );
        }

        if r.disabled {
            item = item.layer_opacity(0.45).disabled(true);
        } else {
            if is_active {
                item = item.fill(k.c.bg_accent.alpha(if s.dark { 0.20 } else { 0.14 }));
                item = look.accent_rim(k, item, k.c.bg_accent);
            } else {
                item = item.hover(k.c.hover);
            }

            let q_hover = query.clone();
            let q_press = query.clone();
            let id_clone = id.clone();
            item = item
                .on_hover(move |s: &mut UiState| {
                    s.popup_active = Some((q_hover.start, q_hover.term.clone(), i));
                })
                .on_press_tx(move |s: &mut UiState, _tx: &mut Tx| {
                    choose(s, slot, &q_press, &id_clone);
                });
        }

        row_els = row_els.child(item);
    }

    let viewport = col()
        .key("popup-viewport")
        .role("listbox")
        .label(label)
        .w_full()
        .h((rows.len() as f32 * 30.0).min(300.0))
        .min_h(30.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .clip_soft(0.0)
        .child(row_els)
        .scroll_bar(k)
        .w_full()
        .shrink()
        .min_h(0.0);

    let mut panel = col()
        .key(format!("{key}-panel"))
        .preserve_focus_on_press(true)
        .w(380.0)
        .padding(4.0)
        .radius(k.radius.md)
        .fill(look.panel_fill(k))
        .border(1.0, k.c.border_strong)
        .on_press(|_: &mut UiState| {})
        .child(header)
        .child(el().w_full().h(1.0).fill(k.c.border_weak).mb(2.0))
        .child(viewport);

    panel = look.accent_rim(k, panel, k.c.bg_accent);
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        panel = panel.box_shadow(shadow);
    }

    let anchored_panel = panel
        .anchored_fit(anchor, rune::Side::Top, rune::Align::Start, 6.0)
        .transition(rune::Transition::opacity().combined(rune::Transition::scale(0.97)))
        .token_motion(k.motion.overlay);

    layer.push(anchored_panel);
}

/// Mount whichever popup the draft calls for, at its field.
pub(super) fn popup(s: &UiState, k: &Kit, layer: &mut Vec<El>, slot: Slot) {
    if let Some((key, label, state)) = current_popup(s, slot) {
        mount_popup(s, k, layer, key, label, slot, state);
    }
}

/// Prompt history picker popup anchored above the composer draft.
pub(super) fn history_popup(s: &UiState, k: &Kit, layer: &mut Vec<El>, slot: Slot) {
    if !s.composer_history_open {
        return;
    }
    let language = s.cfg.general.language;
    let look = s.look();
    let anchor = match slot {
        Slot::Session => "draft",
        Slot::Start => "start-draft",
    };

    // Scrim overlay to catch outside clicks
    layer.push(
        el().key("prompt-history-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .on_press(|s: &mut UiState| {
                s.composer_history_open = false;
            }),
    );

    let count = s.prompt_history.len();
    let count_text = if count == 1 {
        format!("{count} {}", tr_ref(language, "item"))
    } else {
        format!("{count} {}", tr_ref(language, "items"))
    };

    let header = row()
        .key("prompt-history-header")
        .w_full()
        .px(10.0)
        .py(6.0)
        .center_y()
        .child(
            row()
                .gap(6.0)
                .center_y()
                .child(rune::icon(&super::parts::glyphs::TIME, 14.0).color(k.c.text_accent))
                .child(
                    text(tr_ref(language, "Prompt history"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                ),
        )
        .child(el().grow(1.0))
        .child(
            text(count_text)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .mr(8.0),
        )
        .child(
            rune_kit::IconButton::new(&super::parts::glyphs::CLOSE)
                .variant(rune_kit::IconButtonVariant::GhostMuted)
                .label(tr_ref(language, "Close"))
                .on_press(|s: &mut UiState| {
                    s.composer_history_open = false;
                })
                .build(k)
                .key("prompt-history-close"),
        );

    let mut row_els = col()
        .key("prompt-history-rows")
        .w_full()
        .gap(2.0)
        .px(4.0)
        .py(4.0);

    if s.prompt_history.is_empty() {
        row_els = row_els.child(
            col().w_full().center().py(18.0).child(
                text(tr_ref(language, "No prompt history yet"))
                    .font(k.ty.ui)
                    .color(k.c.text_faint),
            ),
        );
    } else {
        for (i, prompt) in s.prompt_history.iter().rev().enumerate() {
            let p_click = prompt.clone();
            let first_line = prompt.lines().next().unwrap_or(prompt);
            let display_text = if first_line.chars().count() > 70 {
                let s: String = first_line.chars().take(67).collect();
                format!("{s}…")
            } else if prompt.lines().count() > 1 {
                format!("{first_line}…")
            } else {
                first_line.to_string()
            };

            let item = row()
                .key(format!("prompt-history-item-{i}"))
                .w_full()
                .px(8.0)
                .py(6.0)
                .radius(k.radius.sm)
                .gap(8.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .hover(k.c.hover)
                .on_press(move |s: &mut UiState| {
                    match slot {
                        Slot::Session => {
                            s.draft.text = p_click.clone();
                            let len = s.draft.text.len();
                            s.draft.set_caret(len);
                            s.draft.focused = true;
                        }
                        Slot::Start => {
                            s.start_draft.text = p_click.clone();
                            let len = s.start_draft.text.len();
                            s.start_draft.set_caret(len);
                            s.start_draft.focused = true;
                        }
                    }
                    s.composer_history_open = false;
                    s.prompt_history_cursor = None;
                    s.prompt_history_draft_stash = None;
                })
                .child(
                    rune::icon(&super::parts::glyphs::TIME, 12.0)
                        .color(k.c.text_faint)
                        .a11y_hidden(),
                )
                .child(
                    text(display_text)
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .shrink()
                        .grow(1.0),
                );
            row_els = row_els.child(item);
        }
    }

    let item_count = s.prompt_history.len().max(1);
    let viewport = col()
        .key("prompt-history-viewport")
        .role("listbox")
        .label(tr_ref(language, "Prompt history"))
        .w_full()
        .h((item_count as f32 * 34.0).clamp(60.0, 320.0))
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .clip_soft(0.0)
        .child(row_els)
        .scroll_bar(k)
        .shrink()
        .min_h(0.0);

    let footer = row()
        .key("prompt-history-footer")
        .w_full()
        .px(10.0)
        .py(4.0)
        .center_y()
        .child(
            text(tr_ref(language, "Click prompt to recall · Esc to close"))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );

    let mut panel = col()
        .key("prompt-history-panel")
        .preserve_focus_on_press(true)
        .w(440.0)
        .padding(4.0)
        .radius(k.radius.md)
        .fill(look.panel_fill(k))
        .border(1.0, k.c.border_strong)
        .on_press(|_: &mut UiState| {})
        .child(header)
        .child(el().w_full().h(1.0).fill(k.c.border_weak).mb(2.0))
        .child(viewport)
        .child(el().w_full().h(1.0).fill(k.c.border_weak).mt(2.0))
        .child(footer);

    panel = look.accent_rim(k, panel, k.c.bg_accent);
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        panel = panel.box_shadow(shadow);
    }

    let anchored_panel = panel
        .anchored_fit(anchor, rune::Side::Top, rune::Align::Start, 6.0)
        .transition(rune::Transition::opacity().combined(rune::Transition::scale(0.97)))
        .token_motion(k.motion.overlay);

    layer.push(anchored_panel);
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
    let current = s
        .current()
        .and_then(|i| i.model.as_ref())
        .or_else(|| s.catalog.as_ref()?.catalog.defaults.model.as_ref());
    let model = choose_model(s.catalog.as_deref(), current, provider_id, model_id);
    apply_session_model(s, asid, model);
}

pub(super) fn choose_model(
    view: Option<&crate::hub::CatalogView>,
    current: Option<&ModelRef>,
    provider_id: String,
    model_id: String,
) -> ModelRef {
    let mut model = ModelRef {
        provider_id,
        model_id,
        variant: None,
    };
    model.variant = current
        .and_then(|m| m.variant.as_ref())
        .filter(|level| {
            view.and_then(|v| super::labels::catalog_model_info(v, &model))
                .and_then(|entry| entry.variants.as_ref())
                .is_some_and(|variants| variants.iter().any(|v| v.id == level.as_str()))
        })
        .cloned();
    model
}

fn apply_session_model(s: &mut UiState, asid: String, model: ModelRef) {
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == asid) {
        info.model = Some(model.clone());
    }
    s.send(Command::Compose(ComposeCommand::SwitchModel {
        asid: AgentSessionId(asid),
        model,
    }));
    remember(s);
}

pub(super) fn switch_effort(s: &mut UiState, slot: Slot, variant: Option<String>) {
    s.menu = None;
    let (view, explicit) = match slot {
        Slot::Start => (s.start_catalog.as_deref(), s.start_model.as_ref()),
        Slot::Session => (
            s.catalog.as_deref(),
            s.current().and_then(|i| i.model.as_ref()),
        ),
    };
    let Some(view) = view else { return };
    let Some(mut model) = explicit.or(view.catalog.defaults.model.as_ref()).cloned() else {
        return;
    };
    let offered = super::labels::catalog_model_info(view, &model).and_then(|m| m.variants.as_ref());
    if !offered.is_some_and(|levels| {
        !levels.is_empty()
            && variant
                .as_ref()
                .is_none_or(|level| levels.iter().any(|v| &v.id == level))
    }) {
        return;
    }
    model.variant = variant;
    match slot {
        Slot::Start => s.start_model = Some(model),
        Slot::Session => {
            if let Some(asid) = s.selected.clone() {
                apply_session_model(s, asid, model);
            }
        }
    }
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
        .on_press(move |s: &mut UiState| s.toggle_composer_menu(menu.clone()))
        .build(k)
        .key(key.to_string())
        .radius(k.radius.lg)
        .h(32.0)
        .px(12.0)
        .py(0.0)
        .fill(k.c.layer_01.alpha(0.35))
        .border(1.0, k.c.border_muted.alpha(0.25))
}

/// The model menu: App-style searchable, provider-grouped picker with inline effort chips.
/// `on_pick(state, provider, model)` runs on a choice.
#[allow(clippy::too_many_arguments)]
pub fn model_menu(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    kind: OpenMenu,
    scope: &str,
    slot: Slot,
    agent_id: &str,
    open: bool,
    label: String,
    catalog: Option<&crate::hub::CatalogView>,
    models: &[(String, String, String)],
    current: Option<&ModelRef>,
    on_pick: impl Fn(&mut UiState, String, String) + Clone + 'static,
) -> El {
    let key = match kind {
        OpenMenu::StartModel => "start-model",
        _ => "model",
    };
    let key = format!("{key}-{scope}");
    let trigger = pill(k, &format!("{key}-pill"), label, open, kind);
    super::model_picker::popup(
        s, k, layer, &key, trigger, open, slot, agent_id, catalog, models, current, on_pick,
    )
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
            row()
                .key(key)
                .gap(6.0)
                .center_y()
                .child(rune::icon(&super::icons::BRANCH, 16.0).color(k.c.icon_muted))
                .child(text(current).font(k.ty.small).color(k.c.text_muted))
                .h(32.0)
                .px(10.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01.alpha(0.35))
                .border(0.5, k.c.border_muted.alpha(0.3)),
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
    scope: &str,
    slot: Slot,
    open: bool,
    current: Option<&str>,
    variants: &[String],
) -> El {
    let label = current
        .map(super::labels::readable_catalog_id)
        .unwrap_or_else(|| "Default".into());
    let mut menu = Menu::new(format!("effort-{scope}"))
        .placement(rune_kit::Placement::TopStart)
        .trigger(pill(
            k,
            "effort-pill",
            label,
            open,
            match slot {
                Slot::Start => OpenMenu::StartEffort,
                Slot::Session => OpenMenu::Effort,
            },
        ))
        .open(open)
        .min_width(160.0)
        .on_close(|s: &mut UiState| s.menu = None);
    menu = menu.item(
        MenuItem::new("Default")
            .checked(current.is_none())
            .on_select(move |s: &mut UiState| switch_effort(s, slot, None)),
    );
    for variant in variants {
        let name = variant.clone();
        menu = menu.item(
            MenuItem::new(super::labels::readable_catalog_id(variant))
                .checked(current == Some(variant.as_str()))
                .on_select(move |s: &mut UiState| switch_effort(s, slot, Some(name.clone()))),
        );
    }
    menu.build(k, layer)
}

pub const AGENT_ICON: f32 = 16.0;
pub const SEND_ICON: f32 = 18.0;

pub fn body() -> El {
    col().w_full().gap(12.0).px(24.0).pt(16.0).pb(14.0)
}

pub fn attachment_button(
    k: &Kit,
    language: crate::config::Language,
    start: Option<PathBuf>,
    enabled: bool,
    key: &str,
) -> El {
    let attach_help = format!(
        "{} · {}",
        tr_ref(language, "Add files or an image"),
        crate::platform::keys::label("Primary+V")
    );
    let attach_press = move |_: &mut UiState| {
        crate::platform::dialogs::attach_files(start.clone());
    };
    rune_kit::IconButton::new(&super::parts::glyphs::ATTACH)
        .control_size(ControlSize::ExtraLarge)
        .variant(rune_kit::IconButtonVariant::GhostMuted)
        .label(tr_ref(language, "Attach"))
        .help(attach_help)
        .disabled(!enabled)
        .on_press(attach_press)
        .build(k)
        .key(key)
}

/// One editor, key handling and attachment paste path for both composer contexts.
fn newline_key(s: &mut UiState, slot: Slot, ev: &rune::KeyEvent) -> bool {
    if ev.key != rune::Key::Enter || ev.mods.ctrl || ev.mods.cmd || ev.mods.alt || ev.mods.shift {
        return false;
    }
    let draft = draft_of_mut(s, slot);
    if draft.key(ev, None).submit {
        draft.insert("\n");
    }
    on_draft_change(s, slot);
    true
}

pub fn editor(
    s: &UiState,
    k: &Kit,
    slot: Slot,
    placeholder: &str,
    min_rows: usize,
    max_rows: usize,
) -> El {
    let language = s.cfg.general.language;
    let accent = k.c.bg_accent;
    let info = k.c.info.icon;
    TextArea::new(draft_of(s, slot))
        .placeholder(placeholder)
        .label(tr_ref(
            language,
            if slot == Slot::Start {
                "First prompt"
            } else {
                "Draft"
            },
        ))
        .width(None)
        .auto_grow(min_rows, max_rows)
        .pill_style(move |id| {
            if id.starts_with("computer:") {
                PillStyle::default().bg(accent.alpha(0.22)).fg(accent)
            } else if id.starts_with("file:") {
                PillStyle::default().bg(info.alpha(0.18)).fg(info)
            } else if id.starts_with("image:") {
                PillStyle::default()
                    .bg(info.mix(accent, 0.4).alpha(0.22))
                    .fg(accent)
                    .icon(&PHOTO)
            } else {
                PillStyle::default()
            }
        })
        .on_edit(move |s: &mut UiState| draft_of_mut(s, slot))
        .on_change(move |s: &mut UiState, _: &mut Tx| on_draft_change(s, slot))
        .on_key(move |s: &mut UiState, ev: &rune::KeyEvent, tx: &mut Tx| {
            if super::keymap::intercept_global_key(s, ev, tx) {
                return true;
            }
            if draft_of(s, slot).preedit().is_some() {
                return false;
            }
            if popup_key(s, slot, ev) {
                return true;
            }
            if super::keymap::bind_matches_cfg(
                super::keymap::entry(super::keymap::Action::Send),
                &s.cfg.keymap,
                ev,
                tx,
            ) {
                match slot {
                    Slot::Start => super::home::submit_start(s),
                    Slot::Session => submit_session(s),
                }
                return true;
            }
            if slot == Slot::Start {
                if ev.key == rune::Key::Escape && s.error.is_some() {
                    s.error = None;
                    return true;
                }
                return newline_key(s, slot, ev) || super::keymap::field_key(s, ev, tx, false);
            }
            if newline_key(s, slot, ev) {
                return true;
            }
            if ev.key == rune::Key::Escape && s.composer_history_open {
                s.composer_history_open = false;
                return true;
            }
            if ev.key == rune::Key::Up
                && !ev.mods.cmd
                && !ev.mods.alt
                && (s.draft.text.is_empty() || s.prompt_history_cursor.is_some())
                && s.prompt_history_prev()
            {
                return true;
            }
            if ev.key == rune::Key::Down
                && !ev.mods.cmd
                && !ev.mods.alt
                && s.prompt_history_cursor.is_some()
                && s.prompt_history_next()
            {
                return true;
            }
            if ev.key == rune::Key::Escape && s.prompt_history_cursor.is_some() {
                s.reset_prompt_history_cursor();
                return true;
            }
            if ev.key == rune::Key::Escape && s.error.is_some() {
                s.error = None;
                return true;
            }
            if ev.key == rune::Key::Escape && s.show_diff {
                s.show_diff = false;
                return true;
            }
            // Alt+Enter while the agent works: queue after this turn.
            if super::keymap::bind_matches_cfg(
                super::keymap::entry(super::keymap::Action::Queue),
                &s.cfg.keymap,
                ev,
                tx,
            ) && s.busy()
            {
                queue_session(s);
                return true;
            }
            // Sidebar, pane, and composer keys, ahead of the field's caret moves.
            let at_end = caret_at_end(&s.draft);
            super::keymap::field_key(s, ev, tx, at_end)
        })
        .on_input_paste(|s: &mut UiState, content, _: &mut Tx| take_paste(s, content))
        .build(&bare_field(k))
        .motion(s.look().focus_fade())
        .key(if slot == Slot::Start {
            "start-draft"
        } else {
            "draft"
        })
        .w_full()
}

fn delivery_button(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str) -> El {
    let language = s.cfg.general.language;
    let queue = s.composer_queue_delivery;
    let open = s.menu == Some(OpenMenu::ComposerDelivery);
    let disabled =
        s.selected.is_none() || !ready_to_send(s, Slot::Session) || s.pending_submit.is_some();
    let caption = tr_ref(language, if queue { "Queue" } else { "Steer" });
    let action = if queue {
        super::keymap::Action::Queue
    } else {
        super::keymap::Action::Send
    };
    let menu = Menu::new(format!("composer-delivery-menu-{asid}"))
        .trigger(
            super::art::direction_button(
                s,
                k,
                format!(
                    "{} / {}",
                    tr_ref(language, "Steer"),
                    tr_ref(language, "Queue")
                ),
                if open {
                    -std::f32::consts::FRAC_PI_2
                } else {
                    std::f32::consts::FRAC_PI_2
                },
            )
            .variant(rune_kit::ButtonVariant::GhostMuted)
            .control_size(ControlSize::Large)
            .on_press(move |s: &mut UiState| s.menu = (!open).then_some(OpenMenu::ComposerDelivery))
            .build(k)
            .key("composer-delivery-options"),
        )
        .open(open)
        .min_width(180.0)
        .on_close(|s: &mut UiState| s.menu = None)
        .item(
            MenuItem::new(tr_ref(language, "Steer"))
                .checked(!queue)
                .shortcut(super::keymap::label_with(
                    super::keymap::Action::Send,
                    Some(&s.cfg.keymap),
                ))
                .on_select(|s: &mut UiState| {
                    s.composer_queue_delivery = false;
                    s.menu = None;
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Queue"))
                .checked(queue)
                .shortcut(super::keymap::label_with(
                    super::keymap::Action::Queue,
                    Some(&s.cfg.keymap),
                ))
                .on_select(|s: &mut UiState| {
                    s.composer_queue_delivery = true;
                    s.menu = None;
                }),
        );
    row()
        .key("composer-delivery")
        .h(32.0)
        .center_y()
        .radius(k.radius.lg)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            Button::new(caption)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .disabled(disabled)
                .shortcut(super::keymap::label_with(action, Some(&s.cfg.keymap)))
                .on_press(send_with_delivery)
                .build(k)
                .key("composer-deliver")
                .h(32.0),
        )
        .child(el().w(1.0).h(16.0).fill(k.c.border_weak))
        .child(menu.build(k, layer))
}

pub(super) fn send_with_delivery(s: &mut UiState) {
    if s.busy() && s.composer_queue_delivery {
        queue_session(s);
    } else {
        submit_session(s);
    }
}

/// Unique layout key for a composer card so split tiles each measure their own width.
pub(super) fn card_layout_key(asid: Option<&str>, in_split: bool) -> String {
    if in_split {
        if let Some(id) = asid.filter(|id| !id.is_empty()) {
            return format!("composer-card-{id}");
        }
    }
    "composer-card".to_string()
}

/// The same controls and metrics for focused and parked session composers.
pub(super) fn session_footer(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    asid: Option<&str>,
    catalog: Option<&crate::hub::CatalogView>,
    diff_stat: &crate::model::types::DiffStat,
    parked: Option<usize>,
) -> El {
    use super::composer_row as controls_row;
    let language = s.cfg.general.language;
    let busy = asid.is_some_and(|id| s.session_busy(id));
    let show_git_btn = asid.is_some_and(|id| {
        crate::plugin::is_enabled("inspector", &s.cfg.plugins_disabled)
            && s.session_by_id(id)
                .and_then(|info| info.directory.as_ref())
                .and_then(|dir| s.branches.get(dir))
                .and_then(|b| b.current.as_ref())
                .is_none()
    });
    let git_label = if diff_stat.files > 0 {
        format!("Git +{} −{}", diff_stat.added, diff_stat.removed)
    } else {
        "Git".into()
    };
    let show_stop = asid.is_some() && busy && (parked.is_some() || s.voice.is_none());
    let mut right_w = 0.0_f32;
    if show_git_btn {
        right_w += rune::text::measure(&git_label, &[], k.ty.small_medium, None).0 + 24.0 + 8.0;
    }
    if asid.is_some() {
        let ctx_w = rune::text::measure(tr_ref(language, "Context"), &[], k.ty.small_medium, None)
            .0
            .max(84.0);
        right_w += ctx_w + 26.0 + 8.0;
    }
    if busy {
        right_w += 108.0;
    }
    if show_stop {
        right_w += 108.0;
    }
    let card_key = card_layout_key(asid, s.in_split());
    let card_w = rune::memo::layout_with(&card_key, |r| r[2].round()).or_else(|| {
        (!s.in_split())
            .then(|| rune::memo::layout_with("composer-card", |r| r[2].round()))
            .flatten()
    });
    let room = card_w.map_or(f32::INFINITY, |w| (w - right_w).max(0.0));
    let own_line = asid
        .and_then(|id| controls_row::Controls::for_session(s, id, catalog))
        .map(|c| controls_row::own_line(s, &c, catalog, k, layer, room).key("composer-controls"));
    let mut footer = row()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .min_h(32.0)
        .gap(8.0)
        .center_y();
    if let Some(controls) = own_line {
        footer = footer.child(controls.w(0.0).grow(1.0).shrink().min_w(0.0).clip_soft(0.0));
    } else if busy {
        footer = footer.child(el().grow(1.0));
    }
    if let Some(asid) = asid {
        if show_git_btn {
            footer = footer.child(
                Button::new(git_label)
                    .variant(ButtonVariant::Neutral)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| s.toggle_git_inspector())
                    .build(k)
                    .key("composer-git")
                    .h(32.0)
                    .radius(k.radius.lg),
            );
        }
        let trigger = super::timeline::context_meter(s, k, asid).unwrap_or_else(|| {
            let target = asid.to_string();
            Button::new(tr_ref(language, "Context"))
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| super::parts::context::open(s, &target))
                .build(k)
                .h(32.0)
                .radius(k.radius.lg)
        });
        footer = footer.child(super::parts::context::popover(s, k, layer, asid, trigger));
    }
    if busy {
        footer = footer.child(delivery_button(s, k, layer, asid.unwrap_or("none")));
    }
    if let Some(i) = parked {
        footer = el()
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .on_background_press(move |s: &mut UiState| s.focus_tile(i))
            .child(footer.inert().w_full());
    }
    let mut bar = row()
        .key("composer-footer")
        .w_full()
        .min_w(0.0)
        .center_y()
        .gap(8.0)
        .child(footer);
    if let Some(asid) = asid.filter(|_| show_stop) {
        let target = AgentSessionId(asid.to_owned());
        bar = bar.child(
            Button::new(tr_ref(language, "Stop"))
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Regular)
                .shortcut(super::keymap::label_with(
                    super::keymap::Action::Stop,
                    Some(&s.cfg.keymap),
                ))
                .on_press(move |s: &mut UiState| {
                    if let Some(i) = parked {
                        s.focus_tile(i);
                    }
                    s.send(Command::Interrupt(target.clone()));
                })
                .build(k)
                .key("stop")
                .h(32.0)
                .radius(k.radius.lg),
        );
    }
    bar
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let language = s.cfg.general.language;
    let busy = s.busy();
    let has_session = s.selected.is_some();
    let focused = s.draft.focused && has_session;
    let start = s
        .current()
        .and_then(|i| i.directory.clone().map(PathBuf::from));
    let has_text = ready_to_send(s, Slot::Session);

    // The row under the field: the session's controls on the left (see
    // `composer_row`), the actions on the right.
    let attach = attachment_button(k, language, start, has_session, "attach");
    // Measure the actual card so controls also fold inside split panes.
    use super::composer_row::{ACTION_GAP, CARD_PAD};
    let compact = s.in_split();
    let card_key = card_layout_key(s.selected.as_deref(), compact);
    let inner = rune::memo::layout_with(&card_key, |r| r[2].round())
        .map_or(f32::INFINITY, |w| w - CARD_PAD - 24.0);
    let mut tools = row()
        .gap(ACTION_GAP)
        .center_y()
        .flex_wrap()
        .shrink()
        .min_w(0.0)
        .child(attach);
    if has_session && !s.in_split() {
        tools = tools.child(super::orchestration::composer_bar(
            s,
            k,
            layer,
            Slot::Session,
        ));
    }
    let mut actions = row().key("composer-actions").gap(ACTION_GAP).center_y();
    if s.cfg.voice.usable() || s.voice.is_some() {
        actions = actions.child(
            super::voice::controls(s, k, inner - 110.0)
                .unwrap_or_else(|| super::voice::mic_button(s, k)),
        );
    }
    let send = Button::new(tr_ref(language, "Send"))
        .label_with(|_, ink| rune::icon(&SEND, SEND_ICON).color(ink))
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Regular)
        .disabled(!has_session || !has_text || s.pending_submit.is_some())
        .shortcut(super::keymap::label_with(
            if busy && s.composer_queue_delivery {
                super::keymap::Action::Queue
            } else {
                super::keymap::Action::Send
            },
            Some(&s.cfg.keymap),
        ))
        .help(tr_ref(
            language,
            if busy && s.composer_queue_delivery {
                "Queue"
            } else if busy {
                "Steer the running turn"
            } else {
                "Send"
            },
        ))
        .on_press(send_with_delivery)
        .build(k)
        .key("send")
        .size(36.0, 36.0)
        .radius(k.radius.lg);
    let send = look.glow(
        send,
        k.c.bg_accent,
        if has_text && has_session { 0.9 } else { 0.0 },
    );
    if s.voice.is_none() {
        actions = actions.child(send);
    }

    let actions = row()
        .w_full()
        .center_y()
        .child(tools)
        .child(el().grow(1.0))
        .child(actions);

    popup(s, k, layer, Slot::Session);
    history_popup(s, k, layer, Slot::Session);
    let mut card = if compact {
        col().w_full().gap(4.0).px(16.0).pt(8.0).pb(8.0)
    } else {
        body()
    }
    .key(card_key);

    if s.drop_hover {
        card = card.child(drop_hint(k, &look, language));
    }
    let placeholder = if has_session {
        tr_ref(language, "Message…")
    } else {
        tr_ref(language, "Pick a session first")
    };
    let draft_editor = editor(
        s,
        k,
        Slot::Session,
        placeholder,
        if compact { 1 } else { 2 },
        12,
    );
    let draft_editor =
        if let Some(voice) = s.voice.as_ref().filter(|v| !v.partial.trim().is_empty()) {
            let mut preview = s.draft.text.clone();
            if !preview.is_empty() && !preview.ends_with(char::is_whitespace) {
                preview.push(' ');
            }
            preview.push_str(&voice.partial);
            // The real draft determines height and stays untouched until final transcription.
            el().key("voice-draft-preview")
                .w_full()
                .child(draft_editor.opacity(0.0))
                .child(
                    col()
                        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                        .clip_soft(0.0)
                        .scroll(rune::ScrollSpec { stick_bottom: true })
                        .on_press(|_: &mut UiState| {})
                        .child(text(preview).font(k.ty.body).color(k.c.text_base)),
                )
        } else {
            draft_editor
        };
    card = card.child(draft_editor);
    card = card.child(actions.w_full());

    // Keep focus visible without competing with the message content.
    let card = composer_card(k, &look, card, focused, busy, s.drop_hover)
        .on_hover_change(|s: &mut UiState, inside, tx| {
            if inside
                && s.menu.is_none()
                && s.row_menu.is_none()
                && s.context_open.is_none()
                && s.zoom.is_none()
                && !s.timeline_search.active
                && s.tile_drag.is_none()
                && s.tile_card_drag.is_none()
                && s.draft.preedit().is_none()
            {
                s.draft.focused = true;
                s.term.sink.focused = false;
                tx.focus("draft", false);
            }
        })
        .on_drop_files(|s: &mut UiState, files, _| attach_dropped_files(s, files))
        .on_drop_hover(|s: &mut UiState, over, _| s.drop_hover = over);

    // The messages' reading column: as wide and as centred as they are on
    // a wide window, the whole width on a narrow one.
    let column = super::timeline::FEED_MAX_W;
    let mut deck = col().w_full().max_w(column).gap(6.0);
    if let Some(asid) = s.selected.as_deref() {
        let pending = super::parts::question::pending_forms(s, asid);
        let permissions = s
            .waiting_perms
            .get(asid)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if !pending.is_empty() || !permissions.is_empty() {
            let height = rune::memo::layout_with("main", |r| r[3] * 0.4)
                .unwrap_or(320.0)
                .clamp(140.0, 360.0);
            deck = deck.child(
                col()
                    .key("pending-questions")
                    .motion(look.focus_fade())
                    .w_full()
                    .max_h(height)
                    .gap(8.0)
                    .children(permissions.iter().map(|request| {
                        super::parts::approval::permission_card(s, k, request)
                            .key(format!("pending-permission-{}", request.id))
                    }))
                    .children(pending.into_iter().map(|form| {
                        let card = super::parts::question::form_card(s, k, form)
                            .key(format!("pending-question-{}", form.id));
                        if look.motion {
                            card.transition(
                                rune::Transition::opacity()
                                    .combined(rune::Transition::offset(0.0, 6.0)),
                            )
                            .motion(look.focus_fade())
                        } else {
                            card.motion(rune::Spring::NONE)
                        }
                    }))
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_bar(k),
            );
        }
    }
    if let Some(chips) = chips(s, k, layer) {
        deck = deck.child(chips);
    }
    let footer = session_footer(
        s,
        k,
        layer,
        s.selected.as_deref(),
        s.catalog.as_deref(),
        &s.diff_stat,
        None,
    );
    deck = deck.child(card.w_full()).child(footer);
    col()
        .key("composer")
        .w_full()
        .items_center()
        .px(16.0)
        .pt(6.0)
        .pb(8.0)
        .child(deck)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_uses_native_default_and_only_follows_compatible_model_switches() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let catalog = std::sync::Arc::new(crate::hub::CatalogView {
            catalog: serde_json::from_value(serde_json::json!({
                "models":[
                    {"id":"one","provider_id":"p","name":"One","variants":[{"id":"deep"}]},
                    {"id":"two","provider_id":"p","name":"Two","variants":[{"id":"deep"}]},
                    {"id":"two","provider_id":"q","name":"Two","variants":[{"id":"quick"}]}
                ],
                "defaults":{"model":{"provider_id":"p","model_id":"one"}},
                "modes":[],"mcp":[]
            }))
            .unwrap(),
        });
        while rx.try_recv().is_ok() {}
        s.start_catalog = Some(catalog.clone());
        switch_effort(&mut s, Slot::Start, Some("deep".into()));
        let picked = s.start_model.as_ref().unwrap();
        assert_eq!(
            (picked.model_id.as_str(), picked.variant.as_deref()),
            ("one", Some("deep"))
        );
        assert!(rx.try_recv().is_err());
        let session = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "composer-idle")
            .unwrap()
            .1;
        session(&mut s);
        let sid = s.selected.clone().unwrap();
        s.sessions
            .iter_mut()
            .find(|i| i.asid.0 == sid)
            .unwrap()
            .model = None;
        s.catalog = Some(catalog);
        switch_effort(&mut s, Slot::Session, Some("deep".into()));
        let selected = s.current().unwrap().model.as_ref().unwrap();
        assert_eq!(
            (selected.model_id.as_str(), selected.variant.as_deref()),
            ("one", Some("deep"))
        );
        switch_model(&mut s, "p".into(), "two".into());
        assert_eq!(
            s.current()
                .unwrap()
                .model
                .as_ref()
                .unwrap()
                .variant
                .as_deref(),
            Some("deep")
        );
        switch_model(&mut s, "q".into(), "two".into());
        assert_eq!(s.current().unwrap().model.as_ref().unwrap().variant, None);
        switch_effort(&mut s, Slot::Session, Some("deep".into()));
        assert_eq!(s.current().unwrap().model.as_ref().unwrap().variant, None);
    }

    #[test]
    fn editing_a_steered_image_preserves_files_and_delivery_without_sending() {
        use crate::model::compose::{InboxItem, QueuedFile};
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("session".into());
        s.composer_queue_delivery = true;
        s.queue.push(InboxItem {
            id: "pending".into(),
            text: "Look at this image".into(),
            steer: true,
            files: vec![QueuedFile {
                path: "/uploads/image.png".into(),
                name: "image.png".into(),
                image: true,
            }],
        });
        edit_queued(&mut s, "pending", "Look at this image".into());
        assert_eq!(s.draft.text, "Look at this image");
        assert!(!s.composer_queue_delivery);
        assert_eq!(s.staged.len(), 1);
        assert_eq!(s.staged[0].path, "/uploads/image.png");
        assert!(s.staged[0].image);
        assert!(s.queue.is_empty());
        let mut cancelled = false;
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Compose(ComposeCommand::InboxCancel { item, .. }) => {
                    assert_eq!(item, "pending");
                    cancelled = true;
                }
                Command::Compose(ComposeCommand::Submit { .. }) => {
                    panic!("editing must not submit")
                }
                _ => {}
            }
        }
        assert!(cancelled);
    }

    #[test]
    fn queued_and_steered_prompts_from_session_never_leak_to_home_chips() {
        use crate::model::compose::{InboxItem, StagedFile};
        use crate::ui::Mode;
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("session-1".into());
        s.queue.push(InboxItem {
            id: "pending-steer".into(),
            text: "Steer prompt".into(),
            steer: true,
            files: Vec::new(),
        });
        let k = Kit::neutral(true);
        let mut layer = Vec::new();

        // In session mode, chips displays the queued/steered items
        s.mode = Mode::Session;
        assert!(chips(&s, &k, &mut layer).is_some());

        // In Home mode, chips NEVER displays queued/steered items from background sessions
        s.mode = Mode::Home;
        assert!(chips(&s, &k, &mut layer).is_none());

        // Adding staged files in Home mode displays only those staged files
        s.staged.push(StagedFile {
            draft_token: None,
            path: "/path/to/file.txt".into(),
            name: "file.txt".into(),
            size: 12,
            image: false,
        });
        assert!(chips(&s, &k, &mut layer).is_some());
    }

    #[test]
    fn clicking_queued_send_button_steers_item_immediately() {
        use crate::model::compose::InboxItem;
        use crate::ui::Mode;
        use rune::{Cx, Env, Runtime};

        fn view(s: &UiState, _: &mut Cx<UiState>) -> El {
            let k = Kit::neutral(true);
            let mut layer = Vec::new();
            let body = chips(s, &k, &mut layer).unwrap_or_else(el);
            rune_kit::root(el().w_full().h_full().center().child(body), layer)
        }

        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.mode = Mode::Session;
        s.selected = Some("session-1".into());
        s.queue.push(InboxItem {
            id: "q-42".into(),
            text: "Follow up right now".into(),
            steer: false,
            files: Vec::new(),
        });

        let mut rt = Runtime::new(s, view, (800.0, 400.0), Env::new(true, 1.0));
        for i in 0..10 {
            rt.frame(i as f64 / 60.0);
        }
        let (x, y) = rt
            .find("queued-send-q-42")
            .expect("queued send-now button must be rendered");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.3);

        assert!(
            rt.state.queue[0].steer,
            "clicking queued send-now should mark item as steer"
        );
        let mut steered = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Compose(ComposeCommand::InboxDelivery { asid, item, steer }) = cmd {
                assert_eq!(asid.0, "session-1");
                assert_eq!(item, "q-42");
                assert!(steer);
                steered = true;
            }
        }
        assert!(
            steered,
            "expected ComposeCommand::InboxDelivery(steer=true)"
        );
    }

    #[test]
    fn composer_grows_and_shrinks_with_content_in_both_contexts() {
        use rune::{Env, Runtime};
        for scene_name in ["home", "session"] {
            let (tx, _rx) = crate::ui::commands::channel();
            let mut s = UiState::new(tx);
            s.splash = None;
            let (_, scene) = crate::ui::scenes::scenes()
                .iter()
                .find(|(name, _)| *name == scene_name)
                .unwrap();
            scene(&mut s);
            let slot = if scene_name == "home" {
                Slot::Start
            } else {
                Slot::Session
            };
            let key = if slot == Slot::Start {
                "start-draft"
            } else {
                "draft"
            };
            *draft_of_mut(&mut s, slot) = fresh("");
            let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            let height = |rt: &Runtime<UiState>| {
                rt.elements()
                    .into_iter()
                    .find(|n| n.key.as_deref() == Some(key))
                    .unwrap()
                    .rect[3]
            };
            let initial = height(&rt);
            *draft_of_mut(&mut rt.state, slot) =
                fresh("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");
            rt.invalidate();
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            assert!(height(&rt) > initial + 40.0, "{scene_name} did not grow");
            *draft_of_mut(&mut rt.state, slot) = fresh("");
            rt.invalidate();
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            assert!(
                (height(&rt) - initial).abs() < 1.0,
                "{scene_name} did not shrink"
            );
        }
    }

    #[test]
    fn delivery_picker_changes_mode_without_sending_until_pressed() {
        use rune::{Env, Runtime};
        for (item, expected) in [(1, "queue"), (0, "steer")] {
            let (tx, mut rx) = crate::ui::commands::channel();
            let mut state = UiState::new(tx);
            state.splash = None;
            let (_, scene) = crate::ui::scenes::scenes()
                .iter()
                .find(|(n, _)| *n == "session")
                .unwrap();
            scene(&mut state);
            state.draft = fresh("Follow-up with chosen delivery");
            let mut rt = Runtime::new(
                state,
                crate::ui::view,
                (1440.0, 1000.0),
                Env::new(true, 1.0),
            );
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            while rx.try_recv().is_ok() {}
            let click = |rt: &mut Runtime<UiState>, key: &str| {
                let node = rt
                    .elements()
                    .into_iter()
                    .find(|n| n.key.as_deref() == Some(key))
                    .unwrap();
                let (x, y) = (
                    node.rect[0] + node.rect[2] / 2.0,
                    node.rect[1] + node.rect[3] / 2.0,
                );
                rt.pointer_down(x, y);
                rt.pointer_up(x, y);
                for _ in 0..30 {
                    rt.frame(1.0 / 60.0);
                }
            };
            click(&mut rt, "composer-delivery-options");
            let asid = rt.state.selected.clone().unwrap();
            click(
                &mut rt,
                &format!("composer-delivery-menu-{asid}-item{item}"),
            );
            assert_eq!(rt.state.composer_queue_delivery, expected == "queue");
            while let Ok(command) = rx.try_recv() {
                assert!(!matches!(
                    command,
                    Command::Compose(ComposeCommand::Submit { .. })
                ));
            }
            click(
                &mut rt,
                if expected == "queue" {
                    "send"
                } else {
                    "composer-deliver"
                },
            );
            let mut sent = Vec::new();
            while let Ok(command) = rx.try_recv() {
                if let Command::Compose(ComposeCommand::Submit { delivery, .. }) = command {
                    sent.push(delivery);
                }
            }
            assert_eq!(sent, vec![Some(expected.into())]);
            assert!(rt.state.draft.text.is_empty());
            click(&mut rt, "send");
            click(&mut rt, "stop");
            let mut interrupted = Vec::new();
            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::Interrupt(target) => interrupted.push(target.0),
                    Command::Compose(ComposeCommand::Submit { .. }) => {
                        panic!("empty draft submitted")
                    }
                    _ => {}
                }
            }
            assert_eq!(interrupted, vec![asid]);
        }
    }

    #[test]
    fn default_enter_send_keeps_shift_enter_for_newlines() {
        use rune::{Env, Key, KeyEvent, Mods, Runtime};
        for name in ["home", "session"] {
            let (tx, mut rx) = crate::ui::commands::channel();
            let mut state = UiState::new(tx);
            state.splash = None;
            let (_, scene) = crate::ui::scenes::scenes()
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap();
            scene(&mut state);
            let slot = if name == "home" {
                Slot::Start
            } else {
                Slot::Session
            };
            *draft_of_mut(&mut state, slot) = fresh("Keyboard check");
            let mut rt = Runtime::new(state, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
            rt.on_key = Some(crate::ui::on_key);
            rt.frame(0.0);
            rt.frame(0.02);
            while rx.try_recv().is_ok() {}
            rt.key(KeyEvent {
                key: Key::Enter,
                mods: Mods {
                    shift: true,
                    ..Default::default()
                },
            });
            rt.frame(0.02);
            assert!(draft_of(&rt.state, slot).text.contains('\n'));
            while let Ok(command) = rx.try_recv() {
                assert!(!matches!(
                    command,
                    Command::Compose(ComposeCommand::Submit { .. })
                ));
            }
            rt.key(KeyEvent {
                key: Key::Enter,
                mods: Mods::default(),
            });
            rt.frame(0.02);
            let mut submits = 0;
            while let Ok(command) = rx.try_recv() {
                if matches!(command, Command::Compose(ComposeCommand::Submit { .. })) {
                    submits += 1;
                }
            }
            assert_eq!(submits, 1, "configured Enter must send once on {name}");
        }
    }

    #[test]
    fn remapped_send_replaces_default_in_both_composers() {
        use rune::{Env, Key, KeyEvent, Mods, Runtime};
        for scene_name in ["home", "session"] {
            let (tx, mut rx) = crate::ui::commands::channel();
            let mut s = UiState::new(tx);
            s.splash = None;
            let (_, scene) = crate::ui::scenes::scenes()
                .iter()
                .find(|(name, _)| *name == scene_name)
                .unwrap();
            scene(&mut s);
            s.cfg.keymap.bindings.insert("send".into(), "alt+s".into());
            let slot = if scene_name == "home" {
                Slot::Start
            } else {
                Slot::Session
            };
            *draft_of_mut(&mut s, slot) = fresh("Verify keyboard routing");
            let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
            rt.on_key = Some(crate::ui::on_key);
            rt.frame(0.0);
            rt.frame(0.02);
            while rx.try_recv().is_ok() {}
            let before = draft_of(&rt.state, slot).text.clone();
            rt.key(KeyEvent {
                key: Key::Enter,
                mods: Mods::default(),
            });
            rt.frame(0.02);
            assert_eq!(
                draft_of(&rt.state, slot).text.matches('\n').count(),
                before.matches('\n').count() + 1,
                "Enter must insert a newline on {scene_name}"
            );
            while let Ok(command) = rx.try_recv() {
                assert!(!matches!(
                    command,
                    Command::Compose(ComposeCommand::Submit { .. })
                ));
            }
            rt.key(KeyEvent {
                key: Key::Enter,
                mods: Mods {
                    ctrl: !cfg!(target_os = "macos"),
                    cmd: cfg!(target_os = "macos"),
                    ..Default::default()
                },
            });
            rt.frame(0.04);
            while let Ok(command) = rx.try_recv() {
                assert!(
                    !matches!(command, Command::Compose(ComposeCommand::Submit { .. })),
                    "old send chord still submits on {scene_name}"
                );
            }
            rt.key(KeyEvent {
                key: Key::Command('s'),
                mods: Mods {
                    alt: true,
                    ..Default::default()
                },
            });
            rt.frame(0.06);
            let mut submits = 0;
            while let Ok(command) = rx.try_recv() {
                if matches!(command, Command::Compose(ComposeCommand::Submit { .. })) {
                    submits += 1;
                }
            }
            assert_eq!(
                submits, 1,
                "custom send must submit exactly once on {scene_name}"
            );
        }
    }

    #[test]
    fn inserting_multiple_images_paces_and_numbers_them() {
        let mut draft = TextEdit::new("Problem 1: ");
        draft.set_caret(draft.text.len());
        insert_image_pill(&mut draft, 101);
        draft.insert("Problem 2: ");
        insert_image_pill(&mut draft, 102);

        assert_eq!(draft.text, "Problem 1: [Image #1] Problem 2: [Image #2] ");
        let pills: Vec<_> = draft.pills().map(|(_, id)| id).collect();
        assert_eq!(pills, vec!["image:101", "image:102"]);
    }

    #[test]
    fn prompt_history_recall_and_escape_restore() {
        let (tx, _rx) = crate::ui::commands::channel();
        let mut s = UiState::new(tx);
        s.record_prompt_history("echo hello");
        s.record_prompt_history("git status");

        s.draft.text = "my unfinished prompt".to_string();
        s.draft.set_caret(s.draft.text.len());

        // Up recalls "git status" and stashes "my unfinished prompt"
        assert!(s.prompt_history_prev());
        assert_eq!(s.draft.text, "git status");
        assert_eq!(
            s.prompt_history_draft_stash.as_deref(),
            Some("my unfinished prompt")
        );

        // Up recalls "echo hello"
        assert!(s.prompt_history_prev());
        assert_eq!(s.draft.text, "echo hello");

        // Down goes back to "git status"
        assert!(s.prompt_history_next());
        assert_eq!(s.draft.text, "git status");

        // Escape restores stashed "my unfinished prompt"
        s.reset_prompt_history_cursor();
        assert_eq!(s.draft.text, "my unfinished prompt");
        assert!(s.prompt_history_cursor.is_none());
        assert!(s.prompt_history_draft_stash.is_none());
    }

    #[test]
    fn attach_dropped_files_stages_files_and_inserts_image_pills() {
        let (tx, mut rx) = crate::ui::commands::channel();
        let mut s = UiState::new(tx);
        s.mode = crate::ui::Mode::Session;
        s.drop_hover = true;

        let files = vec![
            PathBuf::from("/tmp/test-screenshot.png"),
            PathBuf::from("/tmp/app.log"),
        ];

        attach_dropped_files(&mut s, &files);

        // drop_hover should be cleared
        assert!(!s.drop_hover);

        // Draft should contain pill for the image
        assert!(s.draft.text.contains("[Image #1]"));
        let pills: Vec<_> = s.draft.pills().map(|(_, id)| id).collect();
        assert_eq!(pills, vec!["image:1"]);

        // Staging commands should be sent over channel
        let mut staged_names = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Compose(ComposeCommand::Stage { name, .. }) = cmd {
                staged_names.push(name);
            }
        }
        assert_eq!(staged_names, vec!["test-screenshot.png", "app.log"]);
    }

    #[test]
    fn composer_double_click_word_and_triple_click_line_selection() {
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::ui::commands::channel();
        let mut s = UiState::new(tx);
        s.splash = None;
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .expect("session scene");
        scene(&mut s);
        s.draft = fresh("refactor dwindle layout\nand update tests");

        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(crate::ui::on_key);
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);

        let draft = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some("draft"))
            .expect("draft node");
        // Click inside the first word ("refactor")
        let (x, y) = (draft.rect[0] + 18.0, draft.rect[1] + 16.0);

        // Single click places caret
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.draft.focused);
        assert_eq!(rt.state.draft.anchor, rt.state.draft.focus);

        // Second click within double-click interval selects the word "refactor"
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.draft.selected(), "refactor");

        // Third click within interval selects the whole first line
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        let line = rt.state.draft.selected();
        assert!(
            line.starts_with("refactor dwindle layout"),
            "triple-click should select the line, got {line:?}"
        );

        // bare_field clears blend_over so selection keeps full linear alpha
        let mut k = Kit::neutral(true);
        k.blend_over = Some(rune::Color {
            r: 0.05,
            g: 0.08,
            b: 0.07,
            a: 1.0,
        });
        let bare = bare_field(&k);
        assert!(bare.blend_over.is_none());
        assert!(bare.selection().a >= 0.29);
    }

    #[test]
    fn queue_deck_defaults_to_latest_item_and_stacks_background_cards_above_front() {
        use crate::model::compose::InboxItem;
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::ui::commands::channel();
        let mut s = UiState::new(tx);
        s.splash = None;
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .expect("session scene");
        scene(&mut s);
        s.composer_queue_page = None;
        s.composer_queue_expanded = false;
        s.queue = vec![
            InboxItem {
                id: "q1".into(),
                text: "First queued prompt".into(),
                steer: false,
                files: Vec::new(),
            },
            InboxItem {
                id: "q2".into(),
                text: "Second queued prompt".into(),
                steer: false,
                files: Vec::new(),
            },
            InboxItem {
                id: "q3".into(),
                text: "Latest queued prompt".into(),
                steer: false,
                files: Vec::new(),
            },
        ];

        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);

        let nodes = rt.elements();
        let expand = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("queue-deck-expand"))
            .expect("queue-deck-expand button");
        assert!(
            expand.label.as_deref().is_some_and(|l| l.contains("3/3")),
            "expected default page to show 3/3, got {:?}",
            expand.label
        );

        let front = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("queued-row-q3"))
            .expect("latest queued item should be the front card");
        assert!(
            nodes
                .iter()
                .all(|n| n.key.as_deref() != Some("queued-row-q1")),
            "older queued item should not be front card by default"
        );

        let page1 = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("queue-page-1"))
            .expect("queue-page-1 background card");
        let page2 = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("queue-page-2"))
            .expect("queue-page-2 background card");

        // Background cards sit above the front card (smaller y / top) and recede narrower.
        assert!(
            page2.rect[1] < page1.rect[1] && page1.rect[1] < front.rect[1],
            "expected page2.top ({}) < page1.top ({}) < front.top ({})",
            page2.rect[1],
            page1.rect[1],
            front.rect[1]
        );
        assert!(
            page2.rect[2] < page1.rect[2] && page1.rect[2] < front.rect[2],
            "expected page2.width ({}) < page1.width ({}) < front.width ({})",
            page2.rect[2],
            page1.rect[2],
            front.rect[2]
        );
        assert!(
            page2.rect[0] > page1.rect[0] && page1.rect[0] > front.rect[0],
            "expected page2.left ({}) > page1.left ({}) > front.left ({})",
            page2.rect[0],
            page1.rect[0],
            front.rect[0]
        );
    }
}

/// Deliver completed uploads only when their original composer is visible again.
pub(super) fn restore_attachments(s: &mut UiState) {
    let target = match s.mode {
        super::Mode::Home => None,
        super::Mode::Session => s.selected.clone(),
        _ => return,
    };
    if let Some(files) = s
        .deferred_attachments
        .remove(&(s.current_computer().to_owned(), target))
    {
        s.staged.extend(files.into_iter().filter(|file| {
            file.draft_token.is_none_or(|id| match s.mode {
                super::Mode::Home => has_image_pill(&s.start_draft, id),
                _ => has_image_pill(&s.draft, id),
            })
        }));
    }
}

fn has_image_pill(draft: &TextEdit, id: u64) -> bool {
    draft.pills().any(|(_, pill)| pill == format!("image:{id}"))
}

fn remove_image_pill(draft: &mut TextEdit, id: u64) {
    let ranges: Vec<_> = draft
        .pills()
        .filter(|(_, pill)| *pill == format!("image:{id}"))
        .map(|(range, _)| range)
        .collect();
    for range in ranges.into_iter().rev() {
        draft.set_selection(range.start, range.end);
        draft.insert("");
    }
}

pub(super) fn finish_staging(
    s: &mut UiState,
    id: u64,
    result: Result<crate::model::compose::StagedFile, String>,
) {
    let target = s.pending_file_targets.remove(&id);
    let linked = s.pending_image_uploads.remove(&id);
    let mut file = match result {
        Ok(file) => file,
        Err(err) => {
            if linked {
                remove_image_pill(&mut s.draft, id);
                remove_image_pill(&mut s.start_draft, id);
                for tile in &mut s.tiles {
                    remove_image_pill(&mut tile.slot.draft, id);
                }
            }
            s.toast_error("Attachment failed", Some(err));
            return;
        }
    };
    if let Some((computer, slot)) = &target {
        if computer != s.current_computer() {
            file.draft_token = linked.then_some(id);
            s.deferred_attachments
                .entry((computer.clone(), slot.clone()))
                .or_default()
                .push(file);
            return;
        }
    }
    let target = target.map(|(_, slot)| slot);
    if !linked {
        match target {
            Some(Some(asid))
                if s.mode != super::Mode::Session || s.selected.as_deref() != Some(&asid) =>
            {
                if s.selected.as_deref() == Some(&asid) {
                    s.deferred_attachments
                        .entry((s.current_computer().to_owned(), Some(asid)))
                        .or_default()
                        .push(file);
                } else if let Some(tile) = s
                    .tiles
                    .iter_mut()
                    .find(|tile| tile.asid.as_deref() == Some(&asid))
                {
                    tile.slot.staged.push(file);
                } else {
                    s.toast_error(
                        "Attachment failed",
                        Some("The destination session was closed.".into()),
                    );
                }
            }
            Some(None) if s.mode != super::Mode::Home => {
                s.deferred_attachments
                    .entry((s.current_computer().to_owned(), None))
                    .or_default()
                    .push(file);
            }
            _ => s.staged.push(file),
        }
        return;
    }
    file.draft_token = Some(id);
    if has_image_pill(&s.draft, id) || has_image_pill(&s.start_draft, id) {
        s.staged.push(file);
    } else if let Some(tile) = s
        .tiles
        .iter_mut()
        .find(|tile| has_image_pill(&tile.slot.draft, id))
    {
        tile.slot.staged.push(file);
    }
    // A removed pill cancels staging even if the upload finishes later.
}

#[cfg(test)]
#[path = "composer_attachment_tests.rs"]
mod attachment_tests;
