//! The right-hand detail pane: a timeline row's whole story (a command's
//! output, a diff, a file, a subagent's result, a thought), opened by a
//! click or Enter so the timeline itself never jumps. Also the rows'
//! keyboard (j / k, Enter, Space, [ / ], Escape) and their context menu.
//!
//! The pane is its own component: the window puts it in the right-hand
//! slot (`ui::shell`), which other panels may share.

use muqun_gateway::agents::{AgentPart, TimelineItem, TimelineRole};
use rune::{col, icon, row, text, Cursor, El, PointerPress, ScrollSpec, Tx};
use rune_kit::{context_menu, Kit, MenuItem, ScrollBar};

use super::{glyphs, Tcx};
use crate::model::activity::{self, Block};
use crate::model::tool_view as tv;
use crate::ui::timeline::{group_key, SessionTimeline};
use crate::ui::UiState;

/// The row open in the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspect {
    pub asid: String,
    pub id: String,
}

/// A row's context menu, at the pointer.
#[derive(Debug, Clone)]
pub struct RowMenu {
    pub asid: String,
    pub id: String,
    pub point: PointerPress,
    pub selection: Option<String>,
}

/// Rows the keyboard walks: activity rows and group lines, and the calls of
/// an open group.
fn walkable(tl: &SessionTimeline) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (b, block) in tl.blocks.iter().enumerate() {
        match *block {
            Block::Item(i) => {
                let item = &tl.items[i];
                if activity::family(item).is_some() || matches!(item.part, AgentPart::Tool(_)) {
                    out.push((item.id.clone(), b));
                }
            }
            Block::Group { start, end } => {
                let key = group_key(&tl.items[start]);
                let open = tl.is_open(&key, false);
                out.push((key, b));
                if open {
                    for item in &tl.items[start..end] {
                        if !activity::invisible(item) {
                            out.push((item.id.clone(), b));
                        }
                    }
                }
            }
        }
    }
    out
}

/// The virtual list's key of block `b` (what scrolling reveals).
fn block_key(tl: &SessionTimeline, b: usize) -> Option<String> {
    match *tl.blocks.get(b)? {
        Block::Item(i) => Some(tl.items[i].id.clone()),
        Block::Group { start, .. } => Some(group_key(&tl.items[start])),
    }
}

fn item<'a>(tl: &'a SessionTimeline, id: &str) -> Option<&'a TimelineItem> {
    tl.items.iter().rev().find(|i| i.id == id)
}

/// Whether a row has more to show than its line.
fn has_detail(item: &TimelineItem) -> bool {
    match &item.part {
        AgentPart::Reasoning { text, .. } => !text.trim().is_empty(),
        AgentPart::Tool(_) | AgentPart::Shell { .. } | AgentPart::Diff { .. } => true,
        _ => false,
    }
}

impl UiState {
    /// The session the keyboard belongs to.
    fn keyboard_asid(&self) -> Option<String> {
        if self.in_split() {
            self.tiles.get(self.focus).and_then(|t| t.asid.clone())
        } else {
            self.selected.clone()
        }
    }

    /// A row was pressed: it becomes the selection, and its details open in
    /// the pane (pressed again, they close).
    #[allow(dead_code)]
    pub fn press_row(&mut self, asid: &str, id: &str, detail: bool, tx: &mut Tx) {
        tx.blur();
        self.draft.focused = false;
        if let Some(tl) = self.timelines.get_mut(asid) {
            tl.selected = Some(id.to_string());
        }
        let this = Inspect {
            asid: asid.to_string(),
            id: id.to_string(),
        };
        if self.inspect.as_ref() == Some(&this) {
            self.close_inspect();
        } else if detail {
            self.inspect = Some(this);
            self.inspect_pane.set_open(true);
        }
    }

    /// A timeline row was pressed: it becomes the selection and flips its inline
    /// body open or closed. If the detail pane is already open, it follows the selection.
    pub fn press_row_inline(&mut self, asid: &str, id: &str, has_body: bool, tx: &mut Tx) {
        tx.blur();
        self.draft.focused = false;
        if let Some(tl) = self.timelines.get_mut(asid) {
            tl.selected = Some(id.to_string());
            if has_body {
                tl.flip(id);
                tl.rev += 1;
            }
        }
        if self.inspect.as_ref().is_some_and(|i| i.asid == asid) {
            self.inspect = Some(Inspect {
                asid: asid.to_string(),
                id: id.to_string(),
            });
        }
    }

    pub fn close_inspect(&mut self) {
        self.inspect = None;
        self.inspect_pane.set_open(false);
    }

    /// A group line was pressed: it opens or folds, and is the selection.
    pub fn toggle_group(&mut self, asid: &str, key: &str, tx: &mut Tx) {
        tx.blur();
        self.draft.focused = false;
        if let Some(tl) = self.timelines.get_mut(asid) {
            tl.flip(key);
            tl.selected = Some(key.to_string());
            tl.rev += 1;
        }
    }

    /// j / k: the next or previous row, revealed (and shown in the pane
    /// when the pane is open).
    fn step_row(&mut self, forward: bool, tx: &mut Tx) -> bool {
        let Some(asid) = self.keyboard_asid() else {
            return false;
        };
        let Some(tl) = self.timelines.get_mut(&asid) else {
            return false;
        };
        let rows = walkable(tl);
        if rows.is_empty() {
            return true;
        }
        let at = tl
            .selected
            .as_ref()
            .and_then(|sel| rows.iter().position(|(id, _)| id == sel));
        let next = match (at, forward) {
            (None, _) => rows.len() - 1,
            (Some(i), true) => (i + 1).min(rows.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
        };
        let (id, b) = rows[next].clone();
        tl.selected = Some(id.clone());
        if let Some(key) = block_key(tl, b) {
            tx.scroll_into_view(key);
        }
        let detail = item(tl, &id).is_some_and(has_detail);
        if self.inspect.as_ref().is_some_and(|i| i.asid == asid) && detail {
            self.inspect = Some(Inspect { asid, id });
        }
        true
    }

    /// [ / ]: the previous or next message (the owner's or the agent's words).
    fn step_message(&mut self, forward: bool, tx: &mut Tx) -> bool {
        let Some(asid) = self.keyboard_asid() else {
            return false;
        };
        let Some(tl) = self.timelines.get_mut(&asid) else {
            return false;
        };
        let words: Vec<usize> = tl
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(b, block)| match *block {
                Block::Item(i)
                    if matches!(tl.items[i].part, AgentPart::Text { .. })
                        && matches!(
                            tl.items[i].role,
                            TimelineRole::User | TimelineRole::Assistant
                        ) =>
                {
                    Some(b)
                }
                _ => None,
            })
            .collect();
        if words.is_empty() {
            return true;
        }
        let here = tl.selected.as_ref().and_then(|sel| {
            tl.blocks.iter().position(|block| match *block {
                Block::Item(i) => &tl.items[i].id == sel,
                Block::Group { start, end } => {
                    &group_key(&tl.items[start]) == sel
                        || tl.items[start..end].iter().any(|i| &i.id == sel)
                }
            })
        });
        let target = match (here, forward) {
            (None, _) => *words.last().unwrap_or(&0),
            (Some(h), true) => words.iter().copied().find(|b| *b > h).unwrap_or(h),
            (Some(h), false) => words.iter().rev().copied().find(|b| *b < h).unwrap_or(h),
        };
        if let Some(key) = block_key(tl, target) {
            tl.selected = Some(key.clone());
            tx.scroll_into_view(key);
        }
        true
    }

    /// Enter: open the selection's details (a group line opens the group).
    fn open_selected(&mut self, tx: &mut Tx) -> bool {
        let Some(asid) = self.keyboard_asid() else {
            return false;
        };
        let Some(sel) = self.timelines.get(&asid).and_then(|t| t.selected.clone()) else {
            return false;
        };
        if sel.starts_with("grp-") {
            self.toggle_group(&asid, &sel, tx);
            return true;
        }
        let item_opt = self.timelines.get(&asid).and_then(|t| item(t, &sel));
        if let Some(item) = item_opt {
            let (_copy, open) = row_targets(item);
            if let AgentPart::Tool(call) = &item.part {
                if activity::tool_kind(call) == tv::ToolKind::Read {
                    if let Some(path) = open {
                        self.open_timeline_file(&path);
                        return true;
                    }
                }
            }
        }
        let detail = item_opt.is_some_and(has_detail);
        if detail {
            self.inspect = Some(Inspect { asid, id: sel });
            self.inspect_pane.set_open(true);
        }
        true
    }

    /// Space: fold or open the group the selection is in.
    fn toggle_selected_group(&mut self, tx: &mut Tx) -> bool {
        let Some(asid) = self.keyboard_asid() else {
            return false;
        };
        let Some(tl) = self.timelines.get(&asid) else {
            return false;
        };
        let Some(sel) = tl.selected.clone() else {
            return false;
        };
        let key = tl.blocks.iter().find_map(|block| match *block {
            Block::Group { start, end } => {
                let key = group_key(&tl.items[start]);
                (key == sel || tl.items[start..end].iter().any(|i| i.id == sel)).then_some(key)
            }
            _ => None,
        });
        match key {
            Some(key) => {
                self.toggle_group(&asid, &key, tx);
                if let Some(tl) = self.timelines.get_mut(&asid) {
                    tl.selected = Some(key);
                }
                true
            }
            None => false,
        }
    }

    /// A timeline key from the keymap (`ui/keymap.rs`), when no text field
    /// has it. `false` leaves the key to whoever is next.
    pub fn timeline_key(&mut self, act: crate::ui::keymap::Action, tx: &mut Tx) -> bool {
        use crate::ui::keymap::Action;
        if self.mode != crate::ui::Mode::Session || self.zoom.is_some() || self.palette.is_some() {
            return false;
        }
        match act {
            Action::OpenRow => self.open_selected(tx),
            Action::ToggleRow => self.toggle_selected_group(tx),
            Action::NextRow => self.step_row(true, tx),
            Action::PrevRow => self.step_row(false, tx),
            Action::NextMessage => self.step_message(true, tx),
            Action::PrevMessage => self.step_message(false, tx),
            _ => false,
        }
    }
}

/// What a row's Copy takes and the file its Open shows.
fn row_targets(item: &TimelineItem) -> (Option<String>, Option<String>) {
    match &item.part {
        AgentPart::Text { text } => {
            let body = if item.role == TimelineRole::User {
                muqun_gateway::agents::ports::agent::prompt_display_body(text).to_string()
            } else {
                text.clone()
            };
            ((!body.trim().is_empty()).then_some(body), None)
        }
        AgentPart::Tool(call) => {
            let kind = activity::tool_kind(call);
            let target = tv::target(kind, tv::input_record(call).as_ref());
            let file = matches!(
                kind,
                tv::ToolKind::Read | tv::ToolKind::Edit | tv::ToolKind::Write | tv::ToolKind::Patch
            ) && !target.is_empty();
            (
                (!target.is_empty()).then(|| target.clone()),
                file.then_some(target),
            )
        }
        AgentPart::Shell { command, .. } => (Some(command.clone()), None),
        AgentPart::Diff { file, .. } => (Some(file.clone()), Some(file.clone())),
        AgentPart::Reasoning { text, .. } => (Some(text.clone()), None),
        _ => (None, None),
    }
}

/// The open row's context menu, mounted on the window's layer.
pub fn row_menu(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(menu) = s.row_menu.as_ref() else {
        return;
    };
    let found = s.timelines.get(&menu.asid).and_then(|t| item(t, &menu.id));
    let t = |en| crate::ui::copy::tr_ref(s.cfg.general.language, en);
    let close = |s: &mut UiState| s.row_menu = None;
    let (asid, id) = (menu.asid.clone(), menu.id.clone());
    let detail = found.is_some_and(has_detail);
    let (copy, open) = found.map_or((None, None), row_targets);
    let selection = menu.selection.clone().filter(|s| !s.trim().is_empty());

    let is_text = found.is_some_and(|f| matches!(f.part, AgentPart::Text { .. }));
    let is_user_text = found
        .is_some_and(|f| f.role == TimelineRole::User && matches!(f.part, AgentPart::Text { .. }));
    let has_items =
        selection.is_some() || copy.is_some() || detail || open.is_some() || is_user_text;
    if !has_items {
        return;
    }

    let mut m = context_menu(
        "row-menu",
        col().key("row-menu-anchor").size(0.0, 0.0),
        Some(menu.point),
    )
    .min_width(180.0)
    .on_close(close);

    let copy_shortcut = crate::platform::keys::label("Primary+C");

    // Selected text operations take precedence if user highlighted a section.
    if let Some(selected) = selection.clone() {
        let sel_copy = selected.clone();
        m = m.item(
            MenuItem::new(t("Copy"))
                .shortcut(&copy_shortcut)
                .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.row_menu = None;
                    tx.copy(sel_copy.clone());
                }),
        );
        let sel_quote = selected;
        m = m.item(
            MenuItem::new(t("Quote to composer")).on_select(move |s: &mut UiState| {
                s.row_menu = None;
                s.quote_to_composer(&sel_quote);
            }),
        );
        m = m.separator();
    }

    if let Some(copy_text) = copy.filter(|c| !c.is_empty()) {
        let label = if selection.is_some() {
            t("Copy entire message")
        } else {
            t("Copy")
        };
        let mut mi = MenuItem::new(label);
        if selection.is_none() {
            mi = mi.shortcut(&copy_shortcut);
        }
        let copy_all = copy_text.clone();
        m = m.item(mi.on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
            s.row_menu = None;
            tx.copy(copy_all.clone());
        }));

        if selection.is_none() && is_text {
            let quote_all = copy_text;
            m = m.item(
                MenuItem::new(t("Quote to composer")).on_select(move |s: &mut UiState| {
                    s.row_menu = None;
                    s.quote_to_composer(&quote_all);
                }),
            );
        }
    }

    if is_user_text {
        if let Some(f) = found {
            let rollback_asid = muqun_gateway::agents::AgentSessionId(asid.clone());
            let rollback_id = f.message_id.clone();
            m = m.separator();
            m = m.item(
                MenuItem::new(t("Roll back to here")).on_select(move |s: &mut UiState| {
                    s.row_menu = None;
                    s.send(crate::model::commands::Command::StageRevert {
                        asid: rollback_asid.clone(),
                        message_id: rollback_id.clone(),
                    });
                }),
            );
            let fork_asid = muqun_gateway::agents::AgentSessionId(asid.clone());
            let fork_id = f.message_id.clone();
            m = m.item(
                MenuItem::new(t("Fork from here")).on_select(move |s: &mut UiState| {
                    s.row_menu = None;
                    s.send(crate::model::commands::Command::ForkSession {
                        asid: fork_asid.clone(),
                        message_id: Some(fork_id.clone()),
                    });
                }),
            );
        }
    }

    if detail {
        m = m.item(
            MenuItem::new(t("Show details"))
                .shortcut("Enter")
                .on_select(move |s: &mut UiState| {
                    s.row_menu = None;
                    s.inspect = Some(Inspect {
                        asid: asid.clone(),
                        id: id.clone(),
                    });
                    s.inspect_pane.set_open(true);
                }),
        );
    }
    if let Some(path) = open {
        m = m.item(
            MenuItem::new(t("Open file")).on_select(move |s: &mut UiState| {
                s.row_menu = None;
                s.open_timeline_file(&path);
            }),
        );
    }
    let anchor = m.build(k, layer);
    layer.push(anchor);
}

/// The pane's content, when a row is open in it.
pub fn view(s: &UiState, k: &Kit) -> Option<El> {
    let open = s.inspect.as_ref()?;
    let found = s
        .timelines
        .get(&open.asid)
        .and_then(|t| item(t, &open.id))?;
    let cx = Tcx::for_pane(s, k, &open.asid);
    let bar = row()
        .key("pane-bar")
        .w_full()
        // The main card's toolbar row, so the two line up.
        .h(rune_kit::shell::TOOLBAR_HEIGHT)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .child(
            text(cx.t("Details"))
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .grow(1.0),
        )
        .child(
            k.interactive(
                row()
                    .key("pane-close")
                    .size(24.0, 24.0)
                    .center()
                    .radius(k.radius.sm)
                    .child(icon(&glyphs::CLOSE, 13.0).color(k.c.icon_muted)),
            )
            .cursor(Cursor::Pointer)
            .role("button")
            .label(cx.t("Close"))
            .help(cx.t("Close (Esc)"))
            .on_press(|s: &mut UiState| s.close_inspect()),
        );
    let body = col()
        .key(format!("pane-body-{}", found.id))
        .w_full()
        .px(16.0)
        .pt(4.0)
        .pb(24.0)
        .child(super::part(&cx, found));
    Some(
        col()
            .key("detail-pane")
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(bar)
            .child(
                col()
                    .key("pane-scroll")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .child(body)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_bar(k),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Tx;

    #[test]
    fn press_row_inline_toggles_flip_without_opening_inspect_pane() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let asid = "s1";
        s.timelines
            .insert(asid.to_string(), SessionTimeline::default());
        let mut rune_tx = Tx::default();

        // 1. Initial state: timeline not flipped, inspect is None
        assert!(!s.timelines[asid].is_open("row-1", false));
        assert_eq!(s.inspect, None);

        // 2. Pressing row inline with has_body=true flips row open, but does NOT set inspect
        s.press_row_inline(asid, "row-1", true, &mut rune_tx);
        assert!(s.timelines[asid].is_open("row-1", false));
        assert_eq!(s.timelines[asid].selected.as_deref(), Some("row-1"));
        assert_eq!(s.inspect, None);

        // 3. Pressing row inline again collapses it, inspect remains None
        s.press_row_inline(asid, "row-1", true, &mut rune_tx);
        assert!(!s.timelines[asid].is_open("row-1", false));
        assert_eq!(s.inspect, None);
    }

    #[test]
    fn press_row_inline_tracks_already_open_inspect_pane() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let asid = "s1";
        s.timelines
            .insert(asid.to_string(), SessionTimeline::default());
        let mut rune_tx = Tx::default();

        // Inspect pane was explicitly opened for row-1
        s.inspect = Some(Inspect {
            asid: asid.to_string(),
            id: "row-1".to_string(),
        });
        s.inspect_pane.set_open(true);

        // Clicking row-2 inline should flip row-2 AND update inspect selection
        s.press_row_inline(asid, "row-2", true, &mut rune_tx);
        assert!(s.timelines[asid].is_open("row-2", false));
        assert_eq!(
            s.inspect,
            Some(Inspect {
                asid: asid.to_string(),
                id: "row-2".to_string(),
            })
        );
        assert!(s.inspect_pane.is_open());
    }

    #[test]
    fn open_selected_for_read_tool_opens_file() {
        use muqun_gateway::agents::{AgentPart, TimelineItem, TimelineRole, ToolCall};
        use serde_json::json;

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let asid = "s1";
        s.selected = Some(asid.to_string());

        let call: ToolCall = serde_json::from_value(json!({
            "id": "call_read_1",
            "name": "read_file",
            "state": "completed",
            "status": "completed",
            "input": {"path": "/tmp/test_file.rs"},
            "output": "fn main() {}",
        }))
        .expect("valid tool call");

        let item = TimelineItem {
            id: "row-read".into(),
            message_id: "m-1".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Tool(call),
            seq: 0,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        };

        let mut tl = SessionTimeline::default();
        tl.items.push(item);
        tl.selected = Some("row-read".to_string());
        s.timelines.insert(asid.to_string(), tl);

        let mut rune_tx = Tx::default();
        let handled = s.open_selected(&mut rune_tx);
        assert!(handled);
        assert_eq!(s.inspect, None);
    }

    #[test]
    fn row_menu_text_selection_and_quote() {
        use muqun_gateway::agents::{AgentPart, TimelineItem, TimelineRole};
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let asid = "s1";
        s.selected = Some(asid.to_string());

        let item = TimelineItem {
            id: "msg-1".into(),
            message_id: "m-1".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text {
                text: "Here is the full solution for your problem.".into(),
            },
            seq: 0,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        };

        let mut tl = SessionTimeline::default();
        tl.items.push(item);
        s.timelines.insert(asid.to_string(), tl);

        // 1. Right click with highlighted selection
        s.row_menu = Some(RowMenu {
            asid: asid.to_string(),
            id: "msg-1".to_string(),
            point: rune::PointerPress { x: 100.0, y: 100.0 },
            selection: Some("full solution".into()),
        });

        let k = Kit::neutral(false);
        let mut layer = Vec::new();
        row_menu(&s, &k, &mut layer);
        assert!(!layer.is_empty());

        // 2. Quote to composer works
        s.quote_to_composer("full solution");
        assert_eq!(s.draft.text, "> full solution\n\n");
        assert!(s.draft.focused);

        // Appending another quote
        s.quote_to_composer("second line\nthird line");
        assert_eq!(
            s.draft.text,
            "> full solution\n\n> second line\n> third line\n\n"
        );

        // 3. Right click with no selection on user message
        s.timelines.get_mut(asid).unwrap().items.push(TimelineItem {
            id: "user-msg".into(),
            message_id: "m-user".into(),
            role: TimelineRole::User,
            part: AgentPart::Text {
                text: "Please fix this now".into(),
            },
            seq: 1,
            updated_ms: 0,
            ordinal: 1,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        });

        s.row_menu = Some(RowMenu {
            asid: asid.to_string(),
            id: "user-msg".to_string(),
            point: rune::PointerPress { x: 100.0, y: 100.0 },
            selection: None,
        });

        let mut layer_user = Vec::new();
        row_menu(&s, &k, &mut layer_user);
        assert!(!layer_user.is_empty());
    }
}
