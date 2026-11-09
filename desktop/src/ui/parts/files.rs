//! Files a call touched: the inline diff of an edit / write / patch (and a
//! `Diff` part), and the body of a read (file name over its folder,
//! syntax-coloured, line numbers stripped).

use muqun_gateway::agents::{AgentPart, TimelineItem, ToolCall};
use rune::{col, El};
use rune_kit::diff::{parse, FileDiff};
use serde_json::Value;

use super::{chip, glyphs, Tcx, ToolCard};
use crate::model::tool_view::{self as tv, ToolKind};

/// Lines a read or write shows before "N more lines".
pub const CODE_LINES: usize = 400;

/// A unified patch with its `---` / `+++` headers, so the parser reads it as
/// a file even when the engine sent hunks alone.
fn with_headers(path: &str, patch: &str) -> String {
    if patch.contains("\n+++ ") || patch.starts_with("+++ ") || patch.starts_with("diff --git") {
        return patch.to_string();
    }
    let body = if patch.contains("@@") {
        patch.to_string()
    } else {
        let removed = patch.lines().filter(|l| l.starts_with('-')).count();
        let added = patch.lines().filter(|l| l.starts_with('+')).count();
        let context = patch.lines().filter(|l| l.starts_with(' ')).count();
        format!(
            "@@ -1,{} +1,{} @@\n{patch}",
            removed + context,
            added + context
        )
    };
    format!("--- a/{path}\n+++ b/{path}\n{body}")
}

/// ACP diff items the core slimmed to their patch (`model::compact`).
fn acp_patches(call: &ToolCall) -> Vec<(String, String)> {
    call.content
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("diff"))
        .filter_map(|c| {
            let patch = c.get("patch").and_then(Value::as_str)?;
            let path = c.get("path").and_then(Value::as_str).unwrap_or("");
            Some((path.to_string(), patch.to_string()))
        })
        .collect()
}

/// ACP's `{type: "diff", path, oldText, newText}` content items.
fn acp_diffs(call: &ToolCall) -> Vec<(String, String, String)> {
    call.content
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("diff"))
        .filter_map(|c| {
            let path = c.get("path").and_then(Value::as_str)?;
            let old = c.get("oldText").and_then(Value::as_str).unwrap_or("");
            let new = c.get("newText").and_then(Value::as_str).unwrap_or("");
            Some((path.to_string(), old.to_string(), new.to_string()))
        })
        .collect()
}

pub type BuildFiles = Box<dyn FnOnce() -> Vec<FileDiff>>;

fn hash_of(parts: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

/// The change an item carries, as a hash of its source (to tell when it changed)
/// and a builder for the parsed files. App order: `metadata.files` (what the
/// tool did), ACP diff content, the edit's old/new strings, then
/// `apply_patch`'s own sections.
pub fn diff_source(item: &TimelineItem) -> Option<(u64, BuildFiles)> {
    match &item.part {
        AgentPart::Diff { file, diff } if !diff.trim().is_empty() => {
            let patch = with_headers(file, diff);
            Some((hash_of(&[&patch]), Box::new(move || parse(&patch))))
        }
        AgentPart::Tool(call) => call_diff_source(call),
        _ => None,
    }
}

fn call_diff_source(call: &ToolCall) -> Option<(u64, BuildFiles)> {
    let files = tv::call_diff_files(call);
    if !files.is_empty() {
        let patch = files
            .iter()
            .map(|f| with_headers(&f.path, &f.patch))
            .collect::<Vec<_>>()
            .join("\n");
        return Some((hash_of(&[&patch]), Box::new(move || parse(&patch))));
    }
    let patches = acp_patches(call);
    if !patches.is_empty() {
        let patch = patches
            .iter()
            .filter(|(_, p)| p.contains("\n@@ "))
            .map(|(_, p)| p.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if patch.is_empty() {
            return None;
        }
        return Some((hash_of(&[&patch]), Box::new(move || parse(&patch))));
    }
    let acp = acp_diffs(call);
    if !acp.is_empty() {
        let parts: Vec<&str> = acp
            .iter()
            .flat_map(|(p, o, n)| [p.as_str(), o.as_str(), n.as_str()])
            .collect();
        let source = hash_of(&parts);
        return Some((
            source,
            Box::new(move || {
                acp.iter()
                    .map(|(p, o, n)| FileDiff::from_texts(p.as_str(), o, n))
                    .collect()
            }),
        ));
    }
    let kind = tv::classify(&call.name);
    let rec = tv::input_record(call);
    if kind == ToolKind::Edit || call.name.eq_ignore_ascii_case("edit") {
        if let Some((old, new)) = tv::edit_strings(rec.as_ref()) {
            let path = tv::target(ToolKind::Edit, rec.as_ref());
            let source = hash_of(&[&path, &old, &new]);
            return Some((
                source,
                Box::new(move || vec![FileDiff::from_texts(path.as_str(), &old, &new)]),
            ));
        }
    }
    if kind == ToolKind::Patch {
        let text = rec.as_ref().and_then(|r| {
            ["patchText", "patch_text", "patch", "input"]
                .iter()
                .find_map(|k| r.get(*k).and_then(Value::as_str))
        })?;
        let patch = tv::parse_patch_sections(text)
            .into_iter()
            .filter(|s| !s.patch.is_empty())
            .map(|s| s.patch)
            .collect::<Vec<_>>()
            .join("\n");
        if !patch.is_empty() {
            return Some((hash_of(&[&patch]), Box::new(move || parse(&patch))));
        }
    }
    None
}

/// Lines and file headers a parsed diff draws, for the inline box's height.
fn diff_rows(cx: &Tcx, id: &str) -> Option<(usize, usize, usize)> {
    let files = cx.tl()?.diffs.get(id)?.files();
    if files.is_empty() {
        return None;
    }
    let lines: usize = files
        .iter()
        .map(|f| 1 + f.hunks.iter().map(|h| h.lines.len() + 1).sum::<usize>())
        .sum();
    let added = files.iter().map(|f| f.additions()).sum();
    let removed = files.iter().map(|f| f.deletions()).sum();
    Some((lines, added, removed))
}

/// The +N / −M chips of a change.
pub fn diff_chips(cx: &Tcx, added: usize, removed: usize) -> Vec<El> {
    let mut chips = Vec::new();
    if added > 0 {
        chips.push(chip(cx.k, format!("+{added}"), cx.k.c.success.fg));
    }
    if removed > 0 {
        chips.push(chip(cx.k, format!("−{removed}"), cx.k.c.danger.fg));
    }
    chips
}

/// Item `id`'s parsed diff, as rows inside its card.
pub fn inline_diff(cx: &Tcx, id: &str) -> Option<El> {
    let files = cx.tl()?.diffs.get(id)?.files();
    (!files.is_empty()).then(|| super::diff_rows::rows(cx, id, files))
}

/// A `Diff` part: the file and its change.
pub fn diff_part(cx: &Tcx, item: &TimelineItem, file: &str) -> El {
    let mut card = ToolCard::new(&item.id, &glyphs::EDIT_FILE, cx.t("Changed", "已更改"));
    card.title = tv::basename(file).to_string();
    card.caption = tv::dirname(file).to_string();
    card.open = Some(file.to_string());
    card.copy = Some(file.to_string());
    card.diff = Some(file.to_string());
    if let Some((_, a, r)) = diff_rows(cx, &item.id) {
        card.chips = diff_chips(cx, a, r);
    }
    card.body = inline_diff(cx, &item.id);
    card.build(cx)
}

/// The body of an edit / write / patch: its diff, else what was written.
pub fn change_body(
    cx: &Tcx,
    item: &TimelineItem,
    call: &ToolCall,
    kind: ToolKind,
) -> (Option<El>, Vec<El>) {
    if let Some((_, a, r)) = diff_rows(cx, &item.id) {
        return (inline_diff(cx, &item.id), diff_chips(cx, a, r));
    }
    let rec = tv::input_record(call);
    if kind == ToolKind::Write {
        if let Some(content) = rec
            .as_ref()
            .and_then(|r| r.get("content").or_else(|| r.get("contents")))
            .and_then(Value::as_str)
        {
            let path = tv::target(kind, rec.as_ref());
            let chips = vec![chip(
                cx.k,
                format!("+{}", content.lines().count()),
                cx.k.c.success.fg,
            )];
            let body = super::code(cx, "write", content, tv::fence_language(&path), CODE_LINES);
            return (Some(body), chips);
        }
    }
    let out = tv::output_text(call);
    let body = (!out.stdout.trim().is_empty()).then(|| super::mono(cx, "out", &out.stdout, 20));
    (body, Vec::new())
}

/// A read: the file's text, numbers stripped, coloured by its extension.
pub fn read_body(cx: &Tcx, call: &ToolCall, path: &str) -> Option<El> {
    let out = tv::output_text(call);
    if out.stdout.trim().is_empty() {
        return None;
    }
    let body = tv::strip_read_line_numbers(&out.stdout);
    let mut block = col().key("read").w_full().gap(6.0).child(super::code(
        cx,
        "file",
        &body,
        tv::fence_language(path),
        CODE_LINES,
    ));
    if out.truncated {
        block = block.child(super::caption(cx.k, cx.t("Truncated", "已截断")));
    }
    Some(block)
}
