//! The session (and Home) composer: draft, attachments, queue, model / mode /
//! branch pills, and the `@` / `$` / `/` popups.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use muqun_gateway::agents::{AgentSessionId, ModelRef};
use rune::{col, el, row, El, ImageCache, PillStyle, SurfaceFit, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem, PopupItem,
    PopupList, SegmentedControl, Switch, TextArea,
};

use super::art;
use super::{muted, OpenMenu, Slot, UiState};
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Source, Target};

/// Chip thumbnail size (wide enough to read the picture, short for the row).
const CHIP_THUMB_W: f32 = 56.0;
const CHIP_THUMB_H: f32 = 40.0;

/// The most file hits the `@` popup lists (App `FILE_MENTION_LIMIT` page is 20;
/// the panel shows a short head like the App's visible rows).
const MAX_FILE_ROWS: usize = 8;
/// The most `/` command rows.
const MAX_SLASH_ROWS: usize = 12;
/// The most `$` skill rows.
const MAX_SKILL_ROWS: usize = 12;

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

pub fn submit_session(s: &mut UiState) {
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
    let delivery = s.busy().then(|| "queue".to_string());
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
    let images = s.images.as_ref();
    let remove = Button::new(format!("{} ✕", file.name))
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Small)
        .on_press(move |s: &mut UiState| {
            if i < s.staged.len() {
                s.staged.remove(i);
            }
        })
        .build(k)
        .key(format!("staged-btn-{i}"));
    if file.image && Path::new(&file.path).is_file() {
        row()
            .key(format!("staged-{i}"))
            .gap(6.0)
            .center_y()
            .px(4.0)
            .py(2.0)
            .radius(k.radius.md)
            .fill(k.c.bg_base)
            .border(1.0, k.c.border_weak)
            .child(chip_thumb(
                k,
                images,
                &file.path,
                &format!("staged-thumb-{i}"),
            ))
            .child(remove)
    } else {
        remove.key(format!("staged-{i}"))
    }
}

fn queued_chip(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    item: &crate::model::compose::InboxItem,
) -> El {
    let id = item.id.clone();
    let open = s.menu == Some(OpenMenu::Queued(id.clone()));
    let label = format!(
        "{} {}",
        if item.steer { "Steer:" } else { "Queued:" },
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
            MenuItem::new("Edit")
                .on_select(move |s: &mut UiState| edit_queued(s, &edit_id, text.clone())),
        );
    }
    let (steer_id, next_steer) = (id.clone(), !item.steer);
    menu = menu.item(
        MenuItem::new(if item.steer {
            "Keep queued"
        } else {
            "Send next"
        })
        .on_select(move |s: &mut UiState| toggle_queued_steer(s, &steer_id, next_steer)),
    );
    let cancel_id = id.clone();
    let pill = menu
        .item(
            MenuItem::new("Cancel")
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
    let mut line = row().key("chips").w_full().gap(6.0).wrap().center_y();
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
        limit: Some(MAX_SLASH_ROWS),
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
        limit: Some(MAX_SKILL_ROWS),
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

fn mention_trigger(s: &UiState) -> Trigger {
    let mut peers: Vec<_> = s.peers.iter().collect();
    peers.sort_by_key(|p| match (p.current, p.reachable) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    });
    let mut items: Vec<TriggerItem> = Vec::new();
    if !peers.is_empty() {
        items.push(section_header("hdr:computers", "Computers"));
        items.extend(peers.into_iter().map(|p| {
            let detail = if p.current {
                "This computer"
            } else if p.reachable {
                "Computer"
            } else {
                "Offline"
            };
            let insert = format!("@{}", p.name);
            TriggerItem {
                // Empty search keeps headers out of fuzzy filter when term is set;
                // peers still match on name.
                search: vec![p.name.clone(), detail.to_string()],
                row: PopupRow {
                    id: format!("p:{}", p.name),
                    label: insert.clone(),
                    description: detail.into(),
                    insert,
                    disabled: !p.reachable,
                },
            }
        }));
    }
    let files: Vec<_> = s.file_hits.iter().take(MAX_FILE_ROWS).collect();
    if !files.is_empty() {
        items.push(section_header("hdr:files", "Files"));
        items.extend(files.into_iter().map(|h| TriggerItem {
            search: vec![h.name.clone(), h.path.clone()],
            row: PopupRow {
                id: format!("f:{}", h.path),
                label: h.name.clone(),
                description: h.path.clone(),
                insert: h.path.clone(),
                disabled: false,
            },
        }));
    }
    Trigger {
        char: '@',
        anchor: TriggerAnchor::Word,
        items,
        limit: Some(MAX_FILE_ROWS + 16),
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
    let query = state.query().cloned();
    let disabled: std::collections::HashSet<String> = state
        .rows()
        .iter()
        .filter(|r| r.disabled)
        .map(|r| r.id.clone())
        .collect();
    PopupList::<UiState>::new(key, "composer")
        .width(360.0)
        .label(label)
        .items(items)
        .on_choose(move |s: &mut UiState, id: &str, _: &mut Tx| {
            if disabled.contains(id) || id.starts_with("hdr:") {
                return;
            }
            let Some(query) = query.as_ref() else {
                return;
            };
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
            apply_pick(s, slot, query, &insert, kind);
        })
        .on_close(move |s: &mut UiState| {
            let caret = draft_caret(draft_of(s, slot));
            for (ch, anchor) in [
                ('/', TriggerAnchor::Start),
                ('$', TriggerAnchor::Start),
                ('@', TriggerAnchor::Word),
            ] {
                if let Some(q) = crate::model::composer_popup::scan_query(
                    &draft_of(s, slot).text,
                    caret,
                    ch,
                    anchor,
                ) {
                    s.popup_dismissed_at = Some(q.start);
                    return;
                }
            }
        })
        .build(k, layer);
}

/// Mount whichever popup the draft calls for, above the composer.
pub(super) fn popup(s: &UiState, k: &Kit, layer: &mut Vec<El>, slot: Slot) {
    let draft = draft_of(s, slot);
    let caret = Some(draft_caret(draft));
    let dismissed = s.popup_dismissed_at;
    let slash = read_popup(&draft.text, caret, &slash_trigger(s, slot), dismissed, true);
    if slash.open() {
        mount_popup(k, layer, "slash-popup", "Commands", slot, slash);
        return;
    }
    let skills = read_popup(&draft.text, caret, &skill_trigger(s, slot), dismissed, true);
    if skills.open() {
        mount_popup(k, layer, "skill-popup", "Skills", slot, skills);
        return;
    }
    let mentions = read_popup(&draft.text, caret, &mention_trigger(s), dismissed, true);
    if mentions.open() {
        mount_popup(k, layer, "mention-popup", "Mentions", slot, mentions);
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

fn switch_effort(s: &mut UiState, variant: Option<String>) {
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

fn mode_label(id: &str, catalog_name: Option<&str>) -> String {
    if let Some(name) = catalog_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != id)
    {
        return name.to_string();
    }
    super::labels::readable_catalog_id(id)
}

fn effort_menu(
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

fn pills(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let mut line = row().key("pills").gap(8.0).center_y().wrap();
    let Some(info) = s.current() else { return line };
    let catalog = s.catalog.as_ref();
    let model_infos: Vec<&muqun_gateway::agents::ModelInfo> = catalog
        .map(|c| c.catalog.models.iter().filter(|m| m.enabled).collect())
        .unwrap_or_default();
    let models = catalog.map(|c| catalog_models(c)).unwrap_or_default();
    let default_model = catalog.and_then(|c| c.catalog.defaults.model.as_ref());
    let selected_info = info
        .model
        .as_ref()
        .and_then(|m| super::labels::catalog_model_info(&model_infos, m, default_model));

    let agent_name = s.agent_name(&info.agent_id);
    line = line.child(
        row()
            .key("agent")
            .gap(6.0)
            .center_y()
            .px(8.0)
            .h(24.0)
            .radius(k.radius.sm)
            .fill(k.c.layer_03)
            .child(super::agents::mark(&info.agent_id, 12.0, s.dark))
            .child(rune::text(agent_name).font(k.ty.small).color(k.c.text_base)),
    );

    if let Some(model) = &info.model {
        let name = super::labels::format_model_name(
            model,
            "Model",
            selected_info.map(|m| m.name.as_str()),
        );
        let current = Some((model.provider_id.as_str(), model.model_id.as_str()));
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
                current,
                switch_model,
            )
        });
        if let Some(info) = selected_info {
            if let Some(variants) = info.variants.as_ref().filter(|v| !v.is_empty()) {
                let ids: Vec<String> = variants.iter().map(|v| v.id.clone()).collect();
                line = line.child(effort_menu(
                    k,
                    layer,
                    s.menu == Some(OpenMenu::Effort),
                    model.variant.as_deref(),
                    &ids,
                ));
            }
        }
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
    let modes: Vec<(String, String)> = catalog
        .map(|c| {
            c.catalog
                .modes
                .iter()
                .filter(|m| !m.hidden)
                .map(|m| (m.id.clone(), mode_label(&m.id, Some(m.name.as_str()))))
                .collect()
        })
        .unwrap_or_default();
    if (2..=4).contains(&modes.len()) {
        let labels: Vec<&str> = modes.iter().map(|(_, n)| n.as_str()).collect();
        let at = info.mode.as_ref().and_then(|m| {
            modes
                .iter()
                .position(|(id, _)| id == m || super::labels::bare_catalog_id(id) == m.as_str())
        });
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
        let label = modes
            .iter()
            .find(|(id, _)| id == mode)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| mode_label(mode, None));
        line = line.child(
            Badge::new(label)
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
    popup(s, k, layer, Slot::Session);
    let mut composer = col().key("composer").w_full().px(24.0).py(12.0).gap(8.0);
    if let Some(chips) = chips(s, k, layer) {
        composer = composer.child(chips);
    }
    let zh = s.cfg.general.language == crate::config::Language::ZhCn;
    let placeholder = if s.selected.is_some() {
        if zh {
            "发消息…（/ 命令，$ 技能，@ 文件）"
        } else {
            "Message…  (/ commands, $ skills, @ files)"
        }
    } else if zh {
        "先选一个会话"
    } else {
        "Pick a session first"
    };
    let accent = k.c.bg_accent;
    let info = k.c.info.icon;
    composer
        .child(
            TextArea::new(&s.draft)
                .placeholder(placeholder)
                .label("Draft")
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
                .on_submit(|s: &mut UiState, _: &mut Tx| submit_session(s))
                .on_input_paste(|s: &mut UiState, content, _: &mut Tx| take_paste(s, content))
                .build(k)
                .key("draft")
                .w_full(),
        )
        .child(actions)
        .child(muted(
            k,
            format!(
                "{} to send · {} to paste files or an image",
                crate::platform::keys::label("Primary+Enter"),
                crate::platform::keys::label("Primary+V"),
            ),
        ))
}
