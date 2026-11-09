//! Prose parts: text (with its pictures), reasoning, and the small
//! session markers (status, compaction, model / mode / folder switches).

use muqun_gateway::agents::ports::agent::prompt_display_body;
use muqun_gateway::agents::{AgentPart, CompactionStatus, TimelineItem};
use rune::{col, icon, row, text, Cursor, El};
use rune_kit::Note;

use super::{caption, flip, glyphs, images, plate, prose, Tcx};

fn looks_like_note(body: &str) -> bool {
    ["```facts", "```steps", "```callout", ":::facts", ":::steps"]
        .iter()
        .any(|m| body.contains(m))
}

fn rich(cx: &Tcx, id: &str, body: &str) -> El {
    if looks_like_note(body) {
        Note::new(body).key(id).build(cx.k)
    } else {
        prose(cx, id, body)
    }
}

/// Drop Codex-style `[Image #N]` tokens once the real pictures are shown.
pub fn strip_image_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("[Image #") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "[Image #".len()..];
        match after.find(']') {
            Some(end) => rest = after[end + 1..].trim_start(),
            None => {
                out.push_str(&rest[at..at + 1]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Drop markdown images (`![alt](src)`, `![alt](<src>)`) once the message's
/// pictures are drawn as thumbnails: Markdown would only show their alt text.
pub fn strip_markdown_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("![") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(close) = after.find("](") else {
            out.push_str(&rest[at..]);
            rest = "";
            break;
        };
        let dest = &after[close + 2..];
        let end = if let Some(inner) = dest.strip_prefix('<') {
            inner.find(">)").map(|e| e + 3)
        } else {
            dest.find(')').map(|e| e + 1)
        };
        match end {
            Some(end) if !after[..close].contains('\n') => rest = &dest[end..],
            _ => {
                out.push_str("![");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// A centred one-line marker between turns (a status, a switch).
fn marker(cx: &Tcx, key: String, label: String) -> El {
    row()
        .key(key)
        .w_full()
        .gap(10.0)
        .center_y()
        .py(2.0)
        .child(col().h(1.0).grow(1.0).fill(cx.k.c.border_weak))
        .child(
            text(label)
                .font(cx.k.ty.small)
                .color(cx.k.c.text_faint)
                .single_line()
                .truncate()
                .shrink(),
        )
        .child(col().h(1.0).grow(1.0).fill(cx.k.c.border_weak))
}

fn reasoning(cx: &Tcx, item: &TimelineItem, body: &str, duration_ms: Option<u64>) -> El {
    let k = cx.k;
    let id = item.id.as_str();
    let label = match duration_ms {
        Some(ms) => format!("{} · {:.1}s", cx.t("Thought", "思考"), ms as f32 / 1000.0),
        None => cx.t("Thought", "思考过程").to_string(),
    };
    if cx.pane {
        return col()
            .key(format!("think-{id}"))
            .w_full()
            .gap(12.0)
            .child(
                row()
                    .key("head")
                    .gap(8.0)
                    .center_y()
                    .child(icon(&glyphs::REASONING, 15.0).color(k.c.icon_muted))
                    .child(text(label).font(k.ty.ui_medium).color(k.c.text_base)),
            )
            .child(rich(cx, id, body));
    }
    // One dense line: what it thought, in a phrase; the pane has the rest.
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches(['#', '*', '_', ' '])
        .trim_end_matches(['*', '_'])
        .to_string();
    let mut line = row()
        .key("line")
        .w_full()
        .h(super::ROW_H)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .child(
            icon(&glyphs::REASONING, 13.0)
                .color(k.c.icon_faint)
                .a11y_hidden(),
        )
        .child(
            text(label)
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .single_line(),
        )
        .child(
            text(first)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .single_line()
                .truncate()
                .shrink()
                .grow(1.0),
        );
    let has_body = !body.trim().is_empty();
    let is_open = has_body && cx.is_open(id, false);
    if has_body {
        let turn = if is_open {
            0.0
        } else {
            -std::f32::consts::FRAC_PI_2
        };
        line = line.child(
            icon(&glyphs::CHEVRON, 11.0)
                .key("chevron")
                .color(k.c.icon_faint)
                .rotate(turn)
                .a11y_hidden(),
        );
    }
    if cx.selected(id) {
        line = line.fill(k.c.surface_raised_hover);
    }
    let (asid, open_id) = (cx.asid.to_string(), id.to_string());
    let line = k
        .interactive(line)
        .key(format!("think-{id}"))
        .cursor(Cursor::Pointer)
        .role("button")
        .expanded(is_open)
        .on_press_tx(move |s: &mut crate::ui::UiState, tx: &mut rune::Tx| {
            s.press_row_inline(&asid, &open_id, has_body, tx)
        });
    let mut block = col()
        .key(format!("think-block-{id}"))
        .w_full()
        .gap(4.0)
        .child(line);
    if is_open {
        block = block.child(
            col()
                .key("body")
                .w_full()
                .pl(27.0)
                .pb(8.0)
                .child(rich(cx, id, body)),
        );
    }
    block
}

fn compaction(
    cx: &Tcx,
    item: &TimelineItem,
    status: &CompactionStatus,
    reason: Option<&String>,
    summary: Option<&String>,
) -> El {
    let k = cx.k;
    let id = item.id.as_str();
    let state = match status {
        CompactionStatus::Running => cx.t("Compacting the conversation…", "正在压缩对话…"),
        CompactionStatus::Completed => cx.t("Conversation compacted", "对话已压缩"),
        CompactionStatus::Failed => cx.t("Compaction failed", "压缩失败"),
        _ => cx.t("Compaction", "压缩"),
    };
    let mut label = state.to_string();
    if let Some(r) = reason.filter(|r| !r.is_empty()) {
        label = format!("{label} · {r}");
    }
    let Some(summary) = summary.filter(|s| !s.trim().is_empty()) else {
        return marker(cx, format!("cmp-{id}"), label);
    };
    let open = cx.is_open(id, false);
    let mut block = plate(k, format!("cmp-{id}")).child(
        k.interactive(
            row()
                .key("head")
                .w_full()
                .px(10.0)
                .py(7.0)
                .gap(8.0)
                .center_y()
                .child(icon(&glyphs::TASK_LIST, 13.0).color(k.c.icon_muted))
                .child(
                    text(label)
                        .font(k.ty.small_medium)
                        .color(k.c.text_muted)
                        .grow(1.0)
                        .shrink(),
                )
                .child(
                    text(cx.t("Summary", "摘要"))
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                ),
        )
        .cursor(Cursor::Pointer)
        .on_press(flip(cx.asid, id)),
    );
    if open {
        block = block.child(
            col()
                .key("body")
                .px(12.0)
                .pb(12.0)
                .child(rich(cx, id, summary)),
        );
    }
    block
}

/// A text item's words as they should read and its pictures. Codex writes
/// `[Image #1]` where a picture was pasted, and an image an agent made
/// arrives as `![](file)`: the thumbnails stand for both.
pub fn text_parts(cx: &Tcx, item: &TimelineItem) -> (String, Option<El>) {
    let AgentPart::Text { text: body } = &item.part else {
        return (String::new(), None);
    };
    let user = item.role == muqun_gateway::agents::TimelineRole::User;
    if !images::carries_images(item) {
        return (body.clone(), None);
    }
    let paths = images::item_image_paths(cx.s, cx.asid, item);
    let pending = images::pending_images(item, paths.len());
    let body = strip_markdown_images(&strip_image_placeholders(prompt_display_body(body)));
    let height = if user { 150.0 } else { 240.0 };
    (
        body,
        images::gallery_sized(cx, &item.id, &paths, pending, height),
    )
}

/// Rendered text with the rich-note check, for callers outside this module.
pub fn rich_text(cx: &Tcx, id: &str, body: &str) -> El {
    rich(cx, id, body)
}

pub fn part(cx: &Tcx, item: &TimelineItem) -> El {
    let id = item.id.as_str();
    let k = cx.k;
    match &item.part {
        AgentPart::Text { .. } => {
            // An agent's picture follows its words (App MessageAttachments).
            let (body, gallery) = text_parts(cx, item);
            let mut block = col().key(format!("text-{id}")).gap(8.0).w_full();
            if !body.is_empty() {
                block = block.child(rich(cx, id, &body));
            }
            if let Some(g) = gallery {
                block = block.child(g);
            }
            block
        }
        AgentPart::Reasoning { text, duration_ms } => reasoning(cx, item, text, *duration_ms),
        AgentPart::Status { text } => marker(cx, format!("st-{id}"), text.clone()),
        AgentPart::Compaction {
            status,
            reason,
            summary,
            ..
        } => compaction(cx, item, status, reason.as_ref(), summary.as_ref()),
        AgentPart::Skill {
            skill,
            name,
            text: body,
        } => {
            let name = if name.is_empty() { skill } else { name };
            let open = cx.is_open(id, false);
            let mut card = plate(k, format!("skill-{id}")).child(
                k.interactive(
                    row()
                        .key("head")
                        .w_full()
                        .px(10.0)
                        .py(7.0)
                        .gap(8.0)
                        .center_y()
                        .child(icon(&glyphs::WORKFLOW, 13.0).color(k.c.icon_accent))
                        .child(
                            text(cx.t("Skill", "技能"))
                                .font(k.ty.small_medium)
                                .color(k.c.text_muted),
                        )
                        .child(
                            text(name.clone())
                                .font(k.ty.ui_compact)
                                .color(k.c.text_base)
                                .single_line()
                                .truncate()
                                .shrink(),
                        ),
                )
                .cursor(Cursor::Pointer)
                .on_press(flip(cx.asid, id)),
            );
            if open {
                card = card.child(
                    col()
                        .key("body")
                        .px(12.0)
                        .pb(12.0)
                        .child(rich(cx, id, body)),
                );
            }
            card
        }
        AgentPart::ModelSwitched { model, .. } => marker(
            cx,
            format!("ms-{id}"),
            format!(
                "{} {}",
                cx.t("Model →", "模型 →"),
                crate::ui::labels::format_model_name(model, &model.model_id, None)
            ),
        ),
        AgentPart::AgentSwitched { mode, .. } => marker(
            cx,
            format!("as-{id}"),
            format!(
                "{} {}",
                cx.t("Mode →", "模式 →"),
                crate::ui::labels::readable_catalog_id(mode)
            ),
        ),
        AgentPart::LocationSwitched { directory, .. } => marker(
            cx,
            format!("ls-{id}"),
            format!("{} {directory}", cx.t("Moved to", "已移到")),
        ),
        AgentPart::Synthetic { text, description } | AgentPart::System { text, description } => {
            let mut block = col().key(format!("sys-{id}")).w_full().gap(4.0);
            if let Some(d) = description.as_ref().filter(|d| !d.is_empty()) {
                block = block.child(caption(k, d.clone()));
            }
            block.child(rich(cx, id, text))
        }
        // Drawn by their own modules; listed for exhaustiveness.
        AgentPart::Tool(_)
        | AgentPart::Approval { .. }
        | AgentPart::Form { .. }
        | AgentPart::Diff { .. }
        | AgentPart::Todo { .. }
        | AgentPart::Shell { .. } => col(),
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_image_placeholders, strip_markdown_images};

    #[test]
    fn picture_tokens_give_way_to_the_thumbnails() {
        for (given, want) in [
            ("[Image #1] what is this", "what is this"),
            ("before [Image #2] after", "before after"),
            ("![](<file:///tmp/a b.png>)", ""),
            ("see ![chart](out/flow.png) here", "see  here"),
            ("[link](x.png) stays", "[link](x.png) stays"),
            ("unclosed ![alt", "unclosed ![alt"),
        ] {
            assert_eq!(
                strip_markdown_images(&strip_image_placeholders(given)),
                want,
                "{given}"
            );
        }
    }
}
