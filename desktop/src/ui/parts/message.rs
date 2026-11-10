//! Prose parts: text (with its pictures), reasoning, and the small
//! session markers (status, compaction, model / mode / folder switches).

use muqun_gateway::agents::ports::agent::prompt_display_body;
use muqun_gateway::agents::{AgentPart, CompactionStatus, TimelineItem};
use rune::{col, icon, row, text, Cursor, El};
use rune_kit::Note;

use super::{caption, code, flip, glyphs, images, plate, prose, Tcx};

fn looks_like_note(body: &str) -> bool {
    ["```facts", "```steps", "```callout", ":::facts", ":::steps"]
        .iter()
        .any(|m| body.contains(m))
}

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Prose(String),
    Code { lang: String, code: String },
}

struct FenceOpen<'a> {
    char: char,
    len: usize,
    lang: &'a str,
    indent: usize,
}

fn parse_fence_open(line: &str) -> Option<FenceOpen<'_>> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }
    let mut chars = trimmed.chars();
    let fence_char = chars.next()?;
    if fence_char != '`' && fence_char != '~' {
        return None;
    }
    let mut fence_len = 1;
    for c in chars {
        if c == fence_char {
            fence_len += 1;
        } else {
            break;
        }
    }
    if fence_len < 3 {
        return None;
    }
    let rest = &trimmed[fence_len..];
    if fence_char == '`' && rest.contains('`') {
        return None;
    }
    let lang = rest.split_whitespace().next().unwrap_or("");
    Some(FenceOpen {
        char: fence_char,
        len: fence_len,
        lang,
        indent,
    })
}

fn is_fence_close(line: &str, fence_char: char, min_len: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return false;
    }
    let mut count = 0;
    for c in trimmed.chars() {
        if c == fence_char {
            count += 1;
        } else {
            break;
        }
    }
    if count < min_len {
        return false;
    }
    let rest = &trimmed[count..];
    rest.trim().is_empty()
}

fn parse_code_segments(body: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut prose_lines: Vec<&str> = Vec::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut active_fence: Option<FenceOpen<'_>> = None;

    for line in body.lines() {
        if let Some(fence) = &active_fence {
            if is_fence_close(line, fence.char, fence.len) {
                let lang = fence.lang.to_string();
                let code = code_lines.join("\n");
                segments.push(Segment::Code { lang, code });
                code_lines.clear();
                active_fence = None;
            } else {
                let mut stripped = line;
                for _ in 0..fence.indent {
                    if let Some(rest) = stripped.strip_prefix(' ') {
                        stripped = rest;
                    } else {
                        break;
                    }
                }
                code_lines.push(stripped.to_string());
            }
        } else if let Some(fence) = parse_fence_open(line) {
            if !prose_lines.is_empty() {
                let prose = prose_lines.join("\n");
                let trimmed = prose.trim_matches('\n');
                if !trimmed.trim().is_empty() {
                    segments.push(Segment::Prose(trimmed.to_string()));
                }
                prose_lines.clear();
            }
            active_fence = Some(fence);
        } else {
            prose_lines.push(line);
        }
    }

    if let Some(fence) = active_fence {
        let lang = fence.lang.to_string();
        let code = code_lines.join("\n");
        segments.push(Segment::Code { lang, code });
    } else if !prose_lines.is_empty() {
        let prose = prose_lines.join("\n");
        let trimmed = prose.trim_matches('\n');
        if !trimmed.trim().is_empty() {
            segments.push(Segment::Prose(trimmed.to_string()));
        }
    }

    segments
}

fn render_code_card(cx: &Tcx, id: &str, i: usize, lang: &str, code_str: &str) -> El {
    let display_lang = if lang.is_empty() { "code" } else { lang };
    let copy_code = code_str.to_string();
    let copy_label = crate::ui::copy::t_ref(cx.lang, "Copy");

    let copy_btn = rune_kit::Button::new(copy_label)
        .icon(&rune_kit::glyphs::COPY)
        .variant(rune_kit::ButtonVariant::Ghost)
        .control_size(rune_kit::ControlSize::Small)
        .on_press_tx(move |s: &mut crate::ui::UiState, tx: &mut rune::Tx| {
            tx.copy(copy_code.clone());
            s.toast(
                crate::ui::copy::t(s.cfg.general.language, "Copied code to clipboard"),
                None,
            );
        })
        .build(cx.k)
        .key(format!("{id}-copy-{i}"));

    let header = row()
        .key(format!("{id}-code-hdr-{i}"))
        .w_full()
        .center_y()
        .px(10.0)
        .py(4.0)
        .child(
            text(display_lang)
                .font(cx.k.ty.small)
                .color(cx.k.c.text_muted)
                .grow(1.0),
        )
        .child(copy_btn);

    let divider = rune::el()
        .key(format!("{id}-code-div-{i}"))
        .w_full()
        .h(1.0)
        .fill(cx.k.c.border_weak);

    let lang_opt = if lang.is_empty() || lang == "code" {
        None
    } else {
        Some(lang)
    };

    let body_el = col()
        .key(format!("{id}-code-body-wrap-{i}"))
        .w_full()
        .px(10.0)
        .py(8.0)
        .child(code(
            cx,
            &format!("{id}-code-body-{i}"),
            code_str,
            lang_opt,
            1000,
        ));

    col()
        .key(format!("{id}-code-{i}"))
        .w_full()
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(cx.k.c.layer_01.alpha(0.6))
        .border(1.0, cx.k.c.border_weak)
        .mt(4.0)
        .mb(4.0)
        .child(header)
        .child(divider)
        .child(body_el)
}

fn rich(cx: &Tcx, id: &str, body: &str) -> El {
    if looks_like_note(body) {
        return Note::new(body).key(id).build(cx.k);
    }
    let segments = parse_code_segments(body);
    let has_code = segments.iter().any(|s| matches!(s, Segment::Code { .. }));
    if !has_code {
        return prose(cx, id, body);
    }
    let mut block = col().key(format!("rich-{id}")).w_full().gap(4.0);
    for (i, seg) in segments.into_iter().enumerate() {
        match seg {
            Segment::Prose(text) => {
                if !text.trim().is_empty() {
                    block = block.child(prose(cx, &format!("{id}-p{i}"), &text));
                }
            }
            Segment::Code { lang, code } => {
                block = block.child(render_code_card(cx, id, i, &lang, &code));
            }
        }
    }
    block
}

/// Drop Codex-style `[Image #N]` (and `[图片 #N]`) tokens once the real pictures are shown.
pub fn strip_image_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, prefix_len)) = match (rest.find("[Image #"), rest.find("[图片 #")) {
        (Some(a), Some(b)) => {
            if a <= b {
                Some((a, "[Image #".len()))
            } else {
                Some((b, "[图片 #".len()))
            }
        }
        (Some(a), None) => Some((a, "[Image #".len())),
        (None, Some(b)) => Some((b, "[图片 #".len())),
        (None, None) => None,
    } {
        out.push_str(&rest[..at]);
        let after = &rest[at + prefix_len..];
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
    use super::{
        looks_like_note, parse_code_segments, strip_image_placeholders, strip_markdown_images,
        Segment,
    };

    #[test]
    fn picture_tokens_give_way_to_the_thumbnails() {
        for (given, want) in [
            ("[Image #1] what is this", "what is this"),
            ("[图片 #1] what is this", "what is this"),
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

    #[test]
    fn parses_plain_prose_without_code_blocks() {
        let text = "Hello world!\nThis is a simple message without code.";
        let segments = parse_code_segments(text);
        assert_eq!(segments, vec![Segment::Prose(text.to_string())]);
    }

    #[test]
    fn parses_single_code_block() {
        let text = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
        let segments = parse_code_segments(text);
        assert_eq!(
            segments,
            vec![Segment::Code {
                lang: "rust".to_string(),
                code: "fn main() {\n    println!(\"hi\");\n}".to_string(),
            }]
        );
    }

    #[test]
    fn parses_unspecified_lang_and_tilde_fence() {
        let text = "```\necho hello\n```\n~~~~python\nprint(123)\n~~~~";
        let segments = parse_code_segments(text);
        assert_eq!(
            segments,
            vec![
                Segment::Code {
                    lang: "".to_string(),
                    code: "echo hello".to_string(),
                },
                Segment::Code {
                    lang: "python".to_string(),
                    code: "print(123)".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_mixed_prose_and_code_segments() {
        let text = "Intro paragraph.\n\n```bash\nls -la\n```\n\nMiddle text.\n\n```json\n{\"ok\": true}\n```\n\nOutro.";
        let segments = parse_code_segments(text);
        assert_eq!(
            segments,
            vec![
                Segment::Prose("Intro paragraph.".to_string()),
                Segment::Code {
                    lang: "bash".to_string(),
                    code: "ls -la".to_string(),
                },
                Segment::Prose("Middle text.".to_string()),
                Segment::Code {
                    lang: "json".to_string(),
                    code: "{\"ok\": true}".to_string(),
                },
                Segment::Prose("Outro.".to_string()),
            ]
        );
    }

    #[test]
    fn parses_unclosed_code_fence_at_eof() {
        let text = "Streaming in progress:\n```typescript\nconst x = 42;";
        let segments = parse_code_segments(text);
        assert_eq!(
            segments,
            vec![
                Segment::Prose("Streaming in progress:".to_string()),
                Segment::Code {
                    lang: "typescript".to_string(),
                    code: "const x = 42;".to_string(),
                },
            ]
        );
    }

    #[test]
    fn strips_indentation_up_to_opening_fence_indent() {
        let text = "   ```rust\n   let a = 1;\n     let b = 2;\n   ```";
        let segments = parse_code_segments(text);
        assert_eq!(
            segments,
            vec![Segment::Code {
                lang: "rust".to_string(),
                code: "let a = 1;\n  let b = 2;".to_string(),
            }]
        );
    }

    #[test]
    fn ignores_inline_backticks() {
        let text = "Use `cargo test` to run tests and ``let x = 1`` in code.";
        let segments = parse_code_segments(text);
        assert_eq!(segments, vec![Segment::Prose(text.to_string())]);
    }

    #[test]
    fn looks_like_note_preserves_special_blocks() {
        assert!(looks_like_note("```facts\n- item 1\n```"));
        assert!(looks_like_note("```steps\n1. step\n```"));
        assert!(looks_like_note("```callout\nwarning\n```"));
        assert!(looks_like_note(":::facts\ncontent\n:::"));
        assert!(!looks_like_note("```rust\nfn main() {}\n```"));
    }
}
